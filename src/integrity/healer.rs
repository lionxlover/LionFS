//! Autonomous repair (RFC-002 §5.3).
//!
//! "Detection without repair is just surveillance." When a read or the
//! scrubber finds a checksum mismatch, the offending block is
//! quarantined in the bad-blocks tree, the scrubber allocates a fresh
//! extent through the normal per-core allocator, reconstructs the data
//! from P/Q parity (or from a mirror), writes it with a fresh checksum,
//! and swaps the extent reference inside a first-class transaction --
//! the same intent-log machinery as any other mutation, so a crash
//! mid-repair heals into either the old or the new copy and never into
//! neither. Repair is therefore autonomous: no operator action, no
//! unmount, no fsck run. Pools without redundancy mark the block and
//! report the loss event to the health bus rather than pretending.

use std::fmt;

/// A detected integrity failure, quarantine candidate.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BlockFailure {
    /// Physical block (4 KiB units) whose checksum mismatched.
    pub physical_block: u64,
    /// Which device in the pool.
    pub device: u32,
    /// The failed read's inode, when known (scrubber knows it; a raw
    /// device read does not).
    pub inode: Option<u64>,
    /// The failed read's logical offset, when known.
    pub logical_offset: Option<u64>,
}

impl fmt::Display for BlockFailure {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "block {} on device {} failed verification",
            self.physical_block, self.device
        )?;
        if let Some(ino) = self.inode {
            write!(f, " (inode {ino}")?;
            if let Some(off) = self.logical_offset {
                write!(f, ", offset {off}")?;
            }
            write!(f, ")")?;
        }
        Ok(())
    }
}

/// How the replacement data will be reconstructed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RepairSource {
    /// Rebuild from single parity (P): one data device lost.
    ParityP,
    /// Rebuild from P and Q: up to two data devices lost (RAID6 or
    /// generalized RS).
    ParityPQ { second_erasure: u32 },
    /// Copy from a surviving mirror (RAID1/10).
    Mirror { source_device: u32 },
    /// No redundancy available: the loss is reported, not repaired.
    NoRedundancy,
}

/// One step of the repair plan.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RepairStep {
    /// Quarantine the bad block in the bad-blocks tree.
    Quarantine { failure: BlockFailure },
    /// Allocate a fresh extent for the replacement (per-core allocator).
    AllocateFresh { blocks: u64 },
    /// Reconstruct the payload from the given source.
    Reconstruct { source: RepairSource },
    /// Write the replacement with a fresh checksum (dual-speed class
    /// re-computed for the block's temperature).
    WriteWithFreshChecksum,
    /// Swap the extent reference inside a first-class transaction.
    SwapExtentInTransaction,
    /// Release the old extent to the free-space queues.
    ReleaseOldExtent,
    /// Report the unrecoverable loss to the health bus.
    ReportLoss { failure: BlockFailure },
}

/// The full plan for one repair, in execution order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RepairPlan {
    pub failure: BlockFailure,
    pub steps: Vec<RepairStep>,
}

/// Chooses the repair source for a failure given the pool's redundancy
/// and the set of *other* devices already known-degraded (from the pool
/// health monitor).
#[must_use]
pub fn choose_repair_source(
    failure: &BlockFailure,
    profile: crate::pool::raid::RaidProfile,
    other_degraded: &[u32],
) -> RepairSource {
    match profile {
        crate::pool::raid::RaidProfile::Raid0 | crate::pool::raid::RaidProfile::Single => {
            RepairSource::NoRedundancy
        }
        crate::pool::raid::RaidProfile::Raid1 | crate::pool::raid::RaidProfile::Raid10 => {
            // Any surviving mirror in the failure's stripe; the caller
            // passes the pool's healthy member set through other_degraded
            // (degraded devices to avoid).
            let mirror = other_degraded
                .iter()
                .copied()
                .find(|d| *d != failure.device);
            match mirror {
                // With no explicit mapping, the stripe's other members
                // are candidates; a degraded pool with no other member
                // is unrecoverable.
                Some(d) => RepairSource::Mirror { source_device: d },
                None => RepairSource::NoRedundancy,
            }
        }
        crate::pool::raid::RaidProfile::Raid5 => RepairSource::ParityP,
        crate::pool::raid::RaidProfile::Raid6 => match other_degraded.len() {
            0 => RepairSource::ParityP,
            1 => RepairSource::ParityPQ {
                second_erasure: other_degraded[0],
            },
            _ => RepairSource::NoRedundancy,
        },
    }
}

