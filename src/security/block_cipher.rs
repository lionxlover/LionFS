//! Orchestrates compression + encryption for a single on-disk data block,
//! and the small side-table that makes both possible without changing the
//! fixed 4096-byte block layout the rest of LionFS assumes.
//!
//! ## Why a side table
//! AEAD encryption (`security::encryption`) appends a 16-byte tag to its
//! ciphertext and needs a 12-byte nonce to decrypt. Compression produces
//! variable-length output. Neither fits cleanly inside a fixed 4096-byte
//! block *and* leaves room for the pathological case (incompressible input,
//! encryption overhead) without silently losing data. Rather than shrink
//! the usable payload of every block (which would ripple through
//! `file::writer`'s offset arithmetic), the nonce, the tag, and the
//! compressed length are stored out of band in `BlockTransformTree`, a
//! BTree keyed by physical block number -- the same pattern already used
//! by `integrity::checksum_tree::ChecksumTree` and
//! `integrity::bad_blocks::BadBlockManager`. The on-disk block itself
//! always holds exactly `BLOCK_SIZE` bytes, so extent/allocation accounting
//! is completely unaffected.
//!
//! ## Pipeline
//! Write: compress (optional) -> pad/mark-raw to exactly BLOCK_SIZE ->
//! encrypt (optional, whole block, same length in/out) -> write block +
//! side-table entry.
//! Read: read block (existing checksum verification happens on this raw
//! form, unchanged) -> decrypt (optional) -> decompress (optional) ->
//! exactly BLOCK_SIZE bytes of plaintext.
//!
//! When neither compression nor encryption is enabled for an inode, `encode`
//! and `decode` are a direct passthrough and never touch the side table --
//! the default (and today's only) path is completely unaffected.

use crate::btree::tree::BTree;
use crate::fs::compression::CompressionManager;
use crate::ondisk::serialization::BLOCK_SIZE;
use crate::security::encryption::{generate_nonce, EncryptionManager, NONCE_LEN, TAG_LEN};
use crate::transaction::transaction::TxContext;
use bytemuck::{Pod, Zeroable};
use std::io::{Error, ErrorKind, Result};

pub const CRYPTO_META_TREE_NODE_TYPE: u32 = 12;

/// Sentinel for `BlockTransformMeta::content_len` meaning "the container is
/// exactly BLOCK_SIZE raw (uncompressed) bytes" -- used both when
/// compression is off and when it didn't help.
pub const RAW_SENTINEL: u32 = u32::MAX;

#[repr(C)]
#[derive(Debug, Clone, Copy, Pod, Zeroable)]
pub struct BlockTransformMeta {
    pub content_len: u32,
    pub _padding: [u8; 4],
    pub nonce: [u8; NONCE_LEN],
    pub tag: [u8; TAG_LEN],
}

pub struct BlockTransformTree {
    tree: BTree<u64, BlockTransformMeta>,
}

impl BlockTransformTree {
    pub fn new(root_block: u64) -> Self {
        Self {
            tree: BTree::new(root_block, CRYPTO_META_TREE_NODE_TYPE),
        }
    }

    pub fn init_empty(ctx: &mut TxContext, root_block: u64) -> Result<()> {
        BTree::<u64, BlockTransformMeta>::init_empty(ctx, root_block, CRYPTO_META_TREE_NODE_TYPE)
    }

    pub fn load(
        &self,
        ctx: &mut TxContext,
        physical_block: u64,
    ) -> Result<Option<BlockTransformMeta>> {
        self.tree.lookup(ctx, &physical_block)
    }

    pub fn store<F>(
        &mut self,
        ctx: &mut TxContext,
        physical_block: u64,
        meta: BlockTransformMeta,
        allocate_block: F,
    ) -> Result<()>
    where
        F: FnMut(&mut TxContext) -> Result<u64>,
    {
        self.tree.insert(ctx, physical_block, meta, allocate_block)
    }
}

