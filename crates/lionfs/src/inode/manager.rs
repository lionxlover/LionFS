use crate::inode::tree::InodeTree;
use crate::ondisk::serialization::{Inode, Superblock};
use crate::transaction::transaction::TxContext;
use std::io::{Error, ErrorKind, Result};

pub struct InodeManager;

impl InodeManager {
    pub fn read_inode(ctx: &mut TxContext, inode_tree_root: u64, ino: u64) -> Result<Inode> {
        let tree = InodeTree::new(inode_tree_root);
        tree.get_inode(ctx, ino)
    }

    pub fn write_inode(ctx: &mut TxContext, inode_tree_root: u64, inode: &Inode) -> Result<()> {
        let mut tree = InodeTree::new(inode_tree_root);
        // We need an allocator for tree splits
        let mut allocator = |_ctx: &mut TxContext| -> Result<u64> {
            // Very rudimentary allocator for now; ideally we'd use FreeSpaceTree
            // but for simplicity, we will just grab a block from beyond the data region or
            // from some tracked place. Actually, we shouldn't allocate from thin air.
            // Let's assume we can get one from the Context or Filesystem.
            Err(Error::new(
                ErrorKind::OutOfMemory,
                "Allocator not provided to InodeManager::write_inode",
            ))
        };
        tree.write_inode(ctx, inode, &mut allocator)
    }

    pub fn write_inode_with_allocator<F>(
        ctx: &mut TxContext,
        inode_tree_root: u64,
        inode: &Inode,
        allocate_block: F,
    ) -> Result<()>
    where
        F: FnMut(&mut TxContext) -> Result<u64>,
    {
        let mut tree = InodeTree::new(inode_tree_root);
        tree.write_inode(ctx, inode, allocate_block)
    }

    pub fn allocate_inode(sb: &mut Superblock) -> Result<u64> {
        InodeTree::allocate_inode(sb)
    }
}
