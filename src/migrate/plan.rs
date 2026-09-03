//! Import planning (RFC-004 §9.3): which strategy, what it costs, how
//! progress is staged.
//!
//! The strategy decision is small and explicit:
//!
//! * **TarStream** (default): source is mounted and readable through
//!   its own driver; walk + stream. Works for every `FsKind` the host
//!   can mount, including `Other`/`Unknown`-but-mounted.
//! * **PerFile**: the source has semantics the tar layer cannot carry
//!   faithfully (NTFS alternate data streams, HFS+ resource forks,
//!   ext4 immutable+inline flags, POSIX ACLs beyond mode bits) -- each
//!   file imports through per-file xattr-aware code paths.
//! * **RawBlock**: the source refuses to mount (damage, unsupported
//!   feature bits) -- carve recognizable structures from a block image
//!   with explicit human sign-off, and label everything recovered with
//!   provenance. This is a last-resort path, always operator-gated.

use super::detect::FsKind;

/// How the import reads the source.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ImportStrategy {
    /// Walk the mounted source, emit a tar stream, write it through
    /// the destination's POSIX path (the default).
    TarStream,
    /// Per-file import with full metadata fidelity (xattrs, forks,
    /// streams, ACLs).
    PerFile,
    /// Block-carving recovery from an unmountable image; operator
    /// sign-off required.
    RawBlock,
}

impl ImportStrategy {
    /// Stable policy tag.
    #[must_use]
    pub fn tag(self) -> &'static str {
        match self {
            Self::TarStream => "tar-stream",
            Self::PerFile => "per-file",
            Self::RawBlock => "raw-block",
        }
    }
}

/// The planned import.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ImportPlan {
    /// What the source was detected as.
    pub source: FsKind,
    /// Approximate source bytes (the `df`-visible used bytes).
    pub source_bytes: u64,
    /// The strategy chosen and why.
    pub strategy: ImportStrategy,
    /// The reason string (surfaced in `lfs_migrate --dry-run`).
    pub reason: String,
    /// True when the plan needs an operator's explicit `--i-understand`
    /// before it will execute (RawBlock always, PerFile when the
    /// source is unmounted).
    pub needs_operator_signoff: bool,
    /// Progress granularity: how many byte-chunks the progress bar
    /// reports (bounded so a 10 PiB source still renders smoothly).
    pub progress_steps: u64,
}

impl ImportPlan {
    /// Builds a plan from detection + accounting. The strategy table
    /// is RFC-004 §9.3 Table 6, reproduced in code:
    ///
    /// | Source | Strategy | Why |
    /// |--------|----------|-----|
    /// | Ext4 / Xfs / Btrfs / F2fs | TarStream | mounted, tar carries semantics |
    /// | Zfs | TarStream | (snapshot-aware walk at chosen snapshot) |
    /// | Fat32 / ExFat | TarStream | trivial semantics |
    /// | Ntfs | PerFile | alternate data streams |
    /// | HfsPlus | PerFile | resource forks |
    /// | Apfs | PerFile | named forks + clonefile metadata |
    /// | Other / Unknown mounted | TarStream | driver handles semantics |
    /// | anything unmounted | RawBlock | carve with sign-off |
    #[must_use]
    pub fn new(source: FsKind, source_bytes: u64, mounted: bool) -> Self {
        let (strategy, reason, signoff) = if !mounted {
            (
                ImportStrategy::RawBlock,
                format!("source {source} not mountable: block-carve recovery"),
                true,
            )
        } else {
            match source {
                FsKind::Ntfs => (
                    ImportStrategy::PerFile,
                    "ntfs: alternate data streams need per-file fidelity".to_owned(),
                    false,
                ),
                FsKind::HfsPlus => (
                    ImportStrategy::PerFile,
                    "hfs+: resource forks need per-file fidelity".to_owned(),
                    false,
                ),
                FsKind::Apfs => (
                    ImportStrategy::PerFile,
                    "apfs: named forks and clonefile need per-file fidelity".to_owned(),
                    false,
                ),
                other => (
                    ImportStrategy::TarStream,
                    format!("{other}: mounted; tar stream through the POSIX path"),
                    false,
                ),
            }
        };
        Self::with_strategy(source, source_bytes, strategy)
            .with_reason(reason)
            .with_signoff(signoff)
    }

