//! Tiered adaptive compression policy (RFC-002 §7.1, Table 13).
//!
//! A corpus of 40% repeating records, 35% dictionary text, and 25%
//! incompressible bytes compressed 2.90x at zstd level 3 while writing
//! at 407 MiB/s on a 2-vCPU container, and the level sweep showed the
//! honest cliff: level 9 buys 0.08x more ratio for 6.8x the CPU. The
//! 2.0 pipeline therefore adapts **per inode**: the first two clusters
//! written measure compressibility and latency, and the policy engine
//! pins the file to a tier.
//!
//! | Tier | Codec | Write path target | Integrity |
//! |------|-------|-------------------|-----------|
//! | Hot | LZ4 block | inline, no added latency | xxHash64 per page |
//! | Warm | zstd level 3 | 1-3 GB/s per core | BLAKE3 per cluster |
//! | Cold | zstd level 9+ / QAT | background, ratio-first | BLAKE3 per cluster |
//! | Raw fallback | none | incompressible clusters | xxHash64, RAW flag |

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;

use crate::common::constants::{COMPRESSION_LZ4, COMPRESSION_ZSTD};

/// The compression decision for a cluster.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CompressionTier {
    /// LZ4 block codec: decompresses at multiple GB/s per core.
    Hot,
    /// zstd level 3: the warm bulk default.
    Warm,
    /// zstd level 9+: background, ratio-first.
    Cold,
    /// Stored raw: the cluster did not compress (ratio < 1.2).
    Raw,
}

impl CompressionTier {
    #[must_use]
    pub fn codec_id(self) -> u8 {
        match self {
            Self::Hot | Self::Raw => COMPRESSION_LZ4, // Raw's "codec" is a marker; payload is stored.
            Self::Warm | Self::Cold => COMPRESSION_ZSTD,
        }
    }

    #[must_use]
    pub fn zstd_level(self) -> i32 {
        match self {
            Self::Warm => 3,
            Self::Cold => 12, // "9+": ratio-first, paid in idle windows
            Self::Hot | Self::Raw => 0,
        }
    }

    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            Self::Hot => "hot-lz4",
            Self::Warm => "warm-zstd3",
            Self::Cold => "cold-zstd12",
            Self::Raw => "raw",
        }
    }
}

/// What one measured cluster told us.
#[derive(Debug, Clone, Copy)]
pub struct ClusterMeasurement {
    pub logical_bytes: u64,
    pub compressed_bytes: u64,
    /// Nanoseconds the compression took (latency budget input).
    pub encode_ns: u64,
}

impl ClusterMeasurement {
    #[must_use]
    pub fn ratio(self) -> f64 {
        if self.compressed_bytes == 0 {
            return 0.0;
        }
        self.logical_bytes as f64 / self.compressed_bytes as f64
    }

    /// Throughput of the encode, in bytes/second.
    #[must_use]
    pub fn throughput_bps(self) -> f64 {
        if self.encode_ns == 0 {
            return f64::INFINITY;
        }
        self.logical_bytes as f64 / (self.encode_ns as f64 / 1e9)
    }
}

/// The decision emitted for the next cluster.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TierDecision {
    /// Use this tier.
    Tier(CompressionTier),
    /// Keep measuring: the first two clusters are probes.
    Probe,
}

/// Per-inode tiering state, driven by the probe protocol.
#[derive(Debug, Default)]
pub struct InodeTierState {
    probes: Vec<ClusterMeasurement>,
    pinned: Option<CompressionTier>,
}

/// The per-file tier policy engine.
#[derive(Debug, Default)]
pub struct TieringEngine {
    states: Mutex<std::collections::HashMap<u64, InodeTierState>>,
    stats: TierStats,
}

/// Counters for the health bus.
#[derive(Debug, Default)]
pub struct TierStats {
    pub clusters_decided: AtomicU64,
    pub clusters_raw: AtomicU64,
    pub clusters_hot: AtomicU64,
    pub clusters_warm: AtomicU64,
    pub clusters_cold: AtomicU64,
    pub recompressions_to_cold: AtomicU64,
}

/// Number of probe clusters before pinning (RFC: "the first two
/// clusters written measure compressibility and latency").
pub const PROBE_CLUSTERS: usize = 2;

/// Ratio below which compression is pointless (the raw fallback).
pub const MIN_USEFUL_RATIO: f64 = 1.2;

/// Throughput floor for the hot tier: if zstd-3 encode cannot sustain
/// this, the file is latency-sensitive and pins to LZ4.
pub const HOT_TIER_FLOOR_BPS: f64 = 250.0 * 1024.0 * 1024.0;

impl TieringEngine {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    pub fn stats(&self) -> &TierStats {
        &self.stats
    }

    /// The decision for the next cluster of `inode`. `measurements` feed
    /// the probe state; the engine pins after `PROBE_CLUSTERS` probes.
    pub fn decide(&self, inode: u64, measurement: Option<ClusterMeasurement>) -> TierDecision {
        let mut states = self.states.lock().expect("tier state lock");
        let state = states.entry(inode).or_default();

        if let Some(m) = measurement {
            state.probes.push(m);
        }

        // Pinned files keep their tier (until punch-through or a cold
        // re-compression event changes it through `retire_pin`).
        if let Some(t) = state.pinned {
            self.stats.clusters_decided.fetch_add(1, Ordering::Relaxed);
            return TierDecision::Tier(t);
        }

        let probes = &state.probes;
        if probes.len() < PROBE_CLUSTERS {
            // Not enough evidence: default to warm for bulk-ish data --
            // the codec the 1.x measurements actually validated.
            self.stats.clusters_decided.fetch_add(1, Ordering::Relaxed);
            return TierDecision::Probe;
        }

        // Pin from the evidence.
        let avg_ratio = probes.iter().map(|m| m.ratio()).sum::<f64>() / probes.len() as f64;
        let avg_bps = probes.iter().map(|m| m.throughput_bps()).sum::<f64>() / probes.len() as f64;
        let tier = if avg_ratio < MIN_USEFUL_RATIO {
            CompressionTier::Raw
        } else if avg_bps < HOT_TIER_FLOOR_BPS {
            // zstd-3 too slow to keep up: latency-sensitive file -> LZ4.
            CompressionTier::Hot
        } else {
            CompressionTier::Warm
        };
        state.pinned = Some(tier);
        self.stats.clusters_decided.fetch_add(1, Ordering::Relaxed);
        self.note(tier);
        TierDecision::Tier(tier)
    }

