//! On-disk format version + feature-flag compatibility checking.
//!
//! 3.6 introduces the **Format Vault** feature-flag protocol (ZFS-style):
//! the format `version` stays at 2 and every new capability is a named
//! bit in `Superblock::fs_features`. A 3.5 build that encounters a 3.6
//! image keeps mounting and serving the data it already understands (the
//! bits live in fields old builds never read); a 3.6 build encountering
//! a future image with UNKNOWN bits refuses to mount rather than
//! misinterpret fields it does not know. This is the same posture ZFS
//! takes with `feature@...` properties, and it is what makes the on-disk
//! format survivable across decades of evolution.
//!
//! The full 200-year plan (version policy, field-carving rules, feature
//! registry, crypto agility) is normative in
//! `docs/rfc/LFS-RFC-005-format-vault.md`.

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
///
/// 3.6 deliberately does NOT bump this: every 3.6 capability is opt-in
/// via a feature bit (below), and a feature bit is only ever SET on an
/// image after the corresponding on-disk structures exist. Old builds
/// ignore the bits (they never read `fs_features`), so a 3.5 build
/// keeps mounting such an image and serving the data it understands;
/// the new structures are simply invisible to it.
pub const CURRENT_VERSION: u32 = 2;

// -- 3.6 feature-flag registry (Superblock::fs_features) --------------------
//
// Bits are semantic commitments: setting a bit means "the corresponding
// on-disk structures are present on this image". A bit is sticky: it is
// never cleared once set (the structures may already exist).

/// Extended attributes + POSIX ACLs: `Superblock::xattr_tree_root` is a
/// live `XattrTree` (node type 13); per-inode `system.posix_acl_*`
/// entries may exist.
pub const FS_FEATURE_XATTR: u64 = 1 << 0;

/// Reflink clones: `Superblock::clone_tree_root` is a live clone
/// registry and physical blocks may be shared between inodes under
/// refcount pinning (the dedup redirect machinery covers them).
pub const FS_FEATURE_REFLINK: u64 = 1 << 1;

/// Volume key envelope v2: `Superblock::key_envelope_block` roots an
/// `LFSE`-magic envelope blob (kdf_id/aead_id/kem_id agility fields).
pub const FS_FEATURE_ENVELOPE_V2: u64 = 1 << 2;

/// Every feature bit this build knows. A mount of an image with ANY
/// bit outside this mask set is refused (LionFS::new + the mount CLI +
/// fsck): the image may contain structures this build would
/// misinterpret.
pub const KNOWN_FS_FEATURES: u64 = FS_FEATURE_XATTR | FS_FEATURE_REFLINK | FS_FEATURE_ENVELOPE_V2;

/// Unknown feature bits set on an image: the mount must refuse.
#[must_use]
pub fn unknown_features(fs_features: u64) -> u64 {
    fs_features & !KNOWN_FS_FEATURES
}

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

/// 3.6 (Format Vault): the full mount gate. Safe iff the format
/// version is understood AND no unknown feature bits are set. The
/// version-2 stability policy means `is_safe_to_mount` alone is no
/// longer sufficient: a future build could keep version 2 and add a
/// bit this build does not know.
#[must_use]
pub fn is_mountable(on_disk_version: u32, fs_features: u64) -> bool {
    is_safe_to_mount(on_disk_version) && unknown_features(fs_features) == 0
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

    #[test]
    fn known_features_are_mountable() {
        assert!(is_mountable(2, 0));
        assert!(is_mountable(2, FS_FEATURE_XATTR));
        assert!(is_mountable(2, FS_FEATURE_XATTR | FS_FEATURE_REFLINK | FS_FEATURE_ENVELOPE_V2));
    }

    #[test]
    fn unknown_features_refuse_mount() {
        assert!(!is_mountable(2, 1 << 40));
        assert!(!is_mountable(2, KNOWN_FS_FEATURES | (1 << 63)));
        assert_eq!(unknown_features(0), 0);
        assert_eq!(unknown_features(FS_FEATURE_XATTR), 0);
        assert_eq!(unknown_features(1 << 40), 1 << 40);
    }

    #[test]
    fn future_version_refuses_even_with_no_features() {
        assert!(!is_mountable(CURRENT_VERSION + 1, 0));
    }
}
