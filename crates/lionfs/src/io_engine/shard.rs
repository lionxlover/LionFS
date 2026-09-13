//! Per-core engine shards (RFC-002 §3.3 "The lock-free core").
//!
//! A request's entire path -- cache probe, extent resolution, allocation,
//! submission -- executes on one shard with data structures that shard
//! alone owns. Cross-shard interaction is confined to two bounded queues:
//! completed work drains to the completion dispatcher, and free-space
//! returns flow through an MPMC channel (the "releases" queue below).
//!
//! `shard_of` is the RFC's injective routing function: splitmix64 of the
//! (fd, ino) pair masked to a power-of-two shard count. Shards are never
//! pinned to a specific core by the library itself -- pinning is a mount
//! option executed by the caller (`taskset`/`pthread_setaffinity`), which
//! keeps the core testable on hosts with heterogeneous CPU placement.

use std::sync::atomic::{AtomicU64, Ordering};

use super::mpmc::MpmcQueue;

/// Hard ceiling on shard count: 128 shards is beyond any current host's
/// useful core count at storage line rate, and keeps the shard table's
/// memory footprint bounded (128 x 8 KiB queues = 1 MiB).
pub const NUM_SHARDS_MAX: usize = 128;

/// splitmix64 -- the RFC's chosen mixer: one multiply, two shifts, three
/// XORs; fully avalanche at 64 bits; used both here and by the HAMT's
/// secondary mixing.
#[must_use]
pub fn splitmix64(mut x: u64) -> u64 {
    x = x.wrapping_add(0x9E37_79B9_7F4A_7C15);
    let mut z = x;
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^ (z >> 31)
}

/// Routes a (file descriptor, inode) pair to its shard. Power-of-two
/// shard count so the mask is a single AND.
#[must_use]
pub fn shard_of(fd: u32, ino: u64, num_shards: usize) -> usize {
    debug_assert!(num_shards.is_power_of_two());
    let h = splitmix64((fd as u64) ^ (ino << 32));
    (h as usize) & (num_shards.saturating_sub(1))
}

/// One per-core shard. The fields below are the *shape* of the RFC §3.3
/// skeleton, instantiated with the concrete lock-free structures this
/// code base owns:
///
/// ```text
/// struct Shard {                    // RFC-002 listing
///     extent_cache: RcuCache<Lba, Extent>,
///     free_q: FreeQueue,
///     ring: IoRing,
///     tx: TxBuffer,
/// }
/// ```
///
/// * `extent_cache` is provided by `rcu::RcuCache` (Pillar I module).
/// * `free_q` is an MPMC queue of free-space runs -- MPMC because a
///   *different* shard can return space to this device's pool.
/// * `ring` is the shared `IoEngine` (devices are few, shards many; the
///   engine internally multiplexes submission per device ring).
/// * `tx` stages dirty pages awaiting commit (group commit batches
///   across shards through the shared engine flush).
#[derive(Debug)]
pub struct Shard {
    pub id: usize,
    /// Free-space runs available for allocation on this shard. Fed by the
    /// checkpoint/reconcile path and drained by `allocate`.
    pub free_q: MpmcQueue<FreeSpaceRun>,
    /// Local free-space accounting (approximate between reconciles).
    free_blocks_local: AtomicU64,
    /// Dirty bytes staged in this shard's transaction buffer.
    pub tx_dirty_bytes: AtomicU64,
    /// Transactions ready for group commit.
    pub tx_ready: MpmcQueue<u64>,
}

impl Shard {
    fn new(id: usize) -> Self {
        Self {
            id,
            free_q: MpmcQueue::with_capacity_hint(1024),
            free_blocks_local: AtomicU64::new(0),
            tx_dirty_bytes: AtomicU64::new(0),
            tx_ready: MpmcQueue::with_capacity_hint(1024),
        }
    }

    /// Tries to take a free-space run of at least `blocks` from the local
    /// queue. Returns `None` when this shard is empty (the refill path
    /// then consults the volume free-space tree).
    pub fn allocate(&self, blocks: u64) -> Option<FreeSpaceRun> {
        // Pop until a run is big enough; smaller runs go to the back so
        // they are not lost (single-threaded-per-shard by contract, so
        // re-push is safe).
        let mut tries = self.free_q.capacity() as u64;
        while tries > 0 {
            if let Some(run) = self.free_q.pop() {
                if run.blocks >= blocks {
                    let leftover = run.blocks - blocks;
                    self.free_blocks_local.fetch_sub(blocks, Ordering::Relaxed);
                    if leftover > 0 {
                        let _ = self.free_q.push(FreeSpaceRun {
                            start: run.start + blocks,
                            blocks: leftover,
                            media: run.media,
                        });
                    }
                    return Some(FreeSpaceRun {
                        start: run.start,
                        blocks,
                        media: run.media,
                    });
                }
                let _ = self.free_q.push(run);
            } else {
                return None;
            }
            tries -= 1;
        }
        None
    }

