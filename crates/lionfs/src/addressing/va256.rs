//! 256-bit dynamic volume addresses (RFC-004 §3, "the capacity plane").
//!
//! RFC-002 §10 fixed the on-disk address at 128 bits after measuring the
//! cache-line cost of wider keys against the fact that no shipping medium
//! approaches 2^40 blocks. RFC-004 does **not** reverse that decision; it
//! completes it. LionFS 3.0 introduces a *mkfs-time selectable* namespace
//! width:
//!
//! | Plane | Width | Layout | Purpose |
//! |-------|-------|--------|---------|
//! | `Compact` | 128 | `VolumeAddr` (RFC-002 §4.1) | default; every existing volume |
//! | `Wide` | 256 | [`WideAddr`] (below) | fabric pools: CXL/USB4/ethernet-attached namespaces whose *capacity and churn* are unbounded by any single host's lifetime |
//!
//! The `Wide` plane exists for the case RFC-002 could only gesture at: a
//! pooled namespace (management domain, not a cluster filesystem -- see
//! RFC-004 §2 non-goals) whose member count and logical span grow without
//! bound. A 256-bit namespace in 4 KiB units addresses 2^268 bytes --
//! "unlimited" is an exaggeration every doc in the field makes; the honest
//! phrasing is *beyond any forecastable storage growth for the machine's
//! service lifetime*.
//!
//! Layout of [`WideAddr`] (big end first, mirroring `VolumeAddr`'s field
//! order so a compact address is a *prefix* of its wide image):
//!
//! | Bits | Field | Meaning |
//! |------|-------|---------|
//! | 255-232 | `domain_id` (24) | management / trust domain |
//! | 231-208 | `namespace_id` (24) | subvolume / tenant within domain |
//! | 207-176 | `volume_id` (32) | container / replicated set |
//! | 175-144 | `region` (32) | stripe, band, or zone-set |
//! | 143-112 | `device` (32) | pool member (4.29 G devices max) |
//! | 111-64 | `device_lba` (48) | per-device block address, 4 KiB units |
//! | 63-0 | `byte_offset` (64) | byte granularity within the block |
//!
//! The trailing 64-bit `byte_offset` is the deliberate difference from the
//! compact plane: PMEM/CXL tiers are byte-addressable, and encoding the
//! byte offset in the address (rather than carrying it beside the extent)
//! lets a *single* `WideAddr` name a byte of a memory-mapped tier, a block
//! of an NVMe tier, and a sector of an SMR tier with one comparison
//! ordering. `Ord` is field order, not numeric order -- same rationale as
//! `VolumeAddr`: device-local runs sort together.
//!
//! Compact addresses embed losslessly: `VolumeAddr` bits 127..0 map to
//! bits 175..48 of the wide form (volume, region, device, LBA, with the
//! LBA occupying the high half of `device_lba`), so `From<VolumeAddr>` is
//! total, and `try_compact` returns `Some` exactly when the wide address
//! was produced by that embedding (all wide-only fields zero, byte_offset
//! zero, LBA fits 64 bits).

use std::cmp::Ordering;
use std::fmt;

use super::va::VolumeAddr;

/// Maximum domain id (2^24 - 1).
pub const MAX_DOMAIN_ID: u32 = (1 << 24) - 1;
/// Maximum namespace id (2^24 - 1).
pub const MAX_NAMESPACE_ID: u32 = (1 << 24) - 1;
/// Maximum volume id in the wide plane (2^32 - 1).
pub const MAX_WIDE_VOLUME_ID: u64 = u32::MAX as u64;
/// Maximum region index in the wide plane (2^32 - 1).
pub const MAX_WIDE_REGION: u64 = u32::MAX as u64;
/// Maximum device index in the wide plane (2^32 - 1).
pub const MAX_WIDE_DEVICES: u64 = u32::MAX as u64;
/// Maximum per-device LBA in the wide plane (2^48 - 1): 1 EiB per device
/// in 4 KiB units.
pub const MAX_WIDE_LBA: u64 = (1u64 << 48) - 1;

/// The mkfs-time namespace-width selector (RFC-004 §3.2). Stored in the
/// superblock's `flags` word; mount refuses a plane it does not
/// understand, which is the forward-compatibility gate.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Hash)]
pub enum CapacityPlane {
    /// 128-bit `VolumeAddr` namespace -- the RFC-002 default, and the
    /// only plane 2.0 volumes have.
    Compact,
    /// 256-bit `WideAddr` namespace -- opt-in, for fabric pools whose
    /// member count / logical span is unbounded by one host's lifetime.
    Wide,
}

