use crate::btree::tree::BTree;
use crate::ondisk::serialization::{SnapshotRecord, Superblock};
use crate::transaction::transaction::TxContext;
use std::io::{Error, ErrorKind, Result};
use std::time::{SystemTime, UNIX_EPOCH};

pub const SNAPSHOT_TREE_NODE_TYPE: u32 = 8;

pub struct SnapshotManager {
    tree: BTree<u64, SnapshotRecord>,
}

impl SnapshotManager {
    pub fn new(root_block: u64) -> Self {
        Self {
            tree: BTree::new(root_block, SNAPSHOT_TREE_NODE_TYPE),
        }
    }

    pub fn create_snapshot<F>(
        &mut self,
        ctx: &mut TxContext,
        sb: &mut Superblock,
        snapshot_id: u64,
        parent_id: u64,
        allocate_block: &mut F,
    ) -> Result<()>
    where
        F: FnMut(&mut TxContext) -> Result<u64>,
    {
        // Check if snapshot ID already exists
        if self.tree.lookup(ctx, &snapshot_id)?.is_some() {
            return Err(Error::new(
                ErrorKind::AlreadyExists,
                "Snapshot ID already exists",
            ));
        }

        // Increment the last_snapshot_generation.
        // This acts as a barrier: any new writes to the active filesystem
        // will now have generation > last_snapshot_generation,
        // triggering a CoW for any block with generation <= last_snapshot_generation.
        sb.last_snapshot_generation = sb.generation;

        // Current time
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs();

        // Create the snapshot record by directly pointing to the CURRENT roots.
        // Since we bumped last_snapshot_generation, the active FS will CoW its modifications,
        // leaving these blocks intact for the snapshot!
        let record = SnapshotRecord {
            id: snapshot_id,
            parent_id,
            creation_time: now,
            generation: sb.generation,
            inode_tree_root: sb.inode_tree_root,
            dir_tree_root: sb.dir_tree_root,
            extent_tree_root: sb.extent_tree_root,
            checksum_tree_root: sb.checksum_tree_root,
            bad_blocks_root: sb.bad_blocks_root,
            flags: 0,
            padding: 0,
            reserved: [0; 4],
        };

        // Insert into Snapshot Tree
        self.tree.insert(ctx, snapshot_id, record, allocate_block)?;

        // Update superblock's snapshot tree root
        sb.snapshot_tree_root = self.tree.root_block;

        Ok(())
    }

    pub fn get_snapshot(
        &self,
        ctx: &mut TxContext,
        snapshot_id: u64,
    ) -> Result<Option<SnapshotRecord>> {
        self.tree.lookup(ctx, &snapshot_id)
    }

    pub fn delete_snapshot<F>(
        &mut self,
        ctx: &mut TxContext,
        snapshot_id: u64,
        _allocate_block: &mut F,
    ) -> Result<()>
    where
        F: FnMut(&mut TxContext) -> Result<u64>,
    {
        // Deleting a snapshot is tricky because we must decrement refcounts
        // for all blocks exclusively owned by it. For Phase 6, we just remove
        // the record and let the GC worker handle the asynchronous deletion.
        let removed = self.tree.remove(ctx, &snapshot_id)?;
        if !removed {
            return Err(Error::new(ErrorKind::NotFound, "Snapshot not found"));
        }
        Ok(())
    }
}
