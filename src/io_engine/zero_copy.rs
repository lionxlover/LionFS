//! Registered, aligned buffer arena: the zero-copy substrate (RFC-002
//! §3.1 "registered buffers let the device DMA land results directly in
//! application memory").
//!
//! Model (same contract as kernel registered buffers / SPDK DMA buffers):
//!
//! * The arena owns a fixed number of slots, each one page-aligned
//!   allocation of a fixed slot size (default 64 KiB -- one maximum-size
//!   compression cluster, four 16 KiB flash pages, or 16 x 4 KiB blocks).
//! * A slot is *leased* to exactly one in-flight operation at a time.
//!   `lease()` refuses (returns `None`) while a lease is outstanding, so
//!   exclusivity is enforced dynamically, not just by convention.
//! * While leased, the op's buffer range is addressed through
//!   [`BufHandle`]; the slices are handed out as `&mut [u8]` through
//!   `unsafe` accessors whose single safety invariant is precisely the
//!   lease exclusivity the arena itself enforces. This is the standard
//!   registered-buffer contract: the device (or the threaded backend's
//!   worker) writes into the slot while the caller does not touch it.
//! * Misaligned/hostile user buffers are served through
//!   [`BounceStats`]`-`instrumented copy-in/copy-out -- never silently
//!   (RFC-002 §6.2: "a guarantee you do not measure is a hope").
//!
//! The arena never grows at runtime; a full arena is backpressure the
//! submission path batches on, mirroring how SQ fullness behaves.

use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::Arc;

/// Default slot size: 64 KiB (max compression cluster).
pub const DEFAULT_SLOT_SIZE: usize = 64 * 1024;
/// Default slot count: 512 slots x 64 KiB = 32 MiB of registered memory.
pub const DEFAULT_SLOT_COUNT: usize = 512;

/// A lease on (slot, range). Plain data; echoed through the engine.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct BufHandle {
    pub slot: u32,
    pub off: u32,
    pub len: u32,
}

struct Slot {
    // SAFETY (field): written by the engine/device while leased, read by
    // the lease owner otherwise; exclusivity enforced via `leased`.
    buf: UnsafeCellPtr,
    len: usize,
    leased: AtomicUsize,
}

/// Opaque pointer holder; keeps the raw pointer out of derived traits.
struct UnsafeCellPtr {
    ptr: *mut u8,
    len_bytes: usize,
}

// SAFETY: the arena is Send/Sync because slot access is mediated by the
// lease protocol (an atomic ownership flag per slot); every `slice`
// hand-out documents the exact invariant it requires.
unsafe impl Send for UnsafeCellPtr {}
unsafe impl Sync for UnsafeCellPtr {}

/// Instrumented bounce-buffer counters (visible in the health bus).
#[derive(Debug, Default)]
pub struct BounceStats {
    /// Buffers copied in because the caller's pointer was unaligned or
    /// the op needed a device-aligned staging area.
    pub copy_in: AtomicU64,
    /// Buffers copied back out after completion.
    pub copy_out: AtomicU64,
}

#[derive(Debug)]
pub struct RegisteredBufArena {
    slots: Box<[Slot]>,
    pub slot_size: usize,
    pub bounce: BounceStats,
    leases_outstanding: AtomicU64,
}

impl RegisteredBufArena {
    /// Creates an arena of `slot_count` slots, each `slot_size` bytes,
    /// page-aligned. Slot size is rounded up to a page multiple.
    #[must_use]
    pub fn new(slot_count: usize, slot_size: usize) -> Arc<Self> {
        let slot_size = slot_size.max(crate::pal::page_size());
        let slot_size = (slot_size + crate::pal::page_size() - 1) / crate::pal::page_size()
            * crate::pal::page_size();
        let mut slots = Vec::with_capacity(slot_count);
        for _ in 0..slot_count {
            slots.push(Slot::alloc(slot_size));
        }
        Arc::new(Self {
            slots: slots.into_boxed_slice(),
            slot_size,
            bounce: BounceStats::default(),
            leases_outstanding: AtomicU64::new(0),
        })
    }

    /// Default-shaped arena (512 x 64 KiB).
    #[must_use]
    pub fn with_defaults() -> Arc<Self> {
        Self::new(DEFAULT_SLOT_COUNT, DEFAULT_SLOT_SIZE)
    }

