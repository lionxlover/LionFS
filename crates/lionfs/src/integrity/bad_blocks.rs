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

    pub fn get_health_report(ctx: &mut TxContext, root_block: u64) -> String {
        let mut report = String::from("LionFS Integrity Health Report\n");
        report.push_str("------------------------------\n");

        if root_block == 0 {
            report.push_str("Bad Blocks Tree: not initialized.\n");
            report.push_str("Status: HEALTHY\n");
            return report;
        }
        // 3.6: a REAL report -- iterate the ledger and classify.
        let tree = Self::new(root_block);
        match tree.list(ctx) {
            Ok(entries) => {
                if entries.is_empty() {
                    report.push_str("Bad Blocks Tree initialized.\n");
                    report.push_str("Status: HEALTHY (no bad blocks recorded)\n");
                } else {
                    report.push_str(&format!("Bad blocks recorded: {}\n", entries.len()));
                    for (key, val) in entries.iter().take(16) {
                        report.push_str(&format!(
                            "  block {} (first seen by object {}, at epoch {})\n",
                            key.physical_block, val.object_id, val.timestamp
                        ));
                    }
                    if entries.len() > 16 {
                        report.push_str(&format!("  ... and {} more\n", entries.len() - 16));
                    }
                    report.push_str("Status: DEGRADED\n");
                }
            }
            Err(e) => {
                report.push_str(&format!("Bad Blocks Tree READ FAILED: {e}\n"));
                report.push_str("Status: CHECK FAILED\n");
            }
        }

        report
    }

    pub fn init_empty(ctx: &mut TxContext, root_block: u64) -> Result<()> {
        BTree::<BadBlockKey, BadBlockValue>::init_empty(ctx, root_block, BAD_BLOCKS_TREE_NODE_TYPE)
    }

    /// 3.6: every ledger entry, in key order.
    pub fn list(&self, ctx: &mut TxContext) -> Result<Vec<(BadBlockKey, BadBlockValue)>> {
        self.btree.iter_all(ctx)
    }

    /// 3.6: how many blocks are quarantined? (The conformance suite
    /// and the health report's one-line summary.)
    pub fn count(&self, ctx: &mut TxContext) -> Result<u64> {
        if self.btree.root_block == 0 {
            return Ok(0);
        }
        Ok(self.btree.iter_all(ctx)?.len() as u64)
    }

    /// 3.6: remove a block from the ledger (the scrubber calls this
    /// after a successful repair).
    pub fn clear_bad_block<F>(
        &mut self,
        ctx: &mut TxContext,
        physical_block: u64,
        allocate_block: &mut F,
    ) -> Result<bool>
    where
        F: FnMut(&mut TxContext) -> Result<u64>,
    {
        if self.btree.root_block == 0 {
            return Ok(false);
        }
        self.btree
            .remove_with_alloc(ctx, &BadBlockKey { physical_block }, allocate_block)
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
