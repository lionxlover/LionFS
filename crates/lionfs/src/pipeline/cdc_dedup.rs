//! Content-defined dedup analysis with convergent keys (LionFS 8.0 —
//! the HFS merge, pipeline side).
//!
//! The local engine has had FastCDC (gear-hash chunking) and a BLAKE3
//! `DedupIndex` since 3.2 — both **unwired**, block-granular dedup
//! being the only live path. The cluster plane's dedup is
//! content-defined at chunk granularity AND encryption-compatible:
//! keys are derived convergently from (domain, content), so identical
//! plaintext in the same dedup domain produces identical ciphertext —
//! dedup keeps working with encryption ON, which per-file random keys
//! can never offer.
//!
//! This module fuses the two: chunk with the LOCAL engine's FastCDC,
//! key the chunks with the CLUSTER plane's domain-scoped convergent
//! identity, and drive the CLUSTER plane's `DedupIndex`. It is the
//! analysis/planning surface for the `lfs_dedupe` tool and for
//! future write-path integration (`LFS_CDC=1`).
//!
//! Convergent scheme (from the cluster plane's `crypto`):
//! ```text
//! HKDF(master, "hfs/domain/" || domain)      → domain key
//! HKDF(domain_key, "hfs/ext/" || H(content)) → extent key
//! nonce = BLAKE3(domain_key || H(content))[0..12]
//! ```
//! Same (domain, content) ⇒ same key+nonce ⇒ same ciphertext.

use crate::ondisk::serialization::{Inode, Superblock};
use crate::pipeline::fastcdc::{fastcdc_with, FastCdcConfig};
use crate::security::block_cipher::BlockCipherContext;
use crate::transaction::transaction::TxContext;
use lionfs_cluster::core::Hash256;
use lionfs_cluster::crypto::{Cipher, KeyTree};
use lionfs_cluster::dedup::{ChunkLocation, DedupIndex, DomainId};
use std::io::{Error, ErrorKind, Result};

/// Default analysis chunking profile (the engine's FastCDC defaults:
/// 2 KiB min / 8 KiB avg / 32 KiB max).
pub const DEFAULT_CDC: FastCdcConfig = FastCdcConfig {
    min: 2048,
    avg: 8192,
    max: 32768,
};

/// Result of a CDC dedup analysis pass.
#[derive(Clone, Copy, Debug, Default)]
pub struct CdcDedupStats {
    /// Total chunks the input split into.
    pub chunks: u64,
    /// Sum of chunk lengths (= input length).
    pub logical_bytes: u64,
    /// Chunks never seen before (would occupy storage).
    pub unique_chunks: u64,
    /// Bytes of unique chunks.
    pub stored_bytes: u64,
    /// Bytes eliminated by dedup (logical - stored).
    pub dedup_bytes: u64,
    /// Smallest / largest observed chunk.
    pub min_chunk_bytes: u64,
    pub max_chunk_bytes: u64,
}

impl CdcDedupStats {
    /// Dedup ratio: logical / stored (>= 1.0; 2.0 = halved storage).
    pub fn ratio(&self) -> f64 {
        if self.stored_bytes == 0 {
            1.0
        } else {
            self.logical_bytes as f64 / self.stored_bytes as f64
        }
    }

    /// Mean chunk size in bytes.
    pub fn avg_chunk_bytes(&self) -> f64 {
        if self.chunks == 0 {
            0.0
        } else {
            self.logical_bytes as f64 / self.chunks as f64
        }
    }
}

/// The domain-scoped convergent identity of one chunk: the cluster
/// plane's plaintext content hash. Identical bytes in the same domain
/// share an identity; different domains keep identities separate
/// (tenant isolation) while the ENCRYPTION stays convergent within a
/// domain.
pub fn convergent_chunk_hash(chunk: &[u8], domain: &DomainId) -> Hash256 {
    // The cluster plane hashes domain-separated content by prefixing
    // the domain id (its engine builds chunk ids as H(domain||chunk));
    // mirror that here so analyses predict cluster-plane behavior.
    let mut buf = Vec::with_capacity(domain.0.len() + chunk.len());
    buf.extend_from_slice(&domain.0);
    buf.extend_from_slice(chunk);
    Hash256::of(&buf)
}

