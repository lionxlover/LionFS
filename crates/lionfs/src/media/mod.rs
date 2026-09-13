//! # Hardware-Aware Media Tiering (Pillar IV)
//!
//! RFC-002 §6: "The allocator is a policy engine over device classes,
//! and every policy is a function of geometry the engine probes rather
//! than guesses." This module encodes the RFC's media policy matrix
//! (Table 12) as executable types:
//!
//! * [`zns`] -- the zone model: zone states, write-pointer tokens,
//!   `plan_append` placement (one file per zone until 85% full), and the
//!   zone table recovered from device reports at mount (recovery state
//!   4, RECONCILE).
//! * [`smr`] -- band-confined sequential placement for SMR: each
//!   actively-written file confined to one band, elevator-swept
//!   reclaims in device-idle windows, and the honest hard failure
//!   (random writes to host-managed pools rejected at open time).
//! * [`alignment`] -- the universal alignment engine: 4K/16K/64K
//!   page-cluster classes from probed geometry, extent rounding
//!   accounted as padding (never as file size), descriptor split/merge,
//!   and the violation counters that make the guarantee measurable.
//! * [`tier`] -- the memory tier hierarchy (DRAM / CXL-PMEM) and the
//!   placement plan that puts the intent journal, metadata leaves, and
//!   dedup bloom filters on PMEM first (§3.4), with the CLWB+fence hint
//!   API for platforms that expose it.

pub mod alignment;
pub mod smr;
pub mod tier;
pub mod zns;

/// The media class of a device or free-space run. Drives every placement
/// decision; determined by `pal::geometry` probing plus (for zoned
/// devices) the zone report.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum MediaClass {
    /// NVMe Zoned Namespace (host-managed): zone-append writes.
    NvmeZns,
    /// Conventional NVMe / SSD: queued writes, FUA at commit.
    Nvme,
    /// SATA/SAS SSD (no zone semantics, FTL visible).
    Ssd,
    /// Shingled magnetic recording, host-managed: band-sequential.
    HddSmr,
    /// Conventional PMR HDD: outer-LBA-biased free-space runs.
    HddPmr,
    /// CXL-attached persistent memory: journal/leaves/filters tier.
    CxlPmem,
    /// Unknown/plain image file (tests, loop devices).
    Other,
}

impl MediaClass {
    /// The alignment unit class for placement on this media
    /// (RFC-002 Table 12, "Alignment unit" column).
    #[must_use]
    pub fn natural_alignment(&self) -> u32 {
        match self {
            Self::NvmeZns | Self::Nvme => 4096,
            Self::Ssd => 4096,
            Self::HddSmr => 262_144_000, // band, 256 MiB typical
            Self::HddPmr => 1_048_576,   // 1-4 MiB merge window
            Self::CxlPmem => 64,
            Self::Other => 4096,
        }
    }

    /// Whether writes to this media are append-only (zone/band
    /// semantics constrain placement).
    #[must_use]
    pub fn is_sequential_required(&self) -> bool {
        matches!(self, Self::NvmeZns | Self::HddSmr)
    }

    /// Whether the media has device-side redundancy the pool engine
    /// should leave alone (mirror/parity is LionFS's job above it).
    #[must_use]
    pub fn has_native_redundancy(&self) -> bool {
        false
    }

    /// Human-readable name (health-bus strings).
    #[must_use]
    pub fn name(&self) -> &'static str {
        match self {
            Self::NvmeZns => "nvme-zns",
            Self::Nvme => "nvme",
            Self::Ssd => "ssd",
            Self::HddSmr => "hdd-smr",
            Self::HddPmr => "hdd-pmr",
            Self::CxlPmem => "cxl-pmem",
            Self::Other => "other",
        }
    }
}

/// The placement policy row of RFC-002 Table 12, resolved per media.
#[derive(Debug, Clone, Copy)]
pub struct MediaPolicy {
    pub media: MediaClass,
    /// Placement strategy identifier (e.g. "one file per zone until 85%").
    pub placement: Placement,
    /// Alignment unit in bytes (probed at mkfs, stored in the
    /// superblock's geometry triple).
    pub alignment_unit: u32,
    /// Whether the write path uses zone/band append semantics.
    pub append_semantics: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Placement {
    /// ZNS: one file per zone until 85% full.
    OneFilePerZone,
    /// SSD: locality clusters, speculative extents.
    LocalityClusters,
    /// SMR: band-confined sequential.
    BandConfined,
    /// PMR: outer-LBA-biased free-space runs.
    OuterLbaRuns,
    /// PMEM: journal/leaves/filters tier.
    PmemFirst,
    /// Default: best effort.
    BestEffort,
}

/// Resolves the policy for a media class. Geometry overrides
/// (`optimal_io` from probing) refine `alignment_unit` at mkfs time.
#[must_use]
pub fn policy_for(media: MediaClass, probed_alignment: Option<u32>) -> MediaPolicy {
    let (placement, append) = match media {
        MediaClass::NvmeZns => (Placement::OneFilePerZone, true),
        MediaClass::Nvme | MediaClass::Ssd => (Placement::LocalityClusters, false),
        MediaClass::HddSmr => (Placement::BandConfined, true),
        MediaClass::HddPmr => (Placement::OuterLbaRuns, false),
        MediaClass::CxlPmem => (Placement::PmemFirst, false),
        MediaClass::Other => (Placement::BestEffort, false),
    };
    MediaPolicy {
        media,
        placement,
        alignment_unit: probed_alignment.unwrap_or_else(|| media.natural_alignment()),
        append_semantics: append,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn policy_matrix_matches_rfc_table_12() {
        let zns = policy_for(MediaClass::NvmeZns, None);
        assert_eq!(zns.placement, Placement::OneFilePerZone);
        assert!(zns.append_semantics);
        let smr = policy_for(MediaClass::HddSmr, None);
        assert_eq!(smr.placement, Placement::BandConfined);
        let pmr = policy_for(MediaClass::HddPmr, None);
        assert_eq!(pmr.placement, Placement::OuterLbaRuns);
        assert!(!pmr.append_semantics);
        let pmem = policy_for(MediaClass::CxlPmem, None);
        assert_eq!(pmem.placement, Placement::PmemFirst);
    }

    #[test]
    fn probed_alignment_overrides_default() {
        let p = policy_for(MediaClass::Nvme, Some(16_384));
        assert_eq!(p.alignment_unit, 16_384);
        let q = policy_for(MediaClass::Nvme, None);
        assert_eq!(q.alignment_unit, 4096);
    }

    #[test]
    fn sequential_requirement_reflects_zone_semantics() {
        assert!(MediaClass::NvmeZns.is_sequential_required());
        assert!(MediaClass::HddSmr.is_sequential_required());
        assert!(!MediaClass::Nvme.is_sequential_required());
        assert!(!MediaClass::CxlPmem.is_sequential_required());
    }

    #[test]
    fn names_are_stable_for_health_bus() {
        assert_eq!(MediaClass::NvmeZns.name(), "nvme-zns");
        assert_eq!(MediaClass::CxlPmem.name(), "cxl-pmem");
    }
}