/// Per-inode settings needed to transform a block. `crypto_tree_root == 0`
/// means the tree hasn't been initialized yet (fresh filesystem predating
/// this feature, or mkfs run without it); in that case encryption/
/// compression are treated as disabled rather than erroring, since there is
/// nowhere to persist the nonce/tag/length.
#[derive(Debug, Clone, Copy)]
pub struct BlockCipherContext {
    pub compression_algo: u8,
    pub encryption_algo: u8,
    pub key: Option<[u8; 32]>,
    pub crypto_tree_root: u64,
}

impl BlockCipherContext {
    pub fn is_active(&self) -> bool {
        self.crypto_tree_root != 0 && (self.compression_algo != 0 || self.encryption_algo != 0)
    }

    /// A context with no compression and no encryption: blocks pass
    /// through `encode_block`/`decode_block` unchanged. This is what
    /// internal metadata paths (directory blocks, inode blocks, trees)
    /// should use -- only user file data goes through the per-inode
    /// cipher context.
    pub fn none() -> Self {
        BlockCipherContext {
            compression_algo: 0,
            encryption_algo: 0,
            key: None,
            crypto_tree_root: 0,
        }
    }
}

/// Transforms one plaintext logical block into what should be written to
/// disk, writing side-table metadata as needed. `plaintext` must be exactly
/// `BLOCK_SIZE` bytes. Returns the exact `BLOCK_SIZE` bytes to hand to
/// `TxContext::write_block`.
pub fn encode_block<F>(
    ctx: &mut TxContext,
    cctx: &BlockCipherContext,
    physical_block: u64,
    plaintext: &[u8],
    mut allocate_block: F,
) -> Result<Vec<u8>>
where
    F: FnMut(&mut TxContext) -> Result<u64>,
{
    if plaintext.len() != BLOCK_SIZE {
        return Err(Error::new(
            ErrorKind::InvalidInput,
            "encode_block requires exactly one BLOCK_SIZE buffer",
        ));
    }
    if !cctx.is_active() {
        return Ok(plaintext.to_vec());
    }

    // Stage 1: compression (operates on the plaintext, produces a
    // container of at most BLOCK_SIZE bytes; falls back to raw if it
    // doesn't actually help or doesn't fit).
    let (container, content_len): (Vec<u8>, u32) = if cctx.compression_algo != 0 {
        if let Some(algo) = CompressionManager::get_algorithm(cctx.compression_algo) {
            let compressed = algo.compress(plaintext);
            if compressed.len() < BLOCK_SIZE {
                let mut padded = compressed.clone();
                padded.resize(BLOCK_SIZE, 0);
                (padded, compressed.len() as u32)
            } else {
                (plaintext.to_vec(), RAW_SENTINEL)
            }
        } else {
            (plaintext.to_vec(), RAW_SENTINEL)
        }
    } else {
        (plaintext.to_vec(), RAW_SENTINEL)
    };

    // Stage 2: encryption (whole BLOCK_SIZE container in, BLOCK_SIZE
    // ciphertext + separate 16-byte tag out).
    let (disk_bytes, nonce, tag): (Vec<u8>, [u8; NONCE_LEN], [u8; TAG_LEN]) =
        if cctx.encryption_algo != 0 {
            let key = cctx.key.ok_or_else(|| {
                Error::new(
                    ErrorKind::InvalidInput,
                    "encryption enabled for this inode but no key was resolved",
                )
            })?;
            let algo = EncryptionManager::get_algorithm(cctx.encryption_algo).ok_or_else(|| {
                Error::new(ErrorKind::InvalidInput, "unknown encryption algorithm id")
            })?;
            let nonce = generate_nonce()?;
            let mut sealed = algo.encrypt(&key, &container, &nonce)?;
            debug_assert_eq!(sealed.len(), BLOCK_SIZE + TAG_LEN);
            let tag_start = sealed.len() - TAG_LEN;
            let mut tag = [0u8; TAG_LEN];
            tag.copy_from_slice(&sealed[tag_start..]);
            sealed.truncate(tag_start);
            (sealed, nonce, tag)
        } else {
            (container, [0u8; NONCE_LEN], [0u8; TAG_LEN])
        };

    let meta = BlockTransformMeta {
        content_len,
        _padding: [0; 4],
        nonce,
        tag,
    };
    let mut tree = BlockTransformTree::new(cctx.crypto_tree_root);
    tree.store(ctx, physical_block, meta, &mut allocate_block)?;

    Ok(disk_bytes)
}

