//! 3.6 Reflink clones: Btrfs `clone_range` / APFS `clonefile` parity.
//!
//! A clone maps the SOURCE's physical blocks into the DESTINATION and
//! pins them via the refcount coverage tree -- the exact machinery
//! deduplication already uses, and the write path ALREADY handles the
//! safety rule: a write to a pinned block allocates a fresh block,
//! copies the old content, and remaps (redirect-on-write). So two
//! files may share every physical block while both remain writable,
//! and the pin is what makes that safe.
//!
//! Contract (enforced by [`reflink_file`]):
//! * source must be an uncompressed regular file with data;
//! * destination must be an uncompressed regular file of size 0
//!   (the kernel's `copy_file_range` on an empty destination; the
//!   `lfs_clone reflink` tool creates the destination itself);
//! * compressed sources are refused (`ENOTSUP`) -- the cluster path
//!   has no per-block identity to share yet (honest limitation);
//! * the destination gets its OWN spill extent tree populated with
//!   the same mappings -- the source's tree is never shared (its
//!   nodes mutate in place when no snapshot barrier is live).
//!
//! Space accounting: shared blocks are NOT re-allocated; `statfs`
//! free space does not move (Btrfs/APFS behavior). A `CloneRecord`
//! lands in the clone registry tree (node type 9) for tooling and
//! the conformance suite. Deleting either side leaves the blocks
//! pinned (the dedup posture) -- the Copy-GC sweep reclaims them.

use std::io::{Error, ErrorKind, Result};
use std::sync::Arc;

use crate::allocator::bitmap::Allocator;
use crate::fs::clones::CloneManager;
use crate::fs::filesystem::SharedCore;
use crate::ondisk::serialization::Inode;
use crate::transaction::transaction::TxContext;
use crate::{extents::tree::ExtentTree, integrity::refcount::RefCountManager, inode::manager::InodeManager};

