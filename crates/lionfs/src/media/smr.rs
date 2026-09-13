//! SMR band-confined sequential placement (RFC-002 §6.1).
//!
//! For SMR drives the allocator confines each actively-written file to
//! one sequential band, the scheduler batches cross-band reclaims into
//! elevator sweeps at device-idle windows, and random writes to
//! SMR-host-managed pools are **rejected at open time with an explicit
//! error** rather than silently degrading -- an honest failure 1.x-style
//! tooling can surface.

use std::collections::BTreeMap;
use std::sync::Mutex;

/// Typical SMR band size (256 MiB per RFC-002 Table 12).
pub const DEFAULT_BAND_SIZE: u64 = 256 * 1024 * 1024;

/// A sequential band on an SMR device.
#[derive(Debug, Clone, Copy)]
pub struct Band {
    pub id: u32,
    pub start: u64,
    pub capacity: u64,
    pub write_pointer: u64,
    /// Bands currently being reclaimed are read-only to new allocations.
    pub reclaiming: bool,
}

impl Band {
    #[must_use]
    pub fn free_bytes(&self) -> u64 {
        self.capacity.saturating_sub(self.write_pointer)
    }

    #[must_use]
    pub fn is_writable(&self) -> bool {
        !self.reclaiming && self.free_bytes() > 0
    }
}

/// Band-confined allocator state.
#[derive(Debug, Default)]
pub struct BandAllocator {
    bands: Mutex<BTreeMap<u32, Band>>,
    /// file id -> band assignment (one actively-written file per band).
    assignment: Mutex<BTreeMap<u64, u32>>,
}

/// The hard failure the RFC demands for host-managed SMR random writes:
/// an explicit error at open time, not silent degradation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RandomWriteRejected {
    pub band: u32,
    pub offset: u64,
    /// Why: the write is not at the band's write pointer.
    pub write_pointer: u64,
}

impl std::fmt::Display for RandomWriteRejected {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "random write to host-managed SMR band {} at offset {} rejected \
             (band write pointer is {}); LionFS confines writes to sequential bands",
            self.band, self.offset, self.write_pointer
        )
    }
}

impl std::error::Error for RandomWriteRejected {}

impl BandAllocator {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    pub fn add_band(&self, band: Band) {
        self.bands.lock().expect("band lock").insert(band.id, band);
    }

    /// Assigns `file_id` to a writable band (its current band if it has
    /// one, else a fresh one), planning a sequential write of `len`
    /// bytes. Returns the offset, or `None` when no band fits (ENOSPC).
    pub fn plan_sequential(&self, file_id: u64, len: u64) -> Option<(u32, u64)> {
        // Existing assignment first: confinement means one file stays
        // in one band until it is full.
        if let Some(band_id) = self
            .assignment
            .lock()
            .expect("assignment lock")
            .get(&file_id)
            .copied()
        {
            if let Some((b, off)) = self.try_write_band(band_id, len) {
                return Some((b, off));
            }
        }
        // Fresh band: lowest-numbered writable band NOT already assigned
        // to another file (the confinement policy: actively-written
        // files do not share bands -- SMR "one file per band" mirror of
        // the ZNS rule).
        let assigned: std::collections::BTreeSet<u32> = self
            .assignment
            .lock()
            .expect("assignment lock")
            .values()
            .copied()
            .collect();
        let bands = self.bands.lock().expect("band lock");
        let candidate = bands
            .values()
            .filter(|b| b.is_writable() && b.free_bytes() >= len && !assigned.contains(&b.id))
            .map(|b| b.id)
            .min();
        drop(bands);
        if let Some(id) = candidate {
            if let Some((b, off)) = self.try_write_band(id, len) {
                self.assignment
                    .lock()
                    .expect("assignment lock")
                    .insert(file_id, id);
                return Some((b, off));
            }
        }
        None
    }

    fn try_write_band(&self, band_id: u32, len: u64) -> Option<(u32, u64)> {
        let mut bands = self.bands.lock().expect("band lock");
        let b = bands.get_mut(&band_id)?;
        if !b.is_writable() || b.free_bytes() < len {
            return None;
        }
        let off = b.start + b.write_pointer;
        b.write_pointer += len;
        Some((band_id, off))
    }

    /// Validates a write against band confinement. Random (non-append)
    /// writes to host-managed bands are rejected with the RFC's explicit
    /// error.
    pub fn validate_write(
        &self,
        band: u32,
        offset: u64,
        len: u64,
    ) -> Result<(), RandomWriteRejected> {
        let bands = self.bands.lock().expect("band lock");
        let b = bands.get(&band).copied();
        drop(bands);
        match b {
            Some(b) => {
                let in_band = offset.saturating_sub(b.start);
                if b.reclaiming {
                    // Reclaiming bands accept no writes at all.
                    Err(RandomWriteRejected {
                        band,
                        offset,
                        write_pointer: b.write_pointer,
                    })
                } else if in_band == b.write_pointer {
                    let _ = len;
                    Ok(())
                } else {
                    Err(RandomWriteRejected {
                        band,
                        offset,
                        write_pointer: b.write_pointer,
                    })
                }
            }
            None => Ok(()), // Not an SMR band: validation is a no-op.
        }
    }