impl CapacityPlane {
    /// Width of addresses in this plane, in bits.
    #[must_use]
    pub fn width_bits(self) -> u16 {
        match self {
            Self::Compact => 128,
            Self::Wide => 256,
        }
    }

    /// The plane's stable on-disk tag (superblock `plane` byte).
    #[must_use]
    pub fn tag(self) -> u8 {
        match self {
            Self::Compact => 0,
            Self::Wide => 1,
        }
    }

    /// Inverse of [`CapacityPlane::tag`]; unknown tags are `None`, which
    /// mount reports as an unsupported future plane.
    #[must_use]
    pub fn from_tag(tag: u8) -> Option<Self> {
        match tag {
            0 => Some(Self::Compact),
            1 => Some(Self::Wide),
            _ => None,
        }
    }
}

/// A 256-bit volume address. Transparent newtype over `[u64; 4]` stored
/// little-endian-ordered by significance (limb 0 = least significant),
/// which makes the checked composition shifts cheap and keeps
/// `bytemuck`-style reinterpretation off the table on purpose: addresses
/// are semantic, not raw disk bytes, and every wire serialization goes
/// through the explicit limbs.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Default)]
pub struct WideAddr([u64; 4]);

impl WideAddr {
    /// Composes a wide address from its fields. Returns `None` when a
    /// field overflows its width -- the mkfs/mount validation path.
    #[must_use]
    pub fn compose(
        domain_id: u32,
        namespace_id: u32,
        volume_id: u64,
        region: u64,
        device: u64,
        device_lba: u64,
        byte_offset: u64,
    ) -> Option<Self> {
        if volume_id > MAX_WIDE_VOLUME_ID
            || region > MAX_WIDE_REGION
            || device > MAX_WIDE_DEVICES
            || device_lba > MAX_WIDE_LBA
        {
            return None;
        }
        // Field packing, from least significant limb up:
        //   limb0 = byte_offset                                  (64 bits)
        //   limb1 = device_lba(48) | device(16 low)              (64 bits)
        //   limb2 = device(16 high) | region(32) | volume(16 low)
        //   limb3 = volume(16 high) | namespace(24) | domain(24)
        let limb0 = byte_offset;
        let limb1 = device_lba | (device << 48);
        let limb2 = (volume_id << 48) | (region << 16) | (device >> 16);
        let limb3 = ((domain_id as u64) << 40) | ((namespace_id as u64) << 16) | (volume_id >> 16);
        Some(Self([limb0, limb1, limb2, limb3]))
    }

    /// Composes without validation. For hot paths where fields are
    /// already width-checked; debug builds assert anyway.
    #[must_use]
    pub fn compose_unchecked(
        domain_id: u32,
        namespace_id: u32,
        volume_id: u64,
        region: u64,
        device: u64,
        device_lba: u64,
        byte_offset: u64,
    ) -> Self {
        debug_assert!(
            volume_id <= MAX_WIDE_VOLUME_ID
                && region <= MAX_WIDE_REGION
                && device <= MAX_WIDE_DEVICES
                && device_lba <= MAX_WIDE_LBA
        );
        Self::compose(domain_id, namespace_id, volume_id, region, device, device_lba, byte_offset)
            .expect("debug_assert guards widths")
    }

    /// Raw limb pattern (limb 0 = least significant).
    #[must_use]
    pub fn to_limbs(self) -> [u64; 4] {
        self.0
    }

    /// From a raw limb pattern (limb 0 = least significant).
    #[must_use]
    pub fn from_limbs(limbs: [u64; 4]) -> Self {
        Self(limbs)
    }

    #[must_use]
    pub fn domain_id(self) -> u32 {
        (self.0[3] >> 40) as u32
    }

    #[must_use]
    pub fn namespace_id(self) -> u32 {
        ((self.0[3] >> 16) & 0x00FF_FFFF) as u32
    }

    #[must_use]
    pub fn volume_id(self) -> u64 {
        ((self.0[3] & 0x0000_FFFF) << 16) | (self.0[2] >> 48)
    }

