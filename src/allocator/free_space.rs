//! Summarizing free space -- distinct from `allocator::tree::FreeSpaceTree`
//! (the on-disk BTree indexing free runs by physical start), this is
//! plain in-memory reporting logic over a snapshot of that data, for
//! `tools::report`/`tools::health` to render without depending on a live
//! `TxContext`.

use crate::allocator::statistics::{analyze, FragmentationStats};

#[derive(Debug, Clone, PartialEq)]
pub struct FreeSpaceSummary {
    pub stats: FragmentationStats,
    pub largest_extents: Vec<(u64, u64)>, // top N, largest first
}

pub fn summarize(free_extents: &[(u64, u64)], top_n: usize) -> FreeSpaceSummary {
    let stats = analyze(free_extents);
    let mut sorted: Vec<(u64, u64)> = free_extents.to_vec();
    sorted.sort_by(|a, b| b.1.cmp(&a.1));
    sorted.truncate(top_n);
    FreeSpaceSummary {
        stats,
        largest_extents: sorted,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn largest_extents_are_sorted_descending_and_truncated() {
        let extents = vec![(0, 5), (100, 50), (200, 20), (300, 1)];
        let summary = summarize(&extents, 2);
        assert_eq!(summary.largest_extents, vec![(100, 50), (200, 20)]);
    }

    #[test]
    fn top_n_larger_than_available_just_returns_everything() {
        let extents = vec![(0, 5), (100, 50)];
        let summary = summarize(&extents, 10);
        assert_eq!(summary.largest_extents.len(), 2);
    }
}