    /// Tries to lease `len` bytes at offset 0 of a free slot. Returns
    /// `None` when the arena is exhausted (backpressure) or `len` exceeds
    /// the slot size (caller must split the I/O).
    #[must_use]
    pub fn lease(&self, len: u32) -> Option<BufHandle> {
        if len as usize > self.slot_size {
            return None;
        }
        for (idx, slot) in self.slots.iter().enumerate() {
            // Fast path: acquire the lease.
            if slot
                .leased
                .compare_exchange(0, 1, Ordering::AcqRel, Ordering::Acquire)
                .is_ok()
            {
                self.leases_outstanding.fetch_add(1, Ordering::Relaxed);
                return Some(BufHandle {
                    slot: idx as u32,
                    off: 0,
                    len,
                });
            }
        }
        None
    }

    /// Releases the lease (engine completion path). Leasing the returned
    /// handle is legal again after this.
    pub fn release(&self, handle: BufHandle) {
        if let Some(slot) = self.slots.get(handle.slot as usize) {
            let prev = slot.leased.swap(0, Ordering::AcqRel);
            debug_assert_eq!(prev, 1, "double release of slot {}", handle.slot);
            if prev == 1 {
                self.leases_outstanding.fetch_sub(1, Ordering::Relaxed);
            }
        }
    }

    /// Number of currently outstanding leases (telemetry).
    pub fn outstanding_leases(&self) -> u64 {
        self.leases_outstanding.load(Ordering::Relaxed)
    }

    /// Number of slots.
    #[must_use]
    pub fn slot_count(&self) -> usize {
        self.slots.len()
    }

    /// # Safety
    /// The caller must hold the lease for `handle`'s slot (the engine
    /// does: a handle only exists between `lease` and `release`), and
    /// must not call any other accessor on the same slot concurrently.
    #[must_use]
    pub unsafe fn slice(&self, handle: BufHandle) -> &[u8] {
        let slot = &self.slots[handle.slot as usize];
        debug_assert!(handle.off as usize + handle.len as usize <= slot.buf.len_bytes);
        let base = slot.buf.ptr;
        std::slice::from_raw_parts(base.add(handle.off as usize), handle.len as usize)
    }

    /// Mutable form of [`Self::slice`], with the same safety contract.
    ///
    /// # Safety
    /// Lease exclusivity must hold; see [`Self::slice`].
    ///
    /// `mut_from_ref` is the entire point of this API: handing a
    /// `&mut` view of arena-registered memory through `&self` is what
    /// registered-buffer zero-copy IS (the same shape as
    /// `UnsafeCell::get` and SPDK's DMA buffer contract). The lease
    /// protocol above is what makes it sound.
    #[allow(clippy::mut_from_ref)]
    #[must_use]
    pub unsafe fn slice_mut(&self, handle: BufHandle) -> &mut [u8] {
        let slot = &self.slots[handle.slot as usize];
        debug_assert!(handle.off as usize + handle.len as usize <= slot.buf.len_bytes);
        let base = slot.buf.ptr;
        std::slice::from_raw_parts_mut(base.add(handle.off as usize), handle.len as usize)
    }

    /// Copies `data` into a fresh lease (the bounce-buffer slow path for
    /// hostile unaligned user buffers). Bumps `copy_in`.
    ///
    /// # Safety (buffer)
    /// None at the call site: the lease is created and filled under this
    /// method's own exclusivity.
    #[must_use]
    pub fn copy_in(&self, data: &[u8]) -> Option<BufHandle> {
        let handle = self.lease(data.len() as u32)?;
        // SAFETY: we just leased this slot exclusively.
        unsafe { self.slice_mut(handle) }.copy_from_slice(data);
        self.bounce.copy_in.fetch_add(1, Ordering::Relaxed);
        Some(handle)
    }

    /// Copies a leased range back out (completing a bounce read). Bumps
    /// `copy_out`. Releases nothing; the caller still owns the lease.
    ///
    /// # Safety
    /// Same exclusivity contract as [`Self::slice`].
    pub unsafe fn copy_out(&self, handle: BufHandle, out: &mut [u8]) -> usize {
        let n = out.len().min(handle.len as usize);
        out[..n].copy_from_slice(&self.slice(handle)[..n]);
        self.bounce.copy_out.fetch_add(1, Ordering::Relaxed);
        n
    }
}

