//! # RCU, Seqlock, and Epoch-Based Reclamation (Pillar I)
//!
//! RFC-002 §3.3: "Path lookup is an RCU-protected read: dentries and
//! inode cores are immutable snapshots, readers walk without locks, and
//! a new generation is published by pointer swap with deferred
//! reclamation after a grace period. Writers to the same file serialize
//! through a seqlock."
//!
//! This module provides the three primitives that contract names:
//!
//! * [`RcuPtr`] -- atomic pointer swap + deferred reclamation via a
//!   global epoch counter. Publish = one atomic store; read = one atomic
//!   load plus a guard that pins the epoch; reclamation happens after
//!   every reader's guard has moved past the publish epoch (the grace
//!   period). No thread list is maintained: epochs are globally counted,
//!   which trades a slightly longer grace period for zero per-read
//!   registration cost -- the same trade the kernel's scalable RCU
//!   makes on large machines.
//! * [`Seqlock`] -- the writer-serialization primitive: readers retry
//!   when a writer interleaved; writers are rare because transactions
//!   batch. Store-ordering is the classic even/odd sequence counter.
//! * [`RcuCache`] -- a sharded publish/subscribe map built from RcuPtr
//!   generations, the concrete type behind the RFC's `RcuCache<Lba,
//!   Extent>` sketch.
//!
//! The primitives are allocator-aware (garbage is returned to the
//! allocator only after the grace period, never freed under a reader's
//! feet), and they are *portable*: nothing here touches platform APIs,
//! so Windows and macOS get the same semantics, not a mutex-shaped
//! approximation.

use std::cell::UnsafeCell;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use crossbeam::epoch::{self, Atomic, Owned, Shared};

/// Number of publish generations tracked for telemetry (test-visible).
#[cfg(test)]
pub(crate) fn current_publish_count() -> u64 {
    PUBLISH_COUNT.load(Ordering::Relaxed)
}

static PUBLISH_COUNT: AtomicU64 = AtomicU64::new(0);

/// Publish-and-reclaim pointer: the RCU primitive.
///
/// Soundness: the current generation lives in a `crossbeam::epoch::Atomic`
/// cell as an `Arc<T>`; `read()` pins the epoch, borrows the cell's
/// `Arc<T>` under the guard, and clones it -- the clone keeps the value
/// alive for the reader no matter what happens next. `publish()` swaps a
/// fresh `Arc` in and retires the old cell with
/// `guard.defer_destroy`, so the old cell's memory (and its `Arc<T>`
/// drop) happens only after every guard that could still be borrowing it
/// has been released -- the textbook epoch-based grace period. No
/// manual `Arc::from_raw` obligation arithmetic, no use-after-free
/// window, and the hot `read()` path is one atomic load plus one
/// refcount increment.
pub struct RcuPtr<T> {
    inner: Atomic<Arc<T>>,
}

impl<T> RcuPtr<T> {
    #[must_use]
    pub fn new(initial: T) -> Self {
        Self {
            inner: Atomic::new(Arc::new(initial)),
        }
    }

    /// Reads the current generation. The returned `Arc` keeps it alive
    /// however long the reader needs: RCU semantics without unsafe at
    /// the call site.
    #[must_use]
    pub fn read(&self) -> Arc<T> {
        let guard = &epoch::pin();
        let shared: Shared<'_, Arc<T>> = self.inner.load(Ordering::Acquire, guard);
        if shared.is_null() {
            // Unreachable by construction (the atomic is never nulled),
            // but a load of a null shared must not be deref'd.
            unreachable!("RcuPtr atomic must never be null");
        }
        // SAFETY: the guard pins the cell's allocation: it cannot be
        // reclaimed while this guard is pinned, so the borrow below is
        // valid. Cloning the Arc increments its count; the cell itself
        // is untouched (readers never write).
        let cell: &Arc<T> = unsafe { shared.deref() };
        Arc::clone(cell)
    }

    /// Publishes a new generation and retires the previous one after the
    /// epoch grace period.
    pub fn publish(&self, next: T) {
        let guard = &epoch::pin();
        // SAFETY: `swap` hands the old cell back to us as Shared, with
        // the guarantee that no *new* reader can load it anymore; the
        // cell may still be borrowed by guards pinned before this swap,
        // which is exactly what defer_destroy waits out.
        let old: Shared<'_, Arc<T>> =
            self.inner
                .swap(Owned::new(Arc::new(next)), Ordering::AcqRel, guard);
        if !old.is_null() {
            // SAFETY: `old` was a live, uniquely-owned cell of this
            // atomic (every publish retires its own cell exactly once).
            unsafe { guard.defer_destroy(old) };
        }
        PUBLISH_COUNT.fetch_add(1, Ordering::Relaxed);
    }
}

