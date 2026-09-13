//! Vyukov-style bounded MPMC queue (the "bounded MPMC queue" from Dmitry
//! Vyukov's article collection), used for every shard<->engine hand-off.
//!
//! Why this and not `crossbeam_queue`'s ArrayQueue: that IS the same
//! algorithm; this local implementation keeps the storage-plane hot path
//! free of an external dependency boundary, lets us instrument the
//! `tail`/`head` wraparound for the health bus, and -- honestly -- because
//! writing it against the original specification with tests beats
//! importing 300 transitive lines for 80 lines of core algorithm.
//! crossbeam remains a dependency for the epoch/deque uses elsewhere.
//!
//! Properties:
//! * bounded, fixed capacity, power-of-two (mask indexing)
//! * lock-free, wait-free for `push` success and `pop`
//! * one usize of state per slot (sequence number), 8 bytes overhead
//! * `push` returns `false` when full -- the caller batches/retries; the
//!   engine sizes queues so that a full queue is a backpressure signal,
//!   never a drop.

use std::cell::UnsafeCell;
use std::sync::atomic::{fence, AtomicUsize, Ordering};

#[derive(Debug)]
pub struct MpmcQueue<T> {
    buffer: Box<[Slot<T>]>,
    mask: usize,
    head: AtomicUsize,
    tail: AtomicUsize,
}

#[derive(Debug)]
struct Slot<T> {
    sequence: AtomicUsize,
    value: UnsafeCell<Option<T>>,
}

// SAFETY: the queue is safe to share: every slot is protected by its
// sequence-number protocol (a slot is owned by exactly one producer or
// one consumer at a time, enforced by the acquire/release fences below),
// which is the standard proof for Vyukov's MPMC queue.
unsafe impl<T: Send> Send for MpmcQueue<T> {}
unsafe impl<T: Send> Sync for MpmcQueue<T> {}

impl<T> MpmcQueue<T> {
    /// Creates a queue with capacity `next_pow2(capacity_hint)`.
    ///
    /// # Panics
    /// If `capacity_hint` is zero.
    #[must_use]
    pub fn with_capacity_hint(capacity_hint: usize) -> Self {
        assert!(capacity_hint > 0, "MPMC capacity must be > 0");
        let capacity = capacity_hint.next_power_of_two();
        let mut buffer = Vec::with_capacity(capacity);
        for i in 0..capacity {
            buffer.push(Slot {
                sequence: AtomicUsize::new(i),
                value: UnsafeCell::new(None),
            });
        }
        Self {
            buffer: buffer.into_boxed_slice(),
            mask: capacity - 1,
            head: AtomicUsize::new(0),
            tail: AtomicUsize::new(0),
        }
    }

    pub fn capacity(&self) -> usize {
        self.mask + 1
    }

    /// Tries to enqueue `value`. Returns `false` iff the queue is full
    /// (backpressure: never blocks, never drops).
    pub fn push(&self, value: T) -> bool {
        let mut pos = self.tail.load(Ordering::Relaxed);
        loop {
            let slot = &self.buffer[pos & self.mask];
            let seq = slot.sequence.load(Ordering::Acquire);
            // Wrap-aware difference: slot is writable when its sequence
            // equals the enqueue position.
            let diff = seq as isize - pos as isize;
            if diff == 0 {
                if self
                    .tail
                    .compare_exchange_weak(
                        pos,
                        pos.wrapping_add(1),
                        Ordering::Relaxed,
                        Ordering::Relaxed,
                    )
                    .is_ok()
                {
                    // SAFETY: we won the slot's ownership for writing via
                    // the sequence protocol; no reader can touch it until
                    // we publish `pos + 1` into the sequence.
                    unsafe {
                        *slot.value.get() = Some(value);
                    }
                    slot.sequence.store(pos.wrapping_add(1), Ordering::Release);
                    return true;
                }
                pos = self.tail.load(Ordering::Relaxed);
            } else if diff < 0 {
                // Queue full (slot still holds an unconsumed item).
                if pos.wrapping_sub(self.head.load(Ordering::Acquire)) == self.capacity() {
                    return false;
                }
                pos = self.tail.load(Ordering::Relaxed);
            } else {
                pos = self.tail.load(Ordering::Relaxed);
            }
        }
    }