impl Slot {
    fn alloc(len: usize) -> Self {
        // `std::alloc::alloc` guarantees the returned block satisfies the
        // *requested* alignment, so asking for page alignment directly is
        // both simpler and contractually sound -- no manual pointer
        // alignment, no stashed base pointer, and `Drop` frees with the
        // exact same layout.
        let page = crate::pal::page_size();
        let layout = std::alloc::Layout::from_size_align(len, page).expect("slot layout is valid");
        // SAFETY: layout has non-zero size and (page) alignment, which
        // are both powers of two the global allocator supports; freed in
        // Drop with the identical layout.
        let raw = unsafe { std::alloc::alloc(layout) };
        assert!(!raw.is_null(), "arena slot allocation failed");
        debug_assert_eq!(raw as usize % page, 0);
        Self {
            buf: UnsafeCellPtr {
                ptr: raw,
                len_bytes: len,
            },
            len,
            leased: AtomicUsize::new(0),
        }
    }
}

impl std::fmt::Debug for Slot {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Slot")
            .field("len", &self.len)
            .field("leased", &self.leased.load(Ordering::Relaxed))
            .finish()
    }
}

impl Drop for Slot {
    fn drop(&mut self) {
        // SAFETY: freed with the exact layout the slot was allocated with.
        let layout = std::alloc::Layout::from_size_align(self.len, crate::pal::page_size())
            .expect("slot layout is valid");
        unsafe { std::alloc::dealloc(self.buf.ptr, layout) };
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lease_release_exclusivity() {
        let arena = RegisteredBufArena::new(4, 4096);
        let h = arena.lease(512).unwrap();
        // Slot is exclusively leased: re-leasing must skip it, and with
        // all slots leased the arena reports backpressure.
        let h2 = arena.lease(512).unwrap();
        let h3 = arena.lease(512).unwrap();
        let h4 = arena.lease(512).unwrap();
        assert_eq!(arena.outstanding_leases(), 4);
        assert!(arena.lease(8).is_none(), "full arena must be backpressure");
        arena.release(h);
        arena.release(h2);
        arena.release(h3);
        arena.release(h4);
        assert_eq!(arena.outstanding_leases(), 0);
        assert!(arena.lease(8).is_some());
    }

    #[test]
    fn oversize_request_rejected() {
        let arena = RegisteredBufArena::new(2, 4096);
        assert!(arena.lease(8192).is_none());
    }

    #[test]
    fn slots_are_page_aligned() {
        let arena = RegisteredBufArena::new(8, 4096);
        let h = arena.lease(64).unwrap();
        // SAFETY: single-threaded test holds the lease exclusively.
        let s = unsafe { arena.slice(h) };
        assert_eq!(s.len(), 64);
        assert_eq!(s.as_ptr() as usize % crate::pal::page_size(), 0);
    }

    #[test]
    fn write_and_read_back_through_arena() {
        let arena = RegisteredBufArena::new(2, 64 * 1024);
        let h = arena.copy_in(&[9u8; 100]).unwrap();
        // SAFETY: exclusive lease held by this test thread.
        let s = unsafe { arena.slice(h) };
        assert!(s.iter().take(100).all(|&b| b == 9));
        let mut out = [0u8; 50];
        // SAFETY: same.
        unsafe { arena.copy_out(h, &mut out) };
        assert!(out.iter().all(|&b| b == 9));
        assert_eq!(arena.bounce.copy_in.load(Ordering::Relaxed), 1);
        assert_eq!(arena.bounce.copy_out.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn many_slots_alloc_free_cycle() {
        // Exercises the stashed-base-pointer Drop path across a meaningful
        // number of allocations.
        for _ in 0..64 {
            let arena = RegisteredBufArena::new(16, 8192);
            let hs: Vec<_> = (0..16).filter_map(|_| arena.lease(16)).collect();
            assert_eq!(hs.len(), 16);
            for h in hs {
                arena.release(h);
            }
        }
    }
}