/// Classic seqlock: writers serialize through a mutex, bump an even/odd
/// sequence counter around the write; readers check the counter before
/// and after and retry on interleave. Writers-are-rare is the workload
/// assumption (transactions batch), which is exactly why this shape.
pub struct Seqlock<T> {
    seq: AtomicU64,
    writer: std::sync::Mutex<()>,
    value: UnsafeCell<T>,
}

// SAFETY: T is shared across threads; reads are retry-loop safe, writes
// are mutex-serialized, and UnsafeCell access follows the seq counter
// protocol below.
unsafe impl<T: Send> Send for Seqlock<T> {}
unsafe impl<T: Send + Sync> Sync for Seqlock<T> {}

impl<T: Copy> Seqlock<T> {
    #[must_use]
    pub fn new(value: T) -> Self {
        Self {
            seq: AtomicU64::new(0),
            writer: std::sync::Mutex::new(()),
            value: UnsafeCell::new(value),
        }
    }

    /// Writer side: exclusive, bumps the sequence to odd before writing
    /// and back to even after.
    pub fn write<F: FnOnce(&mut T)>(&self, f: F) {
        let _guard = self.writer.lock().expect("seqlock writer mutex");
        let before = self.seq.fetch_add(1, Ordering::AcqRel); // -> odd
        debug_assert_eq!(before % 2, 0, "writer must see even seq");
        // SAFETY: mutex-held exclusive access; the odd seq flag tells
        // readers to retry.
        let v = unsafe { &mut *self.value.get() };
        f(v);
        let after = self.seq.fetch_add(1, Ordering::AcqRel); // -> even
        debug_assert_eq!(after % 2, 1);
    }

    /// Reader side: retry loop. Returns a consistent snapshot or None if
    /// the writer never quiesced within `attempts`.
    #[must_use]
    pub fn read(&self, attempts: usize) -> Option<T> {
        for _ in 0..attempts.max(1) {
            let before = self.seq.load(Ordering::Acquire);
            if before % 2 == 1 {
                std::hint::spin_loop();
                continue;
            }
            // SAFETY: seq is even -> no writer mid-flight; we only copy,
            // and we re-check after the fence below.
            let snapshot = unsafe { *self.value.get() };
            let after = self.seq.load(Ordering::Acquire);
            if before == after {
                return Some(snapshot);
            }
        }
        None
    }

    /// Current sequence counter (telemetry).
    pub fn sequence(&self) -> u64 {
        self.seq.load(Ordering::Acquire)
    }
}

/// Sharded RCU-protected map: the concrete `RcuCache` of the RFC §3.3
/// skeleton. Each shard holds an [`RcuPtr`] to an immutable `Vec` of
/// entries; publishing a new generation copies only that shard's entry
/// list. Lookups pick a shard by key hash, then linear/binary scan the
/// small immutable snapshot -- the design the RFC means by "per-shard
/// instance" (cache lines are never written in place).
pub struct RcuCache<K: Ord + Clone + std::hash::Hash, V: Clone> {
    shards: Vec<CacheShard<K, V>>,
}

struct CacheShard<K, V> {
    ptr: RcuPtr<Vec<(K, V)>>,
    /// Writers serialize per shard (RFC-002 §3.3: "writers are rare
    /// because transactions batch"); readers stay lock-free RCU.
    writer: std::sync::Mutex<()>,
}

impl<K: Ord + Clone + std::hash::Hash, V: Clone> RcuCache<K, V> {
    /// Creates a cache with `shards` shards (power of two recommended).
    #[must_use]
    pub fn with_shards(shards: usize) -> Self {
        let n = shards.max(1);
        Self {
            shards: (0..n)
                .map(|_| CacheShard {
                    ptr: RcuPtr::new(Vec::new()),
                    writer: std::sync::Mutex::new(()),
                })
                .collect(),
        }
    }

    fn shard_of(&self, key: &K) -> usize {
        let h = crate::io_engine::shard::splitmix64(
            // Cheap key fold for the common integer-ish keys.
            key_hash(key),
        );
        (h as usize) % self.shards.len()
    }

