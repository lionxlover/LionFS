use crate::btree::tree::BTree;
use crate::ondisk::serialization::{Inode, Superblock};
use crate::transaction::transaction::TxContext;
use std::io::{Error, ErrorKind, Result};

pub const INODE_TREE_NODE_TYPE: u32 = 1;

pub struct InodeTree {
    pub btree: BTree<u64, Inode>,
}

impl InodeTree {
    pub fn new(root_block: u64) -> Self {
        Self {
            btree: BTree::new(root_block, INODE_TREE_NODE_TYPE),
        }
    }

    pub fn init_empty(ctx: &mut TxContext, root_block: u64) -> Result<()> {
        BTree::<u64, Inode>::init_empty(ctx, root_block, INODE_TREE_NODE_TYPE)
    }

    pub fn get_inode(&self, ctx: &mut TxContext, ino: u64) -> Result<Inode> {
        if let Some(inode) = self.btree.lookup(ctx, &ino)? {
            Ok(inode)
        } else {
            Err(Error::new(ErrorKind::NotFound, "Inode not found"))
        }
    }

    pub fn write_inode<F>(
        &mut self,
        ctx: &mut TxContext,
        inode: &Inode,
        allocate_block: F,
    ) -> Result<()>
    where
        F: FnMut(&mut TxContext) -> Result<u64>,
    {
        self.btree.insert(ctx, inode.ino, *inode, allocate_block)
    }

    pub fn allocate_inode(sb: &mut Superblock) -> Result<u64> {
        let ino = sb.next_ino;
        sb.next_ino += 1;
        sb.inode_count += 1;
        Ok(ino)
    }
}
