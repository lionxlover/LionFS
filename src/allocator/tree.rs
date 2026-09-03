use crate::btree::tree::BTree;
use crate::transaction::transaction::TxContext;
use std::io::Result;

pub const FREESPACE_TREE_NODE_TYPE: u32 = 4;

pub struct FreeSpaceTree {
    pub btree: BTree<u64, u64>, // Key: Physical Start, Value: Length
}

impl FreeSpaceTree {
    pub fn new(root_block: u64) -> Self {
        Self {
            btree: BTree::new(root_block, FREESPACE_TREE_NODE_TYPE),
        }
    }

    pub fn init_empty(ctx: &mut TxContext, root_block: u64) -> Result<()> {
        BTree::<u64, u64>::init_empty(ctx, root_block, FREESPACE_TREE_NODE_TYPE)
    }

    pub fn add_free_space<F>(
        &mut self,
        ctx: &mut TxContext,
        physical_start: u64,
        length: u64,
        allocate_block: F,
    ) -> Result<()>
    where
        F: FnMut(&mut TxContext) -> Result<u64>,
    {
        // A complete implementation would merge adjacent free space extents.
        // For now, we simply insert the extent into the tree.
        self.btree
            .insert(ctx, physical_start, length, allocate_block)
    }

    pub fn remove_free_space(&mut self, ctx: &mut TxContext, physical_start: u64) -> Result<bool> {
        self.btree.remove(ctx, &physical_start)
    }
}
