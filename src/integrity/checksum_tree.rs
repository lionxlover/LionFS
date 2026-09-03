use crate::btree::tree::BTree;
use crate::transaction::transaction::TxContext;
use bytemuck::{Pod, Zeroable};
use std::io::{Error, Result};

pub const CHECKSUM_TREE_NODE_TYPE: u32 = 5;

#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Pod, Zeroable)]
pub struct ChecksumTreeKey {
    pub object_id: u64,
    pub logical_block: u64,
}

#[repr(C)]
#[derive(Debug, Clone, Copy, Pod, Zeroable)]
pub struct ChecksumTreeValue {
    pub physical_block: u64,
    pub checksum_bytes: [u8; 32],
    pub generation: u64,
    pub algorithm_id: u8,
    pub verification_status: u8, // 0 = Unverified, 1 = Verified, 2 = Corrupt
    pub padding: [u8; 6],
}

impl PartialEq for ChecksumTreeValue {
    fn eq(&self, other: &Self) -> bool {
        self.physical_block == other.physical_block
            && self.checksum_bytes == other.checksum_bytes
            && self.algorithm_id == other.algorithm_id
            && self.generation == other.generation
            && self.verification_status == other.verification_status
    }
}
impl Eq for ChecksumTreeValue {}

pub struct ChecksumTree {
    pub btree: BTree<ChecksumTreeKey, ChecksumTreeValue>,
}

impl ChecksumTree {
    pub fn new(root_block: u64) -> Self {
        Self {
            btree: BTree::new(root_block, CHECKSUM_TREE_NODE_TYPE),
        }
    }

    pub fn init_empty(ctx: &mut TxContext, root_block: u64) -> Result<()> {
        BTree::<ChecksumTreeKey, ChecksumTreeValue>::init_empty(
            ctx,
            root_block,
            CHECKSUM_TREE_NODE_TYPE,
        )
    }

    pub fn insert_checksum<F>(
        &mut self,
        ctx: &mut TxContext,
        key: ChecksumTreeKey,
        value: ChecksumTreeValue,
        allocate_block: &mut F,
    ) -> Result<()>
    where
        F: FnMut(&mut TxContext) -> Result<u64>,
    {
        self.btree.insert(ctx, key, value, allocate_block)
    }

    pub fn lookup_checksum(
        &self,
        ctx: &mut TxContext,
        key: &ChecksumTreeKey,
    ) -> Result<Option<ChecksumTreeValue>> {
        self.btree.lookup(ctx, key)
    }

    pub fn mark_corrupt(&mut self, ctx: &mut TxContext, key: &ChecksumTreeKey) -> Result<()> {
        if let Some(mut val) = self.lookup_checksum(ctx, key)? {
            val.verification_status = 2; // Corrupt
                                         // In a real implementation we would update the value in-place,
                                         // but we can just use insert to overwrite.
                                         // Wait, our btree.insert currently does overwrite for existing keys!
                                         // Actually, we need allocate_block just in case, but overwrite doesn't allocate.
            let mut dummy_allocator = |_ctx: &mut TxContext| -> Result<u64> {
                Err(Error::other("Should not allocate on overwrite"))
            };
            self.btree.insert(ctx, *key, val, &mut dummy_allocator)?;
        }
        Ok(())
    }
}
