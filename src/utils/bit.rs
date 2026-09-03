//! Bit-manipulation helpers for bitmap-based allocation
//! (`allocator::bitmap`) and anywhere else that packs booleans into bytes.

/// Index of the first zero (clear) bit in `data`, scanning from the start,
/// or `None` if every bit is set. Used for finding a free block/inode in a
/// bitmap without a bit-by-bit loop at the call site.
pub fn find_first_clear_bit(data: &[u8]) -> Option<usize> {
    for (byte_idx, &byte) in data.iter().enumerate() {
        if byte != 0xFF {
            let bit_idx = byte.trailing_ones() as usize;
            return Some(byte_idx * 8 + bit_idx);
        }
    }
    None
}

pub fn get_bit(data: &[u8], index: usize) -> bool {
    let byte_idx = index / 8;
    let bit_idx = index % 8;
    byte_idx < data.len() && (data[byte_idx] & (1 << bit_idx)) != 0
}

pub fn set_bit(data: &mut [u8], index: usize, value: bool) {
    let byte_idx = index / 8;
    let bit_idx = index % 8;
    if byte_idx >= data.len() {
        return;
    }
    if value {
        data[byte_idx] |= 1 << bit_idx;
    } else {
        data[byte_idx] &= !(1 << bit_idx);
    }
}

/// Number of set bits (population count) across the whole buffer -- e.g.
/// for reporting how many blocks/inodes in a bitmap are currently in use.
pub fn count_set_bits(data: &[u8]) -> u64 {
    data.iter().map(|b| b.count_ones() as u64).sum()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_first_clear_bit_across_byte_boundary() {
        let data = [0xFF, 0b1111_1101]; // bit 9 (byte 1, bit 1) is clear
        assert_eq!(find_first_clear_bit(&data), Some(9));
    }

    #[test]
    fn all_set_returns_none() {
        assert_eq!(find_first_clear_bit(&[0xFF, 0xFF]), None);
    }

    #[test]
    fn set_and_get_bit_round_trip() {
        let mut data = [0u8; 2];
        set_bit(&mut data, 5, true);
        assert!(get_bit(&data, 5));
        assert!(!get_bit(&data, 4));
        set_bit(&mut data, 5, false);
        assert!(!get_bit(&data, 5));
    }

    #[test]
    fn count_set_bits_matches_manual_count() {
        let data = [0b1010_1010, 0b0000_1111];
        assert_eq!(count_set_bits(&data), 4 + 4);
    }
}
