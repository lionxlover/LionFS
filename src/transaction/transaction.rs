use crate::disk::block_io::Disk;
use std::collections::HashMap;
use std::io::Result;

pub struct Transaction {
    pub id: u64,
    pub dirty_blocks: HashMap<u64, Vec<u8>>,
    pub timestamp: u64,
}

impl Transaction {
    pub fn new(id: u64, timestamp: u64) -> Self {
        Self {
            id,
            dirty_blocks: HashMap::new(),
            timestamp,
        }
    }

    pub fn add_block(&mut self, block_num: u64, data: Vec<u8>) {
        self.dirty_blocks.insert(block_num, data);
    }
}

use crate::cache::node_cache::NodeCache;

pub struct TxContext<'a> {
    pub disk: &'a Disk,
    pub tx: &'a mut Transaction,
    pub node_cache: Option<&'a NodeCache>,
    /// In-memory allocation frontier hint (Phase 1): the end of the
    /// most recent run this context allocated. Lets the bitmap scan
    /// start at the frontier instead of rescanning all used bits.
    /// Purely an optimization -- never serialized, always correct to
    /// ignore (the scan falls back to a full first-fit pass).
    pub alloc_cursor: Option<u64>,
    /// How many blocks this context has allocated from the END of the
    /// block group (metadata zone, Phase 1). Metadata allocations
    /// (tree node splits, spill-tree roots) grow downward from the end
    /// while file data grows upward from the frontier, so a
    /// sequentially-written file's speculative extent runs are not
    /// punctured by interleaved metadata allocations -- which was the
    /// dominant extent-fragmentation source. Heuristic, not a hard
    /// invariant: when the two zones meet, allocation falls back to
    /// first-fit anywhere (correct, just less tidy).
    pub meta_high_water: u64,
}

impl<'a> TxContext<'a> {
    pub fn new(disk: &'a Disk, tx: &'a mut Transaction) -> Self {
        Self {
            disk,
            tx,
            node_cache: None,
            alloc_cursor: None,
            meta_high_water: 0,
        }
    }

    pub fn with_cache(disk: &'a Disk, tx: &'a mut Transaction, node_cache: &'a NodeCache) -> Self {
        Self {
            disk,
            tx,
            node_cache: Some(node_cache),
            alloc_cursor: None,
            meta_high_water: 0,
        }
    }

    pub fn read_block(&mut self, block: u64, buf: &mut [u8]) -> Result<()> {
        if let Some(data) = self.tx.dirty_blocks.get(&block) {
            buf.copy_from_slice(data);
            Ok(())
        } else {
            self.disk.read_block(block, buf)
        }
    }

    pub fn write_block(&mut self, block: u64, buf: &[u8]) -> Result<()> {
        self.tx.add_block(block, buf.to_vec());
        Ok(())
    }

    /// Ownership-transfer variant of `write_block` for callers that
    /// already hold a heap buffer (e.g. the cipher-active path in
    /// `FileManager::write_file`, which produces a transformed `Vec`).
    /// Lets the transaction take the Vec as-is instead of copying it
    /// again. (Phase 1 buffer-allocation reduction.)
    pub fn write_block_owned(&mut self, block: u64, data: Vec<u8>) -> Result<()> {
        self.tx.add_block(block, data);
        Ok(())
    }
}
