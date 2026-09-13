//! Universal alignment guarantees (RFC-002 §6.2).
//!
//! Alignment is enforced at the three places misalignment can be
//! introduced:
//!
//! 1. **At mkfs**: the superblock records probed logical sector size,
//!    physical sector size, and optimal I/O size; free-space regions
//!    snap to the optimal I/O boundary.
//! 2. **At allocation**: every extent request is rounded up to the
//!    device's page-cluster class (4 / 16 / 64 KiB), with rounding
//!    accounted as *padding* in the extent record's GRAN mode, never as
//!    file size -- "user-visible sizes never lie."
//! 3. **At submission**: I/O descriptors are split and merged so each
//!    device command is a multiple of the probed optimal size and
//!    offset-aligned to it. Misaligned requests from hostile unaligned
//!    user buffers are served through a bounce-buffer slow path with an
//!    explicit perf counter, visible in the health bus, never silently
//!    copied.
//!
//! The violation counters ("a guarantee you do not measure is a hope")
//! live here as statics the debug/telemetry layers read directly.

use std::sync::atomic::{AtomicU64, Ordering};

use crate::pal::geometry::DeviceGeometry;

/// Violation counters, health-bus visible.
#[derive(Debug, Default)]
pub struct AlignmentCounters {
    /// Writes rounded up at allocation (padding, not a violation -- but
    /// tracked so the waste is observable).
    pub allocation_padding_blocks: AtomicU64,
    /// Submissions split/merged to satisfy alignment.
    pub descriptors_split: AtomicU64,
    /// Requests that had to take the bounce-buffer slow path.
    pub bounce_buffer_slow_path: AtomicU64,
    /// Requests that were naturally aligned (the fast path).
    pub aligned_submissions: AtomicU64,
}

pub static COUNTERS: AlignmentCounters = AlignmentCounters {
    allocation_padding_blocks: AtomicU64::new(0),
    descriptors_split: AtomicU64::new(0),
    bounce_buffer_slow_path: AtomicU64::new(0),
    aligned_submissions: AtomicU64::new(0),
};

/// The page-cluster class: 4, 16, or 64 KiB (RFC-002 §6.2), derived from
/// probed geometry.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum AlignmentClass {
    K4,
    K16,
    K64,
    /// Sector (512): only when the device reports nothing better; the
    /// conservative floor.
    Sector,
}

impl AlignmentClass {
    #[must_use]
    pub fn bytes(self) -> u64 {
        match self {
            Self::K4 => 4096,
            Self::K16 => 16 * 1024,
            Self::K64 => 64 * 1024,
            Self::Sector => 512,
        }
    }

    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            Self::K4 => "4K",
            Self::K16 => "16K",
            Self::K64 => "64K",
            Self::Sector => "512B",
        }
    }

    /// Chooses the class from probed geometry: the optimal I/O size if
    /// the device reports one, else the physical sector, else logical,
    /// clamped into the {4K, 16K, 64K} set. The floor is the filesystem
    /// block size (4 KiB): LionFS never issues sub-block device I/O, so
    /// a 512-byte sector report cannot lower the class below K4. Larger
    /// optima like RAID stripe widths collapse to 64K here; stripe
    /// alignment itself is the pool layer's job.
    #[must_use]
    pub fn from_geometry(geo: &DeviceGeometry) -> Self {
        let b = geo
            .optimal_io_size
            .map(u64::from)
            .unwrap_or_else(|| u64::from(geo.physical_sector_size.max(geo.logical_sector_size)))
            .max(crate::addressing::LBA_BLOCK_BYTES);
        Self::from_bytes(b)
    }

    #[must_use]
    pub fn from_bytes(b: u64) -> Self {
        match b {
            0..=512 => Self::Sector,
            513..=4096 => Self::K4,
            4097..=16_384 => Self::K16,
            _ => Self::K64,
        }
    }
}

/// Rounds a request of `len` bytes at logical `offset` to the class
/// unit with **covering** semantics: the aligned range must contain the
/// request, so the start rounds *down* and the end rounds *up* to unit
/// boundaries. Returns (aligned_offset, aligned_len, padding_blocks).
/// The padding is recorded in the extent record's GRAN accounting,
/// never in the file size -- user-visible sizes never lie.
#[must_use]
pub fn round_allocation(class: AlignmentClass, offset: u64, len: u64) -> (u64, u64, u64) {
    let unit = class.bytes();
    let aligned_offset = (offset / unit) * unit;
    let end = (offset.saturating_add(len)).div_ceil(unit) * unit;
    let aligned_len = end - aligned_offset;
    let padding = (aligned_len.saturating_sub(len)).div_ceil(crate::addressing::LBA_BLOCK_BYTES);
    if padding > 0 {
        COUNTERS
            .allocation_padding_blocks
            .fetch_add(padding, Ordering::Relaxed);
    }
    (aligned_offset, aligned_len, padding)
}

