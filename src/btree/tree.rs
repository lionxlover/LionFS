use crate::btree::node::{BTreeNodeData, BTREE_MAGIC, BTREE_PAYLOAD_SIZE};
use crate::integrity::algorithms::{calculate_checksum, ChecksumAlgorithm};
use crate::transaction::transaction::TxContext;
use bytemuck::{bytes_of, pod_read_unaligned, Pod, Zeroable};
use std::io::{Error, ErrorKind, Result};
use std::marker::PhantomData;
use std::sync::atomic::{AtomicU64, Ordering};

/// Global structural epoch, bumped by every node split, root growth,
/// removal, and (re)initialization in ANY B-tree of ANY key/value type.
/// The fast-append cache (below) records the epoch it was populated
/// under and refuses the fast path if the epoch has moved: a split in
/// some other tree over the same root (a second live handle) can
/// re-shape the rightmost leaf without changing the bytes of the
/// cached one, and the count/last-key revalidation alone cannot catch
/// that -- the epoch check makes staleness impossible rather than
/// merely unlikely.
static TREE_EPOCH: AtomicU64 = AtomicU64::new(0);

fn bump_epoch() {
    TREE_EPOCH.fetch_add(1, Ordering::Release);
}

fn current_epoch() -> u64 {
    TREE_EPOCH.load(Ordering::Acquire)
}

// ---------------------------------------------------------------------
// Phase 9: metadata path-copy CoW.
//
// Every node write is stamped with a global monotone counter
// (`NODE_GEN`); a snapshot records the counter value at its creation
// as its barrier. While snapshots are live, mutating a frozen-tree
// node whose stamp is <= the barrier would corrupt the snapshot's
// recorded view, so the mutation paths path-copy such nodes first.
// ---------------------------------------------------------------------

static NODE_GEN: AtomicU64 = AtomicU64::new(0);

/// The B-trees whose roots a snapshot RECORDS, and whose nodes must
/// therefore never be mutated in place while a snapshot is live:
/// inode (1), dir-name (2), per-inode spill-extent (3), and checksum
/// (5). Every other tree (freespace, refcount, snapshot, clone,
/// subvolume, dedup, cluster, ...) is NOT recorded by snapshots, so
/// CoW-ing it would be pure write amplification with no correctness
/// benefit.
pub fn is_frozen_tree(node_type: u32) -> bool {
    matches!(node_type, 1 | 2 | 3 | 5 | 13)
}

/// Initialize the global node-write stamp counter at mount so that
/// new stamps start ABOVE every on-disk stamp and every persisted
/// snapshot barrier. Idempotent; only ever moves the counter up.
pub fn node_gen_init(floor: u64) {
    NODE_GEN.fetch_max(floor, Ordering::AcqRel);
}

/// Current stamp high-water mark (for persisting into
/// `sb.node_generation`).
pub fn node_gen_current() -> u64 {
    NODE_GEN.load(Ordering::Acquire)
}

fn node_gen_next() -> u64 {
    NODE_GEN.fetch_add(1, Ordering::AcqRel) + 1
}

/// Phase 11 (birth generations): a FRESH stamp for a data-block birth
/// record. Data blocks are stamped when their content is written
/// (checksum-tree `generation`), NOT when they are allocated; the
/// advancing fetch_add is what makes "born after the snapshot's
/// barrier" decidable with one comparison in the write path. Thread-safe
/// (an atomic counter, same as node writes), so the pipelined
/// committer's concurrent groups stamp without serialization.
pub fn node_gen_stamp() -> u64 {
    node_gen_next()
}

/// The effective CoW barrier for an operation: the explicit context
/// barrier (the vfs layer sets it from the superblock) fused with the
/// image's live-barrier mirror on the Disk (so bare-context writers --
/// tools, library callers, tests -- still path-copy frozen nodes while
/// a snapshot is live). Max of the two: over-copying is safe,
/// under-copying is corruption.
fn cow_barrier_of(ctx: &TxContext) -> u64 {
    ctx.cow_barrier
        .max(ctx.disk.live_barrier.load(Ordering::Acquire))
}

/// Fast-append cache entry: the tree's rightmost leaf, its item count
/// and largest key, plus the structural epoch it was recorded under.
/// A monotone insert (key strictly greater than `last_key`) into a
/// not-full rightmost leaf needs no root descent at all -- one node
/// read and one node write. That is the checksum-tree pattern (fixed
/// ino, ascending logical block) that dominates sequential write
/// throughput; before this cache, every per-block checksum insert paid
/// a full root-to-leaf descent with a per-level CRC32C re-verification
/// of the whole 4 KiB node -- the measured ~45% share of write cost
/// (see docs/benchmarks.md).
#[derive(Clone, Copy, Debug)]
struct FastLeaf<K: BTreeKey> {
    leaf_block: u64,
    last_key: K,
    item_count: u16,
    epoch: u64,
}

#[derive(Clone, Copy, Debug)]
struct RangeLeaf<K: BTreeKey> {
    leaf_block: u64,
    min_key: K,
    max_key: K,
    item_count: u16,
    epoch: u64,
}

pub trait BTreeItem: Pod + Zeroable + Clone + Copy + std::fmt::Debug {}
impl<T: Pod + Zeroable + Clone + Copy + std::fmt::Debug> BTreeItem for T {}

pub trait BTreeKey: BTreeItem + Ord {}
impl<T: BTreeItem + Ord> BTreeKey for T {}

#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct KVPair<K: BTreeKey, V: BTreeItem> {
    pub key: K,
    pub value: V,
}
unsafe impl<K: BTreeKey, V: BTreeItem> Zeroable for KVPair<K, V> {}
unsafe impl<K: BTreeKey, V: BTreeItem> Pod for KVPair<K, V> {}

#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct KPtrPair<K: BTreeKey> {
    pub key: K,
    pub ptr: u64,
}
unsafe impl<K: BTreeKey> Zeroable for KPtrPair<K> {}
unsafe impl<K: BTreeKey> Pod for KPtrPair<K> {}

pub struct BTree<K: BTreeKey, V: BTreeItem> {
    pub root_block: u64,
    node_type: u32,
    fast_leaf: Option<FastLeaf<K>>,
    range_leaf: Option<RangeLeaf<K>>,
    /// Phase 9: false (default) = LIVE-tree handle: reads honor the
    /// root cells/Disk mirror (a moved live root is always found).
    /// true = HISTORICAL view handle (snapshot reads, unpin walks):
    /// `root_block` is an explicit frozen root and must be honored
    /// verbatim -- following the live root would read post-snapshot
    /// state into a "frozen" view.
    frozen_view: bool,
    _marker: PhantomData<(K, V)>,
}

impl<K: BTreeKey, V: BTreeItem> BTree<K, V> {
    pub fn new(root_block: u64, node_type: u32) -> Self {
        Self {
            root_block,
            node_type,
            fast_leaf: None,
            range_leaf: None,
            frozen_view: false,
            _marker: PhantomData,
        }
    }

    /// Phase 9: a handle over an explicitly FROZEN root (a snapshot's
    /// recorded view). Root-cell pickup is disabled: the live tree may
    /// have moved on, but this handle reads the recorded past.
    pub fn new_frozen(root_block: u64, node_type: u32) -> Self {
        Self {
            root_block,
            node_type,
            fast_leaf: None,
            range_leaf: None,
            frozen_view: true,
            _marker: PhantomData,
        }
    }

    /// The root this handle should read from: the live effective root
    /// (root cells + Disk mirror) for live handles, the verbatim
    /// `root_block` for frozen-view handles.
    fn view_root(&self, ctx: &TxContext) -> u64 {
        if self.frozen_view {
            self.root_block
        } else {
            ctx.effective_root(self.node_type, self.root_block)
        }
    }

    /// Initializes a new empty root node on disk at `root_block`.
    pub fn init_empty(ctx: &mut TxContext, root_block: u64, node_type: u32) -> Result<()> {
        bump_epoch(); // any prior fast-append caches for this root are void
        let node = BTreeNodeData::new(0, node_type);
        ctx.write_block(root_block, bytes_of(&node))
    }

