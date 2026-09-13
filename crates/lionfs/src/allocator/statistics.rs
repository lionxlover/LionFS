//! Fragmentation and utilization statistics over a set of free-space
//! extents -- for `tools::health`/`tools::report` to surface, and for a
//! future allocation policy (`allocator::policies`) to consult when
//! deciding whether it's worth searching harder for a contiguous run.

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct FragmentationStats {
    pub free_extent_count: u64,
    pub total_free_blocks: u64,
    pub largest_free_extent: u64,
    /// 0.0 (one single free extent -- perfectly unfragmented) to close to
    /// 1.0 (many small, scattered free extents).
    pub fragmentation_ratio: f64,
}

/// `free_extents` is a list of (start, length) pairs, in any order.
pub fn analyze(free_extents: &[(u64, u64)]) -> FragmentationStats {
    let free_extent_count = free_extents.len() as u64;
    let total_free_blocks: u64 = free_extents.iter().map(|(_, len)| len).sum();
    let largest_free_extent = free_extents.iter().map(|(_, len)| *len).max().unwrap_or(0);

    let fragmentation_ratio = if total_free_blocks == 0 || free_extent_count <= 1 {
        0.0
    } else {
        // 1 - (largest extent's share of total free space): a single
        // extent holding all free space scores 0; free space split evenly
        // across many small extents approaches 1.
        1.0 - (largest_free_extent as f64 / total_free_blocks as f64)
    };

    FragmentationStats {
        free_extent_count,
        total_free_blocks,
        largest_free_extent,
        fragmentation_ratio,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn single_extent_is_unfragmented() {
        let stats = analyze(&[(0, 1000)]);
        assert_eq!(stats.fragmentation_ratio, 0.0);
        assert_eq!(stats.largest_free_extent, 1000);
    }

    #[test]
    fn many_equal_extents_score_highly_fragmented() {
        let extents: Vec<(u64, u64)> = (0..10).map(|i| (i * 100, 10)).collect();
        let stats = analyze(&extents);
        assert!(stats.fragmentation_ratio > 0.85);
        assert_eq!(stats.total_free_blocks, 100);
    }

    #[test]
    fn no_free_space_does_not_panic() {
        let stats = analyze(&[]);
        assert_eq!(stats.fragmentation_ratio, 0.0);
        assert_eq!(stats.total_free_blocks, 0);
    }
}
