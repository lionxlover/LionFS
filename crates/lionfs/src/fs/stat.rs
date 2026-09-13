//! Filesystem-wide usage statistics, factored out of
//! `fs::filesystem::LionFS::statfs` so the computation is testable on its
//! own (a live FUSE `ReplyStatfs` isn't constructible/inspectable outside
//! a real mount).

use crate::ondisk::serialization::{Superblock, BLOCK_SIZE};
#[cfg(test)]
use bytemuck::Zeroable;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FsStats {
    pub total_blocks: u64,
    pub free_blocks: u64,
    pub total_inodes: u64,
    pub free_inodes_estimate: u64,
    pub block_size: u32,
    pub max_name_len: u32,
}

pub fn compute_stats(sb: &Superblock) -> FsStats {
    FsStats {
        total_blocks: sb.total_blocks,
        free_blocks: sb.free_blocks,
        total_inodes: sb.inode_count,
        // See the comment in LionFS::statfs: inodes come from an unbounded
        // on-disk BTree, not a fixed table, so free block count is used as
        // a rough proxy for remaining inode capacity.
        free_inodes_estimate: sb.free_blocks,
        block_size: BLOCK_SIZE as u32,
        max_name_len: crate::common::constants::MAX_NAME_LEN as u32,
    }
}

pub fn used_fraction(stats: &FsStats) -> f64 {
    if stats.total_blocks == 0 {
        return 0.0;
    }
    1.0 - (stats.free_blocks as f64 / stats.total_blocks as f64)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_superblock(total: u64, free: u64) -> Superblock {
        let mut sb = Superblock::zeroed();
        sb.total_blocks = total;
        sb.free_blocks = free;
        sb.inode_count = 10;
        sb
    }

    #[test]
    fn computes_basic_fields_from_superblock() {
        let sb = test_superblock(1000, 400);
        let stats = compute_stats(&sb);
        assert_eq!(stats.total_blocks, 1000);
        assert_eq!(stats.free_blocks, 400);
        assert_eq!(stats.block_size, BLOCK_SIZE as u32);
    }

    #[test]
    fn used_fraction_is_correct() {
        let sb = test_superblock(1000, 250);
        let stats = compute_stats(&sb);
        assert!((used_fraction(&stats) - 0.75).abs() < 1e-9);
    }

    #[test]
    fn used_fraction_of_empty_fs_is_zero_not_a_panic() {
        let sb = test_superblock(0, 0);
        let stats = compute_stats(&sb);
        assert_eq!(used_fraction(&stats), 0.0);
    }
}
