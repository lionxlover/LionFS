//! Path-based inspection: resolve a human-readable path to an inode and
//! describe it, combining `path::resolver` with `debug::dump`'s
//! formatting -- what a `tools::inspect <image> <path>` invocation would
//! call, as opposed to `tools::dump`, which works by raw inode number.

use crate::debug::dump::format_inode;
use crate::inode::manager::InodeManager;
use crate::ondisk::serialization::Inode;
use crate::path::resolver::resolve;
use crate::transaction::transaction::TxContext;
use std::io::Result;

pub fn inspect_path(
    ctx: &mut TxContext,
    inode_tree_root: u64,
    checksum_tree_root: u64,
    bad_blocks_root: u64,
    root_ino: u64,
    path: &str,
) -> Result<String> {
    let ino = resolve(
        ctx,
        inode_tree_root,
        checksum_tree_root,
        bad_blocks_root,
        root_ino,
        path,
    )?;
    let inode: Inode = InodeManager::read_inode(ctx, inode_tree_root, ino)?;
    Ok(format!("path: {path}\n{}", format_inode(&inode)))
}