    /// Plan constructor with an explicit strategy (the simulator and
    /// tests use this; `new` derives the strategy for you).
    #[must_use]
    pub fn with_strategy(source: FsKind, source_bytes: u64, strategy: ImportStrategy) -> Self {
        // Progress: 1000 steps, or one per MiB for small sources.
        let steps = (source_bytes / (1 << 20)).max(1).min(1_000);
        Self {
            source,
            source_bytes,
            strategy,
            reason: String::new(),
            needs_operator_signoff: strategy == ImportStrategy::RawBlock,
            progress_steps: steps,
        }
    }

    #[must_use]
    pub fn with_reason(mut self, reason: String) -> Self {
        self.reason = reason;
        self
    }

    #[must_use]
    pub fn with_signoff(mut self, needs: bool) -> Self {
        self.needs_operator_signoff = needs;
        self
    }

    /// Estimated destination bytes after the pipeline: the empirical
    /// 0.62-0.72x for compressible general data, 1.0x for incompress
    /// (raw media) -- reported as a range, never a promise.
    #[must_use]
    pub fn estimated_dest_bytes(&self) -> (u64, u64) {
        let lo = self.source_bytes * 62 / 100;
        let hi = self.source_bytes;
        (lo, hi)
    }

    /// Whether this plan can run unattended (cron/CI safe).
    #[must_use]
    pub fn unattended_ok(&self) -> bool {
        !self.needs_operator_signoff
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mounted_linux_filesystems_stream() {
        for kind in [FsKind::Ext4, FsKind::Xfs, FsKind::Btrfs, FsKind::F2fs, FsKind::Zfs] {
            let p = ImportPlan::new(kind, 1 << 40, true);
            assert_eq!(p.strategy, ImportStrategy::TarStream, "{kind}");
            assert!(p.unattended_ok());
            assert!(!p.reason.is_empty());
        }
    }

    #[test]
    fn fork_semantics_filesystems_import_per_file() {
        for kind in [FsKind::Ntfs, FsKind::HfsPlus, FsKind::Apfs] {
            let p = ImportPlan::new(kind, 1 << 30, true);
            assert_eq!(p.strategy, ImportStrategy::PerFile, "{kind}");
            assert!(p.unattended_ok());
        }
    }

    #[test]
    fn unmounted_sources_require_raw_block_and_signoff() {
        let p = ImportPlan::new(FsKind::Ext4, 1 << 40, false);
        assert_eq!(p.strategy, ImportStrategy::RawBlock);
        assert!(p.needs_operator_signoff);
        assert!(!p.unattended_ok());
        assert!(p.reason.contains("block-carve"));
    }

    #[test]
    fn unknown_but_mounted_still_streams() {
        let p = ImportPlan::new(FsKind::Unknown, 123, true);
        assert_eq!(p.strategy, ImportStrategy::TarStream);
    }

    #[test]
    fn progress_steps_are_bounded_and_smooth() {
        // Tiny source: 1 step.
        assert_eq!(ImportPlan::new(FsKind::Ext4, 1, true).progress_steps, 1);
        // 1 GiB: 1024 -> clamped to 1000.
        assert_eq!(ImportPlan::new(FsKind::Ext4, 1 << 30, true).progress_steps, 1_000);
        // 10 PiB: still 1000.
        let p = ImportPlan::new(FsKind::Ext4, 10 << 50, true);
        assert_eq!(p.progress_steps, 1_000);
        // 500 MiB: 500 steps (one per MiB).
        assert_eq!(ImportPlan::new(FsKind::Ext4, 500 << 20, true).progress_steps, 500);
    }

    #[test]
    fn destination_estimate_is_a_range() {
        let p = ImportPlan::new(FsKind::Ext4, 1_000_000, true);
        let (lo, hi) = p.estimated_dest_bytes();
        assert_eq!(lo, 620_000);
        assert_eq!(hi, 1_000_000);
    }

    #[test]
    fn strategy_tags_are_stable() {
        assert_eq!(ImportStrategy::TarStream.tag(), "tar-stream");
        assert_eq!(ImportStrategy::PerFile.tag(), "per-file");
        assert_eq!(ImportStrategy::RawBlock.tag(), "raw-block");
    }

    #[test]
    fn explicit_strategy_plan_defaults_to_its_own_signoff() {
        let p = ImportPlan::with_strategy(FsKind::Ext4, 100, ImportStrategy::RawBlock);
        assert!(p.needs_operator_signoff);
        let p = ImportPlan::with_strategy(FsKind::Ext4, 100, ImportStrategy::TarStream);
        assert!(!p.needs_operator_signoff);
    }
}