/// Clone (reflink) `src_ino` into the empty inode `dst_ino` inside one
/// already-open staging context. Returns the number of physical blocks
/// now shared.
///
/// Lock discipline: caller holds the staging lock (this runs inside
/// `with_stage_ctx`); superblock writes go through `core.sb_write()`
/// (staging -> superblock-write order, the `allocate_inode` pattern).
pub fn reflink_file_in_ctx(
    core: &Arc<SharedCore>,
    ctx: &mut TxContext,
    sb: &crate::ondisk::serialization::Superblock,
    src_ino: u64,
    dst_ino: u64,
) -> Result<u64> {
    let bg_desc = core.get_bg_desc();
    let blocks_per_group = sb.blocks_per_group;

    let src = InodeManager::read_inode(ctx, sb.inode_tree_root, src_ino)?;
    let mut dst = InodeManager::read_inode(ctx, sb.inode_tree_root, dst_ino)?;

    if crate::pal::posix::file_type_of(src.mode) != crate::pal::posix::S_IFREG {
        return Err(Error::new(ErrorKind::InvalidInput, "reflink source must be a regular file"));
    }
    if crate::pal::posix::file_type_of(dst.mode) != crate::pal::posix::S_IFREG {
        return Err(Error::new(ErrorKind::InvalidInput, "reflink destination must be a regular file"));
    }
    if src.compression_algo != 0 || dst.compression_algo != 0 {
        return Err(Error::new(
            ErrorKind::Unsupported,
            "reflink of compressed inodes is not supported (no per-block identity to share)",
        ));
    }
    if dst.size != 0 || dst.extent_count != 0 || dst.spill_extent_root != 0 {
        return Err(Error::new(ErrorKind::InvalidInput, "reflink destination must be empty"));
    }
    if src.size == 0 {
        // Nothing to share; an empty clone is legal and free.
        dst.size = 0;
        dst.ctime = src.ctime;
        dst.mtime = src.mtime;
        InodeManager::write_inode_with_allocator(ctx, sb.inode_tree_root, &dst, |c| {
            Allocator::allocate_extents(c, &bg_desc, blocks_per_group, 1)
        })?;
        return Ok(0);
    }

    // The refcount tree is the pin ledger; initialize it on first use
    // (mkfs leaves refcount_tree_root at 0).
    let mut refcount_root = sb.refcount_tree_root;
    if refcount_root == 0 {
        let root = Allocator::allocate_extents_meta(ctx, &bg_desc, blocks_per_group, 1)?;
        RefCountManager::init_empty(ctx, root)?;
        refcount_root = root;
        core.sb_write().refcount_tree_root = refcount_root;
        // Durable publication (see vfs_impl setxattr: first-use tree
        // roots ride the transaction root cells).
        ctx.set_root_cell(crate::integrity::refcount::REFCOUNT_TREE_NODE_TYPE, root);
    }
    // The clone registry likewise (clone_tree_root 0 until first clone).
    let mut clone_root = sb.clone_tree_root;
    if clone_root == 0 {
        let root = Allocator::allocate_extents_meta(ctx, &bg_desc, blocks_per_group, 1)?;
        CloneManager::init_tree(ctx, root)?;
        clone_root = root;
        core.sb_write().clone_tree_root = clone_root;
        ctx.set_root_cell(crate::fs::clones::CLONE_TREE_NODE_TYPE, root);
    }

    let mut allocate = |c: &mut TxContext| {
        Allocator::allocate_extents_meta(c, &bg_desc, blocks_per_group, 1)
    };
    let mut shared_blocks: u64 = 0;

    // 1. Inline extents: copy the mapping list by value.
    for i in 0..src.extent_count as usize {
        dst.extents[i] = src.extents[i];
    }
    dst.extent_count = src.extent_count;
    for i in 0..src.extent_count as usize {
        let e = &src.extents[i];
        if e.physical_start != 0 && e.length > 0 {
            RefCountManager::new(refcount_root).pin_range(
                ctx,
                e.physical_start,
                e.length,
                &mut allocate,
            )?;
            shared_blocks += e.length;
        }
    }

    // 2. Spilled extents: build the destination's OWN tree with the
    // same mappings (never share the source's B-tree nodes).
    if src.spill_extent_root != 0 {
        let src_tree = ExtentTree::new(src.spill_extent_root);
        let new_root = Allocator::allocate_extents_meta(ctx, &bg_desc, blocks_per_group, 1)?;
        ExtentTree::init_empty(ctx, new_root)?;
        let mut dst_tree = ExtentTree::new(new_root);
        for (logical, value) in src_tree.iter_extents(ctx)? {
            dst_tree.insert(ctx, logical, value.physical_start, value.length, &mut allocate)?;
            if value.physical_start != 0 && value.length > 0 {
                RefCountManager::new(refcount_root).pin_range(
                    ctx,
                    value.physical_start,
                    value.length,
                    &mut allocate,
                )?;
                shared_blocks += value.length;
            }
        }
        dst.spill_extent_root = dst_tree.btree.root_block;
    }

    // 3. Registry record (root cell published by the B-tree; commit_tx
    // syncs CLONE_TREE_NODE_TYPE into sb.clone_tree_root).
    let mut clones = CloneManager::new(clone_root);
    clones.record_clone(
        ctx,
        dst_ino,
        src_ino,
        crate::btree::tree::node_gen_stamp(),
        shared_blocks,
        &mut allocate,
    )?;

    dst.size = src.size;
    dst.ctime = src.ctime;
    dst.mtime = src.mtime;

    InodeManager::write_inode_with_allocator(ctx, sb.inode_tree_root, &dst, |c| {
        Allocator::allocate_extents(c, &bg_desc, blocks_per_group, 1)
    })?;

    core.sb_write().fs_features |= crate::common::version::FS_FEATURE_REFLINK;
    Ok(shared_blocks)
}

// (The old `create_clone(&mut Superblock)` variant remains for its
// original tool callers; the live path uses `record_clone` + root
// cells -- no superblock handle is available mid-transaction.)

/// What of a source file can be reflinked right now (the tools surface
/// this as a dry-run answer).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ReflinkFeasibility {
    pub supported: bool,
    pub reason: &'static str,
    pub size_bytes: u64,
}

#[must_use]
pub fn feasibility(src: &Inode) -> ReflinkFeasibility {
    if crate::pal::posix::file_type_of(src.mode) != crate::pal::posix::S_IFREG {
        ReflinkFeasibility { supported: false, reason: "not a regular file", size_bytes: src.size }
    } else if src.compression_algo != 0 {
        ReflinkFeasibility {
            supported: false,
            reason: "compressed inode (cluster path)",
            size_bytes: src.size,
        }
    } else {
        ReflinkFeasibility { supported: true, reason: "", size_bytes: src.size }
    }
}