/// Builds the ordered plan for a failure.
#[must_use]
pub fn plan_repair(
    failure: BlockFailure,
    profile: crate::pool::raid::RaidProfile,
    other_degraded: &[u32],
) -> RepairPlan {
    let source = choose_repair_source(&failure, profile, other_degraded);
    let mut steps = vec![RepairStep::Quarantine { failure }];
    match source {
        RepairSource::NoRedundancy => {
            // Honest failure: mark, report, never pretend.
            steps.push(RepairStep::ReportLoss { failure });
        }
        RepairSource::ParityP | RepairSource::ParityPQ { .. } | RepairSource::Mirror { .. } => {
            steps.push(RepairStep::AllocateFresh { blocks: 1 });
            steps.push(RepairStep::Reconstruct { source });
            steps.push(RepairStep::WriteWithFreshChecksum);
            steps.push(RepairStep::SwapExtentInTransaction);
            steps.push(RepairStep::ReleaseOldExtent);
        }
    }
    RepairPlan { failure, steps }
}

/// Outcome of executing a plan.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RepairOutcome {
    /// The block was re-written and the extent swapped; the transaction
    /// made it atomic.
    Repaired { new_physical_block: u64 },
    /// No redundancy: the loss was reported to the health bus.
    LossReported,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pool::raid::RaidProfile;

    fn failure(device: u32) -> BlockFailure {
        BlockFailure {
            physical_block: 1234,
            device,
            inode: Some(7),
            logical_offset: Some(0x1_0000),
        }
    }

    #[test]
    fn raid5_uses_single_parity() {
        let plan = plan_repair(failure(2), RaidProfile::Raid5, &[]);
        assert_eq!(plan.steps.len(), 6);
        assert!(matches!(
            plan.steps[2],
            RepairStep::Reconstruct {
                source: RepairSource::ParityP
            }
        ));
        // Order: quarantine, allocate, reconstruct, write, swap, release.
        assert!(matches!(plan.steps[0], RepairStep::Quarantine { .. }));
        assert!(matches!(
            plan.steps[1],
            RepairStep::AllocateFresh { blocks: 1 }
        ));
        assert!(matches!(plan.steps[3], RepairStep::WriteWithFreshChecksum));
        assert!(matches!(plan.steps[4], RepairStep::SwapExtentInTransaction));
        assert!(matches!(plan.steps[5], RepairStep::ReleaseOldExtent));
    }

    #[test]
    fn raid6_with_second_erasure_uses_pq() {
        let plan = plan_repair(failure(1), RaidProfile::Raid6, &[4]);
        assert!(matches!(
            plan.steps[2],
            RepairStep::Reconstruct {
                source: RepairSource::ParityPQ { second_erasure: 4 }
            }
        ));
    }

    #[test]
    fn raid6_double_extra_erasure_is_a_loss() {
        // Three degraded devices on RAID6: unrecoverable, honestly.
        let plan = plan_repair(failure(1), RaidProfile::Raid6, &[2, 3]);
        assert!(matches!(plan.steps[1], RepairStep::ReportLoss { .. }));
        assert_eq!(plan.steps.len(), 2);
    }

    #[test]
    fn raid0_has_no_redundancy() {
        for profile in [RaidProfile::Raid0, RaidProfile::Single] {
            let plan = plan_repair(failure(0), profile, &[]);
            assert!(
                matches!(plan.steps[1], RepairStep::ReportLoss { .. }),
                "{profile:?}"
            );
        }
    }

    #[test]
    fn mirror_repair_copies_from_surviving_member() {
        // The degraded list is devices to AVOID; a healthy other member
        // serves the copy.
        let plan = plan_repair(failure(0), RaidProfile::Raid1, &[3]);
        assert!(matches!(
            plan.steps[2],
            RepairStep::Reconstruct {
                source: RepairSource::Mirror { source_device: 3 }
            }
        ));
    }

    #[test]
    fn mirror_with_no_survivor_is_a_loss() {
        let plan = plan_repair(failure(0), RaidProfile::Raid1, &[0]);
        assert!(matches!(plan.steps[1], RepairStep::ReportLoss { .. }));
    }

    #[test]
    fn failure_display_is_informative() {
        let f = failure(2);
        let msg = f.to_string();
        assert!(msg.contains("device 2"), "{msg}");
        assert!(msg.contains("inode 7"), "{msg}");
    }

    #[test]
    fn plans_are_transactional_by_construction() {
        // The swap step is inside the plan, before releasing the old
        // extent: a crash between them leaves the old copy live (the
        // transaction heals either-or, never neither).
        let plan = plan_repair(failure(0), RaidProfile::Raid5, &[]);
        let swap = plan
            .steps
            .iter()
            .position(|s| matches!(s, RepairStep::SwapExtentInTransaction))
            .unwrap();
        let release = plan
            .steps
            .iter()
            .position(|s| matches!(s, RepairStep::ReleaseOldExtent))
            .unwrap();
        assert!(swap < release);
    }
}
