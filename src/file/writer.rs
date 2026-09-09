use crate::allocator::bitmap::Allocator;
use crate::ondisk::serialization::{
    BlockGroupDescriptor, Extent, Inode, BLOCK_SIZE, MAX_INLINE_EXTENTS,
};
use crate::transaction::transaction::TxContext;
use std::cmp::min;
use std::collections::HashMap;
use std::io::{Error, ErrorKind, Result};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering as AtomicOrdering};
use std::sync::Mutex;
use std::sync::OnceLock;

use crate::integrity::algorithms::{calculate_checksum, verify_checksum, ChecksumAlgorithm};
use crate::integrity::bad_blocks::BadBlockManager;
use crate::integrity::checksum_tree::{ChecksumTree, ChecksumTreeKey, ChecksumTreeValue};
use crate::security::block_cipher::{self, BlockCipherContext};

pub struct FileManager;

// ---------------------------------------------------------------------------
// Phase 1 readahead: Markov-chain prediction + a physical-block LRU.
//
// Wiring per the plan (optimizer::predictor::PredictiveReadEngine):
// every block read records the (previous, current) logical-block
// transition; when the engine's confidence for the next block exceeds
// its threshold, the predicted block is prefetched into the LRU so the
// following read hits memory instead of the (tx-buffered or physical)
// block device. Writes invalidate the LRU entry for the physical block
// they touch, so stale data can never be served.
//
// A kill switch (LFS_READAHEAD=0) exists because the measured effect on
// this tx-buffered harness is NEGATIVE (see docs/benchmarks.md): the
// bookkeeping costs more than the hits save when reads are already
// served from the transaction's dirty map. On a real mount with cold
// reads, the tradeoff differs. Default ON, honestly reported either way.
// ---------------------------------------------------------------------------

static READAHEAD_ENGINE: OnceLock<crate::optimizer::predictor::PredictiveReadEngine> =
    OnceLock::new();
static READAHEAD_CACHE: OnceLock<moka::sync::Cache<u64, std::sync::Arc<Vec<u8>>>> = OnceLock::new();
static READAHEAD_LAST: OnceLock<Mutex<HashMap<u64, u64>>> = OnceLock::new();
// Default OFF: measured -48%..-51% on every read pattern in the
// lfs_ioperf harness (see docs/benchmarks.md). The bookkeeping
// (per-read LRU insert with a 4 KiB copy, Markov map updates, prefetch
// reads that are already tx-buffered) costs far more than the hits
// save when reads are served from the transaction's dirty map. On a
// real mount with cold reads the tradeoff may differ; enable with
// LFS_READAHEAD=1 or set_readahead_enabled(true).
static READAHEAD_ENABLED: AtomicBool = AtomicBool::new(false);

/// G1 (3.2): block deduplication gate, default OFF (ZFS's own
/// posture: the index costs space and every fresh block pays one
/// probe). Enable with LFS_DEDUP=1 and a filesystem whose superblock
/// carries a nonzero `dedupe_tree_root`.
static DEDUP_ENABLED: AtomicBool = AtomicBool::new(false);

fn dedup_enabled() -> bool {
    DEDUP_ENABLED.load(AtomicOrdering::Relaxed)
}

/// Test/benchmark override for the dedup gate (the env-var path is
/// mount-time; tests toggle it directly). Gate safety: callers that
/// pass `dedupe_tree_root = 0` are unaffected by this flag.
///
/// Counted rather than boolean (3.3): the lib test suite runs tests in
/// parallel threads that share this process global. Two tests that
/// each enable dedup and each reset it on teardown used to race --
/// one teardown flipped the gate OFF while the other test was mid-
/// write, and its second write silently skipped the dedup probe (a
/// one-in-hundreds flake, observed once in the 3.3 full run). With a
/// guard count, concurrent guarded users are independent: the gate is
/// on while ANY guard is held.
pub fn set_dedup_enabled(enabled: bool) {
    if enabled {
        DEDUP_GUARDS.fetch_add(1, AtomicOrdering::AcqRel);
        DEDUP_ENABLED.store(true, AtomicOrdering::Relaxed);
    } else {
        let prev = DEDUP_GUARDS.fetch_sub(1, AtomicOrdering::AcqRel);
        if prev <= 1 {
            DEDUP_ENABLED.store(false, AtomicOrdering::Relaxed);
        }
    }
}

/// Number of active `set_dedup_enabled(true)` guards.
static DEDUP_GUARDS: AtomicU64 = AtomicU64::new(0);

/// Initialize the dedup default from the environment (mount/startup).
pub fn init_dedup_from_env() {
    let on = std::env::var("LFS_DEDUP").map(|v| v == "1" || v.eq_ignore_ascii_case("on")).unwrap_or(false);
    DEDUP_ENABLED.store(on, AtomicOrdering::Relaxed);
}

fn readahead_enabled() -> bool {
    READAHEAD_ENABLED.load(AtomicOrdering::Relaxed)
}

/// Initialize the readahead default from the environment (called once
/// at mount/startup; tests and benchmarks can call
/// set_readahead_enabled directly).
pub fn init_readahead_from_env() {
    let on = std::env::var("LFS_READAHEAD")
        .map(|v| v != "0")
        .unwrap_or(false);
    READAHEAD_ENABLED.store(on, AtomicOrdering::Relaxed);
}

/// Flip readahead on/off (used by mount options / benchmarks / tests).
pub fn set_readahead_enabled(enabled: bool) {
    READAHEAD_ENABLED.store(enabled, AtomicOrdering::Relaxed);
}

fn readahead_engine() -> &'static crate::optimizer::predictor::PredictiveReadEngine {
    READAHEAD_ENGINE.get_or_init(crate::optimizer::predictor::PredictiveReadEngine::new)
}

fn readahead_cache() -> &'static moka::sync::Cache<u64, std::sync::Arc<Vec<u8>>> {
    READAHEAD_CACHE.get_or_init(|| moka::sync::Cache::builder().max_capacity(1024).build())
}

fn readahead_last() -> &'static Mutex<HashMap<u64, u64>> {
    READAHEAD_LAST.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Consult the readahead LRU for a physical block. Returns and clones
/// into `buf` on hit.
fn readahead_try_fill(physical_block: u64, buf: &mut [u8]) -> bool {
    if !readahead_enabled() {
        return false;
    }
    if let Some(hit) = readahead_cache().get(&physical_block) {
        if hit.len() == buf.len() {
            buf.copy_from_slice(&hit);
            return true;
        }
    }
    false
}

/// Insert a block's content into the readahead LRU (used by both the
/// prefetch path and normal reads).
fn readahead_note(physical_block: u64, content: &[u8]) {
    if !readahead_enabled() {
        return;
    }
    readahead_cache().insert(physical_block, std::sync::Arc::new(content.to_vec()));
}

/// Drop a physical block from the LRU -- MUST be called on every write
/// to that block or reads could serve stale content.
fn readahead_invalidate(physical_block: u64) {
    readahead_cache().invalidate(&physical_block);
}

/// Record the observed transition (prev -> current) for this inode and
/// prefetch the predicted next block if confidence allows.
fn readahead_record_and_prefetch(
    ctx: &mut TxContext,
    inode: &Inode,
    logical_block: u64,
    current_physical: u64,
    current_disk_bytes: &[u8],
) {
    if !readahead_enabled() {
        return;
    }
    let engine = readahead_engine();
    {
        let mut last = readahead_last().lock().unwrap();
        if let Some(prev) = last.get(&inode.ino).copied() {
            engine.record_sequence(prev, logical_block);
        }
        last.insert(inode.ino, logical_block);
    }
    // Keep the current block cached so a re-read of it (common for
    // small random reads within one page) is cheap.
    readahead_note(current_physical, current_disk_bytes);
    if let Some(next_logical) = engine.predict_next(logical_block) {
        if next_logical != logical_block {
            if let Ok(next_phys) = FileManager::get_physical_block(ctx, inode, next_logical) {
                if next_phys != 0 && readahead_cache().get(&next_phys).is_none() {
                    let mut prefetch_buf = vec![0u8; BLOCK_SIZE];
                    if ctx.read_block(next_phys, &mut prefetch_buf).is_ok() {
                        readahead_note(next_phys, &prefetch_buf);
                    }
                }
            }
        }
    }
}

