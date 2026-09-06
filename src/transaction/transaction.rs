use crate::disk::block_io::Disk;
use std::collections::HashMap;
use std::io::Result;
use std::sync::Arc;

pub struct Transaction {
    pub id: u64,
    pub dirty_blocks: HashMap<u64, Vec<u8>>,
    pub timestamp: u64,
    /// Phase 9 (metadata path-copy CoW): current root block of every
    /// FROZEN tree (inode / dir / spill-extent / checksum -- see
    /// `btree::tree::is_frozen_tree`) whose root this transaction has
    /// moved. B-tree mutations consult this BEFORE descending, so a
    /// handle built from a stale in-memory superblock still finds the
    /// live tree; the fs layer syncs the cells back into the superblock
    /// at commit. Keyed by B-tree `node_type`. Empty = no root moved
    /// (the common, snapshot-free case).
    pub root_cells: HashMap<u32, u64>,
    /// 3.6 (Format Vault accounting): net blocks allocated minus freed
    /// through this transaction, applied to `Superblock::free_blocks`
    /// at commit so statfs and the conformance battery see LIVE space
    /// accounting instead of a mkfs-time static. (3.5 left the field
    /// static -- the drift the conformance battery now catches.)
    pub alloc_delta: i64,
}

impl Transaction {
    pub fn new(id: u64, timestamp: u64) -> Self {
        Self {
            id,
            dirty_blocks: HashMap::new(),
            timestamp,
            root_cells: HashMap::new(),
            alloc_delta: 0,
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
    /// Phase 11 (pipelined txg): transactions that have been QUIESCED
    /// (frozen out of `active_tx`) but not yet retired (their apply +
    /// sync + root-cell switch have not all landed). Ordered OLDEST
    /// first; `read_block` searches them NEWEST first. Every block the
    /// commit's apply loop mutates on disk is in exactly one of these
    /// overlays, so a reader (or a later stager building on top of a
    /// frozen group) that consults them can never fetch a torn block:
    /// it sees the post-quiesce content instead, which is a legal
    /// linearization point. Without this, a transaction in flight to
    /// disk is visible NOWHERE -- not in `active_tx` (taken), not on
    /// disk (not yet applied) -- and a concurrent stager would build
    /// the next group on pre-quiesce state: a lost update.
    pub pending: &'a [Arc<Transaction>],
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
    /// Phase 9 metadata CoW barrier: the highest node-write stamp of
    /// any LIVE snapshot (0 = no snapshots = CoW off). Frozen-tree
    /// (inode/dir/spill/csum) B-tree mutations path-copy any node
    /// whose stored generation is <= this barrier before mutating it,
    /// so snapshot-recorded roots keep pointing at immutable views.
    /// Set by the vfs layer from `sb.last_snapshot_generation`; 0 in
    /// every non-vfs caller (tests, tools) keeps CoW off and behavior
    /// byte-for-byte identical to 3.2.
    pub cow_barrier: u64,
}

impl<'a> TxContext<'a> {
    pub fn new(disk: &'a Disk, tx: &'a mut Transaction) -> Self {
        Self {
            disk,
            tx,
            pending: &[],
            node_cache: None,
            alloc_cursor: None,
            meta_high_water: 0,
            cow_barrier: 0,
        }
    }

    pub fn with_cache(disk: &'a Disk, tx: &'a mut Transaction, node_cache: &'a NodeCache) -> Self {
        Self {
            disk,
            tx,
            pending: &[],
            node_cache: Some(node_cache),
            alloc_cursor: None,
            meta_high_water: 0,
            cow_barrier: 0,
        }
    }

    /// Phase 11: attach the pending frozen-group overlays (oldest
    /// first). See the field docs. Builder form so the existing
    /// `.with_cow_barrier(...)` chains read naturally.
    pub fn with_pending(mut self, pending: &'a [Arc<Transaction>]) -> Self {
        self.pending = pending;
        self
    }

    /// Phase 11: the effective metadata-CoW/data-birth barrier for this
    /// context: the explicit context barrier (vfs layer sets it from
    /// the superblock) fused with the image-wide live-barrier mirror on
    /// the Disk (so bare-context writers -- tools, library callers,
    /// tests -- still honor snapshots). Max of the two: over-copying is
    /// safe, under-copying is corruption. Mirrors
    /// `btree::tree::cow_barrier_of` for consumers outside the B-tree.
    pub fn effective_cow_barrier(&self) -> u64 {
        self.cow_barrier
            .max(self.disk.live_barrier.load(std::sync::atomic::Ordering::Acquire))
    }

    /// Phase 9: enable metadata path-copy CoW for this operation.
    /// `barrier` must be `sb.last_snapshot_generation` (the max stamp
    /// over live snapshots). 0 / leaving it unset = CoW off.
    pub fn with_cow_barrier(mut self, barrier: u64) -> Self {
        self.cow_barrier = barrier;
        self
    }

    /// Phase 9: the effective root of tree `node_type` -- the root
    /// cell this transaction moved it to, else the image-wide mirror
    /// on the Disk (set by ANY earlier transaction since mount), else
    /// `default_root`. Read paths consult this so an in-flight root
    /// move is always visible -- even to a caller whose superblock
    /// value predates the move and whose transaction is brand new.
    pub fn effective_root(&self, node_type: u32, default_root: u64) -> u64 {
        if let Some(&r) = self.tx.root_cells.get(&node_type) {
            return r;
        }
        if let Some(r) = self.disk.frozen_root(node_type) {
            return r;
        }
        default_root
    }

    /// Phase 9: record that tree `node_type`'s live root is now `root`
    /// (it was path-copied out of a frozen view). Writes BOTH the
    /// per-transaction cell (commit bookkeeping) and the image-wide
    /// Disk mirror (reader truth). Called by the B-tree mutation
    /// paths only.
    pub fn set_root_cell(&mut self, node_type: u32, root: u64) {
        self.tx.root_cells.insert(node_type, root);
        self.disk.set_frozen_root(node_type, root);
    }

    pub fn read_block(&mut self, block: u64, buf: &mut [u8]) -> Result<()> {
        if let Some(data) = self.tx.dirty_blocks.get(&block) {
            buf.copy_from_slice(data);
            return Ok(());
        }
        // Phase 11: a quiesced group's blocks are still ours to see --
        // newest pending first so a block rewritten by a later group
        // reads back that group's version.
        for pending in self.pending.iter().rev() {
            if let Some(data) = pending.dirty_blocks.get(&block) {
                buf.copy_from_slice(data);
                return Ok(());
            }
        }
        self.disk.read_block(block, buf)
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
