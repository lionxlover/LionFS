use crate::btree::tree::{node_gen_current, BTree};
use crate::inode::tree::INODE_TREE_NODE_TYPE;
use crate::integrity::checksum_tree::{ChecksumTree, ChecksumTreeKey, ChecksumTreeValue, CHECKSUM_TREE_NODE_TYPE};
use crate::integrity::refcount::RefCountManager;
use crate::directory::tree::DIR_TREE_NODE_TYPE;
use crate::ondisk::serialization::{Inode, SnapshotRecord, Superblock};
use crate::transaction::transaction::TxContext;
use std::io::{Error, ErrorKind, Result};
use std::time::{SystemTime, UNIX_EPOCH};

pub const SNAPSHOT_TREE_NODE_TYPE: u32 = 8;

/// Phase 11: this snapshot was created in BIRTH mode -- no pin walk
/// ran, no pins exist for it, and its data protection comes from the
/// per-block birth generations in the (frozen) checksum tree. Delete
/// dispatches on this flag: birth mode reclaims by barrier
/// comparison, pin mode unpins what it pinned.
pub const SNAPSHOT_FLAG_BIRTH: u32 = 1;

/// Snapshot creation and lifecycle.
///
/// 3.3 honesty note -- what a snapshot freezes, and what it does not:
///
/// FROZEN (correct since 3.2, CHEAP since 3.3):
/// * DATA blocks: every physical extent run of every inode is pinned
///   in the refcount coverage tree, and the write path redirects
///   (copy-on-write) instead of modifying pinned blocks.
/// * METADATA (3.3, path-copy CoW): the snapshot records the CURRENT
///   inode / dir-name / checksum / spill-extent tree roots, and the
///   B-tree mutation paths path-copy any node whose stamp is <= the
///   snapshot barrier before mutating it. Snapshot creation is O(1)
///   in metadata -- no inode deep-copy (3.2's second full walk), no
///   tree re-materialization. Post-snapshot mutations diverge onto
///   private copies; the recorded roots keep pointing at the frozen
///   originals. This also freezes the directory-name index and the
///   checksum tree, both of which were shared-and-mutable in 3.2 --
///   snapshot reads can now VERIFY against the frozen checksum view.
///
/// NOT FROZEN (known limitation):
/// * Compressed inodes are skipped by the pin walk (cluster
///   bookkeeping is not pin-aware yet); their data can change under a
///   snapshot.
///
/// Cost model (3.3): creation is O(extent runs) for data pinning plus
/// O(1) for metadata -- down from O(2 x inodes + tree nodes) in 3.2.
/// Still not Btrfs/ZFS O(1)-total: those carry per-extent birth
/// stamps; LionFS's inline Extent format has no spare field for one,
/// which is a format-v3 question, not a code question. Deletion is
/// O(extent runs) to release pins, plus O(live snapshots) to recompute
/// the CoW barrier.
pub struct SnapshotManager {
    tree: BTree<u64, SnapshotRecord>,
}

impl SnapshotManager {
    pub fn new(root_block: u64) -> Self {
        Self {
            tree: BTree::new(root_block, SNAPSHOT_TREE_NODE_TYPE),
        }
    }

    /// Initialize an empty snapshot tree at `root_block`.
    pub fn init_empty(ctx: &mut TxContext, root_block: u64) -> Result<()> {
        BTree::<u64, SnapshotRecord>::init_empty(ctx, root_block, SNAPSHOT_TREE_NODE_TYPE)
    }

    /// Maximum `generation` (node-stamp barrier) over all live
    /// snapshot records. 0 when no snapshots exist. Called after any
    /// create/delete so `sb.last_snapshot_generation` is exactly the
    /// barrier the B-tree CoW pass tests against -- keeping it at the
    /// MAX (not the newest) is what keeps multi-snapshot deletes sound:
    /// deleting the newest snapshot must not un-freeze nodes that an
    /// OLDER snapshot can still reach, and lowering the barrier only
    /// ever un-freezes nodes stamped after every remaining snapshot.
    pub fn max_live_barrier(&self, ctx: &mut TxContext) -> Result<u64> {
        let mut max = 0u64;
        for (_, rec) in self.tree.iter_all(ctx)? {
            if rec.generation > max {
                max = rec.generation;
            }
        }
        Ok(max)
    }