impl FileManager {
    pub fn read_file(
        ctx: &mut TxContext,
        checksum_tree_root: u64,
        bad_blocks_root: u64,
        cctx: &BlockCipherContext,
        inode: &mut Inode,
        offset: u64,
        size: u64,
    ) -> Result<Vec<u8>> {
        if offset >= inode.size {
            return Ok(Vec::new());
        }
        // Phase 4: cluster-granular read for compressed inodes.
        if inode.compression_algo != 0 {
            return Self::read_file_cluster(ctx, inode, offset, size);
        }

        let read_size = min(size, inode.size - offset);
        let mut data = vec![0u8; read_size as usize];
        let mut data_pos = 0;
        let csum_tree = if checksum_tree_root != 0 {
            Some(ChecksumTree::new(checksum_tree_root))
        } else {
            None
        };
        let mut current_offset = offset;

        while data_pos < read_size {
            let logical_block = current_offset / BLOCK_SIZE as u64;
            let block_offset = (current_offset % BLOCK_SIZE as u64) as usize;

            let physical_block = Self::get_physical_block(ctx, inode, logical_block)?;
            let mut buf = [0u8; BLOCK_SIZE];

            let chunk_size = min((BLOCK_SIZE - block_offset) as u64, read_size - data_pos) as usize;

            if physical_block != 0 {
                if !readahead_try_fill(physical_block, &mut buf) {
                    ctx.read_block(physical_block, &mut buf)?;
                }
                // Phase 1 readahead: record the transition and prefetch
                // the predicted next block (no-op when disabled).
                readahead_record_and_prefetch(ctx, inode, logical_block, physical_block, &buf);

                // Verify checksum -- always against the on-disk bytes
                // (post-compression/encryption if active), since that's
                // what could actually get corrupted on the physical medium.
                if let Some(ref tree) = csum_tree {
                    let key = ChecksumTreeKey {
                        object_id: inode.ino,
                        logical_block,
                    };
                    if let Ok(Some(val)) = tree.lookup_checksum(ctx, &key) {
                        let algo = ChecksumAlgorithm::from_u8(val.algorithm_id);
                        if !verify_checksum(algo, &buf, &val.checksum_bytes) {
                            // Corruption detected!
                            eprintln!(
                                "CORRUPTION DETECTED: Inode {}, Logical Block {}",
                                inode.ino, logical_block
                            );
                            crate::debug::tracing::log_corruption_detected(
                                inode.ino,
                                logical_block,
                            );
                            if bad_blocks_root != 0 {
                                let mut bb_mgr = BadBlockManager::new(bad_blocks_root);
                                let mut dummy_allocator = |_ctx: &mut TxContext| -> Result<u64> {
                                    Err(Error::other(
                                        "Should not allocate during bad block marking in read",
                                    ))
                                };
                                let _ = bb_mgr.mark_bad_block(
                                    ctx,
                                    physical_block,
                                    inode.ino,
                                    &mut dummy_allocator,
                                );
                            }
                            // Do not return corrupted data
                            return Err(Error::new(
                                ErrorKind::InvalidData,
                                "Checksum mismatch on read",
                            ));
                        }
                    }
                }

                // Phase 1 (buffer pooling / zero-copy): the old code
                // materialized a full `Vec<u8>` plaintext copy of every
                // block (`plain = buf.to_vec()`), even when no
                // compression/encryption was active -- a heap allocation
                // plus a 4 KiB copy per block read. The cipher-inactive
                // path now copies straight from the stack buffer into
                // the caller's slice; only the cipher-active path still
                // pays for a decoded Vec.
                if cctx.is_active() {
                    let plain = block_cipher::decode_block(ctx, cctx, physical_block, &buf)?;
                    data[data_pos as usize..data_pos as usize + chunk_size]
                        .copy_from_slice(&plain[block_offset..block_offset + chunk_size]);
                } else {
                    data[data_pos as usize..data_pos as usize + chunk_size]
                        .copy_from_slice(&buf[block_offset..block_offset + chunk_size]);
                }
            } else {
                // Hole: `data` was allocated zeroed, so a hole's bytes
                // are already correct without touching anything (the
                // old code copied an all-zero `plain` Vec over them).
            }

            data_pos += chunk_size as u64;
            current_offset += chunk_size as u64;
        }

        Ok(data)
    }

