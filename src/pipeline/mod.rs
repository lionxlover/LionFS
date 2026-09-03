//! # Compression & Deduplication Pipeline (Pillar V)
//!
//! RFC-002 §7, built on the proven 1.x substrate (128 KiB compression
//! clusters, ClusterTree, zstd level mount option):
//!
//! * [`tiers`] -- the adaptive tier policy (Table 13): LZ4 for the hot
//!   tier (line-rate writes), zstd-3 for warm bulk, zstd-9+ for the cold
//!   tier where ratio compounds; raw fallback with the RAW flag when
//!   compression does not help (ratio < 1.2).
//! * [`punch`] -- the punch-through escape hatch: a third
//!   read-modify-write against the same cluster transparently
//!   decompresses it into raw extents and retires the ClusterTree entry,
//!   paying the write amplification once instead of unboundedly.
//! * [`fastcdc`] -- content-defined chunking (FastCDC, Xia et al.,
//!   FAST'16): expected 8 KiB, min 2 KiB, max 32 KiB, gear hashing;
//!   insertions shift cut points only locally, so identical content
//!   chunks identically regardless of file alignment.
//! * [`dedup`] -- the three-level dedup index: bloom filter over the
//!   pool, bounded hot-hash LRU, on-disk hash tree consulted only on
//!   "maybe"; RAM budget 0.1% of pool size.
//! * [`offload`] -- the accelerator backend selection (software SIMD /
//!   QAT), chosen per submission, with the selection itself recorded --
//!   the honesty rule applied to hardware offload.

pub mod dedup;
pub mod fastcdc;
pub mod offload;
pub mod punch;
pub mod tiers;

pub use dedup::{BloomIndex, DedupIndex};
pub use fastcdc::{chunk_count_estimate, fastcdc, FastCdcConfig, GearHash};
pub use punch::{PunchThroughDecision, PunchThroughTracker};
pub use tiers::{CompressionTier, TierDecision, TierStats, TieringEngine};
