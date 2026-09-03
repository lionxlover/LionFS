//! Small integer math helpers used throughout block/extent accounting
//! (rounding byte counts up to whole blocks, dividing ranges into chunks).

/// Rounds `value` up to the next multiple of `multiple` (`multiple` must be
/// nonzero). E.g. `round_up(4097, 4096) == 8192`.
#[allow(clippy::manual_div_ceil)]
pub fn round_up(value: u64, multiple: u64) -> u64 {
    debug_assert!(multiple != 0);
    ((value + multiple - 1) / multiple) * multiple
}

/// Rounds `value` up to the next power of two. `0` rounds up to `1`.
pub fn round_up_pow2(value: u64) -> u64 {
    if value <= 1 {
        return 1;
    }
    1u64 << (64 - (value - 1).leading_zeros())
}

pub fn is_power_of_two(value: u64) -> bool {
    value != 0 && (value & (value - 1)) == 0
}

/// Number of `block_size`-sized blocks needed to hold `bytes` -- the same
/// computation `mkfs`/extent-allocation code does inline in several
/// places, pulled out so it's written (and tested) once.
pub fn blocks_needed(bytes: u64, block_size: u64) -> u64 {
    round_up(bytes, block_size) / block_size
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_up_exact_multiple_is_unchanged() {
        assert_eq!(round_up(4096, 4096), 4096);
    }

    #[test]
    fn round_up_rounds_to_next_multiple() {
        assert_eq!(round_up(4097, 4096), 8192);
        assert_eq!(round_up(1, 4096), 4096);
        assert_eq!(round_up(0, 4096), 0);
    }

    #[test]
    fn round_up_pow2_cases() {
        assert_eq!(round_up_pow2(0), 1);
        assert_eq!(round_up_pow2(1), 1);
        assert_eq!(round_up_pow2(5), 8);
        assert_eq!(round_up_pow2(8), 8);
        assert_eq!(round_up_pow2(9), 16);
    }

    #[test]
    fn power_of_two_check() {
        assert!(is_power_of_two(1));
        assert!(is_power_of_two(4096));
        assert!(!is_power_of_two(0));
        assert!(!is_power_of_two(4095));
    }

    #[test]
    fn blocks_needed_matches_manual_calculation() {
        assert_eq!(blocks_needed(0, 4096), 0);
        assert_eq!(blocks_needed(1, 4096), 1);
        assert_eq!(blocks_needed(4096, 4096), 1);
        assert_eq!(blocks_needed(4097, 4096), 2);
    }
}
