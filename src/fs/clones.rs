use crate::btree::tree::BTree;
use crate::ondisk::serialization::{CloneRecord, Superblock};
use crate::transaction::transaction::TxContext;
use std::io::{Error, ErrorKind, Result};

pub const CLONE_TREE_NODE_TYPE: u32 = 9;

pub struct CloneManager {
    tree: BTree<u64, CloneRecord>,
}

impl CloneManager {
    pub fn new(root_block: u64) -> Self {
        Self {
            tree: BTree::new(root_block, CLONE_TREE_NODE_TYPE),
        }
    }

    pub fn create_clone<F>(
        &mut self,
        ctx: &mut TxContext,
        sb: &mut Superblock,
        clone_id: u64,
        source_id: u64,
        allocate_block: &mut F,
    ) -> Result<()>
    where
        F: FnMut(&mut TxContext) -> Result<u64>,
    {
        if self.tree.lookup(ctx, &clone_id)?.is_some() {
            return Err(Error::new(
                ErrorKind::AlreadyExists,
                "Clone ID already exists",
            ));
        }

        let record = CloneRecord {
            id: clone_id,
            source_id,
            generation: sb.generation,
            shared_extents: 0,
            reserved: [0; 4],
        };

        self.tree.insert(ctx, clone_id, record, allocate_block)?;
        sb.clone_tree_root = self.tree.root_block;

        Ok(())
    }

    pub fn get_clone(&self, ctx: &mut TxContext, clone_id: u64) -> Result<Option<CloneRecord>> {
        self.tree.lookup(ctx, &clone_id)
    }

    pub fn delete_clone<F>(
        &mut self,
        ctx: &mut TxContext,
        sb: &mut Superblock,
        clone_id: u64,
        _allocate_block: &mut F,
    ) -> Result<()>
    where
        F: FnMut(&mut TxContext) -> Result<u64>,
    {
        let removed = self.tree.remove(ctx, &clone_id)?;
        if !removed {
            return Err(Error::new(ErrorKind::NotFound, "Clone not found"));
        }
        sb.clone_tree_root = self.tree.root_block;
        Ok(())
    }
}