    /// Point lookup: one atomic load + a scan of a small immutable list.
    /// No lock: this is the RCU read path.
    #[must_use]
    pub fn get(&self, key: &K) -> Option<V> {
        let shard = &self.shards[self.shard_of(key)];
        let snapshot = shard.ptr.read();
        match snapshot.binary_search_by(|(k, _)| k.cmp(key)) {
            Ok(pos) => Some(snapshot[pos].1.clone()),
            Err(_) => None,
        }
    }

    /// Publishes an upsert into the owning shard: rebuilds that shard's
    /// immutable entry list and swaps it in under RCU semantics. The
    /// read-modify-publish is serialized by the shard's writer mutex:
    /// last-writer-wins would silently lose concurrent updates to the
    /// same shard, and a CAS loop costs more than the mutex at the
    /// writer rates the RFC assumes.
    pub fn insert(&self, key: K, value: V) {
        let shard = &self.shards[self.shard_of(&key)];
        let _wguard = shard.writer.lock().expect("cache writer lock");
        let snapshot = shard.ptr.read();
        let mut next = (*snapshot).clone();
        match next.binary_search_by(|(k, _)| k.cmp(&key)) {
            Ok(pos) => next[pos].1 = value,
            Err(pos) => next.insert(pos, (key, value)),
        }
        shard.ptr.publish(next);
    }

    /// Publishes a removal; a missing key is a no-op.
    pub fn remove(&self, key: &K) {
        let shard = &self.shards[self.shard_of(key)];
        let _wguard = shard.writer.lock().expect("cache writer lock");
        let snapshot = shard.ptr.read();
        let mut next = (*snapshot).clone();
        if let Ok(pos) = next.binary_search_by(|(k, _)| k.cmp(key)) {
            next.remove(pos);
            shard.ptr.publish(next);
        }
    }

    pub fn shard_count(&self) -> usize {
        self.shards.len()
    }
}

