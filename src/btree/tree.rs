use crate::btree::node::{BTreeNodeData, BTREE_MAGIC, BTREE_PAYLOAD_SIZE};
use crate::integrity::algorithms::{calculate_checksum, ChecksumAlgorithm};
use crate::transaction::transaction::TxContext;
use bytemuck::{bytes_of, pod_read_unaligned, Pod, Zeroable};
use std::io::{Error, ErrorKind, Result};
use std::marker::PhantomData;

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
    _marker: PhantomData<(K, V)>,
}

impl<K: BTreeKey, V: BTreeItem> BTree<K, V> {
    pub fn new(root_block: u64, node_type: u32) -> Self {
        Self {
            root_block,
            node_type,
            _marker: PhantomData,
        }
    }

    /// Initializes a new empty root node on disk at `root_block`.
    pub fn init_empty(ctx: &mut TxContext, root_block: u64, node_type: u32) -> Result<()> {
        let node = BTreeNodeData::new(0, node_type);
        ctx.write_block(root_block, bytes_of(&node))
    }

    /// Helper to read a node
    fn read_node(&self, ctx: &mut TxContext, block_num: u64) -> Result<BTreeNodeData> {
        if let Some(cache) = &ctx.node_cache {
            if let Some(arc_node) = cache.get(block_num) {
                // If it is dirty in TxContext, we still need the dirty version.
                // Wait, if it is dirty in this transaction, we MUST read the dirty version.
                // Check if dirty in TxContext:
                if !ctx.tx.dirty_blocks.contains_key(&block_num) {
                    let locked = arc_node.read().unwrap();
                    return Ok(*locked);
                }
            }
        }

        let mut buf = [0u8; 4096];
        ctx.read_block(block_num, &mut buf)?;
        let node: BTreeNodeData = pod_read_unaligned(&buf);

        if node.header.magic != BTREE_MAGIC || node.header.node_type != self.node_type {
            return Err(Error::new(
                ErrorKind::InvalidData,
                "Invalid BTree node magic or type",
            ));
        }

        // Verify checksum
        let expected_csum = node.header.checksum;
        let mut node_copy = node;
        node_copy.header.checksum = 0;
        let csum_bytes = calculate_checksum(ChecksumAlgorithm::Crc32c, bytes_of(&node_copy));
        let computed_csum = u32::from_le_bytes(csum_bytes[0..4].try_into().unwrap());

        if expected_csum != computed_csum && expected_csum != 0 {
            // Checksum mismatch
            eprintln!("BTree Node Corruption detected at block {}", block_num);
            return Err(Error::new(
                ErrorKind::InvalidData,
                "BTree Node Checksum Mismatch",
            ));
        }

        if let Some(cache) = &ctx.node_cache {
            if !ctx.tx.dirty_blocks.contains_key(&block_num) {
                // Ensure we insert a cache-line aligned copy
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
        }

        Ok(node)
    }

    /// Helper to write a node
    #[track_caller]
    fn write_node(&self, ctx: &mut TxContext, block_num: u64, node: &BTreeNodeData) -> Result<()> {
        let mut node_copy = *node;
        node_copy.header.checksum = 0;
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
        let mut current_block = self.root_block;

        loop {
            let node = self.read_node(ctx, current_block)?;
            let count = node.header.item_count as usize;

            if node.header.level == 0 {
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
        let mut current_block = self.root_block;

        loop {
            let node = self.read_node(ctx, current_block)?;
            let count = node.header.item_count as usize;

            if node.header.level == 0 {
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

    /// Full iteration over every (key, value) pair in key order, by
    /// descending to the leftmost leaf and walking the `next_leaf`
    /// chain. Used by extent-spill-aware truncate/free paths to visit
    /// every spilled extent. O(n) reads; not for hot paths.
    pub fn iter_all(&self, ctx: &mut TxContext) -> Result<Vec<(K, V)>> {
        // Descend leftmost pointers to the leftmost leaf.
        let mut current_block = self.root_block;
        let mut out = Vec::new();
        loop {
            let node = self.read_node(ctx, current_block)?;
            let count = node.header.item_count as usize;
            if node.header.level == 0 {
                let items: &[KVPair<K, V>] = bytemuck::cast_slice(
                    &node.payload[..count * std::mem::size_of::<KVPair<K, V>>()],
                );
                for kv in items {
                    out.push((kv.key, kv.value));
                }
                if node.header.next_leaf == 0 {
                    break;
                }
                current_block = node.header.next_leaf;
            } else {
                let leftmost_ptr: u64 = pod_read_unaligned(&node.payload[0..8]);
                current_block = leftmost_ptr;
            }
        }
        Ok(out)
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
        // Simple insert without split support first, to establish structure
        let mut path = Vec::new();
        let mut current_block = self.root_block;

        // Find leaf
        let mut node = loop {
            // CoW logic: if node generation <= last_snapshot_generation, CoW it!
            // Wait, we don't have access to last_snapshot_generation here easily unless we pass it.
            // Let's pass `generation: u64` and `last_snapshot: u64` in `TxContext` or as parameters.
            // But we don't want to change the signature if possible.
            // Wait, we do have `ctx`. Maybe `TxContext` doesn't have it.
            let n = self.read_node(ctx, current_block)?;
            path.push(current_block);
            if n.header.level == 0 {
                break n;
            }

            let count = n.header.item_count as usize;
            let leftmost_ptr: u64 = pod_read_unaligned(&n.payload[0..8]);
            let items: &[KPtrPair<K>] =
                bytemuck::cast_slice(&n.payload[8..8 + count * std::mem::size_of::<KPtrPair<K>>()]);

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
        self.write_node(ctx, current_block, &node)
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
        let right_block = allocate_block(ctx)?;
        let mut right_node = BTreeNodeData::new(0, self.node_type);
        right_node.header.next_leaf = leaf_node.header.next_leaf;
        right_node.header.prev_leaf = leaf_block;

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
            let moved_node = self.read_node(ctx, left_block)?;

            // Leaf-chain bookkeeping: if the old root was a leaf, the
            // right sibling's `prev_leaf` was set to `left_block` by
            // `split_leaf`; the left half now lives at `moved`, so
            // point it there instead.
            if moved_node.header.level == 0 {
                let mut right = self.read_node(ctx, right_block)?;
                if right.header.prev_leaf == left_block {
                    right.header.prev_leaf = moved;
                    self.write_node(ctx, right_block, &right)?;
                }
            } else {
                // The moved internal node's children record
                // `parent_block == left_block`; reparent them to
                // `moved`. (Nothing navigates via parent pointers today,
                // but keeping them truthful costs little and keeps
                // future debugging sane.)
                let count = moved_node.header.item_count as usize;
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

    pub fn remove(&mut self, ctx: &mut TxContext, key: &K) -> Result<bool> {
        let mut path = Vec::new();
        let mut current_block = self.root_block;

        // Find leaf
        let mut node = loop {
            let n = self.read_node(ctx, current_block)?;
            path.push(current_block);
            if n.header.level == 0 {
                break n;
            }

            let count = n.header.item_count as usize;
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

        let count = node.header.item_count as usize;
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
            padding2: [0; 3784],
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
            padding2: [0; 3784],
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
            padding2: [0; 3784],
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