    /// Helper to read a node
    fn read_node(&self, ctx: &mut TxContext, block_num: u64) -> Result<BTreeNodeData> {
        let is_dirty = ctx.tx.dirty_blocks.contains_key(&block_num);
        let node: BTreeNodeData = if is_dirty {
            let mut buf = [0u8; 4096];
            ctx.read_block(block_num, &mut buf)?;
            pod_read_unaligned(&buf)
        } else {
            if let Some(cache) = &ctx.node_cache {
                if let Some(arc_node) = cache.get(block_num) {
                    let locked = arc_node.read().unwrap();
                    return Ok(*locked);
                }
            }

            let mut buf = [0u8; 4096];
            ctx.read_block(block_num, &mut buf)?;
            let node: BTreeNodeData = pod_read_unaligned(&buf);

            // Verify checksum
            let expected_csum = node.header.checksum;
            let mut node_copy = node;
            node_copy.header.checksum = 0;
            let csum_bytes = calculate_checksum(ChecksumAlgorithm::Crc32c, bytes_of(&node_copy));
            let computed_csum = u32::from_le_bytes(csum_bytes[0..4].try_into().unwrap());

            if expected_csum != computed_csum && expected_csum != 0 {
                eprintln!("BTree Node Corruption detected at block {}", block_num);
                return Err(Error::new(
                    ErrorKind::InvalidData,
                    "BTree Node Checksum Mismatch",
                ));
            }

            if let Some(cache) = &ctx.node_cache {
                let mut aligned_node = BTreeNodeData::new(node.header.level, node.header.node_type);
                unsafe {
                    std::ptr::copy_nonoverlapping(
                        &node as *const BTreeNodeData as *const u8,
                        &mut aligned_node as *mut BTreeNodeData as *mut u8,
                        std::mem::size_of::<BTreeNodeData>(),
                    );
                }
                cache.insert(block_num, aligned_node);
            }
            node
        };

        if node.header.magic != BTREE_MAGIC || node.header.node_type != self.node_type {
            eprintln!(
                "Invalid BTree node magic or type (block {block_num}: magic {:#x} expected {:#x}, type {} expected {}, item_count {}, dirty {})",
                node.header.magic, BTREE_MAGIC, node.header.node_type, self.node_type, node.header.item_count, is_dirty
            );
            return Err(Error::new(
                ErrorKind::InvalidData,
                format!(
                    "Invalid BTree node magic or type (block {block_num}: magic {:#x} type {} expected {}, item_count {})",
                    node.header.magic, node.header.node_type, self.node_type, node.header.item_count
                ),
            ));
        }

        let max_items = if node.header.level == 0 {
            Self::max_leaf_items()
        } else {
            Self::max_internal_items()
        };
        if node.header.item_count as usize > max_items {
            eprintln!(
                "Invalid BTree item_count (block {block_num}: item_count {} > max {}, level {}, dirty {})",
                node.header.item_count, max_items, node.header.level, is_dirty
            );
            return Err(Error::new(
                ErrorKind::InvalidData,
                format!(
                    "Invalid BTree item_count (block {block_num}: item_count {} > max {})",
                    node.header.item_count, max_items
                ),
            ));
        }

        Ok(node)
    }

    /// Helper to write a node
    #[track_caller]
    fn write_node(&self, ctx: &mut TxContext, block_num: u64, node: &BTreeNodeData) -> Result<()> {
        let mut node_copy = *node;
        node_copy.header.checksum = 0;
        // Phase 9: stamp every node write with a fresh global stamp.
        // The stamp is what makes path-copy CoW sound: a node whose
        // stamp is above every live snapshot barrier provably post-
        // dates all snapshots, so mutating it in place is safe.
        node_copy.header.generation = node_gen_next();
        let csum_bytes = calculate_checksum(ChecksumAlgorithm::Crc32c, bytes_of(&node_copy));
        node_copy.header.checksum = u32::from_le_bytes(csum_bytes[0..4].try_into().unwrap());

        let bytes = bytes_of(&node_copy);
        ctx.write_block(block_num, bytes)?;
        if let Some(cache) = &ctx.node_cache {
            cache.insert(block_num, node_copy);
        }
        Ok(())
    }

    // A leaf node stores pairs of (K, V).
    // An internal node stores K and u64 (block pointers).
    // Usually, internal nodes have N keys and N+1 pointers.
    // For simplicity, we can store pairs of (K, u64), where the pointer represents the right child of the key.
    // The very first pointer (leftmost) can be stored implicitly or explicitly.

    // Max capacity calculations
    pub fn max_leaf_items() -> usize {
        BTREE_PAYLOAD_SIZE / std::mem::size_of::<KVPair<K, V>>()
    }

    pub fn max_internal_items() -> usize {
        (BTREE_PAYLOAD_SIZE - 8) / std::mem::size_of::<KPtrPair<K>>()
    }

    pub fn lookup(&self, ctx: &mut TxContext, key: &K) -> Result<Option<V>> {
        // Phase 9: live handles honor the root cells/Disk mirror (a
        // CoW-relocated root is always found); frozen-view handles
        // honor their recorded root verbatim.
        let mut current_block = self.view_root(ctx);

        loop {
            let node = self.read_node(ctx, current_block)?;
            if node.header.level == 0 {
                let count = (node.header.item_count as usize).min(Self::max_leaf_items());
                // Leaf node
                let items: &[KVPair<K, V>] = bytemuck::cast_slice(
                    &node.payload[..count * std::mem::size_of::<KVPair<K, V>>()],
                );
                // Binary search
                match items.binary_search_by(|kv| kv.key.cmp(key)) {
                    Ok(idx) => return Ok(Some(items[idx].value)),
                    Err(_) => return Ok(None),
                }
            } else {
                let count = (node.header.item_count as usize).min(Self::max_internal_items());
                // Internal node
                // Payload structure: [u64; leftmost_child], [KPtrPair; count]
                let leftmost_ptr: u64 = pod_read_unaligned(&node.payload[0..8]);
                let items: &[KPtrPair<K>] = bytemuck::cast_slice(
                    &node.payload[8..8 + count * std::mem::size_of::<KPtrPair<K>>()],
                );

                let mut next_block = leftmost_ptr;
                for kv in items {
                    if key >= &kv.key {
                        next_block = kv.ptr;
                    } else {
                        break;
                    }
                }
                current_block = next_block;
            }
        }
    }

    /// Floor lookup: returns the (key, value) pair with the largest key
    /// <= `key`, or None if every key in the tree is greater. Used by the
    /// extent-spill mapping to find the extent covering a logical block
    /// (extent keys are `logical_start`, so the covering extent is the
    /// floor of the block number, subject to a length check by the
    /// caller).
    pub fn lookup_floor(&self, ctx: &mut TxContext, key: &K) -> Result<Option<(K, V)>> {
        // Phase 9: root pickup for live handles; frozen views honor
        // their recorded root verbatim.
        let mut current_block = self.view_root(ctx);

        loop {
            let node = self.read_node(ctx, current_block)?;
            if node.header.level == 0 {
                let count = (node.header.item_count as usize).min(Self::max_leaf_items());
                let items: &[KVPair<K, V>] = bytemuck::cast_slice(
                    &node.payload[..count * std::mem::size_of::<KVPair<K, V>>()],
                );
                match items.binary_search_by(|kv| kv.key.cmp(key)) {
                    Ok(idx) => return Ok(Some((items[idx].key, items[idx].value))),
                    // Descending to this leaf already guarantees the key
                    // is inside this leaf's separator range, so Err(0)
                    // can only occur in the tree's leftmost leaf -- no
                    // floor exists.
                    Err(0) => return Ok(None),
                    Err(idx) => return Ok(Some((items[idx - 1].key, items[idx - 1].value))),
                }
            } else {
                let count = (node.header.item_count as usize).min(Self::max_internal_items());
                let leftmost_ptr: u64 = pod_read_unaligned(&node.payload[0..8]);
                let items: &[KPtrPair<K>] = bytemuck::cast_slice(
                    &node.payload[8..8 + count * std::mem::size_of::<KPtrPair<K>>()],
                );
                let mut next_block = leftmost_ptr;
                for kv in items {
                    if key >= &kv.key {
                        next_block = kv.ptr;
                    } else {
                        break;
                    }
                }
                current_block = next_block;
            }
        }
    }

