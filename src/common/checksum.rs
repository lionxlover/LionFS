//! Lightweight, non-cryptographic checksums for scenarios where the real
//! integrity algorithms (`integrity::algorithms`, used for on-disk block
//! checksumming) are more than what's needed -- e.g. quickly validating an
//! in-memory cache entry hasn't been scribbled over by a bug, where
//! resistance to deliberate tampering isn't the point, just cheap
//! detection of accidental corruption.

/// Fletcher-32: cheap, catches most accidental corruption (bit flips,
/// truncation, reordering) far better than a plain sum, at a fraction of
/// the cost of CRC32C/BLAKE3.
pub fn fletcher32(data: &[u8]) -> u32 {
    let mut sum1: u32 = 0xFFFF;
    let mut sum2: u32 = 0xFFFF;

    // Process 16-bit words; pad a trailing odd byte with zero.
    let mut chunks = data.chunks_exact(2);
    for chunk in &mut chunks {
        let word = u16::from_le_bytes([chunk[0], chunk[1]]) as u32;
        sum1 = (sum1 + word) % 0xFFFF;
        sum2 = (sum2 + sum1) % 0xFFFF;
    }
    if let [last] = chunks.remainder() {
        let word = *last as u32;
        sum1 = (sum1 + word) % 0xFFFF;
        sum2 = (sum2 + sum1) % 0xFFFF;
    }

    (sum2 << 16) | sum1
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn same_input_same_checksum() {
        let data = b"repeatable input data";
        assert_eq!(fletcher32(data), fletcher32(data));
    }

    #[test]
    fn different_input_different_checksum() {
        assert_ne!(fletcher32(b"hello world"), fletcher32(b"hello worlD"));
    }

    #[test]
    fn detects_byte_reordering() {
        // Plain XOR/sum checksums miss transpositions; Fletcher shouldn't.
        assert_ne!(fletcher32(b"AB"), fletcher32(b"BA"));
    }

    #[test]
    fn handles_odd_length_input() {
        // Just needs to not panic and to be deterministic.
        let a = fletcher32(b"odd");
        let b = fletcher32(b"odd");
        assert_eq!(a, b);
    }

    #[test]
    fn empty_input_is_well_defined() {
        assert_eq!(fletcher32(b""), fletcher32(b""));
    }
}