    pub fn write_file(
        ctx: &mut TxContext,
        bg_desc: &BlockGroupDescriptor,
        blocks_per_group: u32,
        checksum_tree_root: u64,
        refcount_tree_root: u64,
        dedupe_tree_root: u64,
        cctx: &BlockCipherContext,
        inode: &mut Inode,
        offset: u64,
        data: &[u8],
    ) -> Result<()> {
        // Phase 4: compressed inodes operate at CLUSTER granularity.
        // (Also the honest place to refuse the unsupported
        // compression+encryption combination.)
        if inode.compression_algo != 0 {
            if inode.encryption_algo != 0 {
                return Err(Error::new(
                    ErrorKind::Unsupported,
                    "compression + encryption on one inode is not supported in this phase",
                ));
            }
            return Self::write_file_cluster(ctx, bg_desc, blocks_per_group, inode, offset, data);
        }
        let mut data_pos = 0;
        let mut current_offset = offset;

        // The checksum-tree handle lives for the WHOLE call (Phase 3.2):
        // constructing it per block (the old `store_block_checksum`
        // pattern) threw away the B-tree fast-append cache after every
        // single insert, making every insert pay the full root descent.
        // One handle means the per-block inserts share the rightmost-
        // leaf cache: the first block descends, the rest append in one
        // node read + one node write. (Ascending keys are exactly the
        // fixed-ino, ascending-logical-block pattern.)
        let mut csum_tree = if checksum_tree_root != 0 {
            Some(ChecksumTree::new(checksum_tree_root))
        } else {
            None
        };

        // Phase 1 (locality): the preferred allocation start for this
        // file -- its last physical block + 1. Computed ONCE per call
        // (a spill-tree descent per block would cost more than the
        // scan it saves) and then advanced locally as we allocate.
        let mut locality_hint: Option<u64> = Self::last_physical_end(ctx, inode).unwrap_or(None);

        // Phase 1 (speculative extent sizing): an append (offset ==
        // current size) is provably conflict-free for a single
        // call-level allocation -- every mapped block of an inode is
        // always < ceil(size / BLOCK_SIZE) (writes map only the blocks
        // they cover and advance size past them; truncate frees beyond
        // the new size; punch-holes unmap), so the blocks this call
        // will newly map cannot already be mapped. We therefore
        // allocate ONE speculative run for the whole call at the first
        // unmapped block, sized by allocator::extents::size_for_request
        // (base + 25% growth), marking only the blocks this call
        // actually writes. The unmarked tail stays free and is picked
        // up by the next append (the frontier cursor lands inside it),
        // letting the extent MERGE across calls -- this is what keeps
        // a sequentially-written file at a handful of extents instead
        // of one extent per block (the baseline's dominant
        // fragmentation source: checksum-tree node allocations
        // interleave with 1-block data allocations, so per-block
        // extents almost never merge).
        let append_write =
            !data.is_empty() && crate::allocator::extents::is_sequential_write(offset, inode.size);

        // G1 (3.2): block deduplication. Off by default (ZFS's own
        // posture -- the probe is one tree descent per fresh block and
        // the index costs space); enabled with LFS_DEDUP=1 plus a
        // nonzero `dedupe_tree_root`. Only fresh FULL-block,
        // cipher-inactive writes participate: a partial write or an
        // encrypted block is unique content by construction.
        let dedup_on = dedupe_tree_root != 0 && !cctx.is_active() && dedup_enabled();

        while data_pos < data.len() {
            let logical_block = current_offset / BLOCK_SIZE as u64;
            let block_offset = (current_offset % BLOCK_SIZE as u64) as usize;

            let mut physical_block =
                Self::get_physical_block(ctx, inode, logical_block).unwrap_or(0);

            // PHASE 6 DATA CoW (completed in 3.2): if this logical block
            // is already mapped AND its physical block is pinned by the
            // refcount coverage tree (a dedup share holds a reference),
            // we must NOT modify it in place. Redirect: allocate a
            // fresh block, copy the old content over, and remap the
            // extent. The pin itself is untouched -- the share keeps
            // the original block, the live file moves on.
            //
            // PHASE 11 (birth generations): snapshots no longer pin.
            // A live snapshot at barrier B protects a block whose
            // BIRTH (the checksum record's generation, i.e. the stamp
            // of the content currently at this logical block) is <=
            // B: the frozen csum view may reference it, so redirect.
            // A block born after B postdates every live snapshot's
            // freeze -- no frozen view can contain this content, so
            // in-place is provably safe. One csum-tree descent per
            // overwritten block, ONLY while snapshots are live
            // (barrier 0 skips it entirely: zero regression on the
            // common path). If the record's phys disagrees with the
            // current mapping (should not happen -- both update in
            // one transaction), be conservative and redirect.
            let barrier = ctx.effective_cow_barrier();
            if physical_block != 0
                && (refcount_tree_root != 0 || barrier > 0)
            {
                let pinned = if refcount_tree_root != 0 {
                    let rc =
                        crate::integrity::refcount::RefCountManager::new(refcount_tree_root);
                    rc.is_pinned(ctx, physical_block)?
                } else {
                    false
                };
                let birth_protected = !pinned
                    && barrier > 0
                    && csum_tree.as_ref().is_some_and(|tree| {
                        let key = ChecksumTreeKey {
                            object_id: inode.ino,
                            logical_block,
                        };
                        tree.lookup_checksum(ctx, &key)
                            .ok()
                            .flatten()
                            .is_some_and(|v| {
                                v.physical_block == physical_block && v.generation <= barrier
                            })
                    });
                if pinned || birth_protected {
                    let new_block =
                        Allocator::allocate_extents(ctx, bg_desc, blocks_per_group, 1)?;
                    // Preserve the old bytes (the RMW branch below
                    // would otherwise read the block we are abandoning).
                    let mut old = [0u8; BLOCK_SIZE];
                    ctx.read_block(physical_block, &mut old)?;
                    let old_plain = if cctx.is_active() {
                        block_cipher::decode_block(ctx, cctx, physical_block, &old)?
                    } else {
                        old.to_vec()
                    };
                    // Copy the original content to the new block FIRST
                    // (identical bytes); the normal write path below
                    // then layers the caller's data on top, so the
                    // partial-block RMW semantics are preserved.
                    if cctx.is_active() {
                        let disk_bytes: Vec<u8> = block_cipher::encode_block(
                            ctx,
                            cctx,
                            new_block,
                            &old_plain,
                            |c| Allocator::allocate_extents(c, bg_desc, blocks_per_group, 1),
                        )?;
                        ctx.write_block_owned(new_block, disk_bytes)?;
                    } else {
                        ctx.write_block(new_block, &old_plain)?;
                    }
                    Self::remap_block(
                        ctx,
                        bg_desc,
                        blocks_per_group,
                        inode,
                        logical_block,
                        new_block,
                    )?;
                    readahead_invalidate(physical_block);
                    physical_block = new_block;
                }
            }

            if physical_block == 0 {
                // G1 (3.2) dedup probe, BEFORE allocating: a fresh
                // FULL-block, cipher-inactive write with dedup on first
                // looks for an existing block with identical content.
                // On a verified hit we map onto the existing block
                // (share), pin it against in-place modification by
                // EITHER owner, and skip the data write entirely.
                if dedup_on
                    && block_offset == 0
                    && data.len() - data_pos >= BLOCK_SIZE
                {
                    let hash = crate::fs::dedupe::DeduplicationManager::hash_block(
                        &data[data_pos..data_pos + BLOCK_SIZE],
                    );
                    let dtree = crate::fs::dedupe::DedupeTree::new(dedupe_tree_root);
                    if let Some(rec) = dtree.find(ctx, hash)? {
                        // VERIFY-ON-SHARE: trust the tree only after
                        // re-reading and re-hashing the block. A stale
                        // index entry (crash between record and commit,
                        // or a freed-and-reused block) must degrade to
                        // a normal write, never to a wrong share.
                        let mut probe = [0u8; BLOCK_SIZE];
                        let probe_ok = ctx
                            .read_block(rec.physical_block, &mut probe)
                            .map(|_| ())
                            .is_ok();
                        if probe_ok
                            && crate::fs::dedupe::DeduplicationManager::hash_block(&probe)
                                == hash
                        {
                            Self::add_extent(
                                ctx,
                                bg_desc,
                                blocks_per_group,
                                inode,
                                logical_block,
                                rec.physical_block,
                                1,
                            )?;
                            let mut alloc_meta = |c: &mut TxContext| {
                                Allocator::allocate_extents_meta(
                                    c,
                                    bg_desc,
                                    blocks_per_group,
                                    1,
                                )
                            };
                            let mut dtree_mut =
                                crate::fs::dedupe::DedupeTree::new(dedupe_tree_root);
                            dtree_mut.increment_ref(ctx, hash, &mut alloc_meta)?;
                            // Shared blocks are pinned: an overwrite by
                            // EITHER inode must copy-on-write (the same
                            // protection snapshots get).
                            if refcount_tree_root != 0 {
                                let mut rc = crate::integrity::refcount::RefCountManager::new(
                                    refcount_tree_root,
                                );
                                if rc.coverage_at(ctx, rec.physical_block)? == 0 {
                                    rc.pin_range(
                                        ctx,
                                        rec.physical_block,
                                        1,
                                        &mut alloc_meta,
                                    )?;
                                }
                            }
                            // Checksum records are per-(inode, logical)
                            // and may point at a shared physical block.
                            if let Some(tree) = csum_tree.as_mut() {
                                Self::store_block_checksum(
                                    ctx,
                                    tree,
                                    bg_desc,
                                    blocks_per_group,
                                    inode.ino,
                                    logical_block,
                                    rec.physical_block,
                                    &probe,
                                )?;
                            }
                            data_pos += BLOCK_SIZE;
                            current_offset += BLOCK_SIZE as u64;
                            continue;
                        }
                    }
                }
                if append_write {
                    // One speculative run for the whole call (see the
                    // comment above for why this is conflict-free).
                    // `mark` = blocks from here to the call's last
                    // block, so the extent covers exactly what this
                    // call writes.
                    let last_block = (offset + data.len() as u64 - 1) / BLOCK_SIZE as u64;
                    let mark = last_block - logical_block + 1;
                    let want = crate::allocator::extents::size_for_request(
                        mark * BLOCK_SIZE as u64,
                        BLOCK_SIZE as u64,
                        true,
                    )
                    .max(mark);
                    physical_block = Allocator::allocate_extents_reserved(
                        ctx,
                        bg_desc,
                        blocks_per_group,
                        want,
                        mark,
                        locality_hint,
                    )?;
                    locality_hint = Some(physical_block + mark);
                    // The extent maps the whole call's range at once;
                    // every remaining block of this call now resolves.
                    Self::add_extent(
                        ctx,
                        bg_desc,
                        blocks_per_group,
                        inode,
                        logical_block,
                        physical_block,
                        mark,
                    )?;
                } else {
                    // Non-append (overwrite / hole fill / random):
                    // baseline one-block-at-a-time behavior.
                    physical_block = match locality_hint {
                        Some(h) => Allocator::allocate_extents_hinted(
                            ctx,
                            bg_desc,
                            blocks_per_group,
                            1,
                            h,
                        )?,
                        None => Allocator::allocate_extents(ctx, bg_desc, blocks_per_group, 1)?,
                    };
                    locality_hint = Some(physical_block + 1);
                    Self::add_extent(
                        ctx,
                        bg_desc,
                        blocks_per_group,
                        inode,
                        logical_block,
                        physical_block,
                        1,
                    )?;
                }
            }

            let mut buf = [0u8; BLOCK_SIZE];

            let chunk_size = min(BLOCK_SIZE - block_offset, data.len() - data_pos);

            // Read-modify-write if partial block
            if chunk_size < BLOCK_SIZE && physical_block != 0 {
                // (The shared-block case was redirected above; a block
                // reaching this branch is exclusively ours.)
                let mut disk_buf = [0u8; BLOCK_SIZE];
                ctx.read_block(physical_block, &mut disk_buf)?;
                if cctx.is_active() {
                    // The existing on-disk bytes are compressed/encrypted;
                    // recover the plaintext before applying a partial
                    // overwrite, or we'd be splicing new plaintext into the
                    // middle of an opaque compressed/encrypted blob.
                    let plain = block_cipher::decode_block(ctx, cctx, physical_block, &disk_buf)?;
                    buf.copy_from_slice(&plain);
                } else {
                    buf = disk_buf;
                }
            }

            buf[block_offset..block_offset + chunk_size]
                .copy_from_slice(&data[data_pos..data_pos + chunk_size]);

            // Apply compression/encryption (no-op if cctx is inactive) to
            // the full plaintext block before it touches disk.
            //
            // Phase 1 (buffer pooling / zero-copy): the cipher-inactive
            // path used to do `buf.to_vec()` -- a fresh 4 KiB heap Vec
            // per block written, immediately copied again by the tx
            // layer. It now hands the stack buffer to `write_block`
            // directly; only the cipher-active path produces a Vec,
            // which is moved into the tx layer via `write_block_owned`
            // so the transaction takes ownership instead of copying.
            if cctx.is_active() {
                let disk_bytes: Vec<u8> =
                    block_cipher::encode_block(ctx, cctx, physical_block, &buf, |c| {
                        Allocator::allocate_extents(c, bg_desc, blocks_per_group, 1)
                    })?;
                readahead_invalidate(physical_block);
                ctx.write_block_owned(physical_block, disk_bytes.clone())?;
                // Checksum over the on-disk (post-transform) bytes.
                if let Some(tree) = csum_tree.as_mut() {
                    Self::store_block_checksum(
                        ctx,
                        tree,
                        bg_desc,
                        blocks_per_group,
                        inode.ino,
                        logical_block,
                        physical_block,
                        &disk_bytes,
                    )?;
                }
            } else {
                readahead_invalidate(physical_block);
                ctx.write_block(physical_block, &buf)?;
                // G1 (3.2): record the freshly written block in the
                // dedup index so later identical content can share it.
                if dedup_on && chunk_size == BLOCK_SIZE && block_offset == 0 {
                    let hash =
                        crate::fs::dedupe::DeduplicationManager::hash_block(&buf);
                    let mut dtree = crate::fs::dedupe::DedupeTree::new(dedupe_tree_root);
                    let mut alloc_meta = |c: &mut TxContext| {
                        Allocator::allocate_extents_meta(c, bg_desc, blocks_per_group, 1)
                    };
                    // insert_new only if absent (an identical block
                    // written twice in one call must not reset the count).
                    if dtree.find(ctx, hash)?.is_none() {
                        dtree.insert_new(ctx, hash, physical_block, &mut alloc_meta)?;
                    }
                }
                if let Some(tree) = csum_tree.as_mut() {
                    Self::store_block_checksum(
                        ctx,
                        tree,
                        bg_desc,
                        blocks_per_group,
                        inode.ino,
                        logical_block,
                        physical_block,
                        &buf,
                    )?;
                }
            }

            data_pos += chunk_size;
            current_offset += chunk_size as u64;
        }

        if offset + data.len() as u64 > inode.size {
            inode.size = offset + data.len() as u64;
        }

        Ok(())
    }