    /// Full iteration over every (key, value) pair in key order.
    /// Used by extent-spill-aware truncate/free paths to visit every
    /// spilled extent. O(n) reads; not for hot paths.
    ///
    /// Phase 9: this walks the tree STRUCTURE (internal child
    /// pointers), not the `next_leaf` sibling chain. Under metadata
    /// CoW a copied leaf is only reachable through the parent's
    /// repointed child pointer; the old chain still threads through
    /// the frozen originals, so a chain walk would silently skip the
    /// copies and return stale contents. (Split surgery still keeps
    /// the chain truthful for the frozen views that recorded it.)
    pub fn iter_all(&self, ctx: &mut TxContext) -> Result<Vec<(K, V)>> {
        let root = self.view_root(ctx);
        let mut out = Vec::new();
        self.collect_pairs(ctx, root, &mut out)?;
        Ok(out)
    }

    /// In-order traversal via internal child pointers: leaves in key
    /// order, following exactly the pointers a CoW repoint keeps
    /// current.
    fn collect_pairs(&self, ctx: &mut TxContext, block: u64, out: &mut Vec<(K, V)>) -> Result<()> {
        let node = self.read_node(ctx, block)?;
        let count = node.header.item_count as usize;
        if node.header.level == 0 {
            let items: &[KVPair<K, V>] = bytemuck::cast_slice(
                &node.payload[..count * std::mem::size_of::<KVPair<K, V>>()],
            );
            for kv in items {
                out.push((kv.key, kv.value));
            }
            return Ok(());
        }
        let leftmost: u64 = pod_read_unaligned(&node.payload[0..8]);
        self.collect_pairs(ctx, leftmost, out)?;
        let items: &[KPtrPair<K>] = bytemuck::cast_slice(
            &node.payload[8..8 + count * std::mem::size_of::<KPtrPair<K>>()],
        );
        for kv in items {
            self.collect_pairs(ctx, kv.ptr, out)?;
        }
        Ok(())
    }

    pub fn insert<F>(
        &mut self,
        ctx: &mut TxContext,
        key: K,
        value: V,
        mut allocate_block: F,
    ) -> Result<()>
    where
        F: FnMut(&mut TxContext) -> Result<u64>,
    {
        // FAST-INPLACE-UPDATE PATH: If key is within our cached leaf's [min_key, max_key] range,
        // and it exists in the leaf, update in-place without descending the B-tree!
        if let Some(rl) = self.range_leaf {
            let safe_count = rl.item_count as usize;
            if key >= rl.min_key
                && key <= rl.max_key
                && safe_count > 0
                && rl.epoch == current_epoch()
            {
                if let Ok(node) = self.read_node(ctx, rl.leaf_block) {
                    if node.header.level == 0
                        && node.header.item_count as usize == safe_count
                    {
                        let barrier = cow_barrier_of(ctx);
                        let leaf_frozen = barrier > 0
                            && is_frozen_tree(self.node_type)
                            && node.header.generation <= barrier;
                        if !leaf_frozen {
                            let old_items: &[KVPair<K, V>] = bytemuck::cast_slice(
                                &node.payload[..safe_count * std::mem::size_of::<KVPair<K, V>>()],
                            );
                            if let Ok(idx) = old_items.binary_search_by(|kv| kv.key.cmp(&key)) {
                                let mut node = node;
                                let sz = std::mem::size_of::<KVPair<K, V>>();
                                let at = idx * sz;
                                let pair = KVPair { key, value };
                                node.payload[at..at + sz].copy_from_slice(bytes_of(&pair));
                                self.write_node(ctx, rl.leaf_block, &node)?;
                                return Ok(());
                            }
                        }
                    }
                }
                self.range_leaf = None;
            }
        }

        // FAST-APPEND PATH (Phase 3.2). A monotone key (strictly
        // greater than the cached rightmost-leaf maximum), a not-full
        // leaf, and an unchanged structural epoch let us skip the root
        // descent entirely: read the leaf, revalidate it against the
        // cached (count, last_key), append the pair at the end, write
        // the leaf back. The revalidation makes a stale cache FAIL
        // SAFE: any mismatch falls through to the ordinary descent.
        if let Some(fl) = self.fast_leaf {
            let safe_count = fl.item_count as usize;
            if key > fl.last_key
                && safe_count > 0
                && safe_count < Self::max_leaf_items() - 1
                && fl.epoch == current_epoch()
            {
                if let Ok(node) = self.read_node(ctx, fl.leaf_block) {
                    if node.header.level == 0
                        && node.header.item_count as usize == safe_count
                    {
                        let pair = KVPair { key, value };
                        let sz = std::mem::size_of::<KVPair<K, V>>();
                        let last_at = (safe_count - 1) * sz;
                        let last =
                            pod_read_unaligned::<KVPair<K, V>>(&node.payload[last_at..last_at + sz]);
                        // Phase 9: the cached leaf may be FROZEN (stamp
                        // <= the snapshot barrier). Appending in place
                        // would corrupt the snapshot's recorded view, so
                        // a frozen leaf always takes the slow descent,
                        // where the CoW pass copies it first.
                        let barrier = cow_barrier_of(ctx);
                        let leaf_frozen = barrier > 0
                            && is_frozen_tree(self.node_type)
                            && node.header.generation <= barrier;
                        if last.key == fl.last_key && !leaf_frozen {
                            let mut node = node;
                            let at = safe_count * sz;
                            node.payload[at..at + sz]
                                .copy_from_slice(bytes_of(&pair));
                            node.header.item_count += 1;
                            self.write_node(ctx, fl.leaf_block, &node)?;
                            self.fast_leaf = Some(FastLeaf {
                                leaf_block: fl.leaf_block,
                                last_key: key,
                                item_count: node.header.item_count,
                                epoch: fl.epoch,
                            });
                            return Ok(());
                        }
                    }
                }
                // Revalidation failed: something reshaped this leaf
                // behind our back. Invalidate and take the slow road.
                self.fast_leaf = None;
            }
        }

        // Simple insert without split support first, to establish structure
        let mut path = Vec::new();
        // Phase 9: root-cell pickup -- a CoW move earlier in this same
        // transaction may have relocated the live root past what the
        // (stale) superblock value says.
        let mut current_block = self.view_root(ctx);
        self.root_block = current_block;

        // Whether this descent always took the rightmost child at every
        // level -- if so, the leaf we land in is the tree's rightmost
        // leaf, and a successful insert into it can (re)populate the
        // fast-append cache with the leaf's true maximum key.
        let mut rightmost = true;

        // Find leaf
        let mut node = loop {
            let n = self.read_node(ctx, current_block)?;
            path.push(current_block);
            if n.header.level == 0 {
                break n;
            }

            let count = (n.header.item_count as usize).min(Self::max_internal_items());
            let leftmost_ptr: u64 = pod_read_unaligned(&n.payload[0..8]);
            let items: &[KPtrPair<K>] =
                bytemuck::cast_slice(&n.payload[8..8 + count * std::mem::size_of::<KPtrPair<K>>()]);

            // Rightmost-child check: the descent is still rightmost iff
            // the key is at or past this node's LAST separator.
            if count > 0 && key < items[count - 1].key {
                rightmost = false;
            }

            let mut next_block = leftmost_ptr;
            for kv in items {
                if key >= kv.key {
                    next_block = kv.ptr;
                } else {
                    break;
                }
            }
            current_block = next_block;
        };

        // ------------------------------------------------------------------
        // PHASE 9 METADATA CoW: with a live snapshot barrier, every node
        // on this descent whose stamp is <= the barrier MAY be part of
        // a frozen view recorded by a snapshot. Path-copy such nodes
        // (repointing their parents at the copies) BEFORE the mutation
        // below touches them. Each node is copied at most once per
        // snapshot epoch: the copy is stamped above the barrier, so
        // later descents take it in place. Non-frozen trees and
        // barrier 0 (no snapshots) skip this entirely.
        // ------------------------------------------------------------------
        if cow_barrier_of(ctx) > 0 && is_frozen_tree(self.node_type) {
            let effective = self.cow_descent_path(ctx, &path, &mut allocate_block)?;
            path = effective;
            current_block = *path.last().expect("descent path is non-empty");
        }

        let count = node.header.item_count as usize;
        if count >= Self::max_leaf_items() - 1 {
            // Need to split
            path.pop(); // remove current_block from path
            let new_root = self.split_leaf(
                ctx,
                current_block,
                &mut node,
                &mut path,
                &mut allocate_block,
            )?;
            if let Some(r) = new_root {
                self.root_block = r;
            }
            // After split, we should retry insert to keep logic simple
            return self.insert(ctx, key, value, allocate_block);
        }

        // Insert into leaf
        let mut items = vec![KVPair { key, value }; count + 1];
        let old_items: &[KVPair<K, V>] =
            bytemuck::cast_slice(&node.payload[..count * std::mem::size_of::<KVPair<K, V>>()]);

        let insert_idx = match old_items.binary_search_by(|kv| kv.key.cmp(&key)) {
            Ok(idx) => {
                // Update existing
                items[..count].copy_from_slice(old_items);
                items[idx] = KVPair { key, value };
                let bytes = bytemuck::cast_slice(&items[..count]);
                node.payload[..bytes.len()].copy_from_slice(bytes);
                self.write_node(ctx, current_block, &node)?;
                self.refresh_fast_leaf_if_rightmost(current_block, rightmost, &items[count - 1], node.header.item_count);
                if count > 0 {
                    self.range_leaf = Some(RangeLeaf {
                        leaf_block: current_block,
                        min_key: items[0].key,
                        max_key: items[count - 1].key,
                        item_count: node.header.item_count,
                        epoch: current_epoch(),
                    });
                }
                return Ok(());
            }
            Err(idx) => idx,
        };

        // Shift and insert
        if insert_idx > 0 {
            items[..insert_idx].copy_from_slice(&old_items[..insert_idx]);
        }
        items[insert_idx] = KVPair { key, value };
        if insert_idx < count {
            items[insert_idx + 1..].copy_from_slice(&old_items[insert_idx..]);
        }

        node.header.item_count += 1;
        let bytes = bytemuck::cast_slice(&items);
        node.payload[..bytes.len()].copy_from_slice(bytes);
        self.write_node(ctx, current_block, &node)?;
        self.refresh_fast_leaf_if_rightmost(current_block, rightmost, &items[count], node.header.item_count);
        self.range_leaf = Some(RangeLeaf {
            leaf_block: current_block,
            min_key: items[0].key,
            max_key: items[count].key,
            item_count: node.header.item_count,
            epoch: current_epoch(),
        });
        Ok(())
    }