    /// Phase 11 birth-mode delete: reclaim the deleted snapshot's
    /// uniquely-referenced data blocks. Walks the snapshot's FROZEN
    /// checksum view: for every record whose LIVE EXTENT MAPPING no
    /// longer resolves to that physical block (the block was
    /// redirected, truncated, or unmapped since the freeze -- the
    /// live csum records are NOT the oracle, they can go stale across
    /// truncates), the old phys is reclaimable iff its birth is > the
    /// remaining snapshots' max barrier (born after every remaining
    /// freeze -- provably unreachable from them) AND not pinned (a
    /// dedup share). Conservative by design: a block with birth <=
    /// the remaining barrier is left for a later delete/GC even if no
    /// remaining snapshot actually references it. Compressed inodes
    /// have no csum records, so they contribute nothing here (the
    /// documented hole, same class as the pin walk's skip).
    fn reclaim_birth_snapshot<F>(
        &self,
        ctx: &mut TxContext,
        sb: &Superblock,
        record: &SnapshotRecord,
        remaining_barrier: u64,
        allocate_block: &mut F,
    ) -> Result<()>
    where
        F: FnMut(&mut TxContext) -> Result<u64>,
    {
        if sb.checksum_tree_root == 0 {
            return Ok(()); // pin-mode record: nothing to do here
        }
        let frozen_csum = ChecksumTree::new_frozen(record.checksum_tree_root);
        let live_inode_tree = BTree::<u64, Inode>::new(sb.inode_tree_root, INODE_TREE_NODE_TYPE);
        let rc = if sb.refcount_tree_root != 0 {
            Some(RefCountManager::new(sb.refcount_tree_root))
        } else {
            None
        };
        let bg_desc = crate::ondisk::serialization::BlockGroupDescriptor {
            bg_block_bitmap: sb.bitmap_start,
            bg_inode_bitmap: 0,
            bg_inode_table: sb.inode_table_start,
            bg_free_blocks_count: 0,
            bg_free_inodes_count: 0,
            bg_used_dirs_count: 0,
            bg_padding: 0,
            bg_reserved: [0; 32],
        };
        // The records come sorted by (ino, logical); maintain a
        // per-inode live extent list and a merge cursor over it.
        let mut cur_ino = u64::MAX;
        let mut live_map: Vec<(u64, u64, u64)> = Vec::new(); // (logical, phys, len)
        let mut cursor = 0usize;
        for (key, val) in frozen_csum.btree.iter_all(ctx)? {
            if key.object_id != cur_ino {
                cur_ino = key.object_id;
                live_map = Self::live_extent_list(ctx, &live_inode_tree, cur_ino)?;
                cursor = 0;
            }
            while cursor < live_map.len()
                && live_map[cursor].0 + live_map[cursor].2 <= key.logical_block
            {
                cursor += 1;
            }
            let live_phys = if cursor < live_map.len() && live_map[cursor].0 <= key.logical_block {
                Some(live_map[cursor].1)
            } else {
                None
            };
            if live_phys == Some(val.physical_block) {
                continue; // the live file still maps this block
            }
            if val.generation <= remaining_barrier {
                continue; // possibly reachable from a remaining snapshot
            }
            if let Some(rc) = rc.as_ref() {
                if rc.is_pinned(ctx, val.physical_block)? {
                    continue; // a dedup share holds it
                }
            }
            let _ = allocate_block; // reserved for extent-run coalescing
            crate::allocator::bitmap::Allocator::free_extents(
                ctx, &bg_desc, val.physical_block, 1,
            )?;
        }
        Ok(())
    }

