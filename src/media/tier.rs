//! Memory tiers and CXL PMEM placement (RFC-002 §3.4).
//!
//! CXL-attached persistent memory enters the tier hierarchy as a
//! byte-addressable cache and journal tier. Metadata leaves, the intent
//! journal, and the dedup bloom filters are placed there first: the
//! journal's fdatasync collapses to a cache-line writeback with CLWB
//! plus a fence, roughly two orders of magnitude cheaper than a flash
//! flush, which transforms the fsync-heavy workload class. Device DMA is
//! steered into CXL memory for read-modify-write payloads (parity
//! deltas, cluster recompression) so the transform never round-trips
//! through DRAM it does not need.
//!
//! The engine treats the tier as a first-class placement target with its
//! own bandwidth accounting rather than as a block device, because
//! treating PMEM as a disk is precisely the anti-pattern that wastes it.

/// A memory tier in the hierarchy.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum MemoryTier {
    /// DRAM: everything that is transient.
    Dram,
    /// CXL-attached persistent memory: journal, metadata leaves, bloom
    /// filters; CLWB+fence durability.
    CxlPmem,
}

impl MemoryTier {
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            Self::Dram => "dram",
            Self::CxlPmem => "cxl-pmem",
        }
    }

    /// Whether writes to this tier can be made durable with a cache-line
    /// flush instead of a device flush.
    #[must_use]
    pub fn durable_via_clwb(self) -> bool {
        matches!(self, Self::CxlPmem)
    }
}

/// What is being placed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PlacementTarget {
    /// The circular intent journal (§5.1).
    IntentJournal,
    /// B-epsilon metadata leaves.
    MetadataLeaves,
    /// Dedup bloom filters (§7.2).
    DedupBloomFilter,
    /// Parity deltas / cluster recompression staging (§3.4: DMA target).
    RmwStaging,
    /// Ordinary transient data.
    Transient,
}

/// The placement plan output.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TierPlacement {
    pub target: PlacementTarget,
    pub tier: MemoryTier,
    /// For DMA-steered targets: the CXL buffer pool to draw from.
    pub steer_dma: bool,
}

/// Resolves the placement for a target per the RFC-002 §3.4 policy.
#[must_use]
pub fn place(target: PlacementTarget, pmem_available: bool) -> TierPlacement {
    use PlacementTarget as T;
    match target {
        T::IntentJournal | T::MetadataLeaves | T::DedupBloomFilter => {
            if pmem_available {
                TierPlacement {
                    target,
                    tier: MemoryTier::CxlPmem,
                    steer_dma: false,
                }
            } else {
                TierPlacement {
                    target,
                    tier: MemoryTier::Dram,
                    steer_dma: false,
                }
            }
        }
        T::RmwStaging => {
            // DMA steering prefers PMEM when present so transforms land
            // where the device can write them directly.
            TierPlacement {
                target,
                tier: if pmem_available {
                    MemoryTier::CxlPmem
                } else {
                    MemoryTier::Dram
                },
                steer_dma: pmem_available,
            }
        }
        T::Transient => TierPlacement {
            target,
            tier: MemoryTier::Dram,
            steer_dma: false,
        },
    }
}

/// Durability barrier for a tier: on PMEM, CLWB + fence; on DRAM, a
/// device flush (there is nothing cheaper that is honest).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Barrier {
    /// Cache-line writeback + store fence (the cheap journal path).
    ClwbFence,
    /// Full device flush through the ring (FUA).
    DeviceFlush,
}

#[must_use]
pub fn barrier_for(tier: MemoryTier) -> Barrier {
    if tier.durable_via_clwb() {
        Barrier::ClwbFence
    } else {
        Barrier::DeviceFlush
    }
}