    /// Populate the fast-append cache after a slow-path insert that
    /// landed (by proof of the descent, not by hope) in the tree's
    /// rightmost leaf. `last` is the leaf's post-insert maximum key
    /// (the caller knows whether the new pair or an existing one is
    /// the maximum), `item_count` its post-insert count.
    fn refresh_fast_leaf_if_rightmost(
        &mut self,
        leaf_block: u64,
        rightmost: bool,
        last: &KVPair<K, V>,
        item_count: u16,
    ) {
        if rightmost {
            self.fast_leaf = Some(FastLeaf {
                leaf_block,
                last_key: last.key,
                item_count,
                epoch: current_epoch(),
            });
        }
    }

    fn split_leaf<F>(
        &mut self,
        ctx: &mut TxContext,
        leaf_block: u64,
        leaf_node: &mut BTreeNodeData,
        path: &mut Vec<u64>,
        allocate_block: &mut F,
    ) -> Result<Option<u64>>
    where
        F: FnMut(&mut TxContext) -> Result<u64>,
    {
        // A split reshapes the leaf chain: every fast-append cache in
        // every handle over this tree (or any tree -- the epoch is
        // global and conservative) is void until re-proven.
        bump_epoch();
        self.fast_leaf = None;
        self.range_leaf = None;
        let right_block = allocate_block(ctx)?;
        let mut right_node = BTreeNodeData::new(0, self.node_type);
        right_node.header.next_leaf = leaf_node.header.next_leaf;
        right_node.header.prev_leaf = leaf_block;

        if leaf_node.header.next_leaf != 0 {
            if let Ok(mut old_next) = self.read_node(ctx, leaf_node.header.next_leaf) {
                if old_next.header.prev_leaf == leaf_block {
                    old_next.header.prev_leaf = right_block;
                    let _ = self.write_node(ctx, leaf_node.header.next_leaf, &old_next);
                }
            }
        }

        leaf_node.header.next_leaf = right_block;

        let count = leaf_node.header.item_count as usize;
        let mid = count / 2;
        let right_count = count - mid;

        let old_items: &[KVPair<K, V>] =
            bytemuck::cast_slice(&leaf_node.payload[..count * std::mem::size_of::<KVPair<K, V>>()]);
        let right_items = &old_items[mid..];
        let promote_key = right_items[0].key;

        let right_bytes = bytemuck::cast_slice(right_items);
        right_node.payload[..right_bytes.len()].copy_from_slice(right_bytes);
        right_node.header.item_count = right_count as u16;

        leaf_node.header.item_count = mid as u16;
        // Zero out old data in leaf node to avoid confusion (optional, but good)
        let end_bytes = leaf_node.header.item_count as usize * std::mem::size_of::<KVPair<K, V>>();
        leaf_node.payload[end_bytes..count * std::mem::size_of::<KVPair<K, V>>()].fill(0);

        self.write_node(ctx, right_block, &right_node)?;
        self.write_node(ctx, leaf_block, leaf_node)?;

        let parent_block = path.pop().unwrap_or(0);
        self.insert_into_parent(
            ctx,
            parent_block,
            leaf_block,
            promote_key,
            right_block,
            path,
            allocate_block,
        )
    }

