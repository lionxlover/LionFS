//! # Volume Addressing (Pillar II)
//!
//! RFC-002 §4.1: "Every byte LionFS manages is named by a 128-bit volume
//! address whose format is fixed at mkfs time and validated at mount."
//!
//! * [`VolumeAddr`] -- the 128-bit logical address: 16-bit volume id,
//!   24-bit region, 24-bit device, 64-bit per-device LBA (4 KiB units).
//!   A 128-bit namespace in 4 KiB units addresses 2^140 bytes -- four
//!   orders of magnitude beyond a yottabyte.
//! * [`Extent16`] -- the packed 16-byte physical extent record
//!   (`logical_start: u48 | physical_start: u48 | length: u24 | flags`):
//!   one cache line holds eight; a B-epsilon leaf holds hundreds.
//!   `GRAN=0` counts 4 KiB units (file max 1 EiB); `GRAN=1` counts
//!   64 KiB units (16 EiB).
//!
//! The 256-bit alternative was analyzed and rejected in RFC-002 §10 (and
//! its Table 20): no shipping medium approaches 2^40 blocks, while wider
//! keys measurably split cache lines and slow hashing. This module keeps
//! that decision executable rather than aspirational: `VolumeAddr` is
//! arithmetic-complete (checked add of blocks, range intersection) so
//! the allocator, scrubber, and erasure engine all share one addressing
//! implementation.

pub mod extent16;
pub mod va;
pub mod va256;

pub use extent16::{Extent16, ExtentFlags};
pub use va::VolumeAddr;
pub use va256::{CapacityPlane, WideAddr};

/// The unit of the `device_lba` field: 4 KiB blocks, matching the 1.x
/// format's `BLOCK_SIZE` so the two address spaces interoperate.
pub const LBA_BLOCK_BYTES: u64 = 4096;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reexports_compose() {
        let a = VolumeAddr::compose(0, 0, 0, 123).expect("valid fields");
        let e = Extent16::encode(0, 456, 7, ExtentFlags::empty()).expect("valid fields");
        assert_eq!(a.device_lba(), 123);
        assert_eq!(e.length_blocks(), 7);
    }
}
