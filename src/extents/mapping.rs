//! Looking up a logical block within an inode's inline extent array --
//! the same search `file::writer::FileManager::get_physical_block`
//! performs inline, factored out so other code (fsck-style validation,
//! `debug::inspect`) doesn't need its own copy of the scan.

use crate::extents::extent::ExtentExt;
use crate::ondisk::serialization::Inode;

/// Returns the physical block backing `logical_block`, or `None` if it
/// falls in a hole (no extent covers it) or past the inode's allocated
/// extents.
pub fn map_logical_to_physical(inode: &Inode, logical_block: u64) -> Option<u64> {
    inode.extents[..inode.extent_count as usize]
        .iter()
        .find(|e| e.contains_logical(logical_block))
        .map(|e| e.physical_start + (logical_block - e.logical_start))
}

/// Total logical blocks spanned by an inode's extents (the highest
/// `logical_end()` across all of them) -- distinct from `inode.size`,
/// which is in bytes and may end mid-block.
pub fn allocated_block_count(inode: &Inode) -> u64 {
    inode.extents[..inode.extent_count as usize]
        .iter()
        .map(|e| e.logical_end())
        .max()
        .unwrap_or(0)
}

/// Every physical block currently allocated to `inode`, in extent order --
/// what `unlink`/truncate need to free, and what a scrubber walking "every
/// allocated block" needs to enumerate.
pub fn all_physical_blocks(inode: &Inode) -> Vec<u64> {
    let mut out = Vec::new();
    for e in &inode.extents[..inode.extent_count as usize] {
        for i in 0..e.length {
            out.push(e.physical_start + i);
        }
    }
    out
}

#[cfg(test)]
fn test_inode_with_extents(extents: &[crate::ondisk::serialization::Extent]) -> Inode {
    let mut arr = [crate::ondisk::serialization::Extent {
        logical_start: 0,
        physical_start: 0,
        length: 0,
    }; crate::ondisk::serialization::MAX_INLINE_EXTENTS];
    for (i, e) in extents.iter().enumerate() {
        arr[i] = *e;
    }
    Inode {
        ino: 1,
        mode: 0,
        uid: 0,
        gid: 0,
        links_count: 1,
        flags: 0,
        padding1: 0,
        size: 0,
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ondisk::serialization::Extent;

    #[test]
    fn maps_a_block_inside_an_extent() {
        let inode = test_inode_with_extents(&[Extent {
            logical_start: 10,
            physical_start: 500,
            length: 5,
        }]);
        assert_eq!(map_logical_to_physical(&inode, 12), Some(502));
    }

    #[test]
    fn returns_none_for_a_hole() {
        let inode = test_inode_with_extents(&[Extent {
            logical_start: 10,
            physical_start: 500,
            length: 5,
        }]);
        assert_eq!(map_logical_to_physical(&inode, 100), None);
        assert_eq!(map_logical_to_physical(&inode, 9), None);
    }

    #[test]
    fn allocated_block_count_uses_highest_extent_end() {
        let inode = test_inode_with_extents(&[
            Extent {
                logical_start: 0,
                physical_start: 100,
                length: 3,
            },
            Extent {
                logical_start: 10,
                physical_start: 200,
                length: 2,
            },
        ]);
        assert_eq!(allocated_block_count(&inode), 12);
    }

    #[test]
    fn all_physical_blocks_enumerates_every_block_in_every_extent() {
        let inode = test_inode_with_extents(&[
            Extent {
                logical_start: 0,
                physical_start: 100,
                length: 2,
            },
            Extent {
                logical_start: 5,
                physical_start: 200,
                length: 3,
            },
        ]);
        assert_eq!(all_physical_blocks(&inode), vec![100, 101, 200, 201, 202]);
    }
}