/// Issues a cache-line writeback + fence for `region` on x86-64 Linux
/// where CLWB exists; a no-op returning false on other platforms. This
/// is the PAL-shaped seam: the engine calls it opportunistically and
/// falls back to `pal::sync::sync_data` when it returns false.
///
/// # Safety
/// `region` must be valid for writes and remain mapped for the duration
/// of the call.
///
/// (Clippy note: the outer `unsafe` block spans cfg-gated asm on
/// x86-64; on other targets the function body is the trivial `false`
/// arm and the block is absent.)
pub unsafe fn clwb_region(region: &[u8]) -> bool {
    #[cfg(all(target_arch = "x86_64", target_os = "linux"))]
    {
        // CLWB (0x66 0x0F 0xAE /6) then SFENCE. Probe CPUID first.
        if !has_clwb() {
            return false;
        }
        const CACHELINE: usize = 64;
        let start = region.as_ptr();
        let end = start.add(region.len());
        let mut p = start;
        while p < end {
            // SAFETY: p is within [start, end); clwb on a valid address
            // writes back its cache line without faulting.
            unsafe {
                std::arch::asm!(
                    "clwb [{0}]",
                    in(reg) p,
                    options(nostack, preserves_flags)
                );
            }
            p = p.add(CACHELINE);
        }
        // SAFETY: SFENCE orders the writebacks.
        unsafe {
            std::arch::asm!("sfence", options(nostack, preserves_flags));
        }
        true
    }
    #[cfg(not(all(target_arch = "x86_64", target_os = "linux")))]
    {
        let _ = region;
        false
    }
}

#[cfg(all(target_arch = "x86_64", target_os = "linux"))]
fn has_clwb() -> bool {
    use std::sync::OnceLock;
    static HAS: OnceLock<bool> = OnceLock::new();
    *HAS.get_or_init(|| {
        // CPUID leaf 7, subleaf 0, EBX bit 24 = CLWB. Raw CPUID avoids
        // depending on "clwb" being a named rustc target feature.
        // SAFETY: __cpuid_count is safe to call on any x86_64 CPU (leaf
        // 7 is gated by the max-leaf check first); it writes only its
        // return struct.
        // SAFETY: __cpuid_count is safe to call on any x86_64 CPU (leaf
        // 7 gated by the max-leaf check first); it writes only its
        // return struct.
        let max_leaf = unsafe { std::arch::x86_64::__cpuid(0).eax };
        if max_leaf < 7 {
            return false;
        }
        // SAFETY: as above, with the subleaf argument.
        let info = unsafe { std::arch::x86_64::__cpuid_count(7, 0) };
        (info.ebx >> 24) & 1 == 1
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn journal_leaves_filters_prefer_pmem() {
        for t in [
            PlacementTarget::IntentJournal,
            PlacementTarget::MetadataLeaves,
            PlacementTarget::DedupBloomFilter,
        ] {
            let p = place(t, true);
            assert_eq!(p.tier, MemoryTier::CxlPmem);
            assert!(!p.steer_dma);
        }
    }

    #[test]
    fn without_pmem_everything_is_dram() {
        let p = place(PlacementTarget::IntentJournal, false);
        assert_eq!(p.tier, MemoryTier::Dram);
        assert!(!p.steer_dma);
    }

    #[test]
    fn rmw_staging_steers_dma_to_pmem() {
        let p = place(PlacementTarget::RmwStaging, true);
        assert_eq!(p.tier, MemoryTier::CxlPmem);
        assert!(p.steer_dma);
        let p = place(PlacementTarget::RmwStaging, false);
        assert_eq!(p.tier, MemoryTier::Dram);
        assert!(!p.steer_dma);
    }

    #[test]
    fn transient_stays_dram() {
        let p = place(PlacementTarget::Transient, true);
        assert_eq!(p.tier, MemoryTier::Dram);
    }

    #[test]
    fn barriers_match_tiers() {
        assert_eq!(barrier_for(MemoryTier::CxlPmem), Barrier::ClwbFence);
        assert_eq!(barrier_for(MemoryTier::Dram), Barrier::DeviceFlush);
    }

    #[test]
    fn clwb_region_is_honest_about_support() {
        // On non-x86_64 or CPUs without CLWB this returns false, which
        // the engine turns into a sync_data fallback. Either way it must
        // not crash.
        let mut buf = [0u8; 128];
        let ok = unsafe { clwb_region(&mut buf) };
        let _ = ok;
    }
}
