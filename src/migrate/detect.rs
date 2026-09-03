//! Foreign-filesystem detection (RFC-004 §9.1) by magic bytes.
//!
//! Each row of [`MAGIC_TABLE`] is (kind, offset, expected-bytes): read
//! `len` bytes at `offset` from the source's first megabyte and
//! compare. The table documents *only* magics that are load-bearing
//! for choosing an import strategy; exotic filesystems the kernel can
//! read anyway fall through to the tar-stream strategy regardless of
//! identity, so unknown is fine.

use std::fmt;

/// Filesystems LionFS can import from, and how it recognizes them.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum FsKind {
    Ext4,
    Xfs,
    Btrfs,
    Zfs,
    F2fs,
    Ntfs,
    Apfs,
    Fat32,
    ExFat,
    HfsPlus,
    /// The kernel mounts it but it is not in the table.
    Other,
    /// No known magic matched and no driver claims it.
    Unknown,
}

impl FsKind {
    /// Stable policy-JSON tag.
    #[must_use]
    pub fn tag(self) -> &'static str {
        match self {
            Self::Ext4 => "ext4",
            Self::Xfs => "xfs",
            Self::Btrfs => "btrfs",
            Self::Zfs => "zfs",
            Self::F2fs => "f2fs",
            Self::Ntfs => "ntfs",
            Self::Apfs => "apfs",
            Self::Fat32 => "fat32",
            Self::ExFat => "exfat",
            Self::HfsPlus => "hfs+",
            Self::Other => "other",
            Self::Unknown => "unknown",
        }
    }

    /// Inverse of [`FsKind::tag`].
    #[must_use]
    pub fn from_tag(tag: &str) -> Option<Self> {
        match tag {
            "ext4" => Some(Self::Ext4),
            "xfs" => Some(Self::Xfs),
            "btrfs" => Some(Self::Btrfs),
            "zfs" => Some(Self::Zfs),
            "f2fs" => Some(Self::F2fs),
            "ntfs" => Some(Self::Ntfs),
            "apfs" => Some(Self::Apfs),
            "fat32" => Some(Self::Fat32),
            "exfat" => Some(Self::ExFat),
            "hfs+" => Some(Self::HfsPlus),
            "other" => Some(Self::Other),
            "unknown" => Some(Self::Unknown),
            _ => None,
        }
    }
}

impl fmt::Display for FsKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.tag())
    }
}

/// One detection rule: `image[offset..offset+len] == magic`.
#[derive(Clone, Copy, Debug)]
pub struct MagicRule {
    pub kind: FsKind,
    pub offset: usize,
    pub magic: &'static [u8],
    /// Extra equal-length alternative magic (ext4's old "0xEF53 at
    /// 0x438" covers ext2/3 too; NTFS has two signatures).
    pub alt: &'static [u8],
}

/// The detection table. Offsets are the well-documented superblock
/// magic locations; the first matching rule wins, in table order
/// (most-specific first: filesystem superblocks before partition
/// table signatures).
pub const MAGIC_TABLE: [MagicRule; 10] = [
    // ext2/3/4: 0xEF53 LE at 1080 (0x438) of the superblock (block 0
    // starts the superblock at offset 1024; 1024 + 56 = 1080).
    MagicRule { kind: FsKind::Ext4, offset: 1080, magic: &[0x53, 0xEF], alt: &[] },
    // XFS: "XFSB" at 0.
    MagicRule { kind: FsKind::Xfs, offset: 0, magic: b"XFSB", alt: &[] },
    // Btrfs: "_BHRfS_M" at 65280 (0xFF00), BTRFS_MAGIC raw at superblock
    // copy 0 (which is 64 KiB in on disk, mirrored at 0 for tiny disks
    // is NOT true -- btrfs magic sits at 0xFF00 of the primary copy).
    MagicRule { kind: FsKind::Btrfs, offset: 65_280, magic: b"_BHRfS_M", alt: &[] },
    // ZFS: not a fixed magic at a small offset (labels live at 512 K
    // boundaries); recognized by the label version check at 0x0 in the
    // vdev label "version" -- we key on the nvlist magic 0x00bab10c LE.
    MagicRule { kind: FsKind::Zfs, offset: 0, magic: &[0x0c, 0xb1, 0xba, 0x00], alt: &[] },
    // F2FS: 0x0FF10FF0 LE at 1024.
    MagicRule { kind: FsKind::F2fs, offset: 1024, magic: &[0xF0, 0x0F, 0xF1, 0x0F], alt: &[] },
    // NTFS: "NTFS    " at 3 (OEM name).
    MagicRule { kind: FsKind::Ntfs, offset: 3, magic: b"NTFS    ", alt: &[] },
    // FAT32: at 82 (0x52) "FAT32   ".
    MagicRule { kind: FsKind::Fat32, offset: 82, magic: b"FAT32   ", alt: &[] },
    // exFAT: at 3 "EXFAT   ".
    MagicRule { kind: FsKind::ExFat, offset: 3, magic: b"EXFAT   ", alt: &[] },
    // HFS+: at 1024 (0x400) "H+" / "HX" (case-sensitive variant).
    MagicRule { kind: FsKind::HfsPlus, offset: 1024, magic: b"H+", alt: b"HX" },
    // APFS: "NXSB" at 32 of the container superblock.
    MagicRule { kind: FsKind::Apfs, offset: 32, magic: b"NXSB", alt: &[] },
];

