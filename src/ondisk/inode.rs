//! On-disk-level validation for `Inode` records, for `tools::fsck`.
//! Constructors/convenience methods live in `inode::inode`; this is
//! specifically about whether a given `Inode` as read from disk looks
//! internally consistent.

use crate::extents::extent;
use crate::ondisk::serialization::{Inode, MAX_INLINE_EXTENTS};

pub fn validate(inode: &Inode, total_blocks: u64) -> Vec<String> {
    let mut issues = Vec::new();

    if inode.extent_count as usize > MAX_INLINE_EXTENTS {
        issues.push(format!(
            "extent_count ({}) exceeds MAX_INLINE_EXTENTS ({MAX_INLINE_EXTENTS})",
            inode.extent_count
        ));
        return issues; // further extent checks would index out of bounds
    }

    // Phase 4 (format v2): compressed inodes keep no inline extents --
    // their mapping lives in the ClusterTree rooted at
    // spill_extent_root, which needs a TxContext to walk and is
    // therefore validated by tools that have one, not here. What CAN be
    // checked structurally: no inline extents at all.
    if inode.compression_algo != 0 {
        if inode.extent_count != 0 {
            issues.push(format!("compressed inode has {} inline extents (expected 0: cluster inodes store their mapping in the ClusterTree)", inode.extent_count));
        }
        return issues;
    }

    let used_extents = &inode.extents[..inode.extent_count as usize];
    for (i, e) in used_extents.iter().enumerate() {
        if let Err(msg) = extent::validate(e, total_blocks) {
            issues.push(format!("extent[{i}]: {msg}"));
        }
    }
    for i in 0..used_extents.len() {
        for j in (i + 1)..used_extents.len() {
            if extent::physical_ranges_overlap(&used_extents[i], &used_extents[j]) {
                issues.push(format!(
                    "extent[{i}] and extent[{j}] overlap in physical space"
                ));
            }
        }
    }

    let allocated_bytes = crate::extents::mapping::allocated_block_count(inode)
        * crate::ondisk::serialization::BLOCK_SIZE as u64;
    if inode.size > allocated_bytes {
        issues.push(format!(
            "inode.size ({}) exceeds what its extents actually allocate ({allocated_bytes} bytes)",
            inode.size
        ));
    }

    issues
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ondisk::serialization::Extent;

    fn test_inode(size: u64, extents: &[Extent]) -> Inode {
        let mut arr = [Extent {
            logical_start: 0,
            physical_start: 0,
            length: 0,
        }; MAX_INLINE_EXTENTS];
        for (i, e) in extents.iter().enumerate() {
            arr[i] = *e;
        }
        Inode {
            ino: 2,
            mode: 0,
            uid: 0,
            gid: 0,
            links_count: 1,
            flags: 0,
            padding1: 0,
            size,
            ctime: 0,
            mtime: 0,
            atime: 0,
            extent_count: extents.len() as u16,
            compression_algo: 0,
            encryption_algo: 0,
            key_id: 0,
            extents: arr,
            checksum: 0,
            spill_pad_head: [0; 4],
            spill_extent_root: 0,
        }
    }

    #[test]
    fn well_formed_inode_has_no_issues() {
        let inode = test_inode(
            4096,
            &[Extent {
                logical_start: 0,
                physical_start: 100,
                length: 1,
            }],
        );
        assert!(validate(&inode, 1000).is_empty());
    }

    #[test]
    fn size_exceeding_allocation_is_flagged() {
        let inode = test_inode(
            999_999,
            &[Extent {
                logical_start: 0,
                physical_start: 100,
                length: 1,
            }],
        );
        let issues = validate(&inode, 1000);
        assert!(issues
            .iter()
            .any(|i| i.contains("exceeds what its extents")));
    }

    #[test]
    fn overlapping_extents_are_flagged() {
        let inode = test_inode(
            8192,
            &[
                Extent {
                    logical_start: 0,
                    physical_start: 100,
                    length: 5,
                },
                Extent {
                    logical_start: 5,
                    physical_start: 102,
                    length: 5,
                },
            ],
        );
        let issues = validate(&inode, 1000);
        assert!(issues.iter().any(|i| i.contains("overlap")));
    }
}
