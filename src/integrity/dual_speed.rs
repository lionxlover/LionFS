//! Dual-speed checksum policy (RFC-002 §5.2).
//!
//! Integrity verification is split by temperature because checksum
//! strength and checksum speed trade linearly:
//!
//! | Primitive | Throughput class | Applied to | Property |
//! |-----------|------------------|------------|----------|
//! | xxHash64 | ~20 GB/s/core | hot 4-64 KiB pages | collision-safe for bit rot; fast |
//! | BLAKE3 | multi-GB/s/core, tree-parallel | cold pages, clusters, journal | cryptographic, keyed mode available |
//! | CRC32C | hardware instruction | commit records, superblocks | torn-write detection |
//!
//! The 1.x gap where compressed inodes detected corruption only via zstd
//! decode failure is closed: every cluster record pairs with a 16-byte
//! BLAKE3 tag in the checksum tree, at 0.4 percent metadata overhead.

use crate::integrity::algorithms::ChecksumAlgorithm as Algorithm;

/// Which checksum applies to a piece of data, by temperature and role.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChecksumClass {
    /// Hot 4-64 KiB pages: xxHash64, verified on every read completion.
    HotPage,
    /// Cold pages: BLAKE3.
    ColdPage,
    /// Every compression cluster: BLAKE3 (rides the decompression
    /// already happening -- verified essentially for free).
    CompressionCluster,
    /// Commit records, superblocks: CRC32C (torn-write detection).
    Structural,
}

/// Data temperature input for the policy.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Temperature {
    Hot,
    Cold,
}

impl ChecksumClass {
    #[must_use]
    pub fn for_page(temp: Temperature) -> Self {
        match temp {
            Temperature::Hot => Self::HotPage,
            Temperature::Cold => Self::ColdPage,
        }
    }

    #[must_use]
    pub fn algorithm(self) -> Algorithm {
        match self {
            Self::HotPage => Algorithm::XxHash64,
            Self::ColdPage | Self::CompressionCluster => Algorithm::Blake3,
            Self::Structural => Algorithm::Crc32c,
        }
    }

    /// Digest width in bytes for this class.
    #[must_use]
    pub fn digest_len(self) -> usize {
        match self {
            Self::HotPage => 8,
            Self::ColdPage | Self::CompressionCluster => 16, // BLAKE3-128: 0.4% of a 4 KiB block
            Self::Structural => 4,
        }
    }

    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            Self::HotPage => "xxhash64",
            Self::ColdPage => "blake3",
            Self::CompressionCluster => "blake3-cluster",
            Self::Structural => "crc32c",
        }
    }
}

/// A computed digest with its class, ready for the checksum tree.
#[derive(Debug, Clone)]
pub struct Digest {
    pub class: ChecksumClass,
    pub bytes: Vec<u8>,
}

/// Computes a digest under the dual-speed policy.
#[must_use]
pub fn digest(class: ChecksumClass, data: &[u8]) -> Digest {
    let bytes = match class {
        ChecksumClass::HotPage => {
            let h = xxhash_rust::xxh64::xxh64(data, 0);
            h.to_le_bytes().to_vec()
        }
        ChecksumClass::ColdPage | ChecksumClass::CompressionCluster => {
            // BLAKE3-128: the tree-parallel digest, truncated to 16
            // bytes (128 bits) -- cryptographic strength at the 0.4%
            // metadata overhead the RFC budgets.
            let full = blake3::hash(data);
            full.as_bytes()[..16].to_vec()
        }
        ChecksumClass::Structural => {
            let crc = crate::utils::crc::compute_checksum(data);
            crc.to_le_bytes().to_vec()
        }
    };
    Digest { class, bytes }
}

/// Verifies data against a digest. Returns `Some(true/false)` when the
/// digest's class matches the policy for the current temperature, and
/// `None` when the stored class disagrees with the requested one (a
/// format inconsistency the scrubber should flag, not silently pass).
#[must_use]
pub fn verify(class: ChecksumClass, data: &[u8], stored: &Digest) -> Option<bool> {
    if stored.class != class {
        return None;
    }
    let computed = digest(class, data);
    // Constant-time comparison for the cryptographic classes: no early
    // exit on the first differing byte.
    Some(constant_time_eq(&computed.bytes, &stored.bytes))
}

fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for (x, y) in a.iter().zip(b.iter()) {
        diff |= x ^ y;
    }
    diff == 0
}

