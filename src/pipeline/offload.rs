//! Accelerator backend selection (RFC-002 §7.1, §7.3).
//!
//! Hardware acceleration is a first-class path: Intel QAT devices
//! compress and checksum entire clusters in flight, AVX-512 vectorizes
//! the codec kernels themselves, and the transform pipeline selects
//! among software, SIMD, and QAT backends **per submission** based on
//! availability and queue depth, with the selection itself a measured
//! decision recorded in the health bus.
//!
//! The honesty rule (§7.3) applies to offload numbers specifically:
//! QAT-assisted throughput is reported per backend, per queue depth, and
//! against the software path on the same host in the same run, because
//! "a hardware number without a software control is exactly the kind of
//! claim the honesty rule exists to prevent."

use std::sync::atomic::{AtomicU64, Ordering};

/// Available accelerator backends.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AcceleratorBackend {
    /// Portable software codec path.
    Software,
    /// CPU SIMD kernels (AVX-2/AVX-512/NEON as autodetected by the
    /// codec crates themselves).
    Simd,
    /// Intel QAT device offload (compress + checksum in flight).
    Qat,
}

impl AcceleratorBackend {
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            Self::Software => "software",
            Self::Simd => "simd",
            Self::Qat => "qat",
        }
    }
}

/// The selection input for one submission.
#[derive(Debug, Clone, Copy)]
pub struct SelectionInput {
    /// Whether a QAT device is configured and its queue has room.
    pub qat_available: bool,
    /// Current QAT queue depth (devices degrade past ~64 in-flight).
    pub qat_queue_depth: u32,
    /// Payload bytes of the submission.
    pub payload_bytes: u64,
    /// Whether the CPU supports the SIMD codec kernels.
    pub simd_available: bool,
}

/// Policy constants, documented as decisions rather than magic numbers.
pub const QAT_QUEUE_DEPTH_FLOOR: u32 = 64;
/// Below this payload size, offload submission overhead dominates.
pub const QAT_MIN_PAYLOAD: u64 = 16 * 1024;

/// Selection counters (the health-bus record of each decision).
#[derive(Debug, Default)]
pub struct OffloadStats {
    pub selected_software: AtomicU64,
    pub selected_simd: AtomicU64,
    pub selected_qat: AtomicU64,
    /// Times QAT was available but rejected by policy (queue depth or
    /// payload size) -- the observable that keeps "available" honest.
    pub qat_available_but_rejected: AtomicU64,
}

pub static STATS: OffloadStats = OffloadStats {
    selected_software: AtomicU64::new(0),
    selected_simd: AtomicU64::new(0),
    selected_qat: AtomicU64::new(0),
    qat_available_but_rejected: AtomicU64::new(0),
};

/// Selects the backend for one submission.
///
/// The policy, stated plainly:
/// * QAT wins when the device is present, its queue is under the depth
///   floor, and the payload is large enough to amortize submission.
/// * SIMD wins over scalar software whenever the kernels exist (the
///   codec crates detect this themselves; the selection records intent).
/// * Software is the floor that is always correct.
#[must_use]
pub fn select(input: SelectionInput) -> AcceleratorBackend {
    if input.qat_available {
        if input.qat_queue_depth < QAT_QUEUE_DEPTH_FLOOR && input.payload_bytes >= QAT_MIN_PAYLOAD {
            STATS.selected_qat.fetch_add(1, Ordering::Relaxed);
            return AcceleratorBackend::Qat;
        }
        STATS
            .qat_available_but_rejected
            .fetch_add(1, Ordering::Relaxed);
    }
    if input.simd_available {
        STATS.selected_simd.fetch_add(1, Ordering::Relaxed);
        return AcceleratorBackend::Simd;
    }
    STATS.selected_software.fetch_add(1, Ordering::Relaxed);
    AcceleratorBackend::Software
}

/// Whether the CPU exposes SIMD codec kernels. The codecs probe this
/// internally at their own level; this surface exists so the selection
/// decision can be recorded and asserted in tests.
#[must_use]
pub fn simd_available() -> bool {
    #[cfg(target_arch = "x86_64")]
    {
        std::arch::is_x86_feature_detected!("avx2") || std::arch::is_x86_feature_detected!("sse4.2")
    }
    #[cfg(target_arch = "aarch64")]
    {
        // NEON is baseline on aarch64.
        true
    }
    #[cfg(not(any(target_arch = "x86_64", target_arch = "aarch64")))]
    {
        false
    }
}

/// One-line health summary.
pub fn health_summary() -> String {
    format!(
        "software={} simd={} qat={} qat_rejected={}",
        STATS.selected_software.load(Ordering::Relaxed),
        STATS.selected_simd.load(Ordering::Relaxed),
        STATS.selected_qat.load(Ordering::Relaxed),
        STATS.qat_available_but_rejected.load(Ordering::Relaxed)
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn input(qat: bool, depth: u32, payload: u64, simd: bool) -> SelectionInput {
        SelectionInput {
            qat_available: qat,
            qat_queue_depth: depth,
            payload_bytes: payload,
            simd_available: simd,
        }
    }

    #[test]
    fn qat_wins_when_available_and_healthy() {
        let b = select(input(true, 8, 128 * 1024, true));
        assert_eq!(b, AcceleratorBackend::Qat);
    }

    #[test]
    fn deep_qat_queue_falls_back() {
        let b = select(input(true, QAT_QUEUE_DEPTH_FLOOR, 128 * 1024, true));
        assert_ne!(b, AcceleratorBackend::Qat);
        assert!(STATS.qat_available_but_rejected.load(Ordering::Relaxed) >= 1);
    }

    #[test]
    fn small_payload_rejects_qat() {
        // 8 KiB under the 16 KiB floor: offload overhead dominates.
        let b = select(input(true, 1, 8 * 1024, false));
        assert_ne!(b, AcceleratorBackend::Qat);
    }

    #[test]
    fn simd_beats_software_when_present() {
        assert_eq!(
            select(input(false, 0, 4096, true)),
            AcceleratorBackend::Simd
        );
        assert_eq!(
            select(input(false, 0, 4096, false)),
            AcceleratorBackend::Software
        );
    }

    #[test]
    fn health_summary_shape() {
        let s = health_summary();
        assert!(s.starts_with("software="));
        assert!(s.contains("qat_rejected="));
    }
}
