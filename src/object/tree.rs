use crate::btree::tree::BTree;
use crate::transaction::transaction::TxContext;
use bytemuck::{Pod, Zeroable};
use std::io::Result;

pub const OBJECT_TREE_NODE_TYPE: u32 = 5;

#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct ObjectEntry {
    pub physical_block: u64,
    pub object_type: u8,
    pub ref_count: u64,
    pub padding: [u8; 7],
}
unsafe impl Zeroable for ObjectEntry {}
unsafe impl Pod for ObjectEntry {}

pub struct ObjectTree {
    pub btree: BTree<u64, ObjectEntry>,
}

impl ObjectTree {
    pub fn new(root_block: u64) -> Self {
        Self {
            btree: BTree::new(root_block, OBJECT_TREE_NODE_TYPE),
        }
    }

    pub fn init_empty(ctx: &mut TxContext, root_block: u64) -> Result<()> {
        BTree::<u64, ObjectEntry>::init_empty(ctx, root_block, OBJECT_TREE_NODE_TYPE)
    }

    pub fn lookup(&self, ctx: &mut TxContext, oid: u64) -> Result<Option<ObjectEntry>> {
        self.btree.lookup(ctx, &oid)
    }

    pub fn insert<F>(
        &mut self,
        ctx: &mut TxContext,
        oid: u64,
        entry: ObjectEntry,
        allocate_block: F,
    ) -> Result<()>
    where
        F: FnMut(&mut TxContext) -> Result<u64>,
    {
        self.btree.insert(ctx, oid, entry, allocate_block)
    }
}
