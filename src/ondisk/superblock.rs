//! Selecting the best valid superblock among the primary and secondary
//! copies -- extracted from `fs::filesystem::LionFS::new` (which had this
//! logic inline) so it's independently testable and reusable by
//! `tools::fsck`/`tools::repair`, which need the same "find the newest
//! checksum-valid copy" logic without constructing a full `LionFS`.

use crate::ondisk::serialization::{Superblock, BLOCK_SIZE, LIONFS_MAGIC};
use bytemuck::{bytes_of, pod_read_unaligned};

/// Standard candidate locations a superblock might be found at, matching
/// `mkfs`'s choice of secondary locations and `LionFS::new`'s search.
pub const CANDIDATE_LOCATIONS: [u64; 3] = [0, 8192, 16384];

/// Checks whether `buffer` (a raw `BLOCK_SIZE`-byte block) contains a
/// structurally valid superblock: right magic, and its self-checksum
/// (computed with the checksum field itself zeroed) matches.
pub fn is_valid_superblock_block(buffer: &[u8; BLOCK_SIZE]) -> Option<Superblock> {
    let sb: Superblock = pod_read_unaligned(buffer);
    if sb.magic != LIONFS_MAGIC {
        return None;
    }
    let mut sb_copy = sb;
    let saved_checksum = sb_copy.checksum;
    sb_copy.checksum = 0;
    if crate::utils::crc::compute_checksum(bytes_of(&sb_copy)) == saved_checksum {
        Some(sb)
    } else {
        None
    }
}

/// Picks the highest-`generation` valid superblock among a set of raw
/// candidate blocks (one per `CANDIDATE_LOCATIONS` entry, in the same
/// order, `None` for a location that couldn't be read at all).
pub fn pick_best(candidates: &[Option<[u8; BLOCK_SIZE]>]) -> Option<Superblock> {
    candidates
        .iter()
        .filter_map(|c| c.as_ref())
        .filter_map(is_valid_superblock_block)
        .max_by_key(|sb| sb.generation)
}

#[cfg(test)]
mod tests {
    use super::*;
    use bytemuck::Zeroable;

    fn make_valid_block(generation: u64) -> [u8; BLOCK_SIZE] {
        let mut sb = Superblock::zeroed();
        sb.magic = LIONFS_MAGIC;
        sb.generation = generation;
        sb.checksum = crate::utils::crc::compute_checksum(bytes_of(&sb));
        let mut buf = [0u8; BLOCK_SIZE];
        buf[..std::mem::size_of::<Superblock>()].copy_from_slice(bytes_of(&sb));
        buf
    }

    #[test]
    fn rejects_bad_magic() {
        let buf = [0u8; BLOCK_SIZE]; // all zero: magic won't match
        assert!(is_valid_superblock_block(&buf).is_none());
    }

    #[test]
    fn accepts_valid_checksum() {
        let buf = make_valid_block(5);
        let sb = is_valid_superblock_block(&buf).unwrap();
        assert_eq!(sb.generation, 5);
    }

    #[test]
    fn rejects_tampered_block() {
        let mut buf = make_valid_block(5);
        buf[16] ^= 0xFF; // corrupt a byte inside the superblock region
        assert!(is_valid_superblock_block(&buf).is_none());
    }

    #[test]
    fn picks_the_highest_generation_among_valid_candidates() {
        let candidates = vec![
            Some(make_valid_block(3)),
            Some(make_valid_block(7)),
            Some(make_valid_block(1)),
        ];
        let best = pick_best(&candidates).unwrap();
        assert_eq!(best.generation, 7);
    }

    #[test]
    fn skips_unreadable_and_invalid_candidates() {
        let mut corrupted = make_valid_block(9);
        corrupted[16] ^= 0xFF;
        let candidates = vec![None, Some(corrupted), Some(make_valid_block(2))];
        let best = pick_best(&candidates).unwrap();
        assert_eq!(best.generation, 2);
    }
}