/// The per-cluster BLAKE3 tag pairing (the closed 1.x gap).
///
/// Every 128 KiB compression cluster record carries one of these in the
/// checksum tree: corruption inside a compressed cluster is detected by
/// the tag *before* (and independent of) the codec's own decode failure,
/// which is what makes repair-from-parity possible rather than a decode
/// error you cannot attribute.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClusterTag {
    /// The inode the cluster belongs to.
    pub inode: u64,
    /// Cluster index within the inode.
    pub cluster: u64,
    pub digest: Vec<u8>,
}

impl ClusterTag {
    #[must_use]
    pub fn compute(inode: u64, cluster: u64, payload: &[u8]) -> Self {
        // Domain-separate the digest: the inode and cluster index are
        // folded into a 16-byte prefix so a cluster's tag cannot be
        // confused with another's (the "same bytes, different cluster"
        // attack on naive content-only tags).
        let mut domain = Vec::with_capacity(16);
        domain.extend_from_slice(&inode.to_le_bytes());
        domain.extend_from_slice(&cluster.to_le_bytes());
        let mut hasher = blake3::Hasher::new();
        hasher.update(&domain);
        hasher.update(payload);
        let full = hasher.finalize();
        Self {
            inode,
            cluster,
            digest: full.as_bytes()[..16].to_vec(),
        }
    }

    #[must_use]
    pub fn verify(&self, payload: &[u8]) -> bool {
        ClusterTag::compute(self.inode, self.cluster, payload) == *self
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const PAGE: &[u8] = b"the quick brown fox jumps over the lazy dog 0123456789";

    #[test]
    fn hot_pages_use_xxhash64() {
        let d = digest(ChecksumClass::for_page(Temperature::Hot), PAGE);
        assert_eq!(d.class.algorithm(), Algorithm::XxHash64);
        assert_eq!(d.bytes.len(), 8);
    }

    #[test]
    fn cold_pages_and_clusters_use_blake3_128() {
        let d = digest(ChecksumClass::for_page(Temperature::Cold), PAGE);
        assert_eq!(d.class.algorithm(), Algorithm::Blake3);
        assert_eq!(d.bytes.len(), 16);
        let c = digest(ChecksumClass::CompressionCluster, PAGE);
        assert_eq!(c.bytes.len(), 16);
        // 16 bytes per 4 KiB block = 0.39%: the RFC's 0.4% budget.
        let overhead = 16.0 / 4096.0 * 100.0;
        assert!(overhead < 0.4 + 1e-9);
    }

    #[test]
    fn structural_uses_crc32c() {
        let d = digest(ChecksumClass::Structural, PAGE);
        assert_eq!(d.bytes.len(), 4);
        assert_eq!(d.class.algorithm(), Algorithm::Crc32c);
    }

    #[test]
    fn verify_roundtrip_and_corruption_detection() {
        for class in [
            ChecksumClass::HotPage,
            ChecksumClass::ColdPage,
            ChecksumClass::CompressionCluster,
            ChecksumClass::Structural,
        ] {
            let d = digest(class, PAGE);
            assert_eq!(verify(class, PAGE, &d), Some(true));
            let mut corrupted = PAGE.to_vec();
            corrupted[3] ^= 0x40; // single-bit flip
            assert_eq!(verify(class, &corrupted, &d), Some(false));
        }
    }

    #[test]
    fn class_mismatch_is_flagged_not_passed() {
        let d = digest(ChecksumClass::HotPage, PAGE);
        assert_eq!(verify(ChecksumClass::ColdPage, PAGE, &d), None);
    }

    #[test]
    fn cluster_tags_are_domain_separated() {
        let t = ClusterTag::compute(1, 0, PAGE);
        assert!(t.verify(PAGE));
        assert!(!t.verify(&[0u8; 10]));
        // Same payload, different cluster: different tag.
        let t2 = ClusterTag::compute(1, 1, PAGE);
        assert_ne!(t, t2);
        // Same payload, different inode: different tag.
        let t3 = ClusterTag::compute(2, 0, PAGE);
        assert_ne!(t, t3);
    }

    #[test]
    fn algorithm_names() {
        assert_eq!(ChecksumClass::HotPage.name(), "xxhash64");
        assert_eq!(ChecksumClass::CompressionCluster.name(), "blake3-cluster");
    }
}