    fn insert_into_parent<F>(
        &mut self,
        ctx: &mut TxContext,
        parent_block: u64,
        left_block: u64,
        key: K,
        right_block: u64,
        path: &mut Vec<u64>,
        allocate_block: &mut F,
    ) -> Result<Option<u64>>
    where
        F: FnMut(&mut TxContext) -> Result<u64>,
    {
        if parent_block == 0 {
            // STABLE-ROOT SPLIT (Phase 0 bug fix).
            //
            // `left_block` is the current root: its post-split left-half
            // content is already on disk (written by `split_leaf` or by
            // the internal-split branch below). The old code allocated a
            // brand-new root block and only updated the in-memory
            // `BTree.root_block` field -- every *persistent* caller
            // (superblock fields, `ChecksumTree::new(root)` wrappers,
            // remounts) reconstructs the tree from the original root
            // block number and would find only the left half of the
            // data. Instead we move the left-half content to a freshly
            // allocated block and write the new, taller root INTO the
            // original root block number, so the root pointer held by
            // any caller stays valid for the life of the filesystem.
            let moved = allocate_block(ctx)?;
            let mut moved_node = self.read_node(ctx, left_block)?;
            moved_node.header.parent_block = left_block;

            // Leaf-chain bookkeeping: if the old root was a leaf, the
            // right sibling's `prev_leaf` was set to `left_block` by
            // `split_leaf`; the left half now lives at `moved`, so
            // point it there instead.
            if moved_node.header.level == 0 {
                let mut right = self.read_node(ctx, right_block)?;
                right.header.parent_block = left_block;
                if right.header.prev_leaf == left_block {
                    right.header.prev_leaf = moved;
                }
                self.write_node(ctx, right_block, &right)?;
            } else {
                let mut right = self.read_node(ctx, right_block)?;
                right.header.parent_block = left_block;
                self.write_node(ctx, right_block, &right)?;

                let count = (moved_node.header.item_count as usize).min(Self::max_internal_items());
                let leftmost: u64 = pod_read_unaligned(&moved_node.payload[0..8]);
                let items: &[KPtrPair<K>] = bytemuck::cast_slice(
                    &moved_node.payload[8..8 + count * std::mem::size_of::<KPtrPair<K>>()],
                );
                let reparent = |b: u64, ctx: &mut TxContext| -> Result<()> {
                    let mut child = self.read_node(ctx, b)?;
                    if child.header.parent_block == left_block {
                        child.header.parent_block = moved;
                        self.write_node(ctx, b, &child)?;
                    }
                    Ok(())
                };
                reparent(leftmost, ctx)?;
                for kv in items {
                    reparent(kv.ptr, ctx)?;
                }
            }

            self.write_node(ctx, moved, &moved_node)?;

            // New root, one level taller, at the ORIGINAL root block.
            let mut root_node = BTreeNodeData::new(moved_node.header.level + 1, self.node_type);
            let ptr_bytes: [u8; 8] = bytemuck::cast(moved);
            root_node.payload[0..8].copy_from_slice(&ptr_bytes);
            let item = KPtrPair {
                key,
                ptr: right_block,
            };
            let item_bytes = bytemuck::bytes_of(&item);
            root_node.payload[8..8 + item_bytes.len()].copy_from_slice(item_bytes);
            root_node.header.item_count = 1;

            self.write_node(ctx, left_block, &root_node)?;
            // Root block number is unchanged: no caller fixup needed.
            return Ok(None);
        }

        // Read parent
        let mut parent_node = self.read_node(ctx, parent_block)?;
        let count = parent_node.header.item_count as usize;

        let mut items = vec![KPtrPair { key, ptr: 0 }; count + 1];
        let old_items: &[KPtrPair<K>] = bytemuck::cast_slice(
            &parent_node.payload[8..8 + count * std::mem::size_of::<KPtrPair<K>>()],
        );

        let insert_idx = match old_items.binary_search_by(|kv| kv.key.cmp(&key)) {
            Ok(idx) => idx + 1,
            Err(idx) => idx,
        };

        if insert_idx > 0 {
            items[..insert_idx].copy_from_slice(&old_items[..insert_idx]);
        }
        items[insert_idx] = KPtrPair {
            key,
            ptr: right_block,
        };
        if insert_idx < count {
            items[insert_idx + 1..].copy_from_slice(&old_items[insert_idx..]);
        }

        if count >= Self::max_internal_items() - 1 {
            // Need to split parent internal node
            let right_internal_block = allocate_block(ctx)?;
            let mut right_internal_node =
                BTreeNodeData::new(parent_node.header.level, self.node_type);
            right_internal_node.header.parent_block = parent_node.header.parent_block;

            let total = count + 1;
            let mid = total / 2;
            let right_count = total - mid - 1; // 1 goes up

            let promote_up_key = items[mid].key;

            // Leftmost ptr of right internal is the ptr of the promoted key
            let right_leftmost_ptr: [u8; 8] = bytemuck::cast(items[mid].ptr);
            right_internal_node.payload[0..8].copy_from_slice(&right_leftmost_ptr);

            let right_items = &items[mid + 1..];
            let right_bytes = bytemuck::cast_slice(right_items);
            right_internal_node.payload[8..8 + right_bytes.len()].copy_from_slice(right_bytes);
            right_internal_node.header.item_count = right_count as u16;

            // Phase 0 bug fix: the parent keeps items[..mid], and those
            // kept items MUST be written into the parent's payload.
            // The original code only updated `item_count`, leaving the
            // payload holding the pre-insert items: whenever the new
            // separator landed before the split point (e.g. reverse or
            // random key order) the parent kept stale routing entries
            // and one subtree became unreachable.
            parent_node.header.item_count = mid as u16;
            let kept_bytes = bytemuck::cast_slice(&items[..mid]);
            parent_node.payload[8..8 + kept_bytes.len()].copy_from_slice(kept_bytes);
            let old_end = 8 + count * std::mem::size_of::<KPtrPair<K>>();
            parent_node.payload[8 + kept_bytes.len()..old_end].fill(0);

            self.write_node(ctx, right_internal_block, &right_internal_node)?;
            self.write_node(ctx, parent_block, &parent_node)?;

            // Update parent pointers of children moved to right internal
            let reparent = |b: u64, ctx: &mut TxContext| -> Result<()> {
                let mut child = self.read_node(ctx, b)?;
                child.header.parent_block = right_internal_block;
                self.write_node(ctx, b, &child)?;
                Ok(())
            };
            reparent(items[mid].ptr, ctx)?;
            for item in right_items {
                reparent(item.ptr, ctx)?;
            }

            let parent_parent = path.pop().unwrap_or(0);
            return self.insert_into_parent(
                ctx,
                parent_parent,
                parent_block,
                promote_up_key,
                right_internal_block,
                path,
                allocate_block,
            );
        }

        parent_node.header.item_count += 1;
        let bytes = bytemuck::cast_slice(&items);
        parent_node.payload[8..8 + bytes.len()].copy_from_slice(bytes);
        self.write_node(ctx, parent_block, &parent_node)?;

        Ok(None)
    }

    /// Remove without an allocator. Safe for every NON-frozen tree
    /// (freespace, refcount, snapshot, clone, subvolume, dedup,
    /// cluster): the Phase 9 CoW pass never runs for them, so the
    /// allocator is never invoked. Removing from a FROZEN tree (inode,
    /// dir, spill-extent, checksum) while a snapshot barrier is live
    /// returns an error instead of silently mutating a frozen view --
    /// use [`remove_with_alloc`] there.
    pub fn remove(&mut self, ctx: &mut TxContext, key: &K) -> Result<bool> {
        let mut no_alloc = |_ctx: &mut TxContext| -> Result<u64> {
            Err(Error::new(
                ErrorKind::OutOfMemory,
                "BTree::remove on a frozen tree under a snapshot barrier \
                 requires an allocator (use remove_with_alloc)",
            ))
        };
        self.remove_inner(ctx, key, &mut no_alloc)
    }

    /// Remove with an allocator for frozen-tree CoW copies. Identical
    /// to [`remove`] when no snapshot barrier is live.
    pub fn remove_with_alloc<F>(
        &mut self,
        ctx: &mut TxContext,
        key: &K,
        mut allocate_block: F,
    ) -> Result<bool>
    where
        F: FnMut(&mut TxContext) -> Result<u64>,
    {
        self.remove_inner(ctx, key, &mut allocate_block)
    }

    fn remove_inner<F>(
        &mut self,
        ctx: &mut TxContext,
        key: &K,
        mut allocate_block: F,
    ) -> Result<bool>
    where
        F: FnMut(&mut TxContext) -> Result<u64>,
    {
        // Removals (and any future merges they grow) change leaf
        // contents and can empty the rightmost leaf: the fast-append
        // cache cannot survive them.
        bump_epoch();
        self.fast_leaf = None;
        self.range_leaf = None;
        let mut path = Vec::new();
        // Phase 9: root-cell pickup (same rationale as insert).
        let mut current_block = self.view_root(ctx);
        self.root_block = current_block;

        // Find leaf
        let mut node = loop {
            let n = self.read_node(ctx, current_block)?;
            path.push(current_block);
            if n.header.level == 0 {
                break n;
            }

            let count = (n.header.item_count as usize).min(Self::max_internal_items());
            let leftmost_ptr: u64 = pod_read_unaligned(&n.payload[0..8]);
            let items: &[KPtrPair<K>] =
                bytemuck::cast_slice(&n.payload[8..8 + count * std::mem::size_of::<KPtrPair<K>>()]);

            let mut next_block = leftmost_ptr;
            for kv in items {
                if key >= &kv.key {
                    next_block = kv.ptr;
                } else {
                    break;
                }
            }
            current_block = next_block;
        };

        // PHASE 9 METADATA CoW: same path-copy pass as insert -- a
        // frozen node on this descent must be copied before the
        // removal mutates it in place.
        if cow_barrier_of(ctx) > 0 && is_frozen_tree(self.node_type) {
            let effective = self.cow_descent_path(ctx, &path, &mut allocate_block)?;
            path = effective;
            current_block = *path.last().expect("descent path is non-empty");
        }

        let count = (node.header.item_count as usize).min(Self::max_leaf_items());
        let old_items: &[KVPair<K, V>] =
            bytemuck::cast_slice(&node.payload[..count * std::mem::size_of::<KVPair<K, V>>()]);

        match old_items.binary_search_by(|kv| kv.key.cmp(key)) {
            Ok(idx) => {
                // Key found, remove it
                let mut new_items = Vec::with_capacity(count - 1);
                new_items.extend_from_slice(&old_items[..idx]);
                new_items.extend_from_slice(&old_items[idx + 1..]);

                node.header.item_count -= 1;
                let bytes = bytemuck::cast_slice(&new_items);
                node.payload[..bytes.len()].copy_from_slice(bytes);

                // Clear remaining bytes
                node.payload[bytes.len()..count * std::mem::size_of::<KVPair<K, V>>()].fill(0);

                self.write_node(ctx, current_block, &node)?;

                // Check for underflow and merge if needed
                if node.header.item_count < (Self::max_leaf_items() / 2) as u16
                    && current_block != self.root_block
                {
                    self.merge(ctx, current_block, &mut node)?;
                }

                Ok(true)
            }
            Err(_) => Ok(false), // Key not found
        }
    }

