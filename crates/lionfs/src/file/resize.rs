//! `fallocate`-style pre-allocation: reserving physical blocks for a byte
//! range ahead of time (so a later write can't fail with ENOSPC partway
//! through, and so the range reads back as real zeroed blocks rather than
//! a sparse hole), as opposed to `FileManager::truncate_file`'s "grow"
//! path, which just bumps `inode.size` without allocating anything.

use crate::allocator::bitmap::Allocator;
use crate::ondisk::serialization::{BlockGroupDescriptor, Extent, Inode, BLOCK_SIZE};
use crate::transaction::transaction::TxContext;
use std::io::{Error, ErrorKind, Result};

/// Ensures every logical block in `[start_block, end_block)` has a real
/// physical block backing it, allocating (and zero-filling on disk) any
/// that don't yet. Existing extents covering part of the range are left
/// alone; only genuine holes get new allocations.
pub fn preallocate(
    ctx: &mut TxContext,
    bg_desc: &BlockGroupDescriptor,
    blocks_per_group: u32,
    inode: &mut Inode,
    start_block: u64,
    end_block: u64,
) -> Result<()> {
    if end_block <= start_block {
        return Ok(());
    }
    let zero_block = [0u8; BLOCK_SIZE];
    let mut logical = start_block;
    while logical < end_block {
        if has_backing(inode, logical) {
            logical += 1;
            continue;
        }
        let physical = Allocator::allocate_extents(ctx, bg_desc, blocks_per_group, 1)?;
        ctx.write_block(physical, &zero_block)?;
        add_or_extend(inode, logical, physical)?;
        logical += 1;
    }
    if end_block * BLOCK_SIZE as u64 > inode.size {
        inode.size = end_block * BLOCK_SIZE as u64;
    }
    Ok(())
}

fn has_backing(inode: &Inode, logical_block: u64) -> bool {
    inode.extents[..inode.extent_count as usize]
        .iter()
        .any(|e| logical_block >= e.logical_start && logical_block < e.logical_start + e.length)
}

fn add_or_extend(inode: &mut Inode, logical_block: u64, physical_block: u64) -> Result<()> {
    for i in 0..inode.extent_count as usize {
        let e = &mut inode.extents[i];
        if e.logical_start + e.length == logical_block
            && e.physical_start + e.length == physical_block
        {
            e.length += 1;
            return Ok(());
        }
    }
    if (inode.extent_count as usize) < inode.extents.len() {
        inode.extents[inode.extent_count as usize] = Extent {
            logical_start: logical_block,
            physical_start: physical_block,
            length: 1,
        };
        inode.extent_count += 1;
        Ok(())
    } else {
        Err(Error::new(
            ErrorKind::Other,
            "inode has no free inline extent slots for preallocation",
        ))
    }
}