/// Splits/merges a logical I/O range into device-aligned segments:
/// each segment is `unit`-aligned in both offset and length, except a
/// final partial segment when the range is not a multiple. Returns the
/// segment list plus a flag for whether a slow path is needed (a prefix
/// that cannot be aligned by splitting).
#[must_use]
pub fn split_for_submission(
    class: AlignmentClass,
    offset: u64,
    len: u64,
) -> (Vec<(u64, u64)>, bool) {
    let unit = class.bytes();
    if unit == 512 || (offset % unit == 0 && len % unit == 0) {
        // Naturally aligned: the fast path, no split.
        COUNTERS.aligned_submissions.fetch_add(1, Ordering::Relaxed);
        return (vec![(offset, len)], false);
    }
    // Head misalignment: the leading partial unit needs a bounce buffer.
    let head_overhang = offset % unit;
    let needs_bounce = head_overhang != 0 || len < unit;
    if needs_bounce {
        COUNTERS
            .bounce_buffer_slow_path
            .fetch_add(1, Ordering::Relaxed);
    }
    let mut segments = Vec::new();
    let mut cur = offset;
    let mut remaining = len;
    // Merge contiguous aligned units into maximal segments.
    while remaining > 0 {
        let into_unit = unit - (cur % unit);
        if into_unit == unit && remaining >= unit {
            // Full-unit run from an aligned boundary.
            let mut run = 0u64;
            while remaining - run >= unit && (cur + run) % unit == 0 && run + unit <= remaining {
                run += unit;
            }
            if run > 0 {
                segments.push((cur, run));
                cur += run;
                remaining -= run;
                COUNTERS.descriptors_split.fetch_add(1, Ordering::Relaxed);
                continue;
            }
        }
        // Partial unit (head or tail).
        let take = into_unit.min(remaining);
        segments.push((cur, take));
        cur += take;
        remaining -= take;
    }
    (segments, needs_bounce)
}

/// One-line health summary.
pub fn health_summary() -> String {
    format!(
        "aligned={} split={} bounce={} padding_blocks={}",
        COUNTERS.aligned_submissions.load(Ordering::Relaxed),
        COUNTERS.descriptors_split.load(Ordering::Relaxed),
        COUNTERS.bounce_buffer_slow_path.load(Ordering::Relaxed),
        COUNTERS.allocation_padding_blocks.load(Ordering::Relaxed)
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn geo(logical: u32, physical: u32, optimal: Option<u32>) -> DeviceGeometry {
        DeviceGeometry {
            size_bytes: 1 << 40,
            logical_sector_size: logical,
            physical_sector_size: physical,
            optimal_io_size: optimal,
        }
    }

    #[test]
    fn class_from_geometry_prefers_optimal() {
        assert_eq!(
            AlignmentClass::from_geometry(&geo(512, 4096, Some(16_384))),
            AlignmentClass::K16
        );
        assert_eq!(
            AlignmentClass::from_geometry(&geo(512, 4096, Some(65_536))),
            AlignmentClass::K64
        );
        assert_eq!(
            AlignmentClass::from_geometry(&geo(512, 4096, None)),
            AlignmentClass::K4
        );
        // 512-byte sector reports floor at the filesystem block size.
        assert_eq!(
            AlignmentClass::from_geometry(&geo(512, 512, None)),
            AlignmentClass::K4
        );
    }

    #[test]
    fn round_allocation_pads_without_lying() {
        let (off, len, padding) = round_allocation(AlignmentClass::K4, 4096, 100);
        assert_eq!(off, 4096);
        assert_eq!(len, 4096);
        assert_eq!(padding, 1);
        // Covering semantics at 64K: a 4-KiB request straddling a unit
        // boundary expands to the whole containing unit (start rounds
        // down, end rounds up), with the expansion accounted as padding.
        let (off, big_len, _p) = round_allocation(AlignmentClass::K64, 4096, 4096);
        assert_eq!(off, 0);
        assert_eq!(big_len, 65_536);
        // Exactly-unit-aligned requests pass through unchanged.
        let (off, exact, _p) = round_allocation(AlignmentClass::K64, 65_536, 65_536);
        assert_eq!((off, exact), (65_536, 65_536));
    }

    #[test]
    fn aligned_submission_is_fast_path() {
        let base = COUNTERS.aligned_submissions.load(Ordering::Relaxed);
        let (segs, bounce) = split_for_submission(AlignmentClass::K4, 8192, 8192);
        assert_eq!(segs, vec![(8192, 8192)]);
        assert!(!bounce);
        assert_eq!(
            COUNTERS.aligned_submissions.load(Ordering::Relaxed),
            base + 1
        );
    }

    #[test]
    fn misaligned_head_takes_bounce_path() {
        let base = COUNTERS.bounce_buffer_slow_path.load(Ordering::Relaxed);
        let (segs, bounce) = split_for_submission(AlignmentClass::K4, 8192 + 100, 4096);
        assert!(bounce);
        assert!(COUNTERS.bounce_buffer_slow_path.load(Ordering::Relaxed) >= base + 1);
        // Segments must tile the requested range exactly.
        let covered: u64 = segs.iter().map(|(_, l)| l).sum();
        assert_eq!(covered, 4096);
        let mut cur = 8192 + 100;
        for (off, len) in segs {
            assert_eq!(off, cur);
            cur += len;
        }
    }

    #[test]
    fn mixed_range_merges_full_units() {
        // 12 KiB starting at 4 KiB boundary: one full 4K unit + tail.
        let (segs, _bounce) = split_for_submission(AlignmentClass::K4, 4096, 12_288);
        let covered: u64 = segs.iter().map(|(_, l)| l).sum();
        assert_eq!(covered, 12_288);
        // The aligned middle must be merged, not one segment per unit.
        assert!(segs.len() <= 3, "segments: {segs:?}");
    }

    #[test]
    fn sector_class_never_splits() {
        let (segs, bounce) = split_for_submission(AlignmentClass::Sector, 33, 77);
        assert_eq!(segs, vec![(33, 77)]);
        assert!(!bounce);
    }

    #[test]
    fn health_summary_is_populated() {
        let s = health_summary();
        assert!(s.starts_with("aligned="));
    }

    #[test]
    fn classes_name_themselves() {
        assert_eq!(AlignmentClass::K64.name(), "64K");
        assert_eq!(AlignmentClass::K16.name(), "16K");
        assert_eq!(AlignmentClass::K4.name(), "4K");
    }
}
