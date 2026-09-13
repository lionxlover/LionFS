//! Real block compression backed by lz4_flex, zstd, and flate2.
//!
//! `compress` is infallible (these algorithms can always encode arbitrary
//! bytes). `decompress` is fallible: corrupted or truncated input is a real
//! possibility (bad block, bug elsewhere, disk corruption) and must be
//! reported rather than silently producing garbage or panicking.

use std::io::{Read, Result};

pub trait CompressionAlgorithm {
    fn id(&self) -> u8;
    fn compress(&self, data: &[u8]) -> Vec<u8>;
    fn decompress(&self, data: &[u8]) -> Result<Vec<u8>>;
}

pub struct Lz4;
impl CompressionAlgorithm for Lz4 {
    fn id(&self) -> u8 {
        1
    }
    fn compress(&self, data: &[u8]) -> Vec<u8> {
        lz4_flex::block::compress_prepend_size(data)
    }
    fn decompress(&self, data: &[u8]) -> Result<Vec<u8>> {
        lz4_flex::block::decompress_size_prepended(data).map_err(|e| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!("LZ4 decompress failed: {e}"),
            )
        })
    }
}

pub struct Zstd;
impl CompressionAlgorithm for Zstd {
    fn id(&self) -> u8 {
        2
    }
    fn compress(&self, data: &[u8]) -> Vec<u8> {
        // Level 3 is zstd's own "sensible default" -- reasonable ratio
        // without spending excessive CPU on every 4KB block.
        zstd::stream::encode_all(data, 3).unwrap_or_else(|_| data.to_vec())
    }
    fn decompress(&self, data: &[u8]) -> Result<Vec<u8>> {
        zstd::stream::decode_all(data).map_err(|e| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!("Zstd decompress failed: {e}"),
            )
        })
    }
}

pub struct Deflate;
impl CompressionAlgorithm for Deflate {
    fn id(&self) -> u8 {
        3
    }
    fn compress(&self, data: &[u8]) -> Vec<u8> {
        use flate2::write::DeflateEncoder;
        use flate2::Compression;
        use std::io::Write;
        let mut enc = DeflateEncoder::new(Vec::new(), Compression::default());
        // Writing to an in-memory Vec cannot fail; unwrap is safe here.
        enc.write_all(data)
            .expect("in-memory deflate write cannot fail");
        enc.finish().expect("in-memory deflate finish cannot fail")
    }
    fn decompress(&self, data: &[u8]) -> Result<Vec<u8>> {
        use flate2::read::DeflateDecoder;
        let mut dec = DeflateDecoder::new(data);
        let mut out = Vec::new();
        dec.read_to_end(&mut out).map_err(|e| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!("Deflate decompress failed: {e}"),
            )
        })?;
        Ok(out)
    }
}

// ---------------------------------------------------------------------------
// Phase 4: zstd compression level plumbing.
//
// Level was hardcoded to 3 ("zstd's own sensible default"). It is now a
// mount-time option (mount_lfs -o zstd_level=N, default 3) stored in
// this process-global, consulted by the compression-cluster path. The
// ratio/CPU tradeoff is real and measurable: see the level table in
// docs/benchmarks.md, produced by `lfs_ioperf --compress-level-sweep`.
// ---------------------------------------------------------------------------

use std::sync::atomic::{AtomicI32, Ordering};

static ZSTD_LEVEL: AtomicI32 = AtomicI32::new(3);

/// Set the zstd compression level used for newly written clusters
/// (valid range 1..=22; values outside are clamped).
pub fn set_zstd_level(level: i32) {
    ZSTD_LEVEL.store(level.clamp(1, 22), Ordering::Relaxed);
}

pub fn zstd_level() -> i32 {
    ZSTD_LEVEL.load(Ordering::Relaxed)
}

/// Compress at a specific zstd level (cluster path; the mount-config
/// level is the default).
pub fn zstd_compress_at_level(data: &[u8], level: i32) -> Vec<u8> {
    let level = level.clamp(1, 22);
    zstd::stream::encode_all(data, level).unwrap_or_else(|_| data.to_vec())
}

pub fn zstd_decompress(data: &[u8]) -> std::io::Result<Vec<u8>> {
    zstd::stream::decode_all(data).map_err(|e| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("Zstd decompress failed: {e}"),
        )
    })
}

pub struct CompressionManager;

impl CompressionManager {
    pub fn get_algorithm(id: u8) -> Option<Box<dyn CompressionAlgorithm>> {
        match id {
            1 => Some(Box::new(Lz4)),
            2 => Some(Box::new(Zstd)),
            3 => Some(Box::new(Deflate)),
            _ => None,
        }
    }

    /// Tries `preferred` (falling back to LZ4 if `preferred` is unknown) and
    /// only keeps the compressed form if it's actually smaller than `data`.
    /// Returns `(algorithm_id, bytes)`; `algorithm_id == 0` means "not
    /// compressed, `bytes` is the original input" -- the honest outcome for
    /// data that's already dense (encrypted, already-compressed, random).
    pub fn adaptive_compress(data: &[u8], preferred: u8) -> (u8, Vec<u8>) {
        let algo = Self::get_algorithm(preferred).unwrap_or_else(|| Box::new(Lz4));
        let compressed = algo.compress(data);
        if compressed.len() < data.len() {
            (algo.id(), compressed)
        } else {
            (0, data.to_vec())
        }
    }
}

#[cfg(test)]
mod real_compression_tests {
    use super::*;

    fn roundtrip(algo: &dyn CompressionAlgorithm, data: &[u8]) {
        let compressed = algo.compress(data);
        let back = algo.decompress(&compressed).unwrap();
        assert_eq!(back, data);
    }

    #[test]
    fn lz4_roundtrip() {
        roundtrip(&Lz4, b"repeat repeat repeat repeat repeat repeat repeat");
    }

    #[test]
    fn zstd_roundtrip() {
        roundtrip(&Zstd, b"repeat repeat repeat repeat repeat repeat repeat");
    }

    #[test]
    fn deflate_roundtrip() {
        roundtrip(
            &Deflate,
            b"repeat repeat repeat repeat repeat repeat repeat",
        );
    }

    #[test]
    fn repetitive_data_actually_shrinks() {
        let data = "repeating data ".repeat(64);
        let (algo_id, compressed) = CompressionManager::adaptive_compress(data.as_bytes(), 2);
        assert_ne!(algo_id, 0, "highly repetitive data should compress");
        assert!(compressed.len() < data.len());
        let algo = CompressionManager::get_algorithm(algo_id).unwrap();
        assert_eq!(algo.decompress(&compressed).unwrap(), data.as_bytes());
    }

    #[test]
    fn incompressible_random_data_falls_back_to_uncompressed() {
        // Pseudo-random bytes via a tiny xorshift -- no need for real
        // entropy, just enough to be non-repetitive for this test.
        let mut x: u32 = 0x12345678;
        let data: Vec<u8> = (0..256)
            .map(|_| {
                x ^= x << 13;
                x ^= x >> 17;
                x ^= x << 5;
                (x & 0xFF) as u8
            })
            .collect();
        let (algo_id, out) = CompressionManager::adaptive_compress(&data, 2);
        assert_eq!(algo_id, 0);
        assert_eq!(out, data);
    }
}