/// Prove the convergent property on one chunk: encrypting the same
/// (domain, content) twice yields IDENTICAL ciphertext (so dedup
/// still applies), while a different domain yields different bytes
/// (tenant isolation). Returns `(stable, isolated)`.
pub fn convergent_roundtrip(
    chunk: &[u8],
    domain: &DomainId,
    keytree: &KeyTree,
    cipher: Cipher,
) -> (bool, bool) {
    let hash = convergent_chunk_hash(chunk, domain);
    let dk = keytree.domain_key(&domain.0);
    let c1 = keytree.encrypt_extent(&dk, &hash, chunk, cipher);
    let c2 = keytree.encrypt_extent(&dk, &hash, chunk, cipher);
    let other = DomainId::from(&b"other-tenant"[..]);
    let hash_o = convergent_chunk_hash(chunk, &other);
    let dk_o = keytree.domain_key(&other.0);
    let c3 = keytree.encrypt_extent(&dk_o, &hash_o, chunk, cipher);
    match (c1, c2, c3) {
        (Ok(a), Ok(b), Ok(c)) => (a == b, a != c),
        _ => (false, false),
    }
}

/// Analyze one buffer: FastCDC-split, then drive the cluster plane's
/// domain-scoped `DedupIndex` exactly the way the cluster engine's
/// write path does (probe → miss: insert; hit: account duplicate).
///
/// `fastcdc_with` returns chunk LENGTHS that tile the input exactly.
pub fn analyze(data: &[u8], domain: &DomainId, cfg: &FastCdcConfig) -> (CdcDedupStats, DedupIndex) {
    let mut index = DedupIndex::new();
    let mut stats = CdcDedupStats::default();
    let lens = fastcdc_with(data, cfg);
    let mut pos = 0usize;
    for &len in &lens {
        let chunk = &data[pos..pos + len];
        pos += len;
        ingest_chunk(&mut index, &mut stats, domain, chunk);
    }
    (stats, index)
}
fn ingest_chunk(index: &mut DedupIndex, stats: &mut CdcDedupStats, domain: &DomainId, chunk: &[u8]) {
    let hash = convergent_chunk_hash(chunk, domain);
    stats.chunks += 1;
    stats.logical_bytes += chunk.len() as u64;
    stats.min_chunk_bytes = if stats.min_chunk_bytes == 0 {
        chunk.len() as u64
    } else {
        stats.min_chunk_bytes.min(chunk.len() as u64)
    };
    stats.max_chunk_bytes = stats.max_chunk_bytes.max(chunk.len() as u64);
    if index.lookup(domain, &hash).is_some() {
        index.account_duplicate(chunk.len() as u64);
        stats.dedup_bytes += chunk.len() as u64;
    } else {
        stats.unique_chunks += 1;
        stats.stored_bytes += chunk.len() as u64;
        // Location is analysis-only; offsets encode the logical stream
        // position for reporting.
        index.insert_new(
            domain,
            &hash,
            ChunkLocation {
                zone_id: 0,
                offset_in_zone: (stats.logical_bytes - chunk.len() as u64) as u32,
                length: chunk.len() as u32,
            },
            chunk.len() as u64,
        );
    }
}

/// Analyze TWO buffers as one logical stream (cross-file dedup: the
/// second file's chunks probe the index built from the first).
pub fn analyze_pair(
    first: &[u8],
    second: &[u8],
    domain: &DomainId,
    cfg: &FastCdcConfig,
) -> (CdcDedupStats, DedupIndex) {
    let (mut stats, mut index) = analyze(first, domain, cfg);
    let lens = fastcdc_with(second, cfg);
    let mut pos = 0usize;
    for &len in &lens {
        let chunk = &second[pos..pos + len];
        pos += len;
        ingest_chunk(&mut index, &mut stats, domain, chunk);
    }
    (stats, index)
}

// ---------------------------------------------------------------------------
// Live-image file reading (tool support): walk the LIVE trees, not a
// snapshot. Mirrors `fs::timetravel`'s frozen walk but through the
// superblock's current roots.
// ---------------------------------------------------------------------------

fn is_dir(inode: &Inode) -> bool {
    inode.mode & 0o170000 == 0o040000
}

