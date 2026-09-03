//! # Migration & Foreign-Filesystem Import (RFC-004 §9)
//!
//! A filesystem nobody can move *to* is a filesystem nobody can adopt.
//! The 1.x/2.0 tree had no on-ramp at all; RFC-004 §9 specifies one
//! built around a single honest strategy: **read through the source
//! filesystem's own driver, write through LionFS's own POSIX path**.
//! There is no on-disk-format converter, no dual-format staging area,
//! and never a live in-place conversion -- the failure modes of
//! in-place conversion (power loss mid-rewrite, source-driver bugs
//! corrupting the destination, no rollback) are precisely the failure
//! modes LionFS exists to eliminate.
//!
//! Three pieces:
//!
//! * [`detect`] -- identify the source filesystem from magic bytes at
//!   documented offsets (pure, allocation-light, testable against a
//!   table of synthetic superblock images).
//! * [`manifest`] -- the verification ledger: every imported file gets
//!   a (size, SHA-256) record; the import is not "done" until every
//!   record re-verifies on the destination. Migration is a protocol,
//!   not a copy.
//! * [`plan`] -- the import plan: which strategy the source demands
//!   (tar-stream vs. per-file ioctl vs. raw-block for unmountable
//!   sources), with size accounting and a staged progress model.
//!
//! The engine-side streaming (tar emission from the source tree, POSIX
//! writes into LionFS) is tooling (`lfs_migrate`); these modules are
//! the policy and proof layers the tooling and the simulator share.

pub mod detect;
pub mod manifest;
pub mod plan;

pub use detect::{FsKind, MAGIC_TABLE};
pub use manifest::{Manifest, ManifestEntry, VerifyOutcome};
pub use plan::{ImportPlan, ImportStrategy};

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reexports_compose() {
        assert_eq!(FsKind::from_tag("ext4").expect("known"), FsKind::Ext4);
        let _m = Manifest::new();
        let _p = ImportPlan::new(FsKind::Ext4, 1 << 40, true);
    }
}
