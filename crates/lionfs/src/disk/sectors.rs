//! Conversions between LionFS's 4096-byte logical blocks and a device's
//! native sector size (typically 512 bytes, sometimes 4096 for "4Kn"
//! drives) -- needed once `disk::geometry` probing is used for anything
//! beyond reporting, e.g. validating a device's sectors divide evenly into
//! `BLOCK_SIZE` before trusting it as backing storage.

use crate::ondisk::serialization::BLOCK_SIZE;

pub fn sectors_per_block(sector_size: u32) -> Option<u64> {
    if sector_size == 0 || BLOCK_SIZE as u32 % sector_size != 0 {
        return None;
    }
    Some(BLOCK_SIZE as u64 / sector_size as u64)
}

pub fn block_to_sector(block_num: u64, sector_size: u32) -> Option<u64> {
    Some(block_num * sectors_per_block(sector_size)?)
}

/// How many `BLOCK_SIZE` logical blocks fit in a device of `size_bytes`,
/// rounding down (a partial trailing block isn't usable).
pub fn usable_blocks_for_size(size_bytes: u64) -> u64 {
    size_bytes / BLOCK_SIZE as u64
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn standard_512_byte_sectors() {
        assert_eq!(sectors_per_block(512), Some(8));
        assert_eq!(block_to_sector(2, 512), Some(16));
    }

    #[test]
    fn four_k_native_sectors() {
        assert_eq!(sectors_per_block(4096), Some(1));
    }

    #[test]
    fn non_dividing_sector_size_is_rejected() {
        assert_eq!(sectors_per_block(4000), None);
        assert_eq!(sectors_per_block(0), None);
    }

    #[test]
    fn usable_blocks_rounds_down() {
        assert_eq!(usable_blocks_for_size(BLOCK_SIZE as u64 * 3), 3);
        assert_eq!(usable_blocks_for_size(BLOCK_SIZE as u64 * 3 + 1), 3);
    }
}
