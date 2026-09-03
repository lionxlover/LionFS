//! On-disk format version compatibility checking.
//!
//! `Superblock::version` currently exists but nothing checks it against
//! what the running code actually supports -- a filesystem created by a
//! future, incompatible version of LionFS would currently be mounted
//! anyway (as long as the magic number matches) and likely misinterpreted.

/// The on-disk format version this build of LionFS writes and fully
/// understands.
///
/// Version 2 (Phase 4): compression CLUSTERS. A compressed inode
/// (`Inode::compression_algo != 0`) stores no inline/spilled extents;
/// its `spill_extent_root` field instead roots a ClusterTree mapping
/// cluster index -> variable-length physical extent, so compressed
/// data occupies only as many blocks as its compressed payload needs.
/// Uncompressed inodes are laid out exactly as in v1. v1 images remain
/// readable (they cannot contain compressed inodes).
pub const CURRENT_VERSION: u32 = 2;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Compatibility {
    /// Exact match; fully supported.
    Current,
    /// Older than what this build writes, but this build knows how to
    /// read and upgrade it.
    ReadableOlder,
    /// Newer than what this build understands. Mounting anyway risks
    /// misinterpreting fields this build doesn't know about.
    UnsupportedNewer,
}

pub fn check_version(on_disk_version: u32) -> Compatibility {
    match on_disk_version.cmp(&CURRENT_VERSION) {
        std::cmp::Ordering::Equal => Compatibility::Current,
        std::cmp::Ordering::Less => Compatibility::ReadableOlder,
        std::cmp::Ordering::Greater => Compatibility::UnsupportedNewer,
    }
}

/// Convenience for call sites that just want a yes/no on "is it safe to
/// mount this read-write".
pub fn is_safe_to_mount(on_disk_version: u32) -> bool {
    !matches!(
        check_version(on_disk_version),
        Compatibility::UnsupportedNewer
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn current_version_is_compatible() {
        assert_eq!(check_version(CURRENT_VERSION), Compatibility::Current);
        assert!(is_safe_to_mount(CURRENT_VERSION));
    }

    #[test]
    fn newer_version_is_flagged_unsupported() {
        assert_eq!(
            check_version(CURRENT_VERSION + 1),
            Compatibility::UnsupportedNewer
        );
        assert!(!is_safe_to_mount(CURRENT_VERSION + 1));
    }

    #[test]
    fn older_version_is_readable() {
        if CURRENT_VERSION > 0 {
            assert_eq!(
                check_version(CURRENT_VERSION - 1),
                Compatibility::ReadableOlder
            );
            assert!(is_safe_to_mount(CURRENT_VERSION - 1));
        }
    }
}
