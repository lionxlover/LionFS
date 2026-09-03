//! Small `Extent` predicates and helpers -- whether two extents are
//! adjacent, whether one contains a logical offset, computing where an
//! extent ends. Used by `extents::merge`/`extents::split` and available
//! for the inline-extent scanning in `file::writer::FileManager` to adopt.

use crate::ondisk::serialization::Extent;

pub trait ExtentExt {
    fn logical_end(&self) -> u64;
    fn physical_end(&self) -> u64;
    fn contains_logical(&self, logical_block: u64) -> bool;
    fn is_adjacent_to(&self, other: &Extent) -> bool;
}

impl ExtentExt for Extent {
    fn logical_end(&self) -> u64 {
        self.logical_start + self.length
    }

    fn physical_end(&self) -> u64 {
        self.physical_start + self.length
    }

    fn contains_logical(&self, logical_block: u64) -> bool {
        logical_block >= self.logical_start && logical_block < self.logical_end()
    }

    /// True if `self` immediately precedes `other` in *both* logical and
    /// physical address space -- the condition under which they represent
    /// one contiguous run of blocks and could be merged into a single
    /// extent (see `extents::merge`).
    fn is_adjacent_to(&self, other: &Extent) -> bool {
        self.logical_end() == other.logical_start && self.physical_end() == other.physical_start
    }
}

/// On-disk sanity validation for a single extent: starts must not wrap,
/// length must be nonzero, and the physical range must fit inside the
/// device. Used by `ondisk::inode` (fsck-style checks).
pub fn validate(e: &Extent, total_blocks: u64) -> std::result::Result<(), String> {
    if e.length == 0 {
        return Err("zero-length extent".to_string());
    }
    if e.logical_end() < e.logical_start {
        return Err(format!(
            "logical range wraps u64 (start={}, length={})",
            e.logical_start, e.length
        ));
    }
    if e.physical_end() < e.physical_start {
        return Err(format!(
            "physical range wraps u64 (start={}, length={})",
            e.physical_start, e.length
        ));
    }
    if e.physical_end() > total_blocks {
        return Err(format!(
            "physical range [{}, {}) exceeds device capacity of {} blocks",
            e.physical_start,
            e.physical_end(),
            total_blocks
        ));
    }
    Ok(())
}

/// True if two extents' physical half-open ranges share at least one
/// block -- i.e. they alias the same storage, which is always an
/// inconsistency for non-deduplicated data extents.
pub fn physical_ranges_overlap(a: &Extent, b: &Extent) -> bool {
    a.physical_start < b.physical_end() && b.physical_start < a.physical_end()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ext(logical_start: u64, physical_start: u64, length: u64) -> Extent {
        Extent {
            logical_start,
            physical_start,
            length,
        }
    }

    #[test]
    fn contains_logical_is_half_open() {
        let e = ext(10, 100, 5); // covers logical blocks 10..15
        assert!(!e.contains_logical(9));
        assert!(e.contains_logical(10));
        assert!(e.contains_logical(14));
        assert!(!e.contains_logical(15));
    }

    #[test]
    fn adjacency_requires_both_logical_and_physical_continuity() {
        let a = ext(0, 100, 5); // logical 0..5, physical 100..105
        let contiguous = ext(5, 105, 3);
        let logically_next_but_physically_scattered = ext(5, 999, 3);
        assert!(a.is_adjacent_to(&contiguous));
        assert!(!a.is_adjacent_to(&logically_next_but_physically_scattered));
    }
}