    /// Btrfs-inspired batched multi-run writer:
    /// Processes all drained dirty runs in ONE unified pass, sharing the
    /// `ChecksumTree` handle and `locality_hint` across all runs to amortize
    /// tree descents and preserve spatial disk locality during random 4K writes.
    pub fn write_file_runs(
        ctx: &mut TxContext,
        bg_desc: &BlockGroupDescriptor,
        blocks_per_group: u32,
        checksum_tree_root: u64,
        refcount_tree_root: u64,
        dedupe_tree_root: u64,
        cctx: &BlockCipherContext,
        inode: &mut Inode,
        runs: &[(u64, Vec<u8>)],
    ) -> Result<()> {
        if runs.is_empty() {
            return Ok(());
        }
        if inode.compression_algo != 0 {
            for (start_block, bytes) in runs {
                Self::write_file(
                    ctx,
                    bg_desc,
                    blocks_per_group,
                    checksum_tree_root,
                    refcount_tree_root,
                    dedupe_tree_root,
                    cctx,
                    inode,
                    *start_block * BLOCK_SIZE as u64,
                    bytes,
                )?;
            }
            return Ok(());
        }

        let mut csum_tree = if checksum_tree_root != 0 {
            Some(ChecksumTree::new(checksum_tree_root))
        } else {
            None
        };
        let mut locality_hint: Option<u64> = Self::last_physical_end(ctx, inode).unwrap_or(None);
        let dedup_on = dedupe_tree_root != 0 && !cctx.is_active() && dedup_enabled();
        // Deferred checksum insert map: collect (logical_block → (physical_block, content))
        // during the write loop and flush them to the checksum B-tree in ascending key order
        // afterwards.  Ascending order maximises FastLeaf/RangeLeaf hits; deduplication
        // collapses repeated writes to the same logical block into one insert.
        // Only allocated when the checksum tree is active (avoids the Box cost otherwise).
        let mut deferred_csums: std::collections::BTreeMap<u64, (u64, Box<[u8; BLOCK_SIZE]>)> =
            if checksum_tree_root != 0 {
                std::collections::BTreeMap::new()
            } else {
                std::collections::BTreeMap::new() // empty, will never be inserted into
            };

        for (start_block, data) in runs {
            let offset = *start_block * BLOCK_SIZE as u64;
            let mut data_pos = 0;
            let mut current_offset = offset;
            let append_write = !data.is_empty() && crate::allocator::extents::is_sequential_write(offset, inode.size);

            while data_pos < data.len() {
                let logical_block = current_offset / BLOCK_SIZE as u64;
                let block_offset = (current_offset % BLOCK_SIZE as u64) as usize;

                let mut physical_block =
                    Self::get_physical_block(ctx, inode, logical_block).unwrap_or(0);

                let barrier = ctx.effective_cow_barrier();
                if physical_block != 0 && (refcount_tree_root != 0 || barrier > 0) {
                    let pinned = if refcount_tree_root != 0 {
                        let rc = crate::integrity::refcount::RefCountManager::new(refcount_tree_root);
                        rc.is_pinned(ctx, physical_block)?
                    } else {
                        false
                    };
                    let birth_protected = !pinned
                        && barrier > 0
                        && csum_tree.as_ref().is_some_and(|tree| {
                            let key = ChecksumTreeKey {
                                object_id: inode.ino,
                                logical_block,
                            };
                            tree.lookup_checksum(ctx, &key)
                                .ok()
                                .flatten()
                                .is_some_and(|v| {
                                    v.physical_block == physical_block && v.generation <= barrier
                                })
                        });
                    if pinned || birth_protected {
                        let new_block = Allocator::allocate_extents(ctx, bg_desc, blocks_per_group, 1)?;
                        let mut old = [0u8; BLOCK_SIZE];
                        ctx.read_block(physical_block, &mut old)?;
                        let old_plain = if cctx.is_active() {
                            block_cipher::decode_block(ctx, cctx, physical_block, &old)?
                        } else {
                            old.to_vec()
                        };
                        if cctx.is_active() {
                            let disk_bytes = block_cipher::encode_block(ctx, cctx, new_block, &old_plain, |c| {
                                Allocator::allocate_extents(c, bg_desc, blocks_per_group, 1)
                            })?;
                            ctx.write_block_owned(new_block, disk_bytes)?;
                        } else {
                            ctx.write_block(new_block, &old_plain)?;
                        }
                        Self::remap_block(
                            ctx,
                            bg_desc,
                            blocks_per_group,
                            inode,
                            logical_block,
                            new_block,
                        )?;
                        readahead_invalidate(physical_block);
                        physical_block = new_block;
                    }
                }

                if physical_block == 0 {
                    if dedup_on && block_offset == 0 && data.len() - data_pos >= BLOCK_SIZE {
                        let hash = crate::fs::dedupe::DeduplicationManager::hash_block(
                            &data[data_pos..data_pos + BLOCK_SIZE],
                        );
                        let dtree = crate::fs::dedupe::DedupeTree::new(dedupe_tree_root);
                        if let Some(rec) = dtree.find(ctx, hash)? {
                            let mut probe = [0u8; BLOCK_SIZE];
                            let probe_ok = ctx.read_block(rec.physical_block, &mut probe).is_ok();
                            if probe_ok && crate::fs::dedupe::DeduplicationManager::hash_block(&probe) == hash {
                                Self::add_extent(
                                    ctx,
                                    bg_desc,
                                    blocks_per_group,
                                    inode,
                                    logical_block,
                                    rec.physical_block,
                                    1,
                                )?;
                                let mut alloc_meta = |c: &mut TxContext| {
                                    Allocator::allocate_extents_meta(c, bg_desc, blocks_per_group, 1)
                                };
                                let mut dtree_mut = crate::fs::dedupe::DedupeTree::new(dedupe_tree_root);
                                dtree_mut.increment_ref(ctx, hash, &mut alloc_meta)?;
                                if refcount_tree_root != 0 {
                                    let mut rc = crate::integrity::refcount::RefCountManager::new(refcount_tree_root);
                                    if rc.coverage_at(ctx, rec.physical_block)? == 0 {
                                        rc.pin_range(ctx, rec.physical_block, 1, &mut alloc_meta)?;
                                    }
                                }
                                if csum_tree.is_some() {
                                    let mut checksum_buf = [0u8; BLOCK_SIZE];
                                    checksum_buf.copy_from_slice(&probe);
                                    deferred_csums.insert(logical_block, (rec.physical_block, Box::new(checksum_buf)));
                                }
                                data_pos += BLOCK_SIZE;
                                current_offset += BLOCK_SIZE as u64;
                                continue;
                            }
                        }
                    }
                    if append_write {
                        let last_block = (offset + data.len() as u64 - 1) / BLOCK_SIZE as u64;
                        let mark = last_block - logical_block + 1;
                        let want = crate::allocator::extents::size_for_request(
                            mark * BLOCK_SIZE as u64,
                            BLOCK_SIZE as u64,
                            true,
                        ).max(mark);
                        physical_block = Allocator::allocate_extents_reserved(
                            ctx,
                            bg_desc,
                            blocks_per_group,
                            want,
                            mark,
                            locality_hint,
                        )?;
                        locality_hint = Some(physical_block + mark);
                        Self::add_extent(
                            ctx,
                            bg_desc,
                            blocks_per_group,
                            inode,
                            logical_block,
                            physical_block,
                            mark,
                        )?;
                    } else {
                        physical_block = match locality_hint {
                            Some(h) => Allocator::allocate_extents_hinted(
                                ctx,
                                bg_desc,
                                blocks_per_group,
                                1,
                                h,
                            )?,
                            None => Allocator::allocate_extents(ctx, bg_desc, blocks_per_group, 1)?,
                        };
                        locality_hint = Some(physical_block + 1);
                        Self::add_extent(
                            ctx,
                            bg_desc,
                            blocks_per_group,
                            inode,
                            logical_block,
                            physical_block,
                            1,
                        )?;
                    }
                }

                let mut buf = [0u8; BLOCK_SIZE];
                let chunk_size = min(BLOCK_SIZE - block_offset, data.len() - data_pos);

                if chunk_size < BLOCK_SIZE && physical_block != 0 {
                    let mut disk_buf = [0u8; BLOCK_SIZE];
                    ctx.read_block(physical_block, &mut disk_buf)?;
                    if cctx.is_active() {
                        let plain = block_cipher::decode_block(ctx, cctx, physical_block, &disk_buf)?;
                        buf.copy_from_slice(&plain);
                    } else {
                        buf = disk_buf;
                    }
                }

                buf[block_offset..block_offset + chunk_size].copy_from_slice(&data[data_pos..data_pos + chunk_size]);

                if cctx.is_active() {
                    let disk_bytes: Vec<u8> = block_cipher::encode_block(ctx, cctx, physical_block, &buf, |c| {
                        Allocator::allocate_extents(c, bg_desc, blocks_per_group, 1)
                    })?;
                    readahead_invalidate(physical_block);
                    ctx.write_block_owned(physical_block, disk_bytes.clone())?;
                    // Defer checksum: store encrypted bytes for post-loop batch insert
                    // (same ascending-order / dedup benefit as the plaintext path).
                    if csum_tree.is_some() {
                        let mut checksum_buf = [0u8; BLOCK_SIZE];
                        let copy_len = disk_bytes.len().min(BLOCK_SIZE);
                        checksum_buf[..copy_len].copy_from_slice(&disk_bytes[..copy_len]);
                        deferred_csums.insert(logical_block, (physical_block, Box::new(checksum_buf)));
                    }
                } else {
                    readahead_invalidate(physical_block);
                    ctx.write_block(physical_block, &buf)?;
                    if dedup_on && chunk_size == BLOCK_SIZE && block_offset == 0 {
                        let hash = crate::fs::dedupe::DeduplicationManager::hash_block(&buf);
                        let mut dtree = crate::fs::dedupe::DedupeTree::new(dedupe_tree_root);
                        let mut alloc_meta = |c: &mut TxContext| {
                            Allocator::allocate_extents_meta(c, bg_desc, blocks_per_group, 1)
                        };
                        if dtree.find(ctx, hash)?.is_none() {
                            dtree.insert_new(ctx, hash, physical_block, &mut alloc_meta)?;
                        }
                    }
                    // Deferred checksum: record (logical_block -> (physical, buf)) for
                    // post-loop batch insert in ascending key order.  Overwrites of the
                    // same logical block within this flush are automatically deduplicated;
                    // the last write wins (correct -- it is the final committed content).
                    if csum_tree.is_some() {
                        let mut checksum_buf = [0u8; BLOCK_SIZE];
                        checksum_buf.copy_from_slice(&buf);
                        deferred_csums.insert(logical_block, (physical_block, Box::new(checksum_buf)));
                    }
                }

                data_pos += chunk_size;
                current_offset += chunk_size as u64;
            }

            if offset + data.len() as u64 > inode.size {
                inode.size = offset + data.len() as u64;
            }
        }

        // Flush deferred checksums in ascending logical-block order.
        // Ascending order means every insert is strictly greater than the
        // previous, so the ChecksumTree's FastLeaf cache fires on EVERY
        // insert (no B-tree descent after the first).  This collapses
        // O(N × tree_height) descents to O(1) descents + N leaf appends/
        // overwrites -- the dominant rand-write cost at high IOPS.
        if let Some(ref mut tree) = csum_tree {
            let mut allocate_for_tree = |c: &mut TxContext| {
                Allocator::allocate_extents_meta(c, bg_desc, blocks_per_group, 1)
            };
            for (logical_block, (physical_block, buf)) in &deferred_csums {
                let algo = crate::integrity::algorithms::write_path_algorithm();
                let csum_bytes = crate::integrity::algorithms::calculate_checksum(algo, buf.as_ref());
                let key = ChecksumTreeKey {
                    object_id: inode.ino,
                    logical_block: *logical_block,
                };
                let val = ChecksumTreeValue {
                    physical_block: *physical_block,
                    checksum_bytes: csum_bytes,
                    generation: crate::btree::tree::node_gen_stamp(),
                    algorithm_id: algo as u8,
                    verification_status: 1,
                    padding: [0; 6],
                };
                tree.insert_checksum(ctx, key, val, &mut allocate_for_tree)?;
            }
        }

        Ok(())
    }