/// Detects the filesystem whose superblock image is `image` (at least
/// the first 65_536+8 bytes of the device; smaller images are handled
/// but can only match rules within their length).
///
/// Returns `Some(kind)` on the first rule whose magic (or alt) matches
/// inside the image bounds; `None` when nothing matches (the caller
/// then tries a driver claim, else reports unknown).
#[must_use]
pub fn detect(image: &[u8]) -> Option<FsKind> {
    for rule in &MAGIC_TABLE {
        let end = rule.offset + rule.magic.len();
        if end > image.len() {
            continue;
        }
        let got = &image[rule.offset..end];
        if got == rule.magic || got == rule.alt {
            return Some(rule.kind);
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn padded_image() -> Vec<u8> {
        vec![0u8; 65_536 + 8]
    }

    #[test]
    fn every_rule_matches_its_own_image() {
        for rule in &MAGIC_TABLE {
            let mut img = padded_image();
            img[rule.offset..rule.offset + rule.magic.len()].copy_from_slice(rule.magic);
            assert_eq!(detect(&img), Some(rule.kind), "rule {:?}", rule.kind);
        }
    }

    #[test]
    fn alt_magics_match() {
        // HFS+ case-sensitive "HX".
        let mut img = padded_image();
        img[1024..1026].copy_from_slice(b"HX");
        assert_eq!(detect(&img), Some(FsKind::HfsPlus));
    }

    #[test]
    fn empty_image_detects_nothing() {
        assert_eq!(detect(&[]), None);
    }

    #[test]
    fn blank_image_detects_nothing() {
        assert_eq!(detect(&padded_image()), None);
    }

    #[test]
    fn short_image_skips_out_of_range_rules() {
        // A 100-byte image cannot match anything in the table (the
        // smallest offsets are 0 for ZFS with 4-byte magic).
        let img = vec![0u8; 100];
        assert_eq!(detect(&img), None);
        // XFS magic at 0 in a 4-byte image.
        assert_eq!(detect(b"XFSB"), Some(FsKind::Xfs));
    }

    #[test]
    fn first_match_wins_in_table_order() {
        // ext4's rule comes first; an image that would also satisfy a
        // later rule reports ext4.
        let mut img = padded_image();
        img[1080..1082].copy_from_slice(&[0x53, 0xEF]); // ext4
        img[1024..1028].copy_from_slice(&[0xF0, 0x0F, 0xF1, 0x0F]); // f2fs
        assert_eq!(detect(&img), Some(FsKind::Ext4));
    }

    #[test]
    fn tags_roundtrip() {
        let all = [
            FsKind::Ext4, FsKind::Xfs, FsKind::Btrfs, FsKind::Zfs, FsKind::F2fs,
            FsKind::Ntfs, FsKind::Apfs, FsKind::Fat32, FsKind::ExFat, FsKind::HfsPlus,
            FsKind::Other, FsKind::Unknown,
        ];
        for k in all {
            assert_eq!(FsKind::from_tag(k.tag()), Some(k));
            assert_eq!(k.to_string(), k.tag());
        }
        assert!(FsKind::from_tag("minix").is_none());
    }

    #[test]
    fn ntfs_and_exfat_dont_collide() {
        // Both key at offset 3; NTFS must win when its 8-byte OEM name
        // is present (table order).
        let mut img = vec![0u8; 512];
        img[3..11].copy_from_slice(b"NTFS    ");
        assert_eq!(detect(&img), Some(FsKind::Ntfs));
        let mut img = vec![0u8; 512];
        img[3..11].copy_from_slice(b"EXFAT   ");
        assert_eq!(detect(&img), Some(FsKind::ExFat));
    }
}
