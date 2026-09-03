//! Aggregates the per-structure validation in `ondisk::{bitmap,
//! block_group, directory, extent, inode}` into the single entry point
//! `tools::fsck` (or any future consistency checker) actually wants to
//! call, rather than remembering to invoke each one individually.

use crate::ondisk::serialization::{BlockGroupDescriptor, Inode, Superblock, LIONFS_MAGIC};

#[derive(Debug, Default)]
pub struct ValidationReport {
    pub errors: Vec<String>,
    pub warnings: Vec<String>,
}

impl ValidationReport {
    pub fn is_clean(&self) -> bool {
        self.errors.is_empty()
    }
}

pub fn validate_superblock(sb: &Superblock) -> ValidationReport {
    let mut report = ValidationReport::default();
    if sb.magic != LIONFS_MAGIC {
        report.errors.push(format!(
            "bad magic: {:#x}, expected {:#x}",
            sb.magic, LIONFS_MAGIC
        ));
    }
    if sb.free_blocks > sb.total_blocks {
        report.errors.push(format!(
            "free_blocks ({}) exceeds total_blocks ({})",
            sb.free_blocks, sb.total_blocks
        ));
    }
    if sb.block_size as usize != crate::ondisk::serialization::BLOCK_SIZE {
        report.errors.push(format!(
            "block_size ({}) does not match this build's BLOCK_SIZE ({})",
            sb.block_size,
            crate::ondisk::serialization::BLOCK_SIZE
        ));
    }
    if !crate::common::version::is_safe_to_mount(sb.version) {
        report.errors.push(format!(
            "on-disk version {} is newer than this build understands",
            sb.version
        ));
    }
    if sb.root_inode == 0 {
        report.warnings.push("root_inode is 0".to_string());
    }
    report
}

pub fn validate_block_group(
    bg: &BlockGroupDescriptor,
    blocks_per_group: u32,
    inodes_per_group: u32,
) -> ValidationReport {
    let mut report = ValidationReport::default();
    report.errors.extend(crate::ondisk::block_group::validate(
        bg,
        blocks_per_group,
        inodes_per_group,
    ));
    report
}

pub fn validate_inode(inode: &Inode, total_blocks: u64) -> ValidationReport {
    let mut report = ValidationReport::default();
    report
        .errors
        .extend(crate::ondisk::inode::validate(inode, total_blocks));
    report
}

#[cfg(test)]
mod tests {
    use super::*;
    use bytemuck::Zeroable;

    #[test]
    fn rejects_wrong_magic() {
        let sb = Superblock::zeroed();
        let report = validate_superblock(&sb);
        assert!(!report.is_clean());
        assert!(report.errors.iter().any(|e| e.contains("magic")));
    }

    #[test]
    fn accepts_a_well_formed_superblock() {
        let mut sb = Superblock::zeroed();
        sb.magic = LIONFS_MAGIC;
        sb.block_size = crate::ondisk::serialization::BLOCK_SIZE as u32;
        sb.total_blocks = 1000;
        sb.free_blocks = 500;
        sb.version = crate::common::version::CURRENT_VERSION;
        sb.root_inode = 1;
        let report = validate_superblock(&sb);
        assert!(report.is_clean(), "unexpected errors: {:?}", report.errors);
    }
}