    /// Record this block's checksum in the checksum tree. Split out of
    /// `write_file` so both cipher-mode branches share one code path.
    /// The tree handle is owned by the CALLER (constructed once per
    /// `write_file` call, Phase 3.2) so the B-tree fast-append cache
    /// survives across the per-block inserts.
    ///
    /// 3.6 (crypto/format agility): the algorithm is POLICY, not code
    /// -- `LFS_CSUM` selects the write-path digest (xxh64 | crc32c |
    /// sha256 | blake3), and the chosen id is stored PER RECORD in
    /// `ChecksumTreeValue.algorithm_id`, which the read path already
    /// dispatches on. Mixed-algorithm images verify block-by-block;
    /// a volume can be re-pointed at a stronger digest without a
    /// reformat (old records keep verifying under their own ids).
    fn store_block_checksum(
        ctx: &mut TxContext,
        csum_tree: &mut ChecksumTree,
        bg_desc: &BlockGroupDescriptor,
        blocks_per_group: u32,
        ino: u64,
        logical_block: u64,
        physical_block: u64,
        disk_bytes: &[u8],
    ) -> Result<()> {
        let algo = crate::integrity::algorithms::write_path_algorithm();
        let csum_bytes = calculate_checksum(algo, disk_bytes);
        let key = ChecksumTreeKey {
            object_id: ino,
            logical_block,
        };
        let val = ChecksumTreeValue {
            physical_block,
            checksum_bytes: csum_bytes,
            // Phase 11 (birth generations): the BIRTH STAMP of this
            // content at this logical block -- the node-gen stamp at
            // write time. A snapshot at barrier B protects (redirects,
            // retains) blocks whose birth <= B; a block born > B
            // postdates every live snapshot's freeze, so writing it in
            // place is provably safe. Refreshed on EVERY write of the
            // block (in-place or redirected): the record always says
            // when the content it describes came into existence. This
            // is what lets `create_snapshot` skip the O(extent runs)
            // pin walk entirely on checksummed images.
            generation: crate::btree::tree::node_gen_stamp(),
            algorithm_id: algo as u8,
            verification_status: 1, // Verified (just written)
            padding: [0; 6],
        };
        let mut allocate_for_tree = |c: &mut TxContext| {
            // Metadata (tree nodes) allocates from the group's end zone
            // so it does not puncture the data frontier.
            Allocator::allocate_extents_meta(c, bg_desc, blocks_per_group, 1)
        };
        // Phase 0 bug fix: this insert used to be
        // `let _ = ...insert_checksum(..., &mut dummy_allocator)`
        // with an allocator that ERRORS on any allocation.
        // Once the checksum tree outgrew its root leaf (~85
        // entries), every subsequent insert failed silently --
        // data was written but integrity records vanished, with
        // no error and no log line. Propagate the error and use
        // a real allocator so the tree can actually split.
        csum_tree.insert_checksum(ctx, key, val, &mut allocate_for_tree)
    }