    /// Phase 9 metadata path-copy CoW: given the descent `path` (root
    /// .. leaf, ORIGINAL block numbers), copy every node whose stored
    /// stamp is <= the CoW barrier into a fresh private block and
    /// repoint its parent at the copy. Returns the effective path
    /// (same length; entry i is the original block if it needed no
    /// copy, or the fresh copy if it did). A copied root updates
    /// `self.root_block` AND the transaction's root cell, so every
    /// later handle -- and the superblock sync at commit -- finds the
    /// live tree.
    ///
    /// Soundness argument: a node with stamp <= barrier was last
    /// written before some live snapshot existed, so a frozen view MAY
    /// reach it and in-place mutation could corrupt that view. A node
    /// with stamp > barrier was written after EVERY live snapshot was
    /// created (the caller maintains the barrier as the max stamp over
    /// live snapshots), so no frozen view can include it. The copy is
    /// stamped with the CURRENT global counter, which sits above every
    /// persisted barrier, making it provably private.
    fn cow_descent_path<F>(
        &mut self,
        ctx: &mut TxContext,
        path: &[u64],
        allocate_block: &mut F,
    ) -> Result<Vec<u64>>
    where
        F: FnMut(&mut TxContext) -> Result<u64>,
    {
        let barrier = cow_barrier_of(ctx);
        debug_assert!(barrier > 0 && is_frozen_tree(self.node_type));
        let mut effective: Vec<u64> = Vec::with_capacity(path.len());
        for (i, &orig) in path.iter().enumerate() {
            let mut node = self.read_node(ctx, orig)?;
            if node.header.generation > barrier {
                // Provably private: mutate in place below.
                effective.push(orig);
                continue;
            }
            let copy_block = allocate_block(ctx)?;
            if i > 0 {
                // Keep the bookkeeping parent pointer truthful in the
                // copy (navigation uses child pointers, never this).
                node.header.parent_block = effective[i - 1];
            }
            // Copy the node AS-IS; write_node stamps it with a fresh
            // generation above every barrier, making it private.
            self.write_node(ctx, copy_block, &node)?;
            if i == 0 {
                // The root moved: the root cell is the source of truth
                // until the superblock sync at commit.
                self.root_block = copy_block;
                ctx.set_root_cell(self.node_type, copy_block);
            } else {
                // Fix the parent's child pointer. The parent's effective
                // block is effective[i-1]: either the original (private
                // -- stamp > barrier -- so in-place restamping by
                // write_node is fine) or its fresh copy (also private).
                let parent_block = effective[i - 1];
                let mut parent = self.read_node(ctx, parent_block)?;
                Self::replace_child_ptr(&mut parent, orig, copy_block);
                self.write_node(ctx, parent_block, &parent)?;
            }
            effective.push(copy_block);
        }
        Ok(effective)
    }

    /// Replace the child pointer `old` with `new` in an internal node
    /// (leftmost pointer slot plus every separator item). Leaves have
    /// no child pointers; a no-op there is defensive.
    fn replace_child_ptr(node: &mut BTreeNodeData, old: u64, new: u64) {
        if node.header.level == 0 {
            return;
        }
        let leftmost: u64 = pod_read_unaligned(&node.payload[0..8]);
        if leftmost == old {
            node.payload[0..8].copy_from_slice(bytemuck::bytes_of(&new));
        }
        let count = node.header.item_count as usize;
        if count == 0 {
            return;
        }
        let itemsz = std::mem::size_of::<KPtrPair<K>>();
        let items: &mut [KPtrPair<K>] =
            bytemuck::cast_slice_mut(&mut node.payload[8..8 + count * itemsz]);
        for kv in items.iter_mut() {
            if kv.ptr == old {
                kv.ptr = new;
            }
        }
    }

    fn merge(
        &mut self,
        _ctx: &mut TxContext,
        _block: u64,
        _node: &mut BTreeNodeData,
    ) -> Result<()> {
        // Full B+ tree merge and redistribution logic involves checking sibling capacity.
        // For Phase 4, we mark this stub for future background defragmentation / Vacuum.
        // We will allow under-full nodes until a vacuum process runs.
        Ok(())
    }

    pub fn validate(&self, ctx: &mut TxContext) -> Result<u64> {
        self.validate_node(ctx, self.root_block, 0)
    }

