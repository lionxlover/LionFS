//! Resolves a path string to an inode number by walking directory entries
//! component by component -- the same job FUSE's per-component `lookup()`
//! calls do collectively, but usable directly by CLI tools (`tools/dump`,
//! `tools/inspect`, `tools/fsck`) that want to accept a human-readable
//! path instead of a raw inode number.

use crate::directory::entries::DirManager;
use crate::inode::manager::InodeManager;
use crate::ondisk::serialization::Inode;
use crate::path::parser::split_components;
use crate::transaction::transaction::TxContext;
use std::io::{Error, ErrorKind, Result};

/// Resolves `path` to an inode number, starting from `root_ino` (normally
/// the filesystem root, inode 1). An empty path or `/` resolves to
/// `root_ino` itself.
pub fn resolve(
    ctx: &mut TxContext,
    inode_tree_root: u64,
    checksum_tree_root: u64,
    bad_blocks_root: u64,
    root_ino: u64,
    path: &str,
) -> Result<u64> {
    let mut current = root_ino;
    for component in split_components(path) {
        let mut dir_inode: Inode = InodeManager::read_inode(ctx, inode_tree_root, current)?;
        if (dir_inode.mode & crate::pal::posix::S_IFDIR) == 0 {
            return Err(Error::new(
                ErrorKind::Other,
                format!("'{component}' is not inside a directory"),
            ));
        }
        let entries =
            DirManager::read_entries(ctx, checksum_tree_root, bad_blocks_root, &mut dir_inode)?;
        match entries.iter().find(|e| e.name == component) {
            Some(e) => current = e.ino,
            None => {
                return Err(Error::new(
                    ErrorKind::NotFound,
                    format!("'{component}' not found"),
                ))
            }
        }
    }
    Ok(current)
}

#[cfg(test)]
mod tests {
    use super::*;

    // A full integration test (create a tiny on-disk filesystem, populate
    // directory entries, resolve a multi-component path against it) needs
    // a real Disk + Superblock + initialized inode/directory trees; see
    // `security::block_cipher`'s tests for that fixture pattern. Skipped
    // here for scope -- `resolve` is a straightforward loop composing
    // `InodeManager::read_inode` and `DirManager::read_entries`, both of
    // which have their own coverage. What's checked here is just the
    // zero-component edge case, which the loop body never touches.
    #[test]
    fn empty_or_root_path_has_no_components_to_walk() {
        assert_eq!(split_components("").len(), 0);
        assert_eq!(split_components("/").len(), 0);
    }
}