    /// Marks live extents for an elevator sweep: bands whose garbage
    /// ratio justifies a reclaim are marked read-only and their live
    /// extents are returned for sequential rewrite.
    ///
    /// The elevator model (RFC-002 §6.1): reclaims are batched into
    /// sweeps at device-idle windows, not executed inline; this function
    /// is the sweep planner.
    pub fn plan_elevator_sweep(&self, live_bytes: &BTreeMap<u32, u64>) -> Vec<SweepStep> {
        let mut steps = Vec::new();
        let mut bands = self.bands.lock().expect("band lock");
        for (&id, band) in bands.iter_mut() {
            let live = live_bytes.get(&id).copied().unwrap_or(0);
            let garbage = band.capacity.saturating_sub(live);
            // Reclaim when garbage is at least half the band (threshold
            // from the SMR elevator literature; tunable).
            if garbage * 2 >= band.capacity && band.write_pointer > 0 {
                band.reclaiming = true;
                steps.push(SweepStep {
                    band: id,
                    live_bytes: live,
                    target_band: None, // planner fills in on execution
                });
            }
        }
        steps
    }

    /// Completes a sweep: live data rewritten elsewhere; band resets.
    pub fn finish_sweep(&self, band: u32) {
        let mut bands = self.bands.lock().expect("band lock");
        if let Some(b) = bands.get_mut(&band) {
            b.write_pointer = 0;
            b.reclaiming = false;
        }
    }

    pub fn band_count(&self) -> usize {
        self.bands.lock().expect("band lock").len()
    }

    pub fn band(&self, id: u32) -> Option<Band> {
        self.bands.lock().expect("band lock").get(&id).copied()
    }
}

/// One elevator sweep step.
#[derive(Debug, Clone, Copy)]
pub struct SweepStep {
    pub band: u32,
    pub live_bytes: u64,
    /// Target band for the rewrite; assigned by the executor.
    pub target_band: Option<u32>,
}

/// Builds `n` bands of `band_size` starting at `base`.
#[must_use]
pub fn layout(base: u64, band_size: u64, n: u32) -> Vec<Band> {
    (0..n)
        .map(|id| Band {
            id,
            start: base + u64::from(id) * band_size,
            capacity: band_size,
            write_pointer: 0,
            reclaiming: false,
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn allocator_with(bands: u32, size: u64) -> BandAllocator {
        let a = BandAllocator::new();
        for b in layout(0, size, bands) {
            a.add_band(b);
        }
        a
    }

    #[test]
    fn one_file_confines_to_one_band() {
        let a = allocator_with(4, 4096);
        let first = a.plan_sequential(7, 512).unwrap();
        for _ in 0..7 {
            let next = a.plan_sequential(7, 512).unwrap();
            assert_eq!(next.0, first.0, "file must stay in its band");
        }
    }

    #[test]
    fn sequential_offsets_advance() {
        let a = allocator_with(1, 4096);
        let (b1, o1) = a.plan_sequential(1, 256).unwrap();
        let (b2, o2) = a.plan_sequential(1, 256).unwrap();
        assert_eq!(b1, b2);
        assert_eq!(o2, o1 + 256);
    }

    #[test]
    fn files_spread_across_bands() {
        let a = allocator_with(4, 4096);
        let f1 = a.plan_sequential(1, 64).unwrap().0;
        let f2 = a.plan_sequential(2, 64).unwrap().0;
        let f3 = a.plan_sequential(3, 64).unwrap().0;
        // Distinct files land in distinct bands (fresh band per file).
        assert_ne!(f1, f2);
        assert_ne!(f2, f3);
    }

    #[test]
    fn band_full_moves_file_to_new_band() {
        let a = allocator_with(2, 1024);
        let b1 = a.plan_sequential(1, 1024).unwrap().0;
        // Band 1 is full; the file's next write must move.
        let b2 = a.plan_sequential(1, 512).unwrap().0;
        assert_ne!(b1, b2);
    }

    #[test]
    fn random_write_to_band_is_explicitly_rejected() {
        let a = allocator_with(1, 4096);
        let (band, _off) = a.plan_sequential(1, 512).unwrap();
        // A write not at the write pointer (i.e. random overwrite).
        let err = a.validate_write(band, 0, 16).unwrap_err();
        assert_eq!(err.band, band);
        assert_eq!(err.write_pointer, 512);
        assert!(!err.to_string().is_empty());
        // The in-order write is accepted.
        assert!(a.validate_write(band, 512, 16).is_ok());
    }

    #[test]
    fn elevator_sweep_reclaims_garbage_heavy_bands() {
        let a = allocator_with(2, 4096);
        // File 1 writes 4096 into band 0; only 512 bytes stay live.
        a.plan_sequential(1, 4096).unwrap();
        let mut live = std::collections::BTreeMap::new();
        live.insert(0u32, 512u64);
        live.insert(1u32, 4096u64);
        let steps = a.plan_elevator_sweep(&live);
        assert_eq!(steps.len(), 1, "only band 0 is garbage-heavy");
        assert_eq!(steps[0].band, 0);
        assert_eq!(steps[0].live_bytes, 512);
        // The band is now read-only to new writes.
        assert!(a.validate_write(0, 0, 16).is_err());
        // Finishing resets it.
        a.finish_sweep(0);
        assert!(a.band(0).unwrap().is_writable());
        assert_eq!(a.band(0).unwrap().write_pointer, 0);
    }

    #[test]
    fn validate_on_unknown_band_is_noop() {
        let a = allocator_with(1, 4096);
        assert!(a.validate_write(42, 0, 16).is_ok());
    }

    #[test]
    fn enospc_when_no_band_fits() {
        let a = allocator_with(1, 1024);
        assert!(a.plan_sequential(1, 2048).is_none());
    }
}