/// Reverses `encode_block`. `disk_bytes` must be exactly `BLOCK_SIZE` bytes
/// as read from `physical_block` (after normal checksum verification has
/// already passed). Returns exactly `BLOCK_SIZE` bytes of plaintext.
pub fn decode_block(
    ctx: &mut TxContext,
    cctx: &BlockCipherContext,
    physical_block: u64,
    disk_bytes: &[u8],
) -> Result<Vec<u8>> {
    if disk_bytes.len() != BLOCK_SIZE {
        return Err(Error::new(
            ErrorKind::InvalidInput,
            "decode_block requires exactly one BLOCK_SIZE buffer",
        ));
    }
    if !cctx.is_active() {
        return Ok(disk_bytes.to_vec());
    }

    let tree = BlockTransformTree::new(cctx.crypto_tree_root);
    let meta = match tree.load(ctx, physical_block)? {
        Some(m) => m,
        // No metadata recorded (e.g. a hole in a sparse file that reads as
        // all-zero and was never actually passed through encode_block):
        // nothing to reverse, return as-is rather than failing the read.
        None => return Ok(disk_bytes.to_vec()),
    };

    let container = if cctx.encryption_algo != 0 {
        let key = cctx.key.ok_or_else(|| {
            Error::new(
                ErrorKind::InvalidInput,
                "encryption enabled for this inode but no key was resolved",
            )
        })?;
        let algo = EncryptionManager::get_algorithm(cctx.encryption_algo).ok_or_else(|| {
            Error::new(ErrorKind::InvalidInput, "unknown encryption algorithm id")
        })?;
        let mut sealed = disk_bytes.to_vec();
        sealed.extend_from_slice(&meta.tag);
        algo.decrypt(&key, &sealed, &meta.nonce)?
    } else {
        disk_bytes.to_vec()
    };

    let mut plaintext = if meta.content_len != RAW_SENTINEL && cctx.compression_algo != 0 {
        let compressor =
            CompressionManager::get_algorithm(cctx.compression_algo).ok_or_else(|| {
                Error::new(ErrorKind::InvalidInput, "unknown compression algorithm id")
            })?;
        let len = meta.content_len as usize;
        if len > container.len() {
            return Err(Error::new(
                ErrorKind::InvalidData,
                "corrupt block transform metadata: content_len exceeds block size",
            ));
        }
        compressor.decompress(&container[..len])?
    } else {
        container
    };

    // Defensive: a full logical block is always exactly BLOCK_SIZE bytes.
    plaintext.resize(BLOCK_SIZE, 0);
    Ok(plaintext)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::disk::block_io::Disk;
    use crate::ondisk::serialization::Superblock;
    use crate::transaction::manager::TransactionManager;

    fn test_disk(path: &std::path::Path) -> Disk {
        let total_blocks = 4096u64;
        Disk::create(path, total_blocks * BLOCK_SIZE as u64).unwrap()
    }

    #[test]
    fn roundtrip_compression_only() {
        let dir = std::env::temp_dir().join(format!("lionfs_bc_test_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("comp_only.img");
        let disk = test_disk(&path);
        let sb = Superblock::zeroed();
        let txm = TransactionManager::new(&sb);
        let mut tx = txm.begin(0);
        let mut ctx = TxContext::new(&disk, &mut tx);

        BlockTransformTree::init_empty(&mut ctx, 50).unwrap();
        let cctx = BlockCipherContext {
            compression_algo: 2,
            encryption_algo: 0,
            key: None,
            crypto_tree_root: 50,
        };
        let plaintext = vec![b'A'; BLOCK_SIZE]; // maximally compressible
        let mut next_free = 51u64;
        let encoded = encode_block(&mut ctx, &cctx, 100, &plaintext, |_| {
            let b = next_free;
            next_free += 1;
            Ok(b)
        })
        .unwrap();
        assert_eq!(encoded.len(), BLOCK_SIZE);
        let decoded = decode_block(&mut ctx, &cctx, 100, &encoded).unwrap();
        assert_eq!(decoded, plaintext);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn roundtrip_encryption_only() {
        let dir = std::env::temp_dir().join(format!("lionfs_bc_test2_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("enc_only.img");
        let disk = test_disk(&path);
        let sb = Superblock::zeroed();
        let txm = TransactionManager::new(&sb);
        let mut tx = txm.begin(0);
        let mut ctx = TxContext::new(&disk, &mut tx);

        BlockTransformTree::init_empty(&mut ctx, 50).unwrap();
        let cctx = BlockCipherContext {
            compression_algo: 0,
            encryption_algo: 1,
            key: Some([3u8; 32]),
            crypto_tree_root: 50,
        };
        let mut plaintext = vec![0u8; BLOCK_SIZE];
        for (i, b) in plaintext.iter_mut().enumerate() {
            *b = (i % 251) as u8;
        }
        let mut next_free = 51u64;
        let encoded = encode_block(&mut ctx, &cctx, 200, &plaintext, |_| {
            let b = next_free;
            next_free += 1;
            Ok(b)
        })
        .unwrap();
        assert_eq!(encoded.len(), BLOCK_SIZE);
        assert_ne!(encoded, plaintext, "ciphertext must not equal plaintext");
        let decoded = decode_block(&mut ctx, &cctx, 200, &encoded).unwrap();
        assert_eq!(decoded, plaintext);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn roundtrip_compression_and_encryption() {
        let dir = std::env::temp_dir().join(format!("lionfs_bc_test3_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("both.img");
        let disk = test_disk(&path);
        let sb = Superblock::zeroed();
        let txm = TransactionManager::new(&sb);
        let mut tx = txm.begin(0);
        let mut ctx = TxContext::new(&disk, &mut tx);

        BlockTransformTree::init_empty(&mut ctx, 50).unwrap();
        let cctx = BlockCipherContext {
            compression_algo: 1,
            encryption_algo: 2,
            key: Some([9u8; 32]),
            crypto_tree_root: 50,
        };
        let plaintext = vec![b'Z'; BLOCK_SIZE];
        let mut next_free = 51u64;
        let encoded = encode_block(&mut ctx, &cctx, 300, &plaintext, |_| {
            let b = next_free;
            next_free += 1;
            Ok(b)
        })
        .unwrap();
        let decoded = decode_block(&mut ctx, &cctx, 300, &encoded).unwrap();
        assert_eq!(decoded, plaintext);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn inactive_context_is_a_pure_passthrough() {
        let dir = std::env::temp_dir().join(format!("lionfs_bc_test4_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("inactive.img");
        let disk = test_disk(&path);
        let sb = Superblock::zeroed();
        let txm = TransactionManager::new(&sb);
        let mut tx = txm.begin(0);
        let mut ctx = TxContext::new(&disk, &mut tx);

        let cctx = BlockCipherContext {
            compression_algo: 0,
            encryption_algo: 0,
            key: None,
            crypto_tree_root: 0,
        };
        let plaintext = vec![7u8; BLOCK_SIZE];
        let encoded = encode_block(&mut ctx, &cctx, 400, &plaintext, |_| Ok(0)).unwrap();
        assert_eq!(encoded, plaintext);
        let decoded = decode_block(&mut ctx, &cctx, 400, &encoded).unwrap();
        assert_eq!(decoded, plaintext);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
