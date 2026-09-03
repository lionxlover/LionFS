//! Convenience constructors for `Inode` (defined in
//! `ondisk::serialization`, which stays the single source of truth for the
//! on-disk layout -- this just adds inherent methods to it from here,
//! which Rust allows for a local type regardless of which module defines
//! the struct itself). Reduces the duplicated ~15-field struct literals
//! that were previously repeated by hand in `fs::filesystem` and
//! `tools::mkfs`.

use crate::ondisk::serialization::{Extent, Inode, MAX_INLINE_EXTENTS};

const EMPTY_EXTENT: Extent = Extent {
    logical_start: 0,
    physical_start: 0,
    length: 0,
};

impl Inode {
    /// A new, empty regular file inode. Caller fills in `ino`/`uid`/`gid`
    /// afterward (they're request/allocation-specific).
    pub fn new_file(ino: u64, mode: u32, uid: u32, gid: u32, now: i64) -> Self {
        Self {
            ino,
            mode: mode | crate::pal::posix::S_IFREG,
            uid,
            gid,
            links_count: 1,
            flags: 0,
            padding1: 0,
            size: 0,
            ctime: now,
            mtime: now,
            atime: now,
            extent_count: 0,
            compression_algo: 0,
            encryption_algo: 0,
            key_id: 0,
            extents: [EMPTY_EXTENT; MAX_INLINE_EXTENTS],
            checksum: 0,
            spill_pad_head: [0; 4],
            spill_extent_root: 0,
        }
    }

    /// A new, empty directory inode with the conventional link count of 2
    /// (self, and the "." entry a real directory would contain).
    pub fn new_dir(ino: u64, mode: u32, uid: u32, gid: u32, now: i64) -> Self {
        let mut inode = Self::new_file(ino, mode, uid, gid, now);
        inode.mode = (inode.mode & !(crate::pal::posix::S_IFMT)) | crate::pal::posix::S_IFDIR;
        inode.links_count = 2;
        inode
    }

    pub fn is_empty_file(&self) -> bool {
        self.size == 0 && self.extent_count == 0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_file_has_regular_file_type() {
        let inode = Inode::new_file(5, 0o644, 1000, 1000, 12345);
        assert_eq!(
            inode.mode & crate::pal::posix::S_IFMT,
            crate::pal::posix::S_IFREG
        );
        assert_eq!(inode.mode & 0o777, 0o644);
        assert_eq!(inode.links_count, 1);
        assert!(inode.is_empty_file());
    }

    #[test]
    fn new_dir_has_directory_type_and_link_count_two() {
        let inode = Inode::new_dir(6, 0o755, 1000, 1000, 12345);
        assert_eq!(
            inode.mode & crate::pal::posix::S_IFMT,
            crate::pal::posix::S_IFDIR
        );
        assert_eq!(inode.links_count, 2);
    }
}
