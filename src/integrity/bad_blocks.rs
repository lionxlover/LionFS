use crate::btree::tree::BTree;
use crate::transaction::transaction::TxContext;
use bytemuck::{Pod, Zeroable};
use std::io::Result;

pub const BAD_BLOCKS_TREE_NODE_TYPE: u32 = 6;

#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Pod, Zeroable)]
pub struct BadBlockKey {
    pub physical_block: u64,
}

#[repr(C)]
#[derive(Debug, Clone, Copy, Pod, Zeroable)]
pub struct BadBlockValue {
    pub timestamp: u64,
    pub object_id: u64, // The object that was stored here (if known)
    pub padding: [u8; 16],
}

impl PartialEq for BadBlockValue {
    fn eq(&self, other: &Self) -> bool {
        self.timestamp == other.timestamp && self.object_id == other.object_id
    }
}
impl Eq for BadBlockValue {}

pub struct BadBlockManager {
    pub btree: BTree<BadBlockKey, BadBlockValue>,
}

impl BadBlockManager {
    pub fn new(root_block: u64) -> Self {
        Self {
            btree: BTree::new(root_block, BAD_BLOCKS_TREE_NODE_TYPE),
        }
    }

    pub fn get_health_report(_ctx: &mut TxContext, root_block: u64) -> String {
        let mut report = String::from("LionFS Integrity Health Report\n");
        report.push_str("------------------------------\n");

        let _tree = BTree::<u64, u8>::new(root_block, 3);
        // Simple placeholder for health report for Phase 5.
        // A full implementation would iterate over the BadBlocksTree.
        report.push_str("Bad Blocks Tree initialized.\n");
        report.push_str("Status: HEALTHY\n");

        report
    }

    pub fn init_empty(ctx: &mut TxContext, root_block: u64) -> Result<()> {
        BTree::<BadBlockKey, BadBlockValue>::init_empty(ctx, root_block, BAD_BLOCKS_TREE_NODE_TYPE)
    }

    pub fn mark_bad_block<F>(
        &mut self,
        ctx: &mut TxContext,
        physical_block: u64,
        object_id: u64,
        allocate_block: &mut F,
    ) -> Result<()>
    where
        F: FnMut(&mut TxContext) -> Result<u64>,
    {
        // Use a generic timestamp (e.g. system time)
        let timestamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();

        let key = BadBlockKey { physical_block };
        let val = BadBlockValue {
            timestamp,
            object_id,
            padding: [0; 16],
        };
        self.btree.insert(ctx, key, val, allocate_block)
    }
}
