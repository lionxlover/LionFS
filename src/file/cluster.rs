//! Compression CLUSTERS (Phase 4): the on-disk scheme that makes
//! compression actually save space.
//!
//! Before this module, `block_cipher::encode_block` compressed each
//! 4 KiB block into a shorter byte string that was then padded back
//! out to one full physical block -- entropy was reduced, but zero
//! space was saved. The cluster scheme (Btrfs-like) groups
//! `CLUSTER_BLOCKS` logical blocks (32 = 128 KiB) into one compression
//! unit:
//!
//! * Writing a cluster compresses the whole 128 KiB at once (bigger
//!   window -> better ratio than 32 independent 4 KiB compressions),
//!   and the compressed output occupies only as many physical blocks
//!   as it needs: a variable-length physical extent.
//! * The mapping "logical cluster index -> physical extent" lives in a
//!   per-inode B-tree (the ClusterTree), keyed by cluster index. For
//!   compressed inodes it is rooted at `Inode::spill_extent_root` --
//!   the field is free for this use because a compressed inode never
//!   has inline/spilled extents (all its data lives in clusters; the
//!   two meanings are mutually exclusive by construction, gated on
//!   `Inode::compression_algo != 0`).
//! * Incompressible clusters fall back to raw storage (`is_raw`), so a
//!   corpus of random bytes never GROWS beyond its logical size by
//!   more than one block of padding per cluster.
//!
//! TRADEOFF (stated plainly, per the plan's ground rules): any write
//! into an existing cluster is a whole-cluster read-modify-write --
//! decompress 128 KiB, splice, recompress, rewrite the extent. Random
//! small writes into compressed data are therefore expensive (same
//! class of tradeoff Btrfs makes). Sequential writes and reads are
//! fine, and reads of repeated small ranges hit the decompressed
//! cluster LRU below.
//!
//! Integrity: compressed inodes do not use the per-block checksum tree
//! (its keys and on-disk-block assumptions don't fit variable-length
//! extents). Corruption is detected by zstd frame decoding failure
//! instead. Encryption is not supported on compressed inodes in this
//! phase; the writers reject that combination explicitly rather than
//! silently writing plaintext.

use std::io::{Error, ErrorKind, Result};
use std::sync::{Arc, OnceLock};

use crate::allocator::bitmap::Allocator;
use crate::btree::tree::BTree;
use crate::ondisk::serialization::{BlockGroupDescriptor, Inode, BLOCK_SIZE};
use crate::transaction::transaction::TxContext;

pub const CLUSTER_BLOCKS: u64 = 32;
pub const CLUSTER_BYTES: u64 = CLUSTER_BLOCKS * BLOCK_SIZE as u64;
pub const CLUSTER_TREE_NODE_TYPE: u32 = 6;

/// One cluster's physical placement. 32 bytes, Pod-safe.
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, bytemuck::Pod, bytemuck::Zeroable)]
pub struct ClusterValue {
    /// First physical block of the variable-length extent.
    pub physical_start: u64,
    /// Extent length in physical blocks (ceil(payload/4KiB)).
    pub physical_blocks: u64,
    /// Payload bytes actually stored (the rest of the last block is
    /// zero padding).
    pub compressed_bytes: u64,
    /// 0 = stored raw (incompressible fallback), 2 = zstd.
    pub algo: u8,
    /// Compression level used when written (informational).
    pub level: u8,
    pub is_raw: u8,
    pub padding: [u8; 5],
}

pub struct ClusterTree {
    pub btree: BTree<u64, ClusterValue>,
}

impl ClusterTree {
    pub fn new(root_block: u64) -> Self {
        Self {
            btree: BTree::new(root_block, CLUSTER_TREE_NODE_TYPE),
        }
    }

    pub fn init_empty(ctx: &mut TxContext, root_block: u64) -> Result<()> {
        BTree::<u64, ClusterValue>::init_empty(ctx, root_block, CLUSTER_TREE_NODE_TYPE)
    }

    pub fn get(&self, ctx: &mut TxContext, cluster_idx: u64) -> Result<Option<ClusterValue>> {
        self.btree.lookup(ctx, &cluster_idx)
    }

    pub fn put<F>(
        &mut self,
        ctx: &mut TxContext,
        cluster_idx: u64,
        value: ClusterValue,
        allocate: &mut F,
    ) -> Result<()>
    where
        F: FnMut(&mut TxContext) -> Result<u64>,
    {
        self.btree.insert(ctx, cluster_idx, value, allocate)
    }