    /// Phase 11 (birth generations): free the physical range
    /// `ps..ps+len` (the logical range `logical_start..+len` of
    /// `ino`) birth-aware. Blocks whose csum-record birth is <= the
    /// live barrier are retained (a snapshot's frozen view may
    /// reference them; `delete_snapshot` reclaims); blocks born after
    /// every live snapshot's freeze are freed, subject to the dedup
    /// pin check. Barrier 0 (no snapshots) or a pin-mode image (csum
    /// tree off) reduces to exactly the 3.2-3.4 free: pin check, then
    /// free. Adjacent freeable blocks coalesce into one bitmap call.
    ///
    /// A missing csum record for a block in a checksummed image is
    /// treated as "ancient" (birth 0, retained) -- conservative in the
    /// safe direction, same posture as the pin walk's `.unwrap_or(true)`.
    fn free_range_birth_aware(
        ctx: &mut TxContext,
        bg_desc: &BlockGroupDescriptor,
        refcount_tree_root: u64,
        checksum_tree_root: u64,
        ino: u64,
        logical_start: u64,
        ps: u64,
        len: u64,
    ) -> Result<()> {
        let free_pinned_checked = |ctx: &mut TxContext, start: u64, n: u64| -> Result<()> {
            if refcount_tree_root != 0 {
                let rc = crate::integrity::refcount::RefCountManager::new(refcount_tree_root);
                if rc.is_range_pinned(ctx, start, n).unwrap_or(true) {
                    return Ok(()); // a dedup share holds this range
                }
            }
            Allocator::free_extents(ctx, bg_desc, start, n)
        };

        let barrier = ctx.effective_cow_barrier();
        if barrier == 0 || checksum_tree_root == 0 || len == 0 {
            return free_pinned_checked(ctx, ps, len);
        }

        // Snapshots live + checksummed: per-block birth scan.
        let tree = ChecksumTree::new(checksum_tree_root);
        let mut free_run: Option<(u64, u64)> = None; // (start, len)
        for i in 0..len {
            let logical = logical_start + i;
            let key = ChecksumTreeKey {
                object_id: ino,
                logical_block: logical,
            };
            let birth = tree
                .lookup_checksum(ctx, &key)?
                .map(|v| v.generation)
                .unwrap_or(0);
            if birth > barrier {
                free_run = match free_run {
                    Some((s, l)) => Some((s, l + 1)),
                    None => Some((ps + i, 1)),
                };
            } else if let Some((s, l)) = free_run.take() {
                free_pinned_checked(ctx, s, l)?;
            }
        }
        if let Some((s, l)) = free_run {
            free_pinned_checked(ctx, s, l)?;
        }
        Ok(())
    }

    pub fn truncate_file(
        ctx: &mut TxContext,
        bg_desc: &BlockGroupDescriptor,
        blocks_per_group: u32,
        refcount_tree_root: u64,
        checksum_tree_root: u64,
        inode: &mut Inode,
        new_size: u64,
    ) -> Result<()> {
        if inode.compression_algo != 0 {
            return Self::truncate_file_cluster(ctx, bg_desc, blocks_per_group, inode, new_size);
        }
        if new_size >= inode.size {
            // Expansion is handled via write or explicit fallocate, ignore for now
            inode.size = new_size;
            return Ok(());
        }

        let new_blocks = new_size.div_ceil(BLOCK_SIZE as u64);

        // Phase 11 (birth generations): freeing is birth-aware. A
        // block whose csum-record birth is <= the live barrier may be
        // referenced by a snapshot's frozen view -- RETAIN it (the
        // reclaim happens in `delete_snapshot`'s walk); a block born
        // after every live snapshot's freeze is safe to free. Dedup
        // pins still protect their ranges on top (unchanged). With no
        // snapshots live (barrier 0) or a pin-mode image (csum tree
        // off) this is exactly the 3.2-3.4 behavior: pin check, then
        // free. The per-block birth scan runs ONLY under a live
        // snapshot -- the common truncate pays nothing extra.
        //
        // Cost honesty: the scan is one csum-tree descent per freed
        // block (the records are keyed (ino, logical)). A large
        // truncate under a live snapshot is O(blocks freed) lookups.
        // Rare, correctness-critical, documented in phase11_txg_birth.md.
        let free_birth_aware = |ctx: &mut TxContext,
                                logical_start: u64,
                                ps: u64,
                                len: u64|
         -> Result<()> {
            Self::free_range_birth_aware(
                ctx,
                bg_desc,
                refcount_tree_root,
                checksum_tree_root,
                inode.ino,
                logical_start,
                ps,
                len,
            )
        };

        // Built as a fresh, compacted list rather than zeroing extents in
        // place: extents are not guaranteed to be stored in increasing
        // logical_start order (add_extent appends to the next free slot
        // when it can't merge with an existing one), so zeroing a freed
        // extent without compacting the array could leave a still-valid
        // extent later in the array masked by an earlier, now-empty slot
        // once extent_count shrinks past it.
        let mut surviving: Vec<Extent> = Vec::with_capacity(inode.extent_count as usize);
        for i in 0..inode.extent_count as usize {
            let extent = inode.extents[i];

            if extent.logical_start >= new_blocks {
                // Free the whole extent (birth/pin aware -- see above)
                free_birth_aware(ctx, extent.logical_start, extent.physical_start, extent.length)?;
            } else if extent.logical_start + extent.length > new_blocks {
                // Partial truncate of extent
                let keep_blocks = new_blocks - extent.logical_start;
                let free_blocks = extent.length - keep_blocks;

                free_birth_aware(
                    ctx,
                    extent.logical_start + keep_blocks,
                    extent.physical_start + keep_blocks,
                    free_blocks,
                )?;
                surviving.push(Extent {
                    logical_start: extent.logical_start,
                    physical_start: extent.physical_start,
                    length: keep_blocks,
                });
            } else {
                surviving.push(extent);
            }
        }

        // Extent spooling (Phase 0): apply the same truncation to the
        // spilled extents in the per-inode ExtentTree. Trimmed/removed
        // entries are re-inserted with their surviving length so the
        // tree never holds stale mappings.
        if inode.spill_extent_root != 0 {
            let mut tree = crate::extents::tree::ExtentTree::new(inode.spill_extent_root);
            let spilled = tree.iter_extents(ctx)?;
            let mut allocate =
                |c: &mut TxContext| Allocator::allocate_extents(c, bg_desc, blocks_per_group, 1);
            let mut any_kept = false;
            for (log_start, val) in spilled {
                let extent = Extent {
                    logical_start: log_start,
                    physical_start: val.physical_start,
                    length: val.length,
                };
                if extent.logical_start >= new_blocks {
                    // Free the whole extent (birth/pin aware)
                    free_birth_aware(
                        ctx,
                        extent.logical_start,
                        extent.physical_start,
                        extent.length,
                    )?;
                } else if extent.logical_start + extent.length > new_blocks {
                    // Partial truncate of extent
                    let keep_blocks = new_blocks - extent.logical_start;
                    let free_blocks = extent.length - keep_blocks;
                    free_birth_aware(
                        ctx,
                        extent.logical_start + keep_blocks,
                        extent.physical_start + keep_blocks,
                        free_blocks,
                    )?;
                    // Re-insert with the surviving length (remove first
                    // because the key stays the same but the value changes).
                    // Phase 9: the spill tree is frozen while snapshots
                    // are live -- removal path-copies before mutating.
                    tree.remove_with_alloc(ctx, &log_start, &mut allocate)?;
                    tree.insert(
                        ctx,
                        log_start,
                        extent.physical_start,
                        keep_blocks,
                        &mut allocate,
                    )?;
                    any_kept = true;
                } else {
                    any_kept = true;
                }
            }
            let _ = any_kept; // the tree stays valid even when empty
        }
        let extents_len = inode.extents.len();
        for (i, e) in surviving.iter().enumerate() {
            inode.extents[i] = *e;
        }
        for slot in inode
            .extents
            .iter_mut()
            .take(extents_len)
            .skip(surviving.len())
        {
            *slot = Extent {
                logical_start: 0,
                physical_start: 0,
                length: 0,
            };
        }
        inode.extent_count = surviving.len() as u16;
        inode.size = new_size;
        Ok(())
    }

