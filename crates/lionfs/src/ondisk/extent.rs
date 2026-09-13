//! On-disk-level validation for `Extent` records (the struct itself lives
//! in `ondisk::serialization`; behavioral helpers like adjacency/merging
//! live in `extents::extent`/`extents::merge` -- this is specifically the
//! "is this a well-formed extent to trust from disk" check `tools::fsck`
//! needs).

use crate::ondisk::serialization::Extent;

pub fn validate(extent: &Extent, total_blocks: u64) -> Result<(), String> {
    if extent.length == 0 {
        return Err("extent has zero length".to_string());
    }
    if extent.physical_start == 0 {
        return Err(
            "extent points at physical block 0, which is reserved for the superblock".to_string(),
        );
    }
    let phys_end = extent
        .physical_start
        .checked_add(extent.length)
        .ok_or_else(|| "extent's physical range overflows u64".to_string())?;
    if phys_end > total_blocks {
        return Err(format!(
            "extent's physical range [{}, {phys_end}) exceeds the filesystem's {total_blocks} total blocks",
            extent.physical_start
        ));
    }
    Ok(())
}

/// Whether two extents' physical ranges overlap -- a real corruption
/// signal if found across two different inodes (the same physical blocks
/// can't validly belong to two independent, non-CoW-linked files).
pub fn physical_ranges_overlap(a: &Extent, b: &Extent) -> bool {
    a.physical_start < b.physical_start + b.length && b.physical_start < a.physical_start + a.length
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ext(physical_start: u64, length: u64) -> Extent {
        Extent {
            logical_start: 0,
            physical_start,
            length,
        }
    }

    #[test]
    fn valid_extent_passes() {
        assert!(validate(&ext(100, 10), 1000).is_ok());
    }

    #[test]
    fn zero_length_is_rejected() {
        assert!(validate(&ext(100, 0), 1000).is_err());
    }

    #[test]
    fn pointing_at_block_zero_is_rejected() {
        assert!(validate(&ext(0, 10), 1000).is_err());
    }

    #[test]
    fn out_of_range_extent_is_rejected() {
        assert!(validate(&ext(995, 10), 1000).is_err());
    }

    #[test]
    fn overlap_detection() {
        assert!(physical_ranges_overlap(&ext(100, 10), &ext(105, 10)));
        assert!(!physical_ranges_overlap(&ext(100, 10), &ext(110, 10)));
    }
}
