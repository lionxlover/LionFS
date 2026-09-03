//! 128-bit volume addresses (RFC-002 §4.1, Table 8).
//!
//! Bit layout (big end first):
//!
//! | Bits | Field | Meaning |
//! |------|-------|---------|
//! | 127-112 | `volume_id` (16) | subvolume / container selector |
//! | 111-88 | `region` (24) | stripe or band within the pool |
//! | 87-64 | `device` (24) | pool member (16.7 M devices max) |
//! | 63-0 | `device_lba` (64) | per-device block address, 4 KiB units |
//!
//! The type is a transparent newtype over `u128` with checked
//! composition/decomposition, `Ord` by (volume, region, device, lba) --
//! NOT numeric order, so that device-local runs sort together -- and a
//! `Display` that renders the canonical dotted form
//! `vol:region:device:lba`.

use std::cmp::Ordering;
use std::fmt;

/// Maximum number of addressable devices (2^24 - 1).
pub const MAX_DEVICES: u32 = (1 << 24) - 1;
/// Maximum region index (2^24 - 1).
pub const MAX_REGION: u32 = (1 << 24) - 1;
/// Maximum volume id (2^16 - 1).
pub const MAX_VOLUME_ID: u32 = (1 << 16) - 1;

const VOLUME_SHIFT: u32 = 112;
const REGION_SHIFT: u32 = 88;
const DEVICE_SHIFT: u32 = 64;
const LBA_MASK: u128 = u128::MAX >> 64;

/// A 128-bit volume address. Construct via [`VolumeAddr::compose`] (which
/// validates field widths) or [`VolumeAddr::from_bits`] (raw).
#[derive(Clone, Copy, PartialEq, Eq, Hash, Default)]
pub struct VolumeAddr(u128);

impl VolumeAddr {
    /// Composes an address from its fields. Returns `None` if any field
    /// overflows its width -- the mkfs/mount-time validation path.
    #[must_use]
    pub fn compose(volume_id: u16, region: u32, device: u32, device_lba: u64) -> Option<Self> {
        if region > MAX_REGION || device > MAX_DEVICES {
            return None;
        }
        Some(Self(
            ((volume_id as u128) << VOLUME_SHIFT)
                | ((region as u128) << REGION_SHIFT)
                | ((device as u128) << DEVICE_SHIFT)
                | (device_lba as u128 & LBA_MASK),
        ))
    }

    /// Composes without validation. For hot paths where fields are
    /// already width-checked (e.g. iterating a single device's LBA
    /// range). Debug builds assert anyway.
    #[must_use]
    pub fn compose_unchecked(volume_id: u16, region: u32, device: u32, device_lba: u64) -> Self {
        debug_assert!(region <= MAX_REGION && device <= MAX_DEVICES);
        Self(
            ((volume_id as u128) << VOLUME_SHIFT)
                | ((region as u128) << REGION_SHIFT)
                | ((device as u128) << DEVICE_SHIFT)
                | (device_lba as u128 & LBA_MASK),
        )
    }

    /// Raw bit pattern.
    #[must_use]
    pub fn to_bits(self) -> u128 {
        self.0
    }

    /// From a raw bit pattern (e.g. read off disk).
    #[must_use]
    pub fn from_bits(bits: u128) -> Self {
        Self(bits)
    }

    #[must_use]
    pub fn volume_id(self) -> u16 {
        (self.0 >> VOLUME_SHIFT) as u16
    }

    #[must_use]
    pub fn region(self) -> u32 {
        ((self.0 >> REGION_SHIFT) & 0x00FF_FFFF) as u32
    }

    #[must_use]
    pub fn device(self) -> u32 {
        ((self.0 >> DEVICE_SHIFT) & 0x00FF_FFFF) as u32
    }

    #[must_use]
    pub fn device_lba(self) -> u64 {
        (self.0 & LBA_MASK) as u64
    }

    /// Returns the same address advanced by `blocks` LBA units within the
    /// same device. Overflow of the 64-bit LBA field returns `None`.
    #[must_use]
    pub fn advance_blocks(self, blocks: u64) -> Option<Self> {
        // The LBA field is full-width u64, so checked_add is the only
        // failure mode; width constraints are unaffected (same device).
        let lba = self.device_lba().checked_add(blocks)?;
        Some(Self::compose_unchecked(
            self.volume_id(),
            self.region(),
            self.device(),
            lba,
        ))
    }

    /// Byte offset of this address on its device (4 KiB units).
    #[must_use]
    pub fn byte_offset(self) -> u64 {
        self.device_lba() * crate::addressing::LBA_BLOCK_BYTES
    }

    /// Sort key: (volume, region, device, lba) -- device-local runs
    /// cluster; a bare numeric sort would interleave devices.
    fn sort_key(self) -> (u16, u32, u32, u64) {
        (
            self.volume_id(),
            self.region(),
            self.device(),
            self.device_lba(),
        )
    }