    fn validate_node(&self, ctx: &mut TxContext, block: u64, depth: u64) -> Result<u64> {
        let node = self.read_node(ctx, block)?;
        let mut count = 1;
        let item_count = node.header.item_count as usize;

        if node.header.level > 0 {
            let leftmost_ptr: u64 = pod_read_unaligned(&node.payload[0..8]);
            if leftmost_ptr != 0 {
                count += self.validate_node(ctx, leftmost_ptr, depth + 1)?;
            }

            let items: &[KPtrPair<K>] = bytemuck::cast_slice(
                &node.payload[8..8 + item_count * std::mem::size_of::<KPtrPair<K>>()],
            );
            for kv in items {
                if kv.ptr != 0 {
                    count += self.validate_node(ctx, kv.ptr, depth + 1)?;
                }
            }
        }
        Ok(count)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::disk::block_io::Disk;
    use crate::ondisk::serialization::Superblock;
    use crate::transaction::manager::TransactionManager;
    use bytemuck::{Pod, Zeroable};

    #[repr(C)]
    #[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
    struct TestKey(u64);
    unsafe impl Zeroable for TestKey {}
    unsafe impl Pod for TestKey {}

    #[repr(C)]
    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    struct TestValue(u64);
    unsafe impl Zeroable for TestValue {}
    unsafe impl Pod for TestValue {}

    #[test]
    fn test_btree_insert_lookup() {
        let temp_dir = std::env::temp_dir();
        let path = temp_dir.join("test_btree.img");
        let mut disk = Disk::create(&path, 1024 * 1024 * 10).unwrap();
        // format basic SB
        let sb = Superblock {
            magic: 0,
            version: 0,
            block_size: 4096,
            total_blocks: 1024,
            free_blocks: 0,
            inode_count: 0,
            root_inode: 0,
            flags: 0,
            padding1: 0,
            bitmap_start: 0,
            inode_table_start: 0,
            data_region_start: 0,
            generation: 0,
            checksum: 0,
            padding_csum: 0,
            journal_start: 1,
            journal_blocks: 10,
            secondary_sb_1: 0,
            secondary_sb_2: 0,
            block_group_count: 0,
            blocks_per_group: 0,
            inode_tree_root: 0,
            dir_tree_root: 0,
            extent_tree_root: 0,
            freespace_tree_root: 0,
            next_ino: 2,
            checksum_tree_root: 0,
            bad_blocks_root: 0,
            snapshot_tree_root: 0,
            clone_tree_root: 0,
            refcount_tree_root: 0,
            subvolume_tree_root: 0,
            space_map_root: 0,
            last_snapshot_generation: 0,
            dedupe_tree_root: 0,
            key_tree_root: 0,
            fs_features: 0,
            default_compression: 0,
            default_encryption: 0,
            padding_phase7: [0; 6],
            device_tree_root: 0,
            pool_uuid: [0; 16],
            raid_profile: 0,
            padding_raid: [0; 3],
            chunk_size: 0,
            crypto_tree_root: 0,
            padding2: [0; 3760], xattr_tree_root: 0, key_envelope_block: 0,
            node_generation: 0,
        };
        disk.write_block(0, bytemuck::bytes_of(&sb)).unwrap();

        let tm = TransactionManager::new(&sb);
        let mut tx = tm.begin(0);
        let mut ctx = TxContext::new(&mut disk, &mut tx);

        BTree::<TestKey, TestValue>::init_empty(&mut ctx, 100, 1).unwrap();
        let mut btree = BTree::<TestKey, TestValue>::new(100, 1);

        let mut next_block = 101;
        let mut allocator = |_: &mut TxContext| {
            let b = next_block;
            next_block += 1;
            Ok(b)
        };

        // Insert 10,000 items
        for i in 0..10000 {
            btree
                .insert(&mut ctx, TestKey(i), TestValue(i * 2), &mut allocator)
                .unwrap();
        }

        // Lookup
        for i in 0..10000 {
            let val = btree.lookup(&mut ctx, &TestKey(i)).unwrap();
            assert_eq!(val, Some(TestValue(i * 2)));
        }

        // Lookup missing
        let val = btree.lookup(&mut ctx, &TestKey(10001)).unwrap();
        assert_eq!(val, None);
    }

    /// A zeroed-but-valid Superblock for test disk images (the inline
    /// literal above, factored out so the fast-append tests below stay
    /// readable).
    fn test_sb() -> Superblock {
        let mut sb = Superblock {
            magic: 0,
            version: 0,
            block_size: 4096,
            total_blocks: 1024,
            free_blocks: 0,
            inode_count: 0,
            root_inode: 0,
            flags: 0,
            padding1: 0,
            bitmap_start: 0,
            inode_table_start: 0,
            data_region_start: 0,
            generation: 0,
            checksum: 0,
            padding_csum: 0,
            journal_start: 1,
            journal_blocks: 10,
            secondary_sb_1: 0,
            secondary_sb_2: 0,
            block_group_count: 0,
            blocks_per_group: 0,
            inode_tree_root: 0,
            dir_tree_root: 0,
            extent_tree_root: 0,
            freespace_tree_root: 0,
            next_ino: 2,
            checksum_tree_root: 0,
            bad_blocks_root: 0,
            snapshot_tree_root: 0,
            clone_tree_root: 0,
            refcount_tree_root: 0,
            subvolume_tree_root: 0,
            space_map_root: 0,
            last_snapshot_generation: 0,
            dedupe_tree_root: 0,
            key_tree_root: 0,
            fs_features: 0,
            default_compression: 0,
            default_encryption: 0,
            padding_phase7: [0; 6],
            device_tree_root: 0,
            pool_uuid: [0; 16],
            raid_profile: 0,
            padding_raid: [0; 3],
            chunk_size: 0,
            crypto_tree_root: 0,
            padding2: [0; 3760], xattr_tree_root: 0, key_envelope_block: 0,
            node_generation: 0,
        };
        sb.block_size = 4096;
        sb
    }

    /// Sequential inserts take the fast-append path after the first
    /// insert; the resulting tree must be byte-for-byte equivalent to
    /// the slow path's: every key findable, `validate` happy, and the
    /// item count exact.
    #[test]
    fn fast_append_matches_slow_path_sequential() {
        let dir = std::env::temp_dir();
        let path = dir.join("test_btree_fast_seq.img");
        let mut disk = Disk::create(&path, 1024 * 1024 * 10).unwrap();
        let sb = test_sb();
        disk.write_block(0, bytemuck::bytes_of(&sb)).unwrap();
        let tm = TransactionManager::new(&sb);
        let mut tx = tm.begin(0);
        let mut ctx = TxContext::new(&mut disk, &mut tx);

        BTree::<TestKey, TestValue>::init_empty(&mut ctx, 100, 1).unwrap();
        let mut btree = BTree::<TestKey, TestValue>::new(100, 1);
        let mut next_block = 101;
        let mut allocator = |_: &mut TxContext| {
            let b = next_block;
            next_block += 1;
            Ok(b)
        };

        for i in 0..3000 {
            btree
                .insert(&mut ctx, TestKey(i), TestValue(i * 3), &mut allocator)
                .unwrap();
        }
        for i in 0..3000 {
            assert_eq!(
                btree.lookup(&mut ctx, &TestKey(i)).unwrap(),
                Some(TestValue(i * 3)),
                "key {i}"
            );
        }
        assert_eq!(btree.lookup(&mut ctx, &TestKey(3000)).unwrap(), None);
        assert_eq!(btree.iter_all(&mut ctx).unwrap().len(), 3000);
        assert!(btree.validate(&mut ctx).unwrap() > 0);
        let _ = std::fs::remove_file(&path);
    }

    /// Ascending runs interleaved with mid-tree inserts and overwrites:
    /// the fast cache is populated, invalidated by splits, repopulated,
    /// and bypassed for existing keys -- all orders must stay findable.
    #[test]
    fn fast_append_survives_interleaved_mid_inserts() {
        let dir = std::env::temp_dir();
        let path = dir.join("test_btree_fast_mixed.img");
        let mut disk = Disk::create(&path, 1024 * 1024 * 10).unwrap();
        let sb = test_sb();
        disk.write_block(0, bytemuck::bytes_of(&sb)).unwrap();
        let tm = TransactionManager::new(&sb);
        let mut tx = tm.begin(0);
        let mut ctx = TxContext::new(&mut disk, &mut tx);

        BTree::<TestKey, TestValue>::init_empty(&mut ctx, 100, 1).unwrap();
        let mut btree = BTree::<TestKey, TestValue>::new(100, 1);
        let mut next_block = 101;
        let mut allocator = |_: &mut TxContext| {
            let b = next_block;
            next_block += 1;
            Ok(b)
        };

        // Ascending blocks of 200, with a mid insert and an overwrite of
        // an existing key between blocks.
        let mut top = 0u64;
        for round in 0..15 {
            for i in 0..200 {
                let k = top + i;
                btree
                    .insert(&mut ctx, TestKey(k), TestValue(k + 1), &mut allocator)
                    .unwrap();
            }
            top += 200;
            // Mid insert (forces a slow-path descent; may split).
            btree
                .insert(&mut ctx, TestKey(7 + round), TestValue(999), &mut allocator)
                .unwrap();
            // Overwrite an existing key (never a fast append: equal key).
            btree
                .insert(&mut ctx, TestKey(50), TestValue(4242), &mut allocator)
                .unwrap();
        }
        for i in 0..top {
            // keys 7..=21 were mid-inserted (rounds 0..=14) to 999;
            // key 50 was overwritten to 4242; everything else is k+1.
            let expected = if (7..=21).contains(&i) {
                999
            } else if i == 50 {
                4242
            } else {
                i + 1
            };
            assert_eq!(
                btree.lookup(&mut ctx, &TestKey(i)).unwrap(),
                Some(TestValue(expected)),
                "key {i}"
            );
        }
        // mid inserts hit existing keys; no new keys beyond top
        assert_eq!(btree.iter_all(&mut ctx).unwrap().len(), top as usize);
        assert!(btree.validate(&mut ctx).unwrap() > 0);
        let _ = std::fs::remove_file(&path);
    }

    /// THE safety property of the epoch guard: handle A caches the
    /// rightmost leaf; handle B (a second live handle over the same
    /// root) grows the tree past it, splitting leaves; A's next
    /// monotone insert must NOT land in its stale cached leaf. Without
    /// the epoch check this test corrupts the tree (A's key becomes
    /// unreachable by descent).
    #[test]
    fn two_handles_epoch_safety() {
        let dir = std::env::temp_dir();
        let path = dir.join("test_btree_two_handles.img");
        let mut disk = Disk::create(&path, 1024 * 1024 * 10).unwrap();
        let sb = test_sb();
        disk.write_block(0, bytemuck::bytes_of(&sb)).unwrap();
        let tm = TransactionManager::new(&sb);
        let mut tx = tm.begin(0);
        let mut ctx = TxContext::new(&mut disk, &mut tx);

        BTree::<TestKey, TestValue>::init_empty(&mut ctx, 100, 1).unwrap();
        let mut a = BTree::<TestKey, TestValue>::new(100, 1);
        let mut b = BTree::<TestKey, TestValue>::new(100, 1);
        let mut next_block = 101;
        let mut allocator = |_: &mut TxContext| {
            let blk = next_block;
            next_block += 1;
            Ok(blk)
        };

        // A fills 0..2000 (caches the rightmost leaf under epoch E).
        for i in 0..2000 {
            a.insert(&mut ctx, TestKey(i), TestValue(i), &mut allocator)
                .unwrap();
        }
        // B -- which has NO cache and will split -- appends 2000..3000.
        // Splits bump the global epoch; A's cache is now provably stale.
        for i in 2000..3000 {
            b.insert(&mut ctx, TestKey(i), TestValue(i), &mut allocator)
                .unwrap();
        }
        // A inserts again, monotone w.r.t. ITS cached last key (1999).
        for i in 3000..3200 {
            a.insert(&mut ctx, TestKey(i), TestValue(i), &mut allocator)
                .unwrap();
        }
        // A fresh handle must find every key A and B ever inserted.
        let c = BTree::<TestKey, TestValue>::new(100, 1);
        for i in 0..3200 {
            assert_eq!(
                c.lookup(&mut ctx, &TestKey(i)).unwrap(),
                Some(TestValue(i)),
                "key {i} unreachable -- stale fast-append cache corrupted the tree"
            );
        }
        assert_eq!(c.iter_all(&mut ctx).unwrap().len(), 3200);
        assert!(c.validate(&mut ctx).unwrap() > 0);
        let _ = std::fs::remove_file(&path);
    }

    /// `remove` must invalidate the fast-append cache: removing the
    /// tree maximum and then appending a higher key has to land in the
    /// true rightmost leaf, not a cached phantom.
    #[test]
    fn remove_invalidates_fast_path() {
        let dir = std::env::temp_dir();
        let path = dir.join("test_btree_rm_fast.img");
        let mut disk = Disk::create(&path, 1024 * 1024 * 10).unwrap();
        let sb = test_sb();
        disk.write_block(0, bytemuck::bytes_of(&sb)).unwrap();
        let tm = TransactionManager::new(&sb);
        let mut tx = tm.begin(0);
        let mut ctx = TxContext::new(&mut disk, &mut tx);

        BTree::<TestKey, TestValue>::init_empty(&mut ctx, 100, 1).unwrap();
        let mut btree = BTree::<TestKey, TestValue>::new(100, 1);
        let mut next_block = 101;
        let mut allocator = |_: &mut TxContext| {
            let blk = next_block;
            next_block += 1;
            Ok(blk)
        };

        for i in 0..500 {
            btree
                .insert(&mut ctx, TestKey(i), TestValue(i), &mut allocator)
                .unwrap();
        }
        for k in (400..500).rev() {
            assert!(btree.remove(&mut ctx, &TestKey(k)).unwrap());
        }
        // Append beyond the (now smaller) tree maximum.
        for i in 500..600 {
            btree
                .insert(&mut ctx, TestKey(i), TestValue(i), &mut allocator)
                .unwrap();
        }
        for i in 0..400 {
            assert_eq!(btree.lookup(&mut ctx, &TestKey(i)).unwrap(), Some(TestValue(i)));
        }
        for i in 400..500 {
            assert_eq!(btree.lookup(&mut ctx, &TestKey(i)).unwrap(), None, "removed key {i} resurrected");
        }
        for i in 500..600 {
            assert_eq!(btree.lookup(&mut ctx, &TestKey(i)).unwrap(), Some(TestValue(i)));
        }
        assert_eq!(btree.iter_all(&mut ctx).unwrap().len(), 500);
        assert!(btree.validate(&mut ctx).unwrap() > 0);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn test_btree_backward_insert() {
        let temp_dir = std::env::temp_dir();
        let path = temp_dir.join("test_btree_back.img");
        let mut disk = Disk::create(&path, 1024 * 1024 * 10).unwrap();
        let sb = Superblock {
            magic: 0,
            version: 0,
            block_size: 4096,
            total_blocks: 1024,
            free_blocks: 0,
            inode_count: 0,
            root_inode: 0,
            flags: 0,
            padding1: 0,
            bitmap_start: 0,
            inode_table_start: 0,
            data_region_start: 0,
            generation: 0,
            checksum: 0,
            padding_csum: 0,
            journal_start: 1,
            journal_blocks: 10,
            secondary_sb_1: 0,
            secondary_sb_2: 0,
            block_group_count: 0,
            blocks_per_group: 0,
            inode_tree_root: 0,
            dir_tree_root: 0,
            extent_tree_root: 0,
            freespace_tree_root: 0,
            next_ino: 2,
            checksum_tree_root: 0,
            bad_blocks_root: 0,
            snapshot_tree_root: 0,
            clone_tree_root: 0,
            refcount_tree_root: 0,
            subvolume_tree_root: 0,
            space_map_root: 0,
            last_snapshot_generation: 0,
            dedupe_tree_root: 0,
            key_tree_root: 0,
            fs_features: 0,
            default_compression: 0,
            default_encryption: 0,
            padding_phase7: [0; 6],
            device_tree_root: 0,
            pool_uuid: [0; 16],
            raid_profile: 0,
            padding_raid: [0; 3],
            chunk_size: 0,
            crypto_tree_root: 0,
            padding2: [0; 3760], xattr_tree_root: 0, key_envelope_block: 0,
            node_generation: 0,
        };
        disk.write_block(0, bytemuck::bytes_of(&sb)).unwrap();

        let tm = TransactionManager::new(&sb);
        let mut tx = tm.begin(0);
        let mut ctx = TxContext::new(&mut disk, &mut tx);

        BTree::<TestKey, TestValue>::init_empty(&mut ctx, 100, 1).unwrap();
        let mut btree = BTree::<TestKey, TestValue>::new(100, 1);

        let mut next_block = 101;
        let mut allocator = |_: &mut TxContext| {
            let b = next_block;
            next_block += 1;
            Ok(b)
        };

        // Insert backwards
        for i in (0..5000).rev() {
            btree
                .insert(&mut ctx, TestKey(i), TestValue(i * 3), &mut allocator)
                .unwrap();
        }

        // Lookup
        for i in 0..5000 {
            let val = btree.lookup(&mut ctx, &TestKey(i)).unwrap();
            assert_eq!(val, Some(TestValue(i * 3)));
        }
    }

    #[test]
    fn test_btree_remove() {
        let temp_dir = std::env::temp_dir();
        let path = temp_dir.join("test_btree_remove.img");
        let mut disk = Disk::create(&path, 1024 * 1024 * 10).unwrap();
        let sb = Superblock {
            magic: 0,
            version: 0,
            block_size: 4096,
            total_blocks: 1024,
            free_blocks: 0,
            inode_count: 0,
            root_inode: 0,
            flags: 0,
            padding1: 0,
            bitmap_start: 0,
            inode_table_start: 0,
            data_region_start: 0,
            generation: 0,
            checksum: 0,
            padding_csum: 0,
            journal_start: 1,
            journal_blocks: 10,
            secondary_sb_1: 0,
            secondary_sb_2: 0,
            block_group_count: 0,
            blocks_per_group: 0,
            inode_tree_root: 0,
            dir_tree_root: 0,
            extent_tree_root: 0,
            freespace_tree_root: 0,
            next_ino: 2,
            checksum_tree_root: 0,
            bad_blocks_root: 0,
            snapshot_tree_root: 0,
            clone_tree_root: 0,
            refcount_tree_root: 0,
            subvolume_tree_root: 0,
            space_map_root: 0,
            last_snapshot_generation: 0,
            dedupe_tree_root: 0,
            key_tree_root: 0,
            fs_features: 0,
            default_compression: 0,
            default_encryption: 0,
            padding_phase7: [0; 6],
            device_tree_root: 0,
            pool_uuid: [0; 16],
            raid_profile: 0,
            padding_raid: [0; 3],
            chunk_size: 0,
            crypto_tree_root: 0,
            padding2: [0; 3760], xattr_tree_root: 0, key_envelope_block: 0,
            node_generation: 0,
        };
        disk.write_block(0, bytemuck::bytes_of(&sb)).unwrap();

        let tm = TransactionManager::new(&sb);
        let mut tx = tm.begin(0);
        let mut ctx = TxContext::new(&mut disk, &mut tx);

        BTree::<TestKey, TestValue>::init_empty(&mut ctx, 100, 1).unwrap();
        let mut btree = BTree::<TestKey, TestValue>::new(100, 1);

        let mut next_block = 101;
        let mut allocator = |_: &mut TxContext| {
            let b = next_block;
            next_block += 1;
            Ok(b)
        };

        for i in 0..100 {
            btree
                .insert(&mut ctx, TestKey(i), TestValue(i * 5), &mut allocator)
                .unwrap();
        }

        // Remove evens
        for i in (0..100).step_by(2) {
            let removed = btree.remove(&mut ctx, &TestKey(i)).unwrap();
            assert!(removed);
        }

        // Check lookup
        for i in 0..100 {
            let val = btree.lookup(&mut ctx, &TestKey(i)).unwrap();
            if i % 2 == 0 {
                assert_eq!(val, None);
            } else {
                assert_eq!(val, Some(TestValue(i * 5)));
            }
        }
    }
}