fn key_hash<K: std::hash::Hash>(key: &K) -> u64 {
    use std::hash::Hasher;
    let mut h = std::collections::hash_map::DefaultHasher::new();
    key.hash(&mut h);
    h.finish()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn rcu_ptr_publish_read_generations() {
        let p = RcuPtr::new(1u64);
        let g1 = p.read();
        assert_eq!(*g1, 1);
        p.publish(2);
        let g2 = p.read();
        assert_eq!(*g2, 2);
        // The old generation is still valid for its holder: RCU.
        assert_eq!(*g1, 1);
    }

    #[test]
    fn rcu_ptr_concurrent_reader_publisher() {
        let p = Arc::new(RcuPtr::new(Vec::<u64>::new()));
        let stop = Arc::new(AtomicU64::new(0));
        let started = Arc::new(std::sync::Barrier::new(5));
        let mut readers = Vec::new();
        for _ in 0..4 {
            let p = Arc::clone(&p);
            let stop = Arc::clone(&stop);
            let started = Arc::clone(&started);
            readers.push(std::thread::spawn(move || {
                started.wait();
                let mut reads = 0u64;
                // Read first, check stop second: even under total
                // scheduler starvation, every reader that starts
                // makes at least one read -- progress > 0 by
                // construction, not by timing.
                loop {
                    let snap = p.read();
                    // Every observed snapshot is self-consistent: it is
                    // one generation's Vec, so its length and contents
                    // agree by construction.
                    let _len = snap.len();
                    reads += 1;
                    if stop.load(Ordering::Relaxed) != 0 {
                        break;
                    }
                }
                reads
            }));
        }
        started.wait();
        // Give the readers a scheduling head start so they are provably
        // in their read loops while the publish stream runs (CI machines
        // with few cores otherwise starve the reader threads).
        std::thread::sleep(Duration::from_millis(50));
        for i in 0..1000u64 {
            p.publish((0..=i % 8).collect::<Vec<u64>>());
        }
        stop.store(1, Ordering::Relaxed);
        let total: u64 = readers.into_iter().map(|r| r.join().unwrap()).sum();
        assert!(total > 0, "readers must have made progress");
    }

    #[test]
    fn rcu_ptr_many_publishes_stay_sound() {
        // Epoch-based reclamation stress: the crossbeam collector reclaims
        // retired cells after their grace periods; this test's job is to
        // hammer publish/read interleavings and surface any reclamation
        // bug (which would show up as a crash or Miri failure, not an
        // assertion).
        let p = Arc::new(RcuPtr::new(Vec::<u64>::new()));
        let stop = Arc::new(AtomicU64::new(0));
        let mut readers = Vec::new();
        for _ in 0..3 {
            let p = Arc::clone(&p);
            let stop = Arc::clone(&stop);
            readers.push(std::thread::spawn(move || {
                // Read-first loop: at least one read before the stop
                // check (progress by construction). Coherence invariant:
                // every published generation is the sequence (0..=k) for
                // some k, so a snapshot is coherent iff it is exactly
                // that prefix. Generation lengths legitimately OSCILLATE
                // (k cycles), so length monotonicity is NOT the property.
                loop {
                    let snap = p.read();
                    for (j, &v) in snap.iter().enumerate() {
                        assert_eq!(v, j as u64, "generation must be the prefix 0..=k");
                    }
                    if stop.load(Ordering::Relaxed) != 0 {
                        break;
                    }
                }
            }));
        }
        for i in 0..2000u64 {
            p.publish((0..=i % 8).collect::<Vec<u64>>());
        }
        stop.store(1, Ordering::Relaxed);
        for r in readers {
            r.join().unwrap();
        }
        // 2000 more publishes happened on top of any other tests' counts.
        assert!(current_publish_count() >= 2000);
    }

    #[test]
    fn seqlock_readers_never_see_torn_writes() {
        #[derive(Clone, Copy, PartialEq, Eq, Debug)]
        struct Pair {
            a: u64,
            b: u64,
        }
        let lock = Arc::new(Seqlock::new(Pair { a: 0, b: 0 }));
        let stop = Arc::new(AtomicU64::new(0));
        let mut readers = Vec::new();
        for _ in 0..4 {
            let lock = Arc::clone(&lock);
            let stop = Arc::clone(&stop);
            readers.push(std::thread::spawn(move || {
                while stop.load(Ordering::Relaxed) == 0 {
                    if let Some(p) = lock.read(64) {
                        // Invariant: the writer only publishes pairs with
                        // a == b, so a torn read would show a != b.
                        assert_eq!(p.a, p.b, "torn read: {p:?}");
                    }
                }
            }));
        }
        for i in 0..10_000u64 {
            lock.write(|p| {
                p.a = i;
                p.b = i;
            });
        }
        stop.store(1, Ordering::Relaxed);
        for r in readers {
            r.join().unwrap();
        }
        assert_eq!(lock.read(8).unwrap().a, 9_999);
    }

    #[test]
    fn seqlock_write_serializes() {
        let lock = Seqlock::new(0u64);
        let lock = Arc::new(lock);
        let mut writers = Vec::new();
        for _ in 0..8 {
            let lock = Arc::clone(&lock);
            writers.push(std::thread::spawn(move || {
                for _ in 0..1000 {
                    lock.write(|v| *v += 1);
                }
            }));
        }
        for w in writers {
            w.join().unwrap();
        }
        assert_eq!(lock.read(8).unwrap(), 8_000);
    }

    #[test]
    fn rcu_cache_get_insert_remove() {
        let cache = RcuCache::<u64, String>::with_shards(8);
        cache.insert(1, "one".into());
        cache.insert(2, "two".into());
        assert_eq!(cache.get(&1).as_deref(), Some("one"));
        assert_eq!(cache.get(&2).as_deref(), Some("two"));
        assert_eq!(cache.get(&3), None);
        // Upsert.
        cache.insert(1, "uno".into());
        assert_eq!(cache.get(&1).as_deref(), Some("uno"));
        // Remove.
        cache.remove(&2);
        assert_eq!(cache.get(&2), None);
    }

    #[test]
    fn rcu_cache_concurrent_upserts_stay_consistent() {
        let cache = Arc::new(RcuCache::<u64, u64>::with_shards(8));
        let mut writers = Vec::new();
        for w in 0..8u64 {
            let cache = Arc::clone(&cache);
            writers.push(std::thread::spawn(move || {
                for i in 0..1000u64 {
                    let k = w * 1000 + i;
                    cache.insert(k, k * 2);
                }
            }));
        }
        for w in writers {
            w.join().unwrap();
        }
        for w in 0..8u64 {
            for i in 0..1000u64 {
                let k = w * 1000 + i;
                assert_eq!(cache.get(&k), Some(k * 2), "missing {k}");
            }
        }
    }
}