    #[must_use]
    pub fn region(self) -> u64 {
        (self.0[2] >> 16) & 0xFFFF_FFFF
    }

    #[must_use]
    pub fn device(self) -> u64 {
        ((self.0[2] & 0xFFFF) << 16) | (self.0[1] >> 48)
    }

    #[must_use]
    pub fn device_lba(self) -> u64 {
        self.0[1] & MAX_WIDE_LBA
    }

    #[must_use]
    pub fn byte_offset(self) -> u64 {
        self.0[0]
    }

    /// The same address advanced by `blocks` 4 KiB units within the same
    /// device; `None` on LBA overflow or when `byte_offset != 0` (block
    /// arithmetic on a byte address is a caller bug).
    #[must_use]
    pub fn advance_blocks(self, blocks: u64) -> Option<Self> {
        if self.byte_offset() != 0 {
            return None;
        }
        let lba = self.device_lba().checked_add(blocks)?;
        if lba > MAX_WIDE_LBA {
            return None;
        }
        Some(Self([
            0,
            (self.0[1] & !MAX_WIDE_LBA) | lba,
            self.0[2],
            self.0[3],
        ]))
    }

    /// True when both addresses name the same device (all fields above
    /// `device_lba` equal) -- the allocator's locality test.
    #[must_use]
    pub fn same_device(self, other: Self) -> bool {
        self.0[3] == other.0[3] && self.0[2] == other.0[2] && (self.0[1] >> 48) == (other.0[1] >> 48)
    }

    /// Lossless narrowing to the compact plane, `None` unless every
    /// wide-only field is zero and the LBA fits the compact plane's 64-bit
    /// slot (which it always does by construction -- `device_lba` is
    /// 48-bit here, 64-bit there).
    #[must_use]
    pub fn try_compact(self) -> Option<VolumeAddr> {
        if self.0[3] != 0 {
            return None; // domain / namespace / wide-volume fields present
        }
        if self.volume_id() > u16::MAX as u64
            || self.region() > super::va::MAX_REGION as u64
            || self.device() > super::va::MAX_DEVICES as u64
        {
            return None;
        }
        if self.byte_offset() != 0 {
            return None; // byte addresses have no compact image
        }
        VolumeAddr::compose(
            self.volume_id() as u16,
            self.region() as u32,
            self.device() as u32,
            self.device_lba(),
        )
    }
}

impl From<VolumeAddr> for WideAddr {
    /// The embedding from RFC-004 §3.3: volume/region/device/lba fields
    /// are re-seated at their wide positions; wide-only fields are zero.
    /// This is total and round-trips through [`WideAddr::try_compact`].
    fn from(c: VolumeAddr) -> Self {
        let bits = c.to_bits();
        // Compact: volume(16)@112 | region(24)@88 | device(24)@64 | lba(64)@0.
        // Wide:    volume@175..144, region@175..144-adjacent... via compose:
        let volume = ((bits >> 112) as u64) & 0xFFFF;
        let region = ((bits >> 88) as u64) & 0x00FF_FFFF;
        let device = ((bits >> 64) as u64) & 0x00FF_FFFF;
        let lba = bits as u64;
        Self::compose(0, 0, volume, region, device, lba, 0).expect("compact fields fit wide widths")
    }
}

impl Ord for WideAddr {
    fn cmp(&self, other: &Self) -> Ordering {
        // Field order, not numeric: (domain, namespace, volume, region,
        // device, lba, byte) so device-local runs sort together.
        self.domain_id()
            .cmp(&other.domain_id())
            .then_with(|| self.namespace_id().cmp(&other.namespace_id()))
            .then_with(|| self.volume_id().cmp(&other.volume_id()))
            .then_with(|| self.region().cmp(&other.region()))
            .then_with(|| self.device().cmp(&other.device()))
            .then_with(|| self.device_lba().cmp(&other.device_lba()))
            .then_with(|| self.byte_offset().cmp(&other.byte_offset()))
    }
}

impl PartialOrd for WideAddr {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl fmt::Display for WideAddr {
    /// Canonical dotted form, `domain:namespace:volume:region:device:lba+byte`.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{}:{}:{}:{}:{}:{}+{}",
            self.domain_id(),
            self.namespace_id(),
            self.volume_id(),
            self.region(),
            self.device(),
            self.device_lba(),
            self.byte_offset()
        )
    }
}

