//! Human-readable formatting for on-disk structures -- the actual logic
//! behind what `tools::dump` should print (that tool was previously a
//! one-line stub that printed a fixed status string and inspected
//! nothing), kept separate from the CLI tool itself so it's testable
//! without spawning a process.

use crate::ondisk::serialization::{Inode, Superblock};

pub fn format_superblock(sb: &Superblock) -> String {
    format!(
        "magic:              {:#018x}\n\
         version:            {}\n\
         block_size:         {}\n\
         total_blocks:       {}\n\
         free_blocks:        {}\n\
         inode_count:        {}\n\
         root_inode:         {}\n\
         generation:         {}\n\
         raid_profile:       {:?}\n\
         pool_uuid:          {}\n\
         default_compression:{}\n\
         default_encryption: {}",
        sb.magic,
        sb.version,
        sb.block_size,
        sb.total_blocks,
        sb.free_blocks,
        sb.inode_count,
        sb.root_inode,
        sb.generation,
        crate::pool::raid::RaidProfile::from_u8(sb.raid_profile),
        crate::common::uuid::Uuid::from_bytes(sb.pool_uuid),
        sb.default_compression,
        sb.default_encryption,
    )
}

pub fn format_inode(inode: &Inode) -> String {
    let kind = if crate::inode::attributes::is_dir(inode) {
        "directory"
    } else if crate::inode::attributes::is_regular_file(inode) {
        "regular file"
    } else {
        "other"
    };
    format!(
        "ino:          {}\n\
         type:         {kind}\n\
         mode:         {:#o}\n\
         uid/gid:      {}/{}\n\
         links:        {}\n\
         size:         {} bytes\n\
         extent_count: {}\n\
         compression:  {}\n\
         encryption:   {}",
        inode.ino,
        crate::inode::attributes::permission_bits(inode),
        inode.uid,
        inode.gid,
        inode.links_count,
        inode.size,
        inode.extent_count,
        inode.compression_algo,
        inode.encryption_algo,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use bytemuck::Zeroable;

    #[test]
    fn superblock_dump_includes_key_fields() {
        let mut sb = Superblock::zeroed();
        sb.magic = crate::ondisk::serialization::LIONFS_MAGIC;
        sb.total_blocks = 12345;
        let out = format_superblock(&sb);
        assert!(out.contains("12345"));
        assert!(out.contains("Single")); // default raid_profile is 0 == Single
    }

    #[test]
    fn inode_dump_identifies_directory() {
        let inode = Inode::new_dir(2, 0o755, 0, 0, 0);
        let out = format_inode(&inode);
        assert!(out.contains("directory"));
        assert!(out.contains("755"));
    }
}
