//! Validation for `BlockGroupDescriptor` (defined in
//! `ondisk::serialization`), used by `tools::fsck`.

use crate::ondisk::serialization::BlockGroupDescriptor;

/// Basic sanity checks that don't require reading the actual bitmap/inode
/// table data -- just that the descriptor's own numbers are internally
/// consistent (free counts don't exceed a group's own capacity).
pub fn validate(
    bg: &BlockGroupDescriptor,
    blocks_per_group: u32,
    inodes_per_group: u32,
) -> Vec<String> {
    let mut issues = Vec::new();
    if bg.bg_free_blocks_count > blocks_per_group {
        issues.push(format!(
            "bg_free_blocks_count ({}) exceeds blocks_per_group ({blocks_per_group})",
            bg.bg_free_blocks_count
        ));
    }
    if bg.bg_free_inodes_count > inodes_per_group {
        issues.push(format!(
            "bg_free_inodes_count ({}) exceeds inodes_per_group ({inodes_per_group})",
            bg.bg_free_inodes_count
        ));
    }
    if bg.bg_block_bitmap == 0 {
        issues.push("bg_block_bitmap is 0 (unset)".to_string());
    }
    issues
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_bg(free_blocks: u32, free_inodes: u32, bitmap: u64) -> BlockGroupDescriptor {
        BlockGroupDescriptor {
            bg_block_bitmap: bitmap,
            bg_inode_bitmap: bitmap,
            bg_inode_table: 10,
            bg_free_blocks_count: free_blocks,
            bg_free_inodes_count: free_inodes,
            bg_used_dirs_count: 0,
            bg_padding: 0,
            bg_reserved: [0; 32],
        }
    }

    #[test]
    fn valid_descriptor_has_no_issues() {
        let bg = test_bg(100, 50, 5);
        assert!(validate(&bg, 1000, 200).is_empty());
    }

    #[test]
    fn flags_impossible_free_block_count() {
        let bg = test_bg(2000, 50, 5);
        let issues = validate(&bg, 1000, 200);
        assert_eq!(issues.len(), 1);
        assert!(issues[0].contains("bg_free_blocks_count"));
    }

    #[test]
    fn flags_unset_bitmap_pointer() {
        let bg = test_bg(100, 50, 0);
        let issues = validate(&bg, 1000, 200);
        assert!(issues.iter().any(|i| i.contains("bg_block_bitmap")));
    }
}