/// Resolve a path on the LIVE tree and read the file's bytes.
///
/// Refuses compressed and encrypted inodes with a clear error (the
/// analysis surface operates on plaintext).
pub fn read_live_file(ctx: &mut TxContext, sb: &Superblock, path: &str) -> Result<Vec<u8>> {
    use crate::directory::entries::DirManager;
    use crate::file::writer::FileManager;
    use crate::inode::manager::InodeManager;

    let trimmed = path.trim_start_matches('/');
    if trimmed.is_empty() {
        return Err(Error::new(ErrorKind::IsADirectory, "the root is a directory"));
    }
    let mut current =
        InodeManager::read_inode(ctx, sb.inode_tree_root, 1)?; // FUSE root
    for comp in trimmed.split('/') {
        if !is_dir(&current) {
            return Err(Error::new(
                ErrorKind::NotADirectory,
                format!("component {comp:?} under a non-directory"),
            ));
        }
        let entries = DirManager::read_entries(
            ctx,
            sb.checksum_tree_root,
            sb.bad_blocks_root,
            &mut current,
        )?;
        match entries.into_iter().find(|e| e.name == comp) {
            Some(e) => {
                current = InodeManager::read_inode(ctx, sb.inode_tree_root, e.ino)?;
            }
            None => {
                return Err(Error::new(
                    ErrorKind::NotFound,
                    format!("no such file or directory: {path}"),
                ))
            }
        }
    }
    if is_dir(&current) {
        return Err(Error::new(ErrorKind::IsADirectory, path.to_string()));
    }
    if current.compression_algo != 0 {
        return Err(Error::new(
            ErrorKind::Unsupported,
            "compressed inodes are outside the plaintext analysis surface",
        ));
    }
    if current.encryption_algo != 0 {
        return Err(Error::new(
            ErrorKind::Unsupported,
            "encrypted inodes are outside the plaintext analysis surface",
        ));
    }
    let cctx = BlockCipherContext::none();
    let size = current.size;
    FileManager::read_file(
        ctx,
        sb.checksum_tree_root,
        sb.bad_blocks_root,
        &cctx,
        &mut current,
        0,
        size,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pseudo(len: usize, seed: u64) -> Vec<u8> {
        // xorshift payload: incompressible, deterministic.
        let mut s = seed | 1;
        (0..len)
            .map(|_| {
                s ^= s << 13;
                s ^= s >> 7;
                s ^= s << 17;
                s as u8
            })
            .collect()
    }

    #[test]
    fn identical_input_dedups_fully() {
        let data = pseudo(200_000, 7);
        let (stats, _) = analyze(&data, &DomainId::root(), &DEFAULT_CDC);
        assert_eq!(stats.logical_bytes, 200_000);
        assert_eq!(stats.stored_bytes, 200_000, "no repeats in random data");
        assert_eq!(stats.unique_chunks, stats.chunks);
        assert!((stats.ratio() - 1.0).abs() < 1e-9);
        // The TAIL chunk may be shorter than min; every other chunk
        // respects [min, max] (checked exhaustively in
        // chunking_respects_bounds).
        assert!(stats.max_chunk_bytes <= DEFAULT_CDC.max as u64);
        assert!(stats.avg_chunk_bytes() >= 1.0);
    }

    #[test]
    fn repeated_input_eliminates_duplicates() {
        let block = pseudo(64 * 1024, 9);
        // The same 64 KiB repeated 8 times. Content-defined cuts do not
        // align with the repeat boundary: ONE spanning chunk straddles
        // each copy boundary, then the grid re-syncs (the CDC
        // self-similarity property). The spanning chunks are identical
        // to each other, so storage = first copy + ONE spanning chunk.
        let data = block.repeat(8);
        let (stats, _) = analyze(&data, &DomainId::root(), &DEFAULT_CDC);
        assert_eq!(stats.logical_bytes, 8 * 64 * 1024);
        assert!(
            stats.stored_bytes <= 64 * 1024 + DEFAULT_CDC.max as u64,
            "storage = first copy + at most one spanning chunk, got {}",
            stats.stored_bytes
        );
        let ratio = stats.ratio();
        assert!(ratio > 5.3, "8 repeated copies must dedup heavily, got ratio {ratio:.2}");
        assert!(
            stats.dedup_bytes >= 7 * 64 * 1024 - DEFAULT_CDC.max as u64,
            "all but the first copy (and one spanning chunk) is eliminated"
        );
    }

    #[test]
    fn shifted_input_still_dedups() {
        // The CDC property random-access systems care about: insert a
        // 1000-byte prefix and the tail still matches — fixed-size
        // block dedup would lose EVERY block boundary (0% match);
        // content-defined chunking loses only the first misaligned
        // chunk (bounded by max) before the cut grid re-syncs.
        let base = pseudo(256 * 1024, 11);
        let mut shifted = pseudo(1000, 12); // small prefix
        shifted.extend_from_slice(&base);
        let (stats, _) = analyze_pair(&base, &shifted, &DomainId::root(), &DEFAULT_CDC);
        // The shifted copy contributes ~256 KiB + 1 KiB; everything
        // after the first misaligned chunk must match the base.
        let shifted_len = 1000 + 256 * 1024;
        assert!(
            stats.dedup_bytes >= shifted_len as u64 - DEFAULT_CDC.max as u64,
            "shifted input must dedup after one misaligned chunk, eliminated {}",
            stats.dedup_bytes
        );
    }

    #[test]
    fn domains_are_isolated() {
        let data = pseudo(64 * 1024, 13);
        let (a, _) = analyze(&data, &DomainId::root(), &DEFAULT_CDC);
        let (b, _) = analyze(&data, &DomainId::from(&b"tenant-b"[..]), &DEFAULT_CDC);
        // Same bytes, different domain: identical stats, but the
        // convergent identities differ (checked below via roundtrip).
        assert_eq!(a.unique_chunks, b.unique_chunks);
        let h_root = convergent_chunk_hash(&data[..4096], &DomainId::root());
        let h_b = convergent_chunk_hash(&data[..4096], &DomainId::from(&b"tenant-b"[..]));
        assert_ne!(h_root, h_b, "domain-scoped identities must differ");
    }

    #[test]
    fn convergent_encryption_is_stable_and_isolated() {
        let keytree = KeyTree::generate();
        let chunk = pseudo(8192, 15);
        let (stable, isolated) =
            convergent_roundtrip(&chunk, &DomainId::root(), &keytree, Cipher::Aes256Gcm);
        assert!(stable, "same (domain, content) must encrypt identically");
        assert!(isolated, "different domains must encrypt differently");
        let (s2, i2) =
            convergent_roundtrip(&chunk, &DomainId::root(), &keytree, Cipher::ChaCha20Poly1305);
        assert!(s2 && i2, "the property holds for both AEAD ciphers");
    }

    #[test]
    fn chunking_respects_bounds() {
        // All-zero data splits at deterministic max-boundary points;
        // every chunk but the TAIL must respect [min, max] (the tail
        // is whatever remains and may be shorter than min).
        let data = vec![0u8; 512 * 1024];
        let lens = fastcdc_with(&data, &DEFAULT_CDC);
        assert_eq!(lens.iter().sum::<usize>(), data.len(), "chunks tile the input");
        for (i, &len) in lens.iter().enumerate() {
            let is_tail = i + 1 == lens.len();
            assert!(len <= DEFAULT_CDC.max, "chunk {len} > max");
            if !is_tail {
                assert!(len >= DEFAULT_CDC.min, "non-tail chunk {len} < min");
            }
        }
    }

    #[test]
    fn stats_math_is_closed() {
        let data = pseudo(100_000, 17);
        let (stats, index) = analyze(&data, &DomainId::root(), &DEFAULT_CDC);
        assert_eq!(stats.dedup_bytes + stats.stored_bytes, stats.logical_bytes);
        assert_eq!(stats.unique_chunks as u64 + (stats.chunks - stats.unique_chunks), stats.chunks);
        // The cluster-plane index's own view agrees.
        let is = index.stats();
        assert_eq!(is.physical_bytes, stats.stored_bytes);
        assert_eq!(is.logical_bytes, stats.logical_bytes);
    }

    #[test]
    fn live_file_read_roundtrip() {
        // A real mounted image, one file written through VfsOps, read
        // back through the analysis surface.
        use crate::fs::parallel_tests::{mount, test_path};
        use crate::disk::block_io::Disk;
        use crate::ondisk::serialization::{Superblock, BLOCK_SIZE, LIONFS_MAGIC};
        use crate::transaction::manager::TransactionManager;
        use crate::transaction::transaction::TxContext;
        use crate::vfs::{VfsCreate, VfsOps};

        let tag = "cdc_live_read";
        let path = test_path(tag);
        let _ = std::fs::remove_file(&path);
        let payload = pseudo(150_000, 21);
        {
            let mut fs = mount(tag, 64);
            let ino = fs
                .create(1, "payload.bin", &VfsCreate { mode: 0o100644, uid: 1000, gid: 1000 })
                .expect("create")
                .ino;
            fs.write(ino, 0, &payload).expect("write");
            fs.fsync(ino, true).expect("fsync");
            fs.destroy();
        }
        let disk = Disk::open(&path).expect("open");
        let mut buf = [0u8; BLOCK_SIZE];
        disk.read_block(0, &mut buf).unwrap();
        let sb: Superblock = *bytemuck::from_bytes(&buf[..std::mem::size_of::<Superblock>()]);
        assert_eq!(sb.magic, LIONFS_MAGIC);
        let tm = TransactionManager::new(&sb);
        let mut tx = tm.begin(0);
        let mut ctx = TxContext::new(&disk, &mut tx);
        let read = read_live_file(&mut ctx, &sb, "/payload.bin").expect("read live");
        assert_eq!(read, payload);

        // And the analysis surface composes with it.
        let (stats, _) = analyze(&read, &DomainId::root(), &DEFAULT_CDC);
        assert_eq!(stats.logical_bytes, 150_000);

        // Missing path: NotFound.
        let err = read_live_file(&mut ctx, &sb, "/nope").unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::NotFound);
    }
}