    pub fn remove(&mut self, ctx: &mut TxContext, cluster_idx: u64) -> Result<bool> {
        self.btree.remove(ctx, &cluster_idx)
    }

    pub fn iter(&self, ctx: &mut TxContext) -> Result<Vec<(u64, ClusterValue)>> {
        self.btree.iter_all(ctx)
    }

    /// Total physical blocks consumed by this inode's clusters.
    pub fn total_physical_blocks(&self, ctx: &mut TxContext) -> Result<u64> {
        Ok(self.iter(ctx)?.iter().map(|(_, v)| v.physical_blocks).sum())
    }

    /// Free every cluster's physical extent (inode deletion).
    pub fn free_all(&mut self, ctx: &mut TxContext, bg_desc: &BlockGroupDescriptor) -> Result<()> {
        let entries = self.iter(ctx)?;
        for (_idx, v) in entries {
            Allocator::free_extents(ctx, bg_desc, v.physical_start, v.physical_blocks)?;
        }
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Decompressed-cluster LRU: without it, a 4 KiB read inside a 128 KiB
// cluster would decompress the whole cluster every time (32x
// amplification). 16 clusters = 2 MiB.
// ---------------------------------------------------------------------------
fn cluster_cache() -> &'static moka::sync::Cache<(u64, u64), Arc<Vec<u8>>> {
    static CACHE: OnceLock<moka::sync::Cache<(u64, u64), Arc<Vec<u8>>>> = OnceLock::new();
    CACHE.get_or_init(|| moka::sync::Cache::builder().max_capacity(16).build())
}

fn cache_key(ino: u64, cluster_idx: u64) -> (u64, u64) {
    (ino, cluster_idx)
}

fn invalidate_cluster(ino: u64, cluster_idx: u64) {
    cluster_cache().invalidate(&cache_key(ino, cluster_idx));
}

/// Read one cluster's logical bytes (up to `expected_len`, which is
/// CLUSTER_BYTES or the final partial cluster's length). A missing
/// cluster entry is a hole and reads as zeros.
pub fn read_cluster(
    ctx: &mut TxContext,
    inode: &Inode,
    cluster_idx: u64,
    expected_len: usize,
) -> Result<Vec<u8>> {
    if expected_len == 0 || expected_len > CLUSTER_BYTES as usize {
        return Err(Error::new(
            ErrorKind::InvalidInput,
            "bad cluster read length",
        ));
    }
    let root = inode.spill_extent_root;
    if root == 0 {
        return Ok(vec![0u8; expected_len]); // untouched cluster = hole
    }
    let tree = ClusterTree::new(root);
    let entry = match tree.get(ctx, cluster_idx)? {
        Some(v) => v,
        None => return Ok(vec![0u8; expected_len]),
    };

    // Read the extent's blocks.
    let mut raw = Vec::with_capacity((entry.physical_blocks as usize) * BLOCK_SIZE);
    let mut block_buf = [0u8; BLOCK_SIZE];
    for b in 0..entry.physical_blocks {
        ctx.read_block(entry.physical_start + b, &mut block_buf)?;
        raw.extend_from_slice(&block_buf);
    }
    let payload = &raw[..entry.compressed_bytes.min(raw.len() as u64) as usize];

    let logical: Vec<u8> = if entry.is_raw != 0 {
        payload.to_vec()
    } else {
        crate::fs::compression::zstd_decompress(payload)?
    };
    if logical.len() != expected_len {
        // The final cluster's stored length must match its logical
        // length; anything else is on-disk inconsistency.
        return Err(Error::new(
            ErrorKind::InvalidData,
            format!(
                "cluster {} decompressed to {} bytes, expected {}",
                cluster_idx,
                logical.len(),
                expected_len
            ),
        ));
    }
    Ok(logical)
}

/// Read a cluster through the LRU (hot read path).
pub fn read_cluster_cached(
    ctx: &mut TxContext,
    inode: &Inode,
    cluster_idx: u64,
    expected_len: usize,
) -> Result<Arc<Vec<u8>>> {
    let key = cache_key(inode.ino, cluster_idx);
    if let Some(hit) = cluster_cache().get(&key) {
        if hit.len() == expected_len {
            return Ok(hit);
        }
    }
    let data = Arc::new(read_cluster(ctx, inode, cluster_idx, expected_len)?);
    cluster_cache().insert(key, data.clone());
    Ok(data)
}

/// Write one cluster: compress `logical`, allocate a fresh physical
/// extent sized to the payload, write it, free the previous extent,
/// and update the tree. Returns the physical blocks consumed.
pub fn write_cluster(
    ctx: &mut TxContext,
    bg_desc: &BlockGroupDescriptor,
    blocks_per_group: u32,
    inode: &mut Inode,
    cluster_idx: u64,
    logical: &[u8],
    level: i32,
) -> Result<u64> {
    if logical.is_empty() || logical.len() > CLUSTER_BYTES as usize {
        return Err(Error::new(
            ErrorKind::InvalidInput,
            "bad cluster write length",
        ));
    }
    if inode.encryption_algo != 0 {
        return Err(Error::new(
            ErrorKind::Unsupported,
            "compression + encryption on one inode is not supported in this phase",
        ));
    }

    // Compress; fall back to raw storage when compression does not
    // actually save at least one block.
    let compressed = crate::fs::compression::zstd_compress_at_level(logical, level);
    let (payload, is_raw): (Vec<u8>, u8) =
        if (compressed.len() as u64) + BLOCK_SIZE as u64 >= logical.len() as u64 {
            (logical.to_vec(), 1)
        } else {
            (compressed, 0)
        };

    let payload_blocks = (payload.len() as u64).div_ceil(BLOCK_SIZE as u64);

    // Ensure the cluster tree exists.
    if inode.spill_extent_root == 0 {
        let root = Allocator::allocate_extents_meta(ctx, bg_desc, blocks_per_group, 1)?;
        ClusterTree::init_empty(ctx, root)?;
        inode.spill_extent_root = root;
    }
    let old = ClusterTree::new(inode.spill_extent_root).get(ctx, cluster_idx)?;

    // Allocate the new extent (data zone: hinted/frontier allocation
    // from Phase 1). On failure the old extent is still intact.
    let physical_start = Allocator::allocate_extents_hinted(
        ctx,
        bg_desc,
        blocks_per_group,
        payload_blocks,
        old.map(|o| o.physical_start).unwrap_or(0),
    )?;

    // Write the payload blocks (last block zero-padded).
    let mut block_buf = [0u8; BLOCK_SIZE];
    for b in 0..payload_blocks {
        let start = (b as usize) * BLOCK_SIZE;
        let end = ((b as usize) + 1) * BLOCK_SIZE;
        let chunk = &payload[start..end.min(payload.len())];
        block_buf[..chunk.len()].copy_from_slice(chunk);
        // The rest of block_buf stays zeroed from the previous
        // iteration? No -- explicit zero fill each time:
        for z in block_buf[chunk.len()..].iter_mut() {
            *z = 0;
        }
        ctx.write_block(physical_start + b, &block_buf)?;
    }

    // Free the old extent AFTER the new one is fully written.
    if let Some(o) = old {
        Allocator::free_extents(ctx, bg_desc, o.physical_start, o.physical_blocks)?;
    }

    let mut allocate = |c: &mut TxContext| {
        // Cluster-tree node splits are metadata.
        Allocator::allocate_extents_meta(c, bg_desc, blocks_per_group, 1)
    };
    let tree_root = inode.spill_extent_root;
    let mut tree = ClusterTree::new(tree_root);
    tree.put(
        ctx,
        cluster_idx,
        ClusterValue {
            physical_start,
            physical_blocks: payload_blocks,
            compressed_bytes: payload.len() as u64,
            algo: crate::common::constants::COMPRESSION_ZSTD,
            level: level.clamp(0, 255) as u8,
            is_raw,
            padding: [0; 5],
        },
        &mut allocate,
    )?;

    invalidate_cluster(inode.ino, cluster_idx);
    Ok(payload_blocks)
}

/// Drop every cluster at or beyond `first_to_drop`, freeing their
/// extents (truncate support).
pub fn drop_clusters_from(
    ctx: &mut TxContext,
    bg_desc: &BlockGroupDescriptor,
    inode: &mut Inode,
    first_to_drop: u64,
) -> Result<()> {
    if inode.spill_extent_root == 0 {
        return Ok(());
    }
    let tree_root = inode.spill_extent_root;
    let mut tree = ClusterTree::new(tree_root);
    let entries = tree.iter(ctx)?;
    for (idx, v) in entries {
        if idx >= first_to_drop {
            Allocator::free_extents(ctx, bg_desc, v.physical_start, v.physical_blocks)?;
            tree.remove(ctx, idx)?;
            invalidate_cluster(inode.ino, idx);
        }
    }
    Ok(())
}
