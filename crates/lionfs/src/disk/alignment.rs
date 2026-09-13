//! Alignment helpers for O_DIRECT-style I/O, where the kernel requires
//! buffers, offsets, and lengths to be aligned to the device's logical
//! block size (typically 512 bytes) or, for good performance, its
//! physical/optimal I/O size (often 4096 on modern drives). LionFS's
//! current I/O path goes through the page cache rather than O_DIRECT, so
//! nothing enforces this today, but a future direct-I/O mode would need
//! exactly these checks.

pub fn is_aligned(value: u64, alignment: u64) -> bool {
    alignment != 0 && value % alignment == 0
}

pub fn align_down(value: u64, alignment: u64) -> u64 {
    if alignment == 0 {
        return value;
    }
    value - (value % alignment)
}

pub fn align_up(value: u64, alignment: u64) -> u64 {
    crate::utils::math::round_up(value, alignment.max(1))
}

/// Whether a buffer's address is aligned -- relevant for O_DIRECT, which
/// on Linux requires the *memory* address, not just the file offset, to be
/// aligned.
pub fn is_ptr_aligned(ptr: *const u8, alignment: usize) -> bool {
    (ptr as usize) % alignment == 0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn alignment_checks() {
        assert!(is_aligned(4096, 512));
        assert!(!is_aligned(4097, 512));
        assert!(!is_aligned(100, 0));
    }

    #[test]
    fn align_down_and_up() {
        assert_eq!(align_down(4097, 4096), 4096);
        assert_eq!(align_down(4096, 4096), 4096);
        assert_eq!(align_up(4097, 4096), 8192);
        assert_eq!(align_up(4096, 4096), 4096);
    }
}
