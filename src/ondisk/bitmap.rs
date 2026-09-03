//! Consistency checking for the block/inode bitmap regions written by
//! `mkfs` and maintained by `allocator::bitmap::Allocator`. The struct
//! layouts themselves live in `ondisk::serialization`/`allocator::bitmap`;
//! this is validation logic for `tools::fsck` to call, not a new format.

use crate::utils::bit::count_set_bits;

/// Cross-checks a bitmap block's set-bit count against a Superblock's
/// claimed `free_blocks`/`total_blocks` for that region, returning a
/// human-readable description of any mismatch found.
pub fn check_bitmap_consistency(
    bitmap_data: &[u8],
    expected_used_blocks: u64,
) -> Result<(), String> {
    let actual_used = count_set_bits(bitmap_data);
    if actual_used != expected_used_blocks {
        return Err(format!(
            "bitmap reports {actual_used} used blocks, superblock/bg descriptor expects {expected_used_blocks}"
        ));
    }
    Ok(())
}

/// Whether `total_blocks` bits (rounded up to whole bytes) fit within a
/// bitmap region of `bitmap_blocks` blocks -- a basic sizing sanity check
/// mkfs's own layout math should already guarantee, but worth confirming
/// independently when validating an existing image.
pub fn bitmap_region_is_large_enough(
    total_blocks: u64,
    bitmap_blocks: u64,
    block_size: u64,
) -> bool {
    let bits_available = bitmap_blocks * block_size * 8;
    bits_available >= total_blocks
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_mismatched_used_count() {
        let bitmap = [0b0000_1111u8]; // 4 bits set
        assert!(check_bitmap_consistency(&bitmap, 4).is_ok());
        assert!(check_bitmap_consistency(&bitmap, 5).is_err());
    }

    #[test]
    fn region_sizing_check() {
        assert!(bitmap_region_is_large_enough(8, 1, 1)); // 1 block * 1 byte/block... trivial units for the test
        assert!(!bitmap_region_is_large_enough(1000, 1, 1));
    }
}
