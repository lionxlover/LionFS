//! File-type and flag-bit helpers for `Inode::mode`/`Inode::flags`,
//! consolidating the `mode & S_IFMT == S_IFDIR`-style checks that were
//! previously repeated inline (e.g. in `fs::filesystem::to_file_attr` and
//! the new `setattr`/`rename` code).

use crate::ondisk::serialization::Inode;

pub fn is_dir(inode: &Inode) -> bool {
    (inode.mode & crate::pal::posix::S_IFMT) == crate::pal::posix::S_IFDIR
}

pub fn is_regular_file(inode: &Inode) -> bool {
    (inode.mode & crate::pal::posix::S_IFMT) == crate::pal::posix::S_IFREG
}

pub fn is_symlink(inode: &Inode) -> bool {
    (inode.mode & crate::pal::posix::S_IFMT) == crate::pal::posix::S_IFLNK
}

/// Permission bits only (mode with the file-type bits masked off),
/// e.g. for chmod, which must never be able to change the file type.
pub fn permission_bits(inode: &Inode) -> u32 {
    inode.mode & 0o7777
}

// Inode::flags bits. Only a couple are meaningful today; the rest are
// reserved for future use (e.g. no-compression / no-dedup per-file hints).
pub const FLAG_IMMUTABLE: u32 = 1 << 0;
pub const FLAG_APPEND_ONLY: u32 = 1 << 1;

pub fn is_immutable(inode: &Inode) -> bool {
    inode.flags & FLAG_IMMUTABLE != 0
}

pub fn is_append_only(inode: &Inode) -> bool {
    inode.flags & FLAG_APPEND_ONLY != 0
}

/// Whether a write starting at `offset` is allowed given the inode's
/// immutable/append-only flags. Immutable inodes reject all writes;
/// append-only inodes only accept writes that start exactly at the
/// current end of the file.
pub fn write_allowed_by_flags(inode: &Inode, offset: u64) -> bool {
    if is_immutable(inode) {
        return false;
    }
    if is_append_only(inode) && offset != inode.size {
        return false;
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ondisk::serialization::Extent;

    fn test_inode(mode: u32, flags: u32, size: u64) -> Inode {
        Inode {
            ino: 2,
            mode,
            uid: 0,
            gid: 0,
            links_count: 1,
            flags,
            padding1: 0,
            size,
            ctime: 0,
            mtime: 0,
            atime: 0,
            extent_count: 0,
            compression_algo: 0,
            encryption_algo: 0,
            key_id: 0,
            extents: [Extent {
                logical_start: 0,
                physical_start: 0,
                length: 0,
            }; 7],
            checksum: 0,
            spill_pad_head: [0; 4],
            spill_extent_root: 0,
        }
    }

    #[test]
    fn file_type_checks() {
        let dir = test_inode(crate::pal::posix::S_IFDIR | 0o755, 0, 0);
        let file = test_inode(crate::pal::posix::S_IFREG | 0o644, 0, 0);
        assert!(is_dir(&dir));
        assert!(!is_regular_file(&dir));
        assert!(is_regular_file(&file));
        assert!(!is_dir(&file));
    }

    #[test]
    fn permission_bits_excludes_file_type() {
        let file = test_inode(crate::pal::posix::S_IFREG | 0o644, 0, 0);
        assert_eq!(permission_bits(&file), 0o644);
    }

    #[test]
    fn immutable_blocks_all_writes() {
        let inode = test_inode(crate::pal::posix::S_IFREG | 0o644, FLAG_IMMUTABLE, 100);
        assert!(!write_allowed_by_flags(&inode, 0));
        assert!(!write_allowed_by_flags(&inode, 100));
    }

    #[test]
    fn append_only_blocks_non_tail_writes() {
        let inode = test_inode(crate::pal::posix::S_IFREG | 0o644, FLAG_APPEND_ONLY, 100);
        assert!(!write_allowed_by_flags(&inode, 50));
        assert!(write_allowed_by_flags(&inode, 100));
    }
}