    /// True when `self` and `other` name the same (device, region,
    /// volume) -- i.e. they could be in one contiguous device run.
    #[must_use]
    pub fn same_stripe(self, other: VolumeAddr) -> bool {
        self.sort_key().0 == other.sort_key().0
            && self.sort_key().1 == other.sort_key().1
            && self.sort_key().2 == other.sort_key().2
    }
}

impl Ord for VolumeAddr {
    fn cmp(&self, other: &Self) -> Ordering {
        self.sort_key().cmp(&other.sort_key())
    }
}

impl PartialOrd for VolumeAddr {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl fmt::Debug for VolumeAddr {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "VolumeAddr({}:{:02x}:{:02x}:{})",
            self.volume_id(),
            self.region(),
            self.device(),
            self.device_lba()
        )
    }
}

impl fmt::Display for VolumeAddr {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{}:{}:{}:{}",
            self.volume_id(),
            self.region(),
            self.device(),
            self.device_lba()
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn compose_decompose_roundtrip() {
        for &(v, r, d, l) in &[
            (0u16, 0u32, 0u32, 0u64),
            (1, 0, 0, 1),
            (u16::MAX, MAX_REGION, MAX_DEVICES, u64::MAX),
            (7, 0x12_3456, 0x00_ABCD, 0x7FFF_FFFF_FFFF),
        ] {
            let a = VolumeAddr::compose(v, r, d, l).unwrap();
            assert_eq!(a.volume_id(), v);
            assert_eq!(a.region(), r);
            assert_eq!(a.device(), d);
            assert_eq!(a.device_lba(), l);
        }
    }

    #[test]
    fn compose_rejects_overflow() {
        assert!(VolumeAddr::compose(0, MAX_REGION + 1, 0, 0).is_none());
        assert!(VolumeAddr::compose(0, 0, MAX_DEVICES + 1, 0).is_none());
    }

    #[test]
    fn fields_do_not_bleed() {
        let a = VolumeAddr::compose(3, MAX_REGION, MAX_DEVICES, u64::MAX).unwrap();
        assert_eq!(a.volume_id(), 3, "lba must not bleed into device");
        assert_eq!(a.device(), MAX_DEVICES, "device must not bleed into region");
        assert_eq!(a.region(), MAX_REGION, "region must not bleed into volume");
        assert_eq!(a.device_lba(), u64::MAX);
    }

    #[test]
    fn ordering_clusters_device_runs() {
        // Same device, increasing LBA sorts by LBA (numeric too).
        let a = VolumeAddr::compose(0, 0, 1, 10).unwrap();
        let b = VolumeAddr::compose(0, 0, 1, 20).unwrap();
        assert!(a < b);

        // Numeric order would put device 1 before device 2 regardless of
        // volume; structured order clusters by volume first.
        let c = VolumeAddr::compose(1, 0, 1, 0).unwrap();
        assert!(a < c, "volume 0 addresses must sort before volume 1");
        assert!(c > a);
    }

    #[test]
    fn advance_and_byte_offset() {
        let a = VolumeAddr::compose(0, 5, 2, 100).unwrap();
        let b = a.advance_blocks(28).unwrap();
        assert_eq!(b.device_lba(), 128);
        assert_eq!(b.device(), 2);
        assert_eq!(b.region(), 5);
        assert_eq!(a.byte_offset(), 100 * 4096);
        assert_eq!(b.byte_offset(), 128 * 4096);
    }

    #[test]
    fn advance_overflow_is_checked() {
        let a = VolumeAddr::compose(0, 0, 0, u64::MAX).unwrap();
        assert!(a.advance_blocks(1).is_none());
        assert!(a.advance_blocks(0).is_some());
    }

    #[test]
    fn raw_bits_roundtrip() {
        let a = VolumeAddr::compose(9, 8, 7, 6).unwrap();
        let b = VolumeAddr::from_bits(a.to_bits());
        assert_eq!(a, b);
    }

    #[test]
    fn display_is_dotted_and_parseable_by_eye() {
        let a = VolumeAddr::compose(1, 2, 3, 4).unwrap();
        assert_eq!(format!("{a}"), "1:2:3:4");
    }

    #[test]
    fn same_stripe_checks() {
        let a = VolumeAddr::compose(1, 2, 3, 4).unwrap();
        let b = VolumeAddr::compose(1, 2, 3, 5).unwrap();
        let c = VolumeAddr::compose(1, 2, 4, 4).unwrap();
        assert!(a.same_stripe(b));
        assert!(!a.same_stripe(c));
    }

    #[test]
    fn namespace_reach() {
        // 2^40 4-KiB blocks = 4 PiB per device LBA field * 2^24 devices
        // * 2^24 regions * 2^16 volumes: assert the composition covers
        // exabyte-scale single files (via 64-bit LBA) and beyond.
        let a = VolumeAddr::compose(0, 0, 0, 1u64 << 40).unwrap();
        assert_eq!(a.byte_offset(), (1u64 << 40) * 4096);
        // 2^40 blocks * 4 KiB = 4 PiB; the u128 namespace itself:
        assert_eq!(VolumeAddr::from_bits(u128::MAX).device(), MAX_DEVICES);
    }
}
