//! Packed 16-byte extent records (RFC-002 §4.1).
//!
//! Extents are the single most numerous structure in any file system --
//! a 10-TB streaming workload allocates them millions of times per hour
//! -- so the on-disk width is minimized to 16 bytes: one cache line holds
//! eight, a B-epsilon leaf holds hundreds.
//!
//! Wire layout (little-endian):
//!
//! ```text
//! byte 0..6   logical_start  : u48
//! byte 6..12  physical_start : u48
//! byte 12..15 length         : u24
//! byte 15     flags          : u8
//!               bit 0 GRAN   : 0 = fields count 4 KiB units (file max 1 EiB)
//!                              1 = fields count 64 KiB units (file max 16 EiB)
//!               bit 1 RAW    : stored uncompressed
//!               bit 2 ENC    : payload encrypted
//!               bit 3 SHARED : refcounted (dedup/snapshot)
//!               bit 4 DEDUP  : reference target of the chunk layer
//!               bits 5-7 reserved (must be zero on decode)
//! ```
//!
//! The deliberate conservatism is in the packed record, not the
//! namespace (see [`crate::addressing::va`]): per-file and per-device
//! encoded reach is exabyte-class, which is beyond every device on any
//! roadmap.

use crate::addressing::LBA_BLOCK_BYTES;

pub const GRAN_SHIFT: u8 = 0;
pub const RAW_SHIFT: u8 = 1;
pub const ENC_SHIFT: u8 = 2;
pub const SHARED_SHIFT: u8 = 3;
pub const DEDUP_SHIFT: u8 = 4;

/// Flags on an [`Extent16`]: a plain `u8` newtype with const
/// construction, keeping the on-disk representation exactly one byte
/// with no dependency on the `bitflags` crate.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct ExtentFlags(pub u8);

impl ExtentFlags {
    #[must_use]
    pub const fn empty() -> Self {
        Self(0)
    }

    #[must_use]
    pub const fn gran(self) -> bool {
        (self.0 >> GRAN_SHIFT) & 1 == 1
    }

    #[must_use]
    pub const fn raw(self) -> bool {
        (self.0 >> RAW_SHIFT) & 1 == 1
    }

    #[must_use]
    pub const fn encrypted(self) -> bool {
        (self.0 >> ENC_SHIFT) & 1 == 1
    }

    #[must_use]
    pub const fn shared(self) -> bool {
        (self.0 >> SHARED_SHIFT) & 1 == 1
    }

    #[must_use]
    pub const fn dedup(self) -> bool {
        (self.0 >> DEDUP_SHIFT) & 1 == 1
    }

    #[must_use]
    pub const fn with_gran(mut self, on: bool) -> Self {
        self.0 = (self.0 & !(1 << GRAN_SHIFT)) | ((on as u8) << GRAN_SHIFT);
        self
    }

    #[must_use]
    pub const fn with_raw(mut self, on: bool) -> Self {
        self.0 = (self.0 & !(1 << RAW_SHIFT)) | ((on as u8) << RAW_SHIFT);
        self
    }

    #[must_use]
    pub const fn with_encrypted(mut self, on: bool) -> Self {
        self.0 = (self.0 & !(1 << ENC_SHIFT)) | ((on as u8) << ENC_SHIFT);
        self
    }

    #[must_use]
    pub const fn with_shared(mut self, on: bool) -> Self {
        self.0 = (self.0 & !(1 << SHARED_SHIFT)) | ((on as u8) << SHARED_SHIFT);
        self
    }

    #[must_use]
    pub const fn with_dedup(mut self, on: bool) -> Self {
        self.0 = (self.0 & !(1 << DEDUP_SHIFT)) | ((on as u8) << DEDUP_SHIFT);
        self
    }
}

/// The 16-byte packed extent record. Access via the encode/decode pair
/// and the typed getters; the raw bytes are `bytemuck`-castable for
/// on-disk use (`#[repr(transparent)]` over `[u8; 16]`).
#[derive(Clone, Copy, PartialEq, Eq, Hash, Default)]
#[repr(transparent)]
pub struct Extent16([u8; 16]);

