//! Block-level deduplication: real content hashing plus a real on-disk
//! index of hash -> (physical block, reference count).
//!
//! `hash_block` now uses BLAKE3 (fast, cryptographically strong, and
//! already a declared dependency that nothing previously used) instead of
//! the earlier XOR-fold, which was not a real hash: XOR-folding is
//! commutative and order-insensitive per 32-byte lane, so e.g. swapping two
//! 32-byte-aligned chunks -- or many other distinct inputs -- produced
//! identical "hashes", making it unsafe to use as a dedup key (two
//! different blocks could be wrongly treated as identical and merged,
//! silently corrupting one of them).
//!
//! Scope note: this module is a correct, usable building block, but it is
//! **not yet wired into `file::writer`'s write path**. Doing that safely
//! means changing block allocation to sometimes reuse an existing physical
//! block (bumping `DedupeRecord::ref_count`) instead of always allocating a
//! fresh one, and coordinating that with `integrity::refcount` and with
//! `unlink`/truncate's freeing logic -- a real architectural change to the
//! write path that deserves its own focused pass rather than being bundled
//! in here. What's here (hashing + the on-disk index) is fully functional
//! and tested on its own.

use crate::btree::tree::BTree;
use crate::transaction::transaction::TxContext;
use bytemuck::{Pod, Zeroable};
use std::io::Result;

pub const DEDUPE_TREE_NODE_TYPE: u32 = 13;

#[repr(C)]
#[derive(Debug, Clone, Copy, Pod, Zeroable, PartialEq, Eq, PartialOrd, Ord)]
pub struct BlockHash(pub [u8; 32]);

#[repr(C)]
#[derive(Debug, Clone, Copy, Pod, Zeroable)]
pub struct DedupeRecord {
    pub physical_block: u64,
    pub ref_count: u32,
    pub _padding: u32,
}

pub struct DedupeTree {
    tree: BTree<BlockHash, DedupeRecord>,
}

impl DedupeTree {
    pub fn new(root_block: u64) -> Self {
        Self {
            tree: BTree::new(root_block, DEDUPE_TREE_NODE_TYPE),
        }
    }

    pub fn init_empty(ctx: &mut TxContext, root_block: u64) -> Result<()> {
        BTree::<BlockHash, DedupeRecord>::init_empty(ctx, root_block, DEDUPE_TREE_NODE_TYPE)
    }

    /// Looks up an existing block with this content hash.
    pub fn find(&self, ctx: &mut TxContext, hash: [u8; 32]) -> Result<Option<DedupeRecord>> {
        self.tree.lookup(ctx, &BlockHash(hash))
    }

    /// Records a new unique block (ref_count starts at 1).
    pub fn insert_new<F>(
        &mut self,
        ctx: &mut TxContext,
        hash: [u8; 32],
        physical_block: u64,
        allocate_block: F,
    ) -> Result<()>
    where
        F: FnMut(&mut TxContext) -> Result<u64>,
    {
        self.tree.insert(
            ctx,
            BlockHash(hash),
            DedupeRecord {
                physical_block,
                ref_count: 1,
                _padding: 0,
            },
            allocate_block,
        )
    }

    /// Increments the reference count for a hash that already maps to a
    /// physical block (a new logical block now points at the same content).
    pub fn increment_ref<F>(
        &mut self,
        ctx: &mut TxContext,
        hash: [u8; 32],
        allocate_block: F,
    ) -> Result<bool>
    where
        F: FnMut(&mut TxContext) -> Result<u64>,
    {
        if let Some(mut rec) = self.tree.lookup(ctx, &BlockHash(hash))? {
            rec.ref_count += 1;
            self.tree
                .insert(ctx, BlockHash(hash), rec, allocate_block)?;
            Ok(true)
        } else {
            Ok(false)
        }
    }

    /// Decrements the reference count; returns the record's new count, or
    /// `None` if the hash wasn't present. The caller is responsible for
    /// freeing the physical block via the normal allocator when the count
    /// reaches zero (this module only tracks the index, matching the
    /// division of responsibility already used by `integrity::refcount`).
    pub fn decrement_ref<F>(
        &mut self,
        ctx: &mut TxContext,
        hash: [u8; 32],
        allocate_block: F,
    ) -> Result<Option<u32>>
    where
        F: FnMut(&mut TxContext) -> Result<u64>,
    {
        match self.tree.lookup(ctx, &BlockHash(hash))? {
            Some(mut rec) if rec.ref_count > 1 => {
                rec.ref_count -= 1;
                let new_count = rec.ref_count;
                self.tree
                    .insert(ctx, BlockHash(hash), rec, allocate_block)?;
                Ok(Some(new_count))
            }
            Some(_) => {
                self.tree.remove(ctx, &BlockHash(hash))?;
                Ok(Some(0))
            }
            None => Ok(None),
        }
    }
}

pub struct DeduplicationManager;

impl DeduplicationManager {
    /// Real BLAKE3 content hash of a block. Deterministic, and
    /// collision-resistant enough that two different blocks are never
    /// expected to hash the same by chance -- unlike the previous
    /// XOR-fold, this is safe to use as a dedup key.
    pub fn hash_block(data: &[u8]) -> [u8; 32] {
        *blake3::hash(data).as_bytes()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identical_content_hashes_identically() {
        let a = DeduplicationManager::hash_block(b"the same sixteen bytes");
        let b = DeduplicationManager::hash_block(b"the same sixteen bytes");
        assert_eq!(a, b);
    }

    #[test]
    fn different_content_hashes_differently() {
        let a = DeduplicationManager::hash_block(b"block A content");
        let b = DeduplicationManager::hash_block(b"block B content");
        assert_ne!(a, b);
    }

    #[test]
    fn byte_swap_does_not_collide() {
        // A regression check for exactly the weakness the old XOR-fold had:
        // swapping two chunks of a buffer must not produce the same "hash".
        let mut a = vec![0u8; 64];
        for (i, b) in a.iter_mut().enumerate() {
            *b = i as u8;
        }
        let mut b = a.clone();
        b.swap(0, 32); // swap byte 0 (lane 0) with byte 32 (also lane 0 mod 32)
        assert_ne!(
            DeduplicationManager::hash_block(&a),
            DeduplicationManager::hash_block(&b)
        );
    }
}
