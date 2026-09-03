//! Hole-punching: deallocating a byte range in the *middle* of a file
//! without changing its size, unlike `file::writer::FileManager::truncate_file`
//! (which only shortens/extends from the end). Not wired into a FUSE
//! handler (there's no `fallocate` callback implemented yet), but a real,
//! usable primitive built on the same `extents::split` logic truncate uses
//! for its partial-extent case.

use crate::allocator::bitmap::Allocator;
use crate::extents::split::remove_range;
use crate::ondisk::serialization::{BlockGroupDescriptor, Extent, Inode, BLOCK_SIZE};
use std::io::Result;

/// Frees the blocks covering `[start_block, end_block)` and removes them
/// from `inode`'s extent list, leaving a hole. `inode.size` is unchanged
/// (a hole reads back as zeros, same as any other gap between extents).
pub fn punch_hole(
    ctx: &mut crate::transaction::transaction::TxContext,
    bg_desc: &BlockGroupDescriptor,
    inode: &mut Inode,
    start_block: u64,
    end_block: u64,
) -> Result<()> {
    if end_block <= start_block {
        return Ok(());
    }

    let mut surviving: Vec<Extent> = Vec::with_capacity(inode.extent_count as usize);
    for i in 0..inode.extent_count as usize {
        let extent = inode.extents[i];
        let overlaps =
            start_block < extent.logical_start + extent.length && end_block > extent.logical_start;
        if !overlaps {
            surviving.push(extent);
            continue;
        }
        // Free exactly the overlapping physical range, then keep whatever
        // pieces (0, 1, or 2) remain outside the punched range.
        let overlap_start = start_block.max(extent.logical_start);
        let overlap_end = end_block.min(extent.logical_start + extent.length);
        let phys_overlap_start = extent.physical_start + (overlap_start - extent.logical_start);
        Allocator::free_extents(
            ctx,
            bg_desc,
            phys_overlap_start,
            overlap_end - overlap_start,
        )?;
        surviving.extend(remove_range(&extent, start_block, end_block));
    }

    let extents_len = inode.extents.len();
    for (i, e) in surviving.iter().enumerate() {
        inode.extents[i] = *e;
    }
    for slot in inode
        .extents
        .iter_mut()
        .take(extents_len)
        .skip(surviving.len())
    {
        *slot = Extent {
            logical_start: 0,
            physical_start: 0,
            length: 0,
        };
    }
    inode.extent_count = surviving.len() as u16;
    Ok(())
}

pub fn punch_hole_bytes(
    ctx: &mut crate::transaction::transaction::TxContext,
    bg_desc: &BlockGroupDescriptor,
    inode: &mut Inode,
    start_byte: u64,
    length_bytes: u64,
) -> Result<()> {
    let start_block = start_byte / BLOCK_SIZE as u64;
    let end_block = (start_byte + length_bytes).div_ceil(BLOCK_SIZE as u64);
    punch_hole(ctx, bg_desc, inode, start_block, end_block)
}
