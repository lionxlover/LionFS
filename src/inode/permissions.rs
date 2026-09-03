//! Real POSIX permission checking. Previously nothing in the FUSE layer
//! checked a requesting uid/gid against an inode's mode bits at all --
//! every request was implicitly allowed regardless of ownership or mode,
//! with `DefaultPermissions` in `userspace/cli/mount.rs`'s mount options
//! meaning the *kernel* would enforce permissions using whatever
//! `getattr` reported, but nothing enforced them on the filesystem's own
//! `access()` path (which didn't exist -- see `fs::filesystem::access`).

use crate::ondisk::serialization::Inode;

/// FUSE/libc access mask bits (`F_OK`/`R_OK`/`W_OK`/`X_OK`).
pub const F_OK: i32 = 0;
pub const R_OK: i32 = 4;
pub const W_OK: i32 = 2;
pub const X_OK: i32 = 1;

/// Checks whether a process with the given uid/gid may access `inode` in
/// the way `mask` describes (any combination of `R_OK`/`W_OK`/`X_OK`, or
/// `F_OK` to just check existence). Root (uid 0) always passes, matching
/// standard POSIX semantics.
pub fn check_access(inode: &Inode, uid: u32, gid: u32, mask: i32) -> bool {
    if uid == 0 {
        return true;
    }
    if mask == F_OK {
        return true;
    }

    let mode = inode.mode;
    let bits = if uid == inode.uid {
        (mode >> 6) & 0o7
    } else if gid == inode.gid {
        (mode >> 3) & 0o7
    } else {
        mode & 0o7
    } as i32;

    (bits & mask) == mask
}

pub fn can_read(inode: &Inode, uid: u32, gid: u32) -> bool {
    check_access(inode, uid, gid, R_OK)
}

pub fn can_write(inode: &Inode, uid: u32, gid: u32) -> bool {
    check_access(inode, uid, gid, W_OK)
}

pub fn can_execute(inode: &Inode, uid: u32, gid: u32) -> bool {
    check_access(inode, uid, gid, X_OK)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ondisk::serialization::Extent;

    fn test_inode(mode: u32, uid: u32, gid: u32) -> Inode {
        Inode {
            ino: 2,
            mode,
            uid,
            gid,
            links_count: 1,
            flags: 0,
            padding1: 0,
            size: 0,
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
    fn owner_gets_owner_bits() {
        let inode = test_inode(0o640, 1000, 1000); // rw-r-----
        assert!(can_read(&inode, 1000, 1000));
        assert!(can_write(&inode, 1000, 1000));
        assert!(!can_execute(&inode, 1000, 1000));
    }

    #[test]
    fn group_member_gets_group_bits_not_owner_bits() {
        let inode = test_inode(0o640, 1000, 1000); // owner rw-, group r--
        assert!(can_read(&inode, 2000, 1000));
        assert!(!can_write(&inode, 2000, 1000));
    }

    #[test]
    fn other_gets_other_bits() {
        let inode = test_inode(0o644, 1000, 1000); // other r--
        assert!(can_read(&inode, 3000, 3000));
        assert!(!can_write(&inode, 3000, 3000));
    }

    #[test]
    fn root_always_passes() {
        let inode = test_inode(0o000, 1000, 1000); // no permissions for anyone
        assert!(check_access(&inode, 0, 0, R_OK | W_OK | X_OK));
    }

    #[test]
    fn f_ok_only_checks_existence() {
        let inode = test_inode(0o000, 1000, 1000);
        assert!(check_access(&inode, 3000, 3000, F_OK));
    }

    #[test]
    fn combined_mask_requires_all_bits() {
        let inode = test_inode(0o644, 1000, 1000); // owner rw-
        assert!(check_access(&inode, 1000, 1000, R_OK));
        assert!(!check_access(&inode, 1000, 1000, R_OK | X_OK));
    }
}