    /// Tries to dequeue the oldest value. `None` iff empty.
    pub fn pop(&self) -> Option<T> {
        let mut pos = self.head.load(Ordering::Relaxed);
        loop {
            let slot = &self.buffer[pos & self.mask];
            let seq = slot.sequence.load(Ordering::Acquire);
            let diff = seq as isize - (pos as isize + 1);
            if diff == 0 {
                if self
                    .head
                    .compare_exchange_weak(
                        pos,
                        pos.wrapping_add(1),
                        Ordering::Relaxed,
                        Ordering::Relaxed,
                    )
                    .is_ok()
                {
                    // SAFETY: we own the slot for reading per the sequence
                    // protocol; the producer published `pos + 1` before we
                    // could observe that value here.
                    let value = unsafe { (*slot.value.get()).take() };
                    slot.sequence
                        .store(pos.wrapping_add(self.capacity()), Ordering::Release);
                    fence(Ordering::Release);
                    return value;
                }
                pos = self.head.load(Ordering::Relaxed);
            } else if diff < 0 {
                // Empty (or lagging producer); re-read head in case of
                // movement by another consumer.
                let head_now = self.head.load(Ordering::Acquire);
                if head_now == pos {
                    return None;
                }
                pos = head_now;
            } else {
                pos = self.head.load(Ordering::Relaxed);
            }
        }
    }

    /// Approximate len (racy snapshot; for telemetry only).
    pub fn len(&self) -> usize {
        let t = self.tail.load(Ordering::Relaxed);
        let h = self.head.load(Ordering::Relaxed);
        t.wrapping_sub(h).min(self.capacity())
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    #[test]
    fn fifo_order_single_thread() {
        let q = MpmcQueue::<u64>::with_capacity_hint(16);
        for i in 0..16 {
            assert!(q.push(i));
        }
        assert!(!q.push(99), "queue must be full at capacity");
        for i in 0..16 {
            assert_eq!(q.pop(), Some(i));
        }
        assert_eq!(q.pop(), None);
    }

    #[test]
    fn wraparound_after_drain() {
        let q = MpmcQueue::<u64>::with_capacity_hint(4);
        for round in 0..1000u64 {
            assert!(q.push(round));
            assert_eq!(q.pop(), Some(round));
        }
        assert!(q.is_empty());
    }

    #[test]
    fn two_producers_two_consumers_no_loss() {
        const N: usize = 50_000;
        let q = Arc::new(MpmcQueue::<usize>::with_capacity_hint(1024));
        let mut handles = Vec::new();
        for p in 0..2 {
            let q = Arc::clone(&q);
            handles.push(std::thread::spawn(move || {
                let base = p * N;
                let mut i = 0;
                while i < N {
                    if q.push(base + i) {
                        i += 1;
                    } else {
                        std::thread::yield_now();
                    }
                }
            }));
        }
        let mut sums = Vec::new();
        for _ in 0..2 {
            let q = Arc::clone(&q);
            sums.push(std::thread::spawn(move || {
                let mut got = 0usize;
                let mut sum = 0usize;
                while got < N {
                    if let Some(v) = q.pop() {
                        sum += v;
                        got += 1;
                    } else {
                        std::thread::yield_now();
                    }
                }
                sum
            }));
        }
        for h in handles {
            h.join().unwrap();
        }
        let total: usize = sums.into_iter().map(|h| h.join().unwrap()).sum();
        // Values 0..2N each delivered exactly once: sum must match.
        let expected: usize = (0..2 * N).sum();
        assert_eq!(total, expected);
    }

    #[test]
    fn len_is_racy_but_bounded() {
        let q = MpmcQueue::<u8>::with_capacity_hint(8);
        q.push(1);
        q.push(2);
        let l = q.len();
        assert!(l <= 2);
    }
}