    /// Returns freed blocks into this shard's queue (called by the same
    /// shard's truncate/GC path; cross-shard returns go through the
    /// device region's release queue in the engine).
    pub fn release(&self, run: FreeSpaceRun) -> bool {
        self.free_blocks_local
            .fetch_add(run.blocks, Ordering::Relaxed);
        self.free_q.push(run)
    }

    pub fn local_free_blocks(&self) -> u64 {
        self.free_blocks_local.load(Ordering::Relaxed)
    }

    /// Stage `bytes` of dirty data in the shard's tx buffer.
    pub fn stage_dirty(&self, bytes: u64) {
        self.tx_dirty_bytes.fetch_add(bytes, Ordering::Relaxed);
    }

    pub fn drain_dirty(&self) -> u64 {
        self.tx_dirty_bytes.swap(0, Ordering::Relaxed)
    }
}

/// A run of free physical blocks owned by a shard.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FreeSpaceRun {
    pub start: u64,
    pub blocks: u64,
    /// Media class of the run (placement hint for the allocator policy).
    pub media: crate::media::MediaClass,
}

/// The shard table: `NUM_SHARDS = next_pow2(cpu_count)` clamped to
/// [1, NUM_SHARDS_MAX].
#[derive(Debug)]
pub struct ShardTable {
    shards: Box<[Shard]>,
}

impl ShardTable {
    /// Creates a shard table sized for `cpu_hint` logical CPUs.
    #[must_use]
    pub fn for_cpus(cpu_hint: usize) -> Self {
        let n = cpu_hint.max(1).next_power_of_two().min(NUM_SHARDS_MAX);
        let shards = (0..n).map(Shard::new).collect::<Vec<_>>();
        Self {
            shards: shards.into_boxed_slice(),
        }
    }

    pub fn len(&self) -> usize {
        self.shards.len()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.shards.is_empty()
    }

    /// Shard index for (fd, ino).
    #[must_use]
    pub fn shard_of(&self, fd: u32, ino: u64) -> usize {
        shard_of(fd, ino, self.shards.len())
    }

    #[must_use]
    pub fn get(&self, id: usize) -> &Shard {
        &self.shards[id]
    }

    pub fn shards(&self) -> &[Shard] {
        &self.shards
    }

    /// Sum of all shards' local free-block accounting (checkpoint
    /// reconciliation input).
    pub fn total_local_free(&self) -> u64 {
        self.shards.iter().map(|s| s.local_free_blocks()).sum()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn splitmix_avalanches() {
        // Adjacent inputs must produce outputs with a healthy Hamming
        // distance (avalanche): expected 32, accept 20..44.
        let a = splitmix64(1);
        let b = splitmix64(2);
        let dist = (a ^ b).count_ones();
        assert!(
            (20..44).contains(&dist),
            "hamming distance {dist} not avalanche-like"
        );
    }

    #[test]
    fn shard_routing_is_stable_and_spread() {
        let n = 8;
        let mut counts = vec![0usize; n];
        for fd in 0..16u32 {
            for ino in 1..=64u64 {
                let s = shard_of(fd, ino, n);
                assert!(s < n);
                counts[s] += 1;
            }
        }
        // With 1024 samples over 8 shards, every shard must have been
        // hit (a shard starvation here would be a routing bug).
        assert!(counts.iter().all(|&c| c > 0));
        // And no single shard may absorb a pathological share.
        assert!(counts.iter().all(|&c| c < 512), "counts: {counts:?}");
    }

    #[test]
    fn shard_table_sizing() {
        let t = ShardTable::for_cpus(1);
        assert_eq!(t.len(), 1);
        let t = ShardTable::for_cpus(5);
        assert_eq!(t.len(), 8);
        let t = ShardTable::for_cpus(1 << 20);
        assert_eq!(t.len(), NUM_SHARDS_MAX);
    }

    #[test]
    fn allocate_takes_and_splits_runs() {
        let t = ShardTable::for_cpus(1);
        let s = t.get(0);
        assert!(s.release(FreeSpaceRun {
            start: 100,
            blocks: 10,
            media: crate::media::MediaClass::Ssd
        }));
        let got = s.allocate(4).unwrap();
        assert_eq!(got.start, 100);
        assert_eq!(got.blocks, 4);
        // Leftover run remains.
        let got2 = s.allocate(6).unwrap();
        assert_eq!(got2.start, 104);
        assert_eq!(got2.blocks, 6);
        assert!(s.allocate(1).is_none());
    }

    #[test]
    fn allocate_returns_none_when_only_small_runs() {
        let t = ShardTable::for_cpus(1);
        let s = t.get(0);
        s.release(FreeSpaceRun {
            start: 0,
            blocks: 2,
            media: crate::media::MediaClass::Ssd,
        });
        assert!(s.allocate(4).is_none());
        // The small run is still there for a small allocation.
        assert!(s.allocate(2).is_some());
    }

    #[test]
    fn cross_shard_accounting_sums() {
        let t = ShardTable::for_cpus(4);
        for s in t.shards() {
            s.release(FreeSpaceRun {
                start: 0,
                blocks: 7,
                media: crate::media::MediaClass::Ssd,
            });
        }
        assert_eq!(t.total_local_free(), 28);
    }
}