    /// Cold re-compression event (RFC §7.1: clusters that go cold and
    /// unmodified for a scrub cycle re-compress into the cold tier
    /// during idle windows).
    pub fn pin_cold(&self, inode: u64) {
        let mut states = self.states.lock().expect("tier state lock");
        let state = states.entry(inode).or_default();
        state.pinned = Some(CompressionTier::Cold);
        self.stats
            .recompressions_to_cold
            .fetch_add(1, Ordering::Relaxed);
    }

    /// Punch-through retires the pin (the file leaves the cluster path).
    pub fn retire_pin(&self, inode: u64) {
        let mut states = self.states.lock().expect("tier state lock");
        if let Some(state) = states.get_mut(&inode) {
            state.pinned = None;
        }
    }

    fn note(&self, tier: CompressionTier) {
        match tier {
            CompressionTier::Raw => self.stats.clusters_raw.fetch_add(1, Ordering::Relaxed),
            CompressionTier::Hot => self.stats.clusters_hot.fetch_add(1, Ordering::Relaxed),
            CompressionTier::Warm => self.stats.clusters_warm.fetch_add(1, Ordering::Relaxed),
            CompressionTier::Cold => self.stats.clusters_cold.fetch_add(1, Ordering::Relaxed),
        };
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn measure(logical: u64, compressed: u64, ns: u64) -> ClusterMeasurement {
        ClusterMeasurement {
            logical_bytes: logical,
            compressed_bytes: compressed,
            encode_ns: ns,
        }
    }

    #[test]
    fn probe_then_pin_warm_for_bulk_data() {
        let eng = TieringEngine::new();
        // Two probes: 128 KiB clusters compressing to ~44 KiB (ratio ~2.9)
        // at zstd-3-class speed (407 MiB/s -> ~318 us/cluster).
        assert_eq!(
            eng.decide(1, Some(measure(128 * 1024, 44 * 1024, 318_000))),
            TierDecision::Probe
        );
        let d = eng.decide(1, Some(measure(128 * 1024, 45 * 1024, 320_000)));
        assert_eq!(d, TierDecision::Tier(CompressionTier::Warm));
        // Pinned: subsequent decisions are instant.
        assert_eq!(
            eng.decide(1, None),
            TierDecision::Tier(CompressionTier::Warm)
        );
    }

    #[test]
    fn incompressible_pins_raw() {
        let eng = TieringEngine::new();
        eng.decide(2, Some(measure(128 * 1024, 126 * 1024, 100_000)));
        let d = eng.decide(2, Some(measure(128 * 1024, 127 * 1024, 100_000)));
        assert_eq!(d, TierDecision::Tier(CompressionTier::Raw));
        assert!(eng.stats().clusters_raw.load(Ordering::Relaxed) >= 1);
    }

    #[test]
    fn slow_encode_pins_hot_lz4() {
        let eng = TieringEngine::new();
        // Ratio is fine but encode is far below the floor: latency-bound.
        eng.decide(3, Some(measure(128 * 1024, 40 * 1024, 4_000_000)));
        let d = eng.decide(3, Some(measure(128 * 1024, 41 * 1024, 4_100_000)));
        assert_eq!(d, TierDecision::Tier(CompressionTier::Hot));
        assert_eq!(CompressionTier::Hot.codec_id(), COMPRESSION_LZ4);
    }

    #[test]
    fn separate_files_have_separate_pins() {
        let eng = TieringEngine::new();
        eng.decide(10, Some(measure(128 * 1024, 40 * 1024, 300_000)));
        eng.decide(10, Some(measure(128 * 1024, 41 * 1024, 300_000)));
        // File 11 is still probing.
        assert_eq!(
            eng.decide(11, Some(measure(128 * 1024, 41 * 1024, 300_000))),
            TierDecision::Probe
        );
    }

    #[test]
    fn cold_repin_and_retire() {
        let eng = TieringEngine::new();
        eng.decide(20, Some(measure(128 * 1024, 40 * 1024, 300_000)));
        eng.decide(20, Some(measure(128 * 1024, 41 * 1024, 300_000)));
        eng.pin_cold(20);
        assert_eq!(
            eng.decide(20, None),
            TierDecision::Tier(CompressionTier::Cold)
        );
        assert_eq!(CompressionTier::Cold.zstd_level(), 12);
        eng.retire_pin(20);
        // Pin retired, probes exhausted -> re-pins from evidence.
        let d = eng.decide(20, None);
        assert_eq!(d, TierDecision::Tier(CompressionTier::Warm));
    }

    #[test]
    fn ratio_math() {
        let m = measure(128 * 1024, 44 * 1024, 318_000);
        let r = m.ratio();
        assert!((r - 2.90).abs() < 0.1, "ratio {r}");
        let t = m.throughput_bps();
        assert!(t > 300.0 * 1024.0 * 1024.0, "throughput {t}");
        assert_eq!(measure(0, 0, 1).ratio(), 0.0);
        assert!(measure(1, 1, 0).throughput_bps().is_infinite());
    }
}