impl Extent16 {
    /// Encodes (logical_start, physical_start, length, flags) into 16
    /// bytes. Returns `None` when a field exceeds its packed width --
    /// the mkfs/mount validation path -- never truncates silently.
    #[must_use]
    pub fn encode(
        logical_start: u64,
        physical_start: u64,
        length: u64,
        flags: ExtentFlags,
    ) -> Option<Self> {
        const U48_MAX: u64 = (1u64 << 48) - 1;
        const U24_MAX: u64 = (1u64 << 24) - 1;
        if logical_start > U48_MAX || physical_start > U48_MAX || length > U24_MAX {
            return None;
        }
        let mut bytes = [0u8; 16];
        bytes[0..6].copy_from_slice(&logical_start.to_le_bytes()[0..6]);
        bytes[6..12].copy_from_slice(&physical_start.to_le_bytes()[0..6]);
        bytes[12..15].copy_from_slice(&length.to_le_bytes()[0..3]);
        bytes[15] = flags.0;
        Some(Self(bytes))
    }

    /// Decodes from a raw 16-byte slice. Returns `None` when reserved
    /// flag bits are set (forward-format detection).
    #[must_use]
    pub fn decode(bytes: &[u8; 16]) -> Option<Self> {
        let flags = bytes[15];
        if flags & 0b1110_0000 != 0 {
            return None;
        }
        Some(Self(*bytes))
    }

    #[must_use]
    pub fn raw_bytes(&self) -> &[u8; 16] {
        &self.0
    }

    #[must_use]
    pub fn logical_start(&self) -> u64 {
        u64::from_le_bytes([
            self.0[0], self.0[1], self.0[2], self.0[3], self.0[4], self.0[5], 0, 0,
        ])
    }

    #[must_use]
    pub fn physical_start(&self) -> u64 {
        u64::from_le_bytes([
            self.0[6], self.0[7], self.0[8], self.0[9], self.0[10], self.0[11], 0, 0,
        ])
    }

    #[must_use]
    pub fn length_blocks(&self) -> u64 {
        u64::from_le_bytes([self.0[12], self.0[13], self.0[14], 0, 0, 0, 0, 0])
    }

    #[must_use]
    pub fn flags(&self) -> ExtentFlags {
        ExtentFlags(self.0[15])
    }

    /// The extent's unit in bytes, per the GRAN flag.
    #[must_use]
    pub fn granularity_bytes(&self) -> u64 {
        if self.flags().gran() {
            64 * 1024
        } else {
            LBA_BLOCK_BYTES
        }
    }

    /// Length in bytes (granularity-aware, saturating at the u64 edge).
    #[must_use]
    pub fn length_bytes(&self) -> u64 {
        self.length_blocks()
            .saturating_mul(self.granularity_bytes())
    }

    /// Maximum logical byte offset covered (exclusive end). Saturating:
    /// a maximally-packed GRAN=1 record reaches the u64 edge, and
    /// overflowing callers get `u64::MAX` rather than a wrapped lie.
    #[must_use]
    pub fn logical_end(&self) -> u64 {
        self.logical_start()
            .saturating_add(self.length_blocks())
            .saturating_mul(self.granularity_bytes())
    }

    /// Whether a byte range [offset, offset+len) intersects this extent's
    /// logical range -- the read path's extent-lookup probe.
    #[must_use]
    pub fn intersects_logical(&self, offset: u64, len: u64) -> bool {
        let start = self.logical_start() * self.granularity_bytes();
        let end = self.logical_end();
        offset < end && offset + len > start
    }

    /// Physical byte offset of a logical byte offset inside this extent,
    /// if the offset is covered.
    #[must_use]
    pub fn map_logical_to_physical(&self, logical_offset: u64) -> Option<u64> {
        let start = self.logical_start() * self.granularity_bytes();
        let end = self.logical_end();
        if logical_offset < start || logical_offset >= end {
            return None;
        }
        let delta = logical_offset - start;
        Some(self.physical_start() * self.granularity_bytes() + delta)
    }

    /// True when `self` and `next` are physically adjacent and could be
    /// coalesced by the B-epsilon flusher (RFC-002 §4.3: "the flusher
    /// coalesces adjacent extents before writing").
    #[must_use]
    pub fn coalescable_with(&self, next: Extent16) -> bool {
        self.flags().0 == next.flags().0
            && self.logical_end() == next.logical_start() * self.granularity_bytes()
            && self
                .physical_start()
                .saturating_add(self.length_blocks())
                .saturating_mul(self.granularity_bytes())
                == next
                    .physical_start()
                    .saturating_mul(next.granularity_bytes())
    }
}

impl std::fmt::Debug for Extent16 {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Extent16")
            .field("logical_start", &self.logical_start())
            .field("physical_start", &self.physical_start())
            .field("length_blocks", &self.length_blocks())
            .field("flags", &self.flags().0)
            .finish()
    }
}

