//! Extent *sizing* heuristics: given a requested write size (and whether
//! it looks like part of a larger sequential write), how many blocks to
//! actually request from the allocator at once. Allocating a bit ahead of
//! an immediate need reduces how many separate extents a sequentially-
//! written file ends up fragmented across -- relevant because
//! `Inode` only has room for `MAX_INLINE_EXTENTS` (7) extent slots.

use crate::utils::math::blocks_needed;

/// Blocks needed for `requested_bytes`, rounded up to whole blocks, with
/// optional speculative extra growth for sequential-looking writes.
pub fn size_for_request(requested_bytes: u64, block_size: u64, looks_sequential: bool) -> u64 {
    let base = blocks_needed(requested_bytes, block_size);
    if looks_sequential {
        // Modest speculative growth (25%, at least one extra block) --
        // enough to absorb a few more sequential writes into the same
        // extent without over-committing space for a write that turns out
        // to be a one-off.
        base + (base / 4).max(1)
    } else {
        base
    }
}

/// A simple sequential-write detector: true if `offset` picks up exactly
/// where the file's current end is.
pub fn is_sequential_write(offset: u64, current_file_size: u64) -> bool {
    offset == current_file_size
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn non_sequential_write_gets_exact_size() {
        assert_eq!(size_for_request(4096, 4096, false), 1);
    }

    #[test]
    fn sequential_write_gets_speculative_growth() {
        let sized = size_for_request(4096 * 4, 4096, true);
        assert!(sized > 4); // base 4 blocks plus growth
    }

    #[test]
    fn sequential_detector() {
        assert!(is_sequential_write(4096, 4096));
        assert!(!is_sequential_write(0, 4096));
    }
}