    // ------------------------------------------------------------------
    // Phase 4: cluster-granular operations for compressed inodes.
    // ------------------------------------------------------------------

    fn read_file_cluster(
        ctx: &mut TxContext,
        inode: &mut Inode,
        offset: u64,
        size: u64,
    ) -> Result<Vec<u8>> {
        let read_size = min(size, inode.size - offset);
        let mut out = vec![0u8; read_size as usize];
        let first_cluster = offset / crate::file::cluster::CLUSTER_BYTES;
        let last_cluster = (offset + read_size - 1) / crate::file::cluster::CLUSTER_BYTES;
        let mut out_pos = 0usize;
        for ci in first_cluster..=last_cluster {
            let c_off = ci * crate::file::cluster::CLUSTER_BYTES;
            let cluster_len = min(inode.size - c_off, crate::file::cluster::CLUSTER_BYTES) as usize;
            let local_start = (offset.saturating_sub(c_off)) as usize;
            let local_end = min(
                offset + read_size - c_off,
                crate::file::cluster::CLUSTER_BYTES,
            ) as usize;
            let want = local_end
                .saturating_sub(local_start)
                .min(cluster_len.saturating_sub(local_start));
            if want == 0 {
                continue;
            }
            let data = crate::file::cluster::read_cluster_cached(ctx, inode, ci, cluster_len)?;
            let src_start = local_start.min(data.len());
            let src_end = (local_start + want).min(data.len());
            out[out_pos..out_pos + (src_end - src_start)]
                .copy_from_slice(&data[src_start..src_end]);
            out_pos += src_end - src_start;
        }
        Ok(out)
    }

    fn write_file_cluster(
        ctx: &mut TxContext,
        bg_desc: &BlockGroupDescriptor,
        blocks_per_group: u32,
        inode: &mut Inode,
        offset: u64,
        data: &[u8],
    ) -> Result<()> {
        if data.is_empty() {
            if offset > inode.size {
                inode.size = offset;
            }
            return Ok(());
        }
        let level = crate::fs::compression::zstd_level();
        let first_cluster = offset / crate::file::cluster::CLUSTER_BYTES;
        let last_cluster = (offset + data.len() as u64 - 1) / crate::file::cluster::CLUSTER_BYTES;
        let mut data_pos = 0usize;

        for ci in first_cluster..=last_cluster {
            let c_off = ci * crate::file::cluster::CLUSTER_BYTES;
            let local_start = (offset.saturating_sub(c_off)) as usize;
            let local_end = min(
                offset + data.len() as u64 - c_off,
                crate::file::cluster::CLUSTER_BYTES,
            ) as usize;
            let chunk_len = local_end - local_start;

            // Current logical length of this cluster (0 = never
            // written = hole).
            let existing_len = inode
                .size
                .saturating_sub(c_off)
                .min(crate::file::cluster::CLUSTER_BYTES) as usize;
            let new_len = local_end.max(existing_len);

            // Whole-cluster overwrite of a full cluster skips the
            // read-decompress step; anything else is a cluster RMW
            // (the documented tradeoff).
            let mut buf: Vec<u8> =
                if local_start == 0 && local_end == crate::file::cluster::CLUSTER_BYTES as usize {
                    vec![0u8; new_len]
                } else if existing_len > 0 {
                    crate::file::cluster::read_cluster(ctx, inode, ci, existing_len)?
                } else {
                    vec![0u8; new_len]
                };
            buf.resize(new_len, 0);
            buf[local_start..local_end].copy_from_slice(&data[data_pos..data_pos + chunk_len]);

            crate::file::cluster::write_cluster(
                ctx,
                bg_desc,
                blocks_per_group,
                inode,
                ci,
                &buf,
                level,
            )?;

            data_pos += chunk_len;
            if c_off + new_len as u64 > inode.size {
                inode.size = c_off + new_len as u64;
            }
        }
        Ok(())
    }

    fn truncate_file_cluster(
        ctx: &mut TxContext,
        bg_desc: &BlockGroupDescriptor,
        blocks_per_group: u32,
        inode: &mut Inode,
        new_size: u64,
    ) -> Result<()> {
        use crate::file::cluster::{drop_clusters_from, CLUSTER_BYTES};
        if new_size >= inode.size {
            inode.size = new_size;
            return Ok(());
        }
        let first_full_drop = new_size.div_ceil(CLUSTER_BYTES);
        // Drop every fully-truncated cluster.
        drop_clusters_from(ctx, bg_desc, inode, first_full_drop)?;

        // Partial final cluster: rewrite it shorter.
        let rem = new_size % CLUSTER_BYTES;
        if rem != 0 {
            let ci = new_size / CLUSTER_BYTES;
            let c_off = ci * CLUSTER_BYTES;
            let old_len = inode.size.saturating_sub(c_off).min(CLUSTER_BYTES) as usize;
            if old_len > rem as usize && inode.spill_extent_root != 0 {
                let tree = crate::file::cluster::ClusterTree::new(inode.spill_extent_root);
                if tree.get(ctx, ci)?.is_some() {
                    let mut buf = crate::file::cluster::read_cluster(ctx, inode, ci, old_len)?;
                    buf.truncate(rem as usize);
                    crate::file::cluster::write_cluster(
                        ctx,
                        bg_desc,
                        blocks_per_group,
                        inode,
                        ci,
                        &buf,
                        crate::fs::compression::zstd_level(),
                    )?;
                }
            }
        }
        inode.size = new_size;
        Ok(())
    }