// bytemuck integration: Extent16 is a POD newtype over [u8; 16].
unsafe impl bytemuck::Zeroable for Extent16 {}
unsafe impl bytemuck::Pod for Extent16 {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encode_decode_roundtrip() {
        let e = Extent16::encode(12345, 67890, 4321, ExtentFlags::empty().with_raw(true)).unwrap();
        let bytes = *e.raw_bytes();
        let back = Extent16::decode(&bytes).unwrap();
        assert_eq!(back.logical_start(), 12345);
        assert_eq!(back.physical_start(), 67890);
        assert_eq!(back.length_blocks(), 4321);
        assert!(back.flags().raw());
        assert!(!back.flags().gran());
    }

    #[test]
    fn max_widths_roundtrip() {
        let u48 = (1u64 << 48) - 1;
        let u24 = (1u64 << 24) - 1;
        let e = Extent16::encode(u48, u48, u24, ExtentFlags::empty().with_gran(true)).unwrap();
        assert_eq!(e.logical_start(), u48);
        assert_eq!(e.physical_start(), u48);
        assert_eq!(e.length_blocks(), u24);
        assert!(e.flags().gran());
        // GRAN=1: 64 KiB units -> length_bytes for u24 max.
        assert_eq!(e.length_bytes(), u24.saturating_mul(64 * 1024));
        // File-size reach in GRAN=1 mode: a start near 2^48 in 64 KiB
        // units approaches 16 EiB without wrapping.
        let near_max = Extent16::encode(u48, 0, 4, ExtentFlags::empty().with_gran(true)).unwrap();
        assert!(near_max.logical_end() > (1u64 << 63));
        // And a maximally packed record saturates rather than wraps.
        assert_eq!(e.logical_end(), u64::MAX);
    }

    #[test]
    fn encode_rejects_overflow() {
        assert!(Extent16::encode(1u64 << 48, 0, 0, ExtentFlags::empty()).is_none());
        assert!(Extent16::encode(0, 1u64 << 48, 0, ExtentFlags::empty()).is_none());
        assert!(Extent16::encode(0, 0, 1u64 << 24, ExtentFlags::empty()).is_none());
    }

    #[test]
    fn decode_rejects_reserved_flags() {
        let e = Extent16::encode(1, 2, 3, ExtentFlags::empty()).unwrap();
        let mut bytes = *e.raw_bytes();
        bytes[15] |= 0b1000_0000;
        assert!(Extent16::decode(&bytes).is_none());
    }

    #[test]
    fn one_cache_line_holds_eight() {
        assert_eq!(std::mem::size_of::<Extent16>(), 16);
        assert!(std::mem::align_of::<Extent16>() <= 16);
    }

    #[test]
    fn logical_mapping_and_intersection() {
        // GRAN=0 (4 KiB units): logical blocks 10..14 -> physical 100..104.
        let e = Extent16::encode(10, 100, 4, ExtentFlags::empty()).unwrap();
        assert!(e.intersects_logical(10 * 4096, 1));
        assert!(e.intersects_logical(13 * 4096, 4096));
        assert!(!e.intersects_logical(14 * 4096, 4096));
        assert!(!e.intersects_logical(9 * 4096, 4096));
        assert_eq!(e.map_logical_to_physical(11 * 4096), Some(101 * 4096));
        assert_eq!(e.map_logical_to_physical(14 * 4096), None);
        assert_eq!(e.map_logical_to_physical(9 * 4096), None);
    }

    #[test]
    fn coalescing_detection() {
        let a = Extent16::encode(10, 100, 4, ExtentFlags::empty()).unwrap();
        let b = Extent16::encode(14, 104, 8, ExtentFlags::empty()).unwrap();
        assert!(a.coalescable_with(b));
        let c = Extent16::encode(14, 105, 8, ExtentFlags::empty()).unwrap();
        assert!(!a.coalescable_with(c));
        // Flag mismatch blocks coalescing even when physically adjacent.
        let d = Extent16::encode(14, 104, 8, ExtentFlags::empty().with_raw(true)).unwrap();
        assert!(!a.coalescable_with(d));
    }

    #[test]
    fn flag_mutators_are_idempotent() {
        let f = ExtentFlags::empty().with_raw(true).with_shared(true);
        assert!(f.raw() && f.shared() && !f.encrypted() && !f.gran() && !f.dedup());
        let f2 = f.with_raw(true);
        assert_eq!(f, f2);
    }

    #[test]
    fn bytemuck_casts() {
        let e = Extent16::encode(5, 6, 7, ExtentFlags::empty()).unwrap();
        let bytes: [u8; 16] = bytemuck::cast(e);
        let back: Extent16 = bytemuck::cast(bytes);
        assert_eq!(e, back);
    }
}