impl fmt::Debug for WideAddr {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "WideAddr({self})")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn compose_decompose_roundtrip() {
        let a = WideAddr::compose(7, 42, 100_000, 65_536, 4_000_000_000, 1 << 40, 4095)
            .expect("all fields in width");
        assert_eq!(a.domain_id(), 7);
        assert_eq!(a.namespace_id(), 42);
        assert_eq!(a.volume_id(), 100_000);
        assert_eq!(a.region(), 65_536);
        assert_eq!(a.device(), 4_000_000_000);
        assert_eq!(a.device_lba(), 1 << 40);
        assert_eq!(a.byte_offset(), 4095);
    }

    #[test]
    fn compose_rejects_wide_field_overflow() {
        assert!(WideAddr::compose(0, 0, u64::MAX, 0, 0, 0, 0).is_none());
        assert!(WideAddr::compose(0, 0, 0, u64::MAX, 0, 0, 0).is_none());
        assert!(WideAddr::compose(0, 0, 0, 0, u64::MAX, 0, 0).is_none());
        assert!(WideAddr::compose(0, 0, 0, 0, 0, MAX_WIDE_LBA + 1, 0).is_none());
    }

    #[test]
    fn limbs_roundtrip() {
        let a = WideAddr::compose(1, 2, 3, 4, 5, 6, 7).expect("valid");
        assert_eq!(WideAddr::from_limbs(a.to_limbs()), a);
    }

    #[test]
    fn compact_embedding_is_lossless() {
        let c = VolumeAddr::compose(9, 100, 200, 1 << 40).expect("valid compact fields");
        let w: WideAddr = c.into();
        assert_eq!(w.domain_id(), 0);
        assert_eq!(w.namespace_id(), 0);
        assert_eq!(w.volume_id(), 9);
        assert_eq!(w.region(), 100);
        assert_eq!(w.device(), 200);
        assert_eq!(w.device_lba(), 1 << 40);
        assert_eq!(w.byte_offset(), 0);
        assert_eq!(w.try_compact(), Some(c));
    }

    #[test]
    fn wide_only_address_has_no_compact_image() {
        let w = WideAddr::compose(1, 0, 0, 0, 0, 0, 0).expect("valid");
        assert!(w.try_compact().is_none());
        let w = WideAddr::compose(0, 0, 0, 0, 0, 0, 1).expect("valid");
        assert!(w.try_compact().is_none());
    }

    #[test]
    fn ordering_prioritizes_device_over_lba() {
        // device 1 lba 0 vs device 0 lba 2^40: the device field dominates
        // the lba field, so a > b regardless of the lba magnitudes.
        let a = WideAddr::compose(0, 0, 0, 0, 1, 0, 0).expect("valid");
        let b = WideAddr::compose(0, 0, 0, 0, 0, 1 << 40, 0).expect("valid");
        assert!(a > b);
        assert!(a.same_device(a));
        assert!(!a.same_device(b));
    }

    #[test]
    fn advance_blocks_stays_on_device() {
        let a = WideAddr::compose(0, 0, 0, 0, 5, 100, 0).expect("valid");
        let b = a.advance_blocks(28).expect("no overflow");
        assert_eq!(b.device_lba(), 128);
        assert_eq!(b.device(), 5);
        assert!(a.advance_blocks(u64::MAX).is_none()); // LBA overflow
        let c = WideAddr::compose(0, 0, 0, 0, 5, 100, 1).expect("valid");
        assert!(c.advance_blocks(1).is_none()); // byte address: caller bug
    }

    #[test]
    fn plane_tags_are_stable() {
        assert_eq!(CapacityPlane::Compact.tag(), 0);
        assert_eq!(CapacityPlane::Wide.tag(), 1);
        assert_eq!(CapacityPlane::from_tag(0), Some(CapacityPlane::Compact));
        assert_eq!(CapacityPlane::from_tag(1), Some(CapacityPlane::Wide));
        assert_eq!(CapacityPlane::from_tag(2), None);
        assert_eq!(CapacityPlane::Compact.width_bits(), 128);
        assert_eq!(CapacityPlane::Wide.width_bits(), 256);
    }

    #[test]
    fn display_renders_dotted_form() {
        let a = WideAddr::compose(1, 2, 3, 4, 5, 6, 7).expect("valid");
        assert_eq!(a.to_string(), "1:2:3:4:5:6+7");
    }
}