    /// The file's highest INLINE-extent physical block + 1 (the locality
    /// hint for allocation). Deliberately inline-only: a spill-tree
    /// floor lookup per write_file call costs a BTree descent (3-4 node
    /// reads, each CRC32-verified) which measurably EXCEEDED the bitmap
    /// scans it saved on 4 KiB-per-call workloads. The per-call +1
    /// advance and TxContext's allocation frontier cover sequential
    /// locality; the inline scan alone is O(7).
    fn last_physical_end(_ctx: &mut TxContext, inode: &Inode) -> Result<Option<u64>> {
        let mut end: Option<u64> = None;
        for i in 0..inode.extent_count as usize {
            let e = &inode.extents[i];
            let this = e.physical_start + e.length;
            end = Some(match end {
                Some(cur) => cur.max(this),
                None => this,
            });
        }
        Ok(end)
    }

    fn get_physical_block(ctx: &mut TxContext, inode: &Inode, logical_block: u64) -> Result<u64> {
        for i in 0..inode.extent_count as usize {
            let extent = &inode.extents[i];
            if logical_block >= extent.logical_start
                && logical_block < extent.logical_start + extent.length
            {
                return Ok(extent.physical_start + (logical_block - extent.logical_start));
            }
        }
        // Extent spooling (Phase 0): once the 7 inline slots are full,
        // extents live in a per-inode ExtentTree keyed by logical_start.
        // `physical_block == 0` keeps its original meaning of "hole",
        // which is also what a miss in the spill tree means.
        if inode.spill_extent_root != 0 {
            let tree = crate::extents::tree::ExtentTree::new(inode.spill_extent_root);
            if let Some((phys_start, _len, log_start)) = tree.lookup_covering(ctx, logical_block)? {
                return Ok(phys_start + (logical_block - log_start));
            }
        }
        Ok(0) // 0 implies hole
    }

    /// Add an extent mapping to the inode. Inline first (with
    /// adjacency merge); when all 7 inline slots are taken, spool into
    /// the per-inode extent tree. This removes the hard ~7-extent file
    /// cap from the baseline, which made any file too fragmented (or
    /// simply too large under 1-block-at-a-time allocation) fail with
    /// "Max inline extents reached".
    /// Public mapping resolver (diagnostics and tests): where does
    /// `logical_block` of `inode` live physically? 0 = hole.
    pub fn resolve_physical_block(
        ctx: &mut TxContext,
        inode: &Inode,
        logical_block: u64,
    ) -> Result<u64> {
        Self::get_physical_block(ctx, inode, logical_block)
    }

    fn add_extent(
        ctx: &mut TxContext,
        bg_desc: &BlockGroupDescriptor,
        blocks_per_group: u32,
        inode: &mut Inode,
        logical_block: u64,
        physical_block: u64,
        length: u64,
    ) -> Result<()> {
        // Try to merge with adjacent extent
        for i in 0..inode.extent_count as usize {
            let extent = &mut inode.extents[i];

            if extent.logical_start + extent.length == logical_block
                && extent.physical_start + extent.length == physical_block
            {
                extent.length += length;
                return Ok(());
            }
            if logical_block + length == extent.logical_start
                && physical_block + length == extent.physical_start
            {
                extent.logical_start = logical_block;
                extent.physical_start = physical_block;
                extent.length += length;
                return Ok(());
            }
        }

        // Cannot merge inline, append new extent
        if (inode.extent_count as usize) < MAX_INLINE_EXTENTS {
            inode.extents[inode.extent_count as usize] = Extent {
                logical_start: logical_block,
                physical_start: physical_block,
                length,
            };
            inode.extent_count += 1;
            Ok(())
        } else {
            // Spill: ensure the per-inode extent tree exists, then try
            // to merge with the LAST spilled extent (the floor of
            // `logical_block`) before inserting a fresh entry.
            if inode.spill_extent_root == 0 {
                let root = Allocator::allocate_extents_meta(ctx, bg_desc, blocks_per_group, 1)?;
                crate::extents::tree::ExtentTree::init_empty(ctx, root)?;
                inode.spill_extent_root = root;
            }
            let mut tree = crate::extents::tree::ExtentTree::new(inode.spill_extent_root);
            let mut allocate = |c: &mut TxContext| {
                Allocator::allocate_extents_meta(c, bg_desc, blocks_per_group, 1)
            };

            // Merge into the floor extent when the new mapping is
            // physically and logically adjacent to its end.
            if let Some((k, v)) = tree.btree.lookup_floor(ctx, &logical_block)? {
                if k + v.length == logical_block && v.physical_start + v.length == physical_block {
                    tree.insert(ctx, k, v.physical_start, v.length + length, &mut allocate)?;
                    return Ok(());
                }
            }
            tree.insert(ctx, logical_block, physical_block, length, &mut allocate)
        }
    }

    /// Redirect one logical block's mapping to a different physical
    /// block (Phase 6 data CoW, 3.2). Splits the covering extent around
    /// the block when needed, inline or spilled. The OLD physical block
    /// is not freed here -- it is pinned (that is why we are remapping)
    /// and stays owned by the snapshot/share that pinned it.
    fn remap_block(
        ctx: &mut TxContext,
        bg_desc: &BlockGroupDescriptor,
        blocks_per_group: u32,
        inode: &mut Inode,
        logical_block: u64,
        new_physical: u64,
    ) -> Result<()> {
        // Inline extents first.
        for i in 0..inode.extent_count as usize {
            let e = inode.extents[i];
            let off = logical_block - e.logical_start;
            if logical_block >= e.logical_start && off < e.length {
                if e.length == 1 {
                    inode.extents[i].physical_start = new_physical;
                    return Ok(());
                }
                // Split into (left, block, right) and rebuild the inline
                // array truncate-style, spilling what does not fit.
                let mut surviving: Vec<Extent> = Vec::with_capacity(9);
                for j in 0..inode.extent_count as usize {
                    if j != i {
                        surviving.push(inode.extents[j]);
                    }
                }
                if off > 0 {
                    surviving.push(Extent {
                        logical_start: e.logical_start,
                        physical_start: e.physical_start,
                        length: off,
                    });
                }
                surviving.push(Extent {
                    logical_start: logical_block,
                    physical_start: new_physical,
                    length: 1,
                });
                if off + 1 < e.length {
                    surviving.push(Extent {
                        logical_start: logical_block + 1,
                        physical_start: e.physical_start + off + 1,
                        length: e.length - off - 1,
                    });
                }
                // Reset and re-add through add_extent (handles the
                // inline capacity limit and the spill tree, and merges
                // adjacent pieces where the split allows).
                let pieces: Vec<Extent> = surviving;
                for slot in inode.extents.iter_mut() {
                    *slot = Extent {
                        logical_start: 0,
                        physical_start: 0,
                        length: 0,
                    };
                }
                inode.extent_count = 0;
                for p in pieces {
                    Self::add_extent(
                        ctx,
                        bg_desc,
                        blocks_per_group,
                        inode,
                        p.logical_start,
                        p.physical_start,
                        p.length,
                    )?;
                }
                return Ok(());
            }
        }

        // Spilled extents: the covering entry is removed and its
        // pieces re-inserted (truncate's spill pattern).
        if inode.spill_extent_root != 0 {
            let mut tree = crate::extents::tree::ExtentTree::new(inode.spill_extent_root);
            if let Some((log_start, val)) = tree.btree.lookup_floor(ctx, &logical_block)? {
                if log_start <= logical_block
                    && logical_block < log_start + val.length
                {
                    let off = logical_block - log_start;
                    let mut allocate = |c: &mut TxContext| {
                        Allocator::allocate_extents(c, bg_desc, blocks_per_group, 1)
                    };
                    // Remove the whole covering extent, re-insert the pieces.
                    // Phase 9: spill-tree removal under a live snapshot
                    // barrier must path-copy first (frozen tree).
                    tree.remove_with_alloc(ctx, &log_start, &mut allocate)?;
                    if off > 0 {
                        tree.insert(
                            ctx,
                            log_start,
                            val.physical_start,
                            off,
                            &mut allocate,
                        )?;
                    }
                    tree.insert(ctx, logical_block, new_physical, 1, &mut allocate)?;
                    if off + 1 < val.length {
                        tree.insert(
                            ctx,
                            logical_block + 1,
                            val.physical_start + off + 1,
                            val.length - off - 1,
                            &mut allocate,
                        )?;
                    }
                    return Ok(());
                }
            }
        }
        Err(Error::new(
            ErrorKind::NotFound,
            "remap_block: logical block is not mapped",
        ))
    }
}