    /// The live inode's (logical, phys, len) list, inline + spilled,
    /// sorted by logical start. `Err` (empty) for unknown inodes --
    /// nothing live to protect.
    fn live_extent_list(
        ctx: &mut TxContext,
        live_inode_tree: &BTree<u64, Inode>,
        ino: u64,
    ) -> Result<Vec<(u64, u64, u64)>> {
        let mut out: Vec<(u64, u64, u64)> = Vec::new();
        if let Some(inode) = live_inode_tree.lookup(ctx, &ino)? {
            for i in 0..inode.extent_count as usize {
                let e = inode.extents[i];
                if e.length > 0 {
                    out.push((e.logical_start, e.physical_start, e.length));
                }
            }
            if inode.spill_extent_root != 0 {
                let spill = crate::extents::tree::ExtentTree::new(inode.spill_extent_root);
                for (log_start, val) in spill.iter_extents(ctx)? {
                    if val.length > 0 {
                        out.push((log_start, val.physical_start, val.length));
                    }
                }
            }
        }
        out.sort_by_key(|&(logical, _, _)| logical);
        Ok(out)
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

        // ------------------------------------------------------------------
        // 3.3: the barrier is the node-write stamp high-water mark AT
        // THIS MOMENT. Every node on disk was stamped <= this value,
        // so the roots we record below are, by construction, entirely
        // frozen (any later mutation path-copies before touching them).
        // Stamps only grow, and mount re-initializes the counter above
        // every persisted value, so this is monotone across remounts.
        // ------------------------------------------------------------------
        let barrier = node_gen_current().max(1);
        sb.last_snapshot_generation = sb.last_snapshot_generation.max(barrier);
        // Mirror onto the Disk: bare-context writers (tools, library
        // callers) must also see the barrier, not just vfs contexts.
        ctx.disk
            .live_barrier
            .fetch_max(barrier, std::sync::atomic::Ordering::AcqRel);

        // Current time
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs();

        // ------------------------------------------------------------------
        // 3.3: sync the CoW-moved roots out of the transaction's root
        // cells into the superblock FIRST, so the recorded roots, the
        // in-memory superblock, and (at commit) the persisted
        // superblock all agree on what "the current tree" is.
        // ------------------------------------------------------------------
        sb.inode_tree_root = ctx.effective_root(INODE_TREE_NODE_TYPE, sb.inode_tree_root);
        sb.dir_tree_root = ctx.effective_root(DIR_TREE_NODE_TYPE, sb.dir_tree_root);
        sb.checksum_tree_root =
            ctx.effective_root(CHECKSUM_TREE_NODE_TYPE, sb.checksum_tree_root);

        // ------------------------------------------------------------------
        // PHASE 11 (birth generations): on a CHECKSUMMED image (the
        // mkfs default) creation is O(1) TOTAL -- no pin walk at all.
        // The data protection the walk used to establish eagerly is
        // derived lazily from the per-block birth generations the
        // write path records in the checksum tree: a block is
        // protected iff its birth <= this barrier (see the
        // write-redirect / truncate-retain / delete-reclaim paths).
        // The pin walk still runs on images with the checksum tree
        // OFF (pin mode, recorded in the record's flags), so both
        // modes coexist per image and delete dispatches on the flag.
        // ------------------------------------------------------------------
        let birth_mode = sb.checksum_tree_root != 0;
        if !birth_mode {
            // PIN MODE (checksums off): walk every inode's extents
            // (inline and spilled) and pin each physical run in the
            // refcount coverage tree. Pinned blocks are redirected
            // (never modified) by the write path from now on. This
            // walk is the O(extent runs) cost pin mode still pays.
            if sb.refcount_tree_root == 0 {
                let root = allocate_block(ctx)?;
                RefCountManager::init_empty(ctx, root)?;
                sb.refcount_tree_root = root;
            }
            let mut rc = RefCountManager::new(sb.refcount_tree_root);
            let inode_tree = BTree::<u64, Inode>::new(sb.inode_tree_root, INODE_TREE_NODE_TYPE);
            for ino in 1..sb.next_ino {
                let inode = match inode_tree.lookup(ctx, &ino)? {
                    Some(i) => i,
                    None => continue,
                };
                if inode.compression_algo != 0 {
                    // Cluster bookkeeping is not pin-aware (documented above).
                    continue;
                }
                for i in 0..inode.extent_count as usize {
                    let e = inode.extents[i];
                    if e.length > 0 {
                        rc.pin_range(ctx, e.physical_start, e.length, &mut *allocate_block)?;
                    }
                }
                if inode.spill_extent_root != 0 {
                    let spill = crate::extents::tree::ExtentTree::new(inode.spill_extent_root);
                    for (log_start, val) in spill.iter_extents(ctx)? {
                        if val.length > 0 {
                            rc.pin_range(
                                ctx,
                                val.physical_start,
                                val.length,
                                &mut *allocate_block,
                            )?;
                        }
                        let _ = log_start;
                    }
                }
            }
        }
        // else: birth mode -- record the roots below and done. O(1).

        // ------------------------------------------------------------------
        // 3.3: RECORD THE CURRENT ROOTS -- that IS the freeze. The
        // path-copy CoW in the B-tree mutation paths guarantees these
        // roots keep their exact contents: any node they can reach
        // with stamp <= barrier is copied (not mutated) on write. No
        // deep copy, no re-insertion walk, O(1).
        // ------------------------------------------------------------------
        let record = SnapshotRecord {
            id: snapshot_id,
            parent_id,
            creation_time: now,
            generation: barrier,
            inode_tree_root: sb.inode_tree_root,
            dir_tree_root: sb.dir_tree_root,
            extent_tree_root: sb.extent_tree_root,
            checksum_tree_root: sb.checksum_tree_root,
            bad_blocks_root: sb.bad_blocks_root,
            flags: if birth_mode { SNAPSHOT_FLAG_BIRTH } else { 0 },
            padding: 0,
            reserved: [0; 4],
        };

        // Insert into Snapshot Tree (type 8: NOT a frozen tree -- the
        // registry itself must stay mutable while snapshots exist).
        self.tree.insert(ctx, snapshot_id, record, &mut *allocate_block)?;

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

    /// List every live snapshot record.
    pub fn list_snapshots(&self, ctx: &mut TxContext) -> Result<Vec<SnapshotRecord>> {
        Ok(self.tree.iter_all(ctx)?.into_iter().map(|(_, r)| r).collect())
    }

    /// Read one inode AS THIS SNAPSHOT SEES IT. The recorded root is
    /// frozen by metadata CoW, and the data blocks it points at are
    /// pinned, so both the extent list and the bytes are stable.
    pub fn read_snapshot_inode(
        &self,
        ctx: &mut TxContext,
        snapshot_id: u64,
        ino: u64,
    ) -> Result<Option<Inode>> {
        match self.get_snapshot(ctx, snapshot_id)? {
            Some(record) => {
                let frozen =
                    BTree::<u64, Inode>::new_frozen(record.inode_tree_root, INODE_TREE_NODE_TYPE);
                frozen.lookup(ctx, &ino)
            }
            None => Ok(None),
        }
    }

    /// Look up a directory NAME as this snapshot sees it, through the
    /// frozen dir-name tree (new in 3.3: the dir tree used to be
    /// shared and mutable, so name resolution against a snapshot could
    /// return post-snapshot state).
    pub fn read_snapshot_dir_entry(
        &self,
        ctx: &mut TxContext,
        snapshot_id: u64,
        name: &str,
    ) -> Result<Option<crate::directory::tree::DirTreeValue>> {
        match self.get_snapshot(ctx, snapshot_id)? {
            Some(record) => {
                let dir = crate::directory::tree::DirectoryTree::new_frozen(record.dir_tree_root);
                dir.lookup(ctx, name)
            }
            None => Ok(None),
        }
    }

    /// Read one checksum entry AS THIS SNAPSHOT SEES IT, through the
    /// frozen checksum tree (new in 3.3: the checksum tree used to be
    /// updated in place by key, forcing snapshot reads to disable
    /// verification). Snapshot reads can now verify their data.
    pub fn read_snapshot_csum(
        &self,
        ctx: &mut TxContext,
        snapshot_id: u64,
        ino: u64,
        logical_block: u64,
    ) -> Result<Option<ChecksumTreeValue>> {
        match self.get_snapshot(ctx, snapshot_id)? {
            Some(record) => {
                let csum = crate::integrity::checksum_tree::ChecksumTree::new_frozen(
                    record.checksum_tree_root,
                );
                let key = ChecksumTreeKey {
                    object_id: ino,
                    logical_block,
                };
                csum.lookup_checksum(ctx, &key)
            }
            None => Ok(None),
        }
    }

    /// Delete a snapshot: recompute the CoW barrier over the remaining
    /// live snapshots, then release this snapshot's claim on data:
    ///
    /// * PIN MODE (record created with checksums off): unpin exactly
    ///   what the creation walk pinned, by walking the snapshot's OWN
    ///   recorded inode tree (which metadata CoW kept exactly as it
    ///   was at creation -- whatever it pinned, it unpins, regardless
    ///   of what the live tree did since).
    /// * BIRTH MODE (Phase 11, checksums on): no pins exist. Reclaim
    ///   the deleted snapshot's uniquely-referenced blocks by walking
    ///   its frozen checksum view and freeing old-phys blocks whose
    ///   birth postdates every remaining snapshot's barrier (provably
    ///   unreachable from them) and which the live tree no longer
    ///   maps. Conservative: blocks born at-or-below the remaining
    ///   barrier are left for a later delete or GC.
    ///
    /// Space note: the path-copied tree nodes created while this
    /// snapshot was live are not freed here. They are unreferenced by
    /// both the live tree and the remaining snapshots, but block
    /// reclamation of metadata nodes is the garbage collector's job
    /// (see specifications/gc.md); this function only balances pins /
    /// reclaims data blocks.
    pub fn delete_snapshot<F>(
        &mut self,
        ctx: &mut TxContext,
        sb: &mut Superblock,
        snapshot_id: u64,
        allocate_block: &mut F,
    ) -> Result<()>
    where
        F: FnMut(&mut TxContext) -> Result<u64>,
    {
        let record = self
            .tree
            .lookup(ctx, &snapshot_id)?
            .ok_or_else(|| Error::new(ErrorKind::NotFound, "Snapshot not found"))?;

        let is_birth = record.flags & SNAPSHOT_FLAG_BIRTH != 0;
        if is_birth {
            // Compute the remaining barrier BEFORE removing the record
            // (the max over the OTHER live snapshots) so the reclaim
            // rule uses exactly the snapshots that survive this delete.
            let remaining_barrier = self
                .tree
                .iter_all(ctx)?
                .into_iter()
                .filter(|(id, _)| *id != snapshot_id)
                .map(|(_, rec)| rec.generation)
                .max()
                .unwrap_or(0);
            self.reclaim_birth_snapshot(ctx, sb, &record, remaining_barrier, allocate_block)?;
        } else if sb.refcount_tree_root != 0 {
            let mut rc = RefCountManager::new(sb.refcount_tree_root);
            let frozen =
                BTree::<u64, Inode>::new_frozen(record.inode_tree_root, INODE_TREE_NODE_TYPE);
            for ino in 1..sb.next_ino {
                if let Some(inode) = frozen.lookup(ctx, &ino)? {
                    if inode.compression_algo != 0 {
                        continue;
                    }
                    for i in 0..inode.extent_count as usize {
                        let e = inode.extents[i];
                        if e.length > 0 {
                            // The snapshot pinned this run; the unpin
                            // must balance. An unbalanced unpin is an
                            // error we surface, not paper over.
                            rc.unpin_range(ctx, e.physical_start, e.length, &mut *allocate_block)?;
                        }
                    }
                    if inode.spill_extent_root != 0 {
                        let spill =
                            crate::extents::tree::ExtentTree::new(inode.spill_extent_root);
                        for (_log_start, val) in spill.iter_extents(ctx)? {
                            if val.length > 0 {
                                rc.unpin_range(
                                    ctx,
                                    val.physical_start,
                                    val.length,
                                    &mut *allocate_block,
                                )?;
                            }
                        }
                    }
                }
            }
        }

        let removed = self.tree.remove(ctx, &snapshot_id)?;
        if !removed {
            return Err(Error::new(ErrorKind::NotFound, "Snapshot not found"));
        }

        // 3.3: barrier = max stamp over the REMAINING live snapshots.
        // Lowering it is what "unfreezes" nodes that only the deleted
        // (newest) snapshot could reach; keeping it at the max of the
        // rest is what keeps their views safe.
        sb.last_snapshot_generation = self.max_live_barrier(ctx)?;
        ctx.disk
            .live_barrier
            .store(sb.last_snapshot_generation, std::sync::atomic::Ordering::Release);

        Ok(())
    }
}
