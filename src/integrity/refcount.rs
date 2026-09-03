use crate::btree::tree::BTree;
use crate::transaction::transaction::TxContext;
use std::io::{Error, ErrorKind, Result};

pub const REFCOUNT_TREE_NODE_TYPE: u32 = 7;

/// RefCountManager tracks block reference counts for CoW.
/// Blocks with implicit refcount = 1 are not stored in the tree.
/// Only blocks with refcount > 1 (shared) or some special cases are in the tree.
pub struct RefCountManager {
    tree: BTree<u64, u32>,
}

impl RefCountManager {
    pub fn new(root_block: u64) -> Self {
        Self {
            tree: BTree::new(root_block, REFCOUNT_TREE_NODE_TYPE),
        }
    }

    pub fn get_refcount(&self, ctx: &mut TxContext, block_num: u64) -> Result<u32> {
        match self.tree.lookup(ctx, &block_num)? {
            Some(count) => Ok(count),
            None => Ok(1), // Default to 1 if allocated but not explicitly tracked
        }
    }

    pub fn increment<F>(
        &mut self,
        ctx: &mut TxContext,
        block_num: u64,
        allocate_block: &mut F,
    ) -> Result<u32>
    where
        F: FnMut(&mut TxContext) -> Result<u64>,
    {
        let count = match self.tree.lookup(ctx, &block_num)? {
            Some(c) => c + 1,
            None => 2, // 1 -> 2
        };

        self.tree.insert(ctx, block_num, count, allocate_block)?;
        Ok(count)
    }

    pub fn decrement<F>(
        &mut self,
        ctx: &mut TxContext,
        block_num: u64,
        allocate_block: &mut F,
    ) -> Result<u32>
    where
        F: FnMut(&mut TxContext) -> Result<u64>,
    {
        let count = match self.tree.lookup(ctx, &block_num)? {
            Some(c) => {
                if c == 0 {
                    return Err(Error::new(
                        ErrorKind::InvalidData,
                        "Cannot decrement refcount of 0",
                    ));
                }
                c - 1
            }
            None => 0, // 1 -> 0
        };

        if count <= 1 {
            // Remove from tree if refcount drops to 1 or 0
            // Note: BTree remove is not fully implemented for nodes, but we'll try or just update it to 1/0
            // For now, we just insert the updated count. If we have a remove function, we'd use it.
            // Let's just store the value. 1 means unique, 0 means free.
            // When free space manager runs, it can check if refcount == 0.
            self.tree.insert(ctx, block_num, count, allocate_block)?;
        } else {
            self.tree.insert(ctx, block_num, count, allocate_block)?;
        }

        Ok(count)
    }
}
