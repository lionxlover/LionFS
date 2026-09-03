//! Name lookup within a directory, as its own function rather than the
//! "call `read_entries` then linear-search the returned `Vec`" pattern
//! that was previously duplicated inline in `fs::filesystem::lookup` and
//! `path::resolver::resolve`.
//!
//! This wraps `DirManager::read_entries` rather than re-parsing the
//! on-disk directory record format independently: a from-scratch
//! short-circuiting byte scanner would save an allocation on a hit, but
//! duplicating a hand-parsed binary format in a second place risks the two
//! silently drifting apart, which is worse than the allocation it would
//! save here.

use crate::directory::entries::DirManager;
use crate::ondisk::serialization::Inode;
use crate::transaction::transaction::TxContext;
use std::io::Result;

/// Looks up `name` in `inode` (which must be a directory), returning its
/// inode number and file type if present.
pub fn find_entry(
    ctx: &mut TxContext,
    checksum_tree_root: u64,
    bad_blocks_root: u64,
    inode: &mut Inode,
    name: &str,
) -> Result<Option<(u64, u8)>> {
    let entries = DirManager::read_entries(ctx, checksum_tree_root, bad_blocks_root, inode)?;
    Ok(entries
        .into_iter()
        .find(|e| e.name == name)
        .map(|e| (e.ino, e.file_type)))
}

pub fn contains_entry(
    ctx: &mut TxContext,
    checksum_tree_root: u64,
    bad_blocks_root: u64,
    inode: &mut Inode,
    name: &str,
) -> Result<bool> {
    Ok(find_entry(ctx, checksum_tree_root, bad_blocks_root, inode, name)?.is_some())
}
