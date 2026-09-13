use crate::ondisk::serialization::Superblock;
use crate::utils::crc::compute_checksum;
use bytemuck::bytes_of;

pub fn calculate_superblock_checksum(sb: &Superblock) -> u32 {
    let mut sb_copy = *sb;
    sb_copy.checksum = 0;
    compute_checksum(bytes_of(&sb_copy))
}
