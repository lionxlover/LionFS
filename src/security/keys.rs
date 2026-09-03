//! Symmetric key generation and storage for per-file encryption.
//!
//! Keys are 256-bit and generated from the OS CSPRNG (see
//! `security::encryption::fill_random`). Each key is identified by a
//! `key_id` (the same `u32` stored in `Inode::key_id`). Keys live in an
//! in-memory keyring for the lifetime of the mount, backed by a real
//! on-disk BTree (`KeyTree`, rooted at `Superblock::key_tree_root`) so a
//! key generated in one session is still available after a remount.
//!
//! Threat model note: this stores keys unencrypted in the key tree, i.e.
//! "encryption at rest" here protects data from someone reading the disk
//! image directly, not from someone with access to a live, mounted
//! filesystem's on-disk key tree. Wrapping the key tree itself with a
//! passphrase-derived key is a natural next step and is not implemented
//! here.

use crate::btree::tree::BTree;
use crate::transaction::transaction::TxContext;
use bytemuck::{Pod, Zeroable};
use std::collections::HashMap;
use std::io::Result;

pub const KEY_TREE_NODE_TYPE: u32 = 11;

#[repr(C)]
#[derive(Debug, Clone, Copy, Pod, Zeroable)]
pub struct KeyTreeRecord {
    pub algorithm: u8,
    pub flags: u8,
    pub _padding: [u8; 6],
    pub key: [u8; 32],
}

impl KeyTreeRecord {
    pub fn new(algorithm: u8, key: [u8; 32]) -> Self {
        Self {
            algorithm,
            flags: 0,
            _padding: [0; 6],
            key,
        }
    }
}

/// Thin wrapper around `BTree<u32, KeyTreeRecord>`, mirroring the pattern
/// used by `integrity::checksum_tree::ChecksumTree` and
/// `integrity::bad_blocks::BadBlockManager`.
pub struct KeyTree {
    tree: BTree<u32, KeyTreeRecord>,
}

impl KeyTree {
    pub fn new(root_block: u64) -> Self {
        Self {
            tree: BTree::new(root_block, KEY_TREE_NODE_TYPE),
        }
    }

    pub fn init_empty(ctx: &mut TxContext, root_block: u64) -> Result<()> {
        BTree::<u32, KeyTreeRecord>::init_empty(ctx, root_block, KEY_TREE_NODE_TYPE)
    }

    pub fn load(&self, ctx: &mut TxContext, key_id: u32) -> Result<Option<KeyTreeRecord>> {
        self.tree.lookup(ctx, &key_id)
    }

    pub fn store<F>(
        &mut self,
        ctx: &mut TxContext,
        key_id: u32,
        record: KeyTreeRecord,
        allocate_block: F,
    ) -> Result<()>
    where
        F: FnMut(&mut TxContext) -> Result<u64>,
    {
        self.tree.insert(ctx, key_id, record, allocate_block)
    }
}

/// In-memory keyring, lazily backed by the on-disk `KeyTree`.
pub struct KeyManager {
    cache: HashMap<u32, (u8, [u8; 32])>, // key_id -> (algorithm, key bytes)
    next_id: u32,
}

impl Default for KeyManager {
    fn default() -> Self {
        Self::new()
    }
}

impl KeyManager {
    pub fn new() -> Self {
        // key_id 0 is reserved to mean "no encryption" throughout LionFS
        // (see Inode::key_id), so generated ids start at 1.
        Self {
            cache: HashMap::new(),
            next_id: 1,
        }
    }

    /// Generates a fresh random key for `algorithm`, assigns it an unused
    /// id, and caches it in memory. Call `persist` afterwards to write it
    /// to the on-disk key tree so it survives a remount.
    pub fn generate_key(&mut self, algorithm: u8) -> Result<(u32, [u8; 32])> {
        let mut key = [0u8; 32];
        crate::security::encryption::fill_random(&mut key)?;
        let id = self.next_id;
        self.next_id = self.next_id.wrapping_add(1).max(1);
        self.cache.insert(id, (algorithm, key));
        Ok((id, key))
    }

    /// Returns a key from the in-memory cache only (no disk I/O).
    pub fn get_key_cached(&self, key_id: u32) -> Option<[u8; 32]> {
        if key_id == 0 {
            return None;
        }
        self.cache.get(&key_id).map(|(_, k)| *k)
    }

    pub fn set_key_cached(&mut self, key_id: u32, algorithm: u8, key: [u8; 32]) {
        if key_id != 0 {
            self.cache.insert(key_id, (algorithm, key));
        }
    }

    /// Returns a key, checking the in-memory cache first and falling back
    /// to a single on-disk lookup (caching the result) if the key tree is
    /// initialized (`key_tree_root != 0`).
    pub fn get_key(
        &mut self,
        ctx: &mut TxContext,
        key_tree_root: u64,
        key_id: u32,
    ) -> Result<Option<[u8; 32]>> {
        if key_id == 0 {
            return Ok(None);
        }
        if let Some(k) = self.get_key_cached(key_id) {
            return Ok(Some(k));
        }
        if key_tree_root == 0 {
            return Ok(None);
        }
        let tree = KeyTree::new(key_tree_root);
        match tree.load(ctx, key_id)? {
            Some(rec) => {
                self.cache.insert(key_id, (rec.algorithm, rec.key));
                Ok(Some(rec.key))
            }
            None => Ok(None),
        }
    }

    /// Writes a cached key out to the on-disk key tree.
    pub fn persist<F>(
        &self,
        ctx: &mut TxContext,
        key_tree_root: u64,
        key_id: u32,
        allocate_block: F,
    ) -> Result<()>
    where
        F: FnMut(&mut TxContext) -> Result<u64>,
    {
        if key_tree_root == 0 || key_id == 0 {
            return Ok(());
        }
        if let Some((algorithm, key)) = self.cache.get(&key_id) {
            let mut tree = KeyTree::new(key_tree_root);
            tree.store(
                ctx,
                key_id,
                KeyTreeRecord::new(*algorithm, *key),
                allocate_block,
            )?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generated_keys_are_unique_and_nonzero_id() {
        let mut km = KeyManager::new();
        let (id1, k1) = km.generate_key(1).unwrap();
        let (id2, k2) = km.generate_key(1).unwrap();
        assert_ne!(id1, 0);
        assert_ne!(id2, 0);
        assert_ne!(id1, id2);
        assert_ne!(k1, k2); // vanishingly unlikely to collide if RNG is real
        assert_eq!(km.get_key_cached(id1), Some(k1));
    }

    #[test]
    fn key_id_zero_means_no_key() {
        let km = KeyManager::new();
        assert_eq!(km.get_key_cached(0), None);
    }
}
