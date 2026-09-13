//! Inode metadata update logic, factored out of
//! `fs::filesystem::LionFS::setattr` so the "which fields change" decision
//! is unit-testable without a live FUSE request.

use crate::ondisk::serialization::Inode;
/// Neutral time setter: `Now` resolves at apply time, `At` is a fixed
/// epoch-seconds value. Replaces the 1.x direct use of fuser's
/// TimeOrNow so the metadata core stays platform-free.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TimeOrNow {
    Now,
    At(i64),
}

#[derive(Debug, Clone, Copy, Default)]
pub struct AttrChanges {
    pub mode: Option<u32>,
    pub uid: Option<u32>,
    pub gid: Option<u32>,
    pub atime: Option<TimeOrNow>,
    pub mtime: Option<TimeOrNow>,
}

/// Applies `changes` to `inode`, always bumping `ctime` to `now` (a
/// metadata change always updates ctime, per POSIX, regardless of which
/// specific fields changed). Does not touch `size` -- callers handle
/// truncation separately via `file::writer::FileManager::truncate_file`,
/// since that also needs to free/allocate blocks, not just flip a field.
pub fn apply_attr_changes(inode: &mut Inode, changes: AttrChanges, now: i64) {
    if let Some(m) = changes.mode {
        inode.mode = (inode.mode & crate::pal::posix::S_IFMT) | (m & 0o7777);
    }
    if let Some(u) = changes.uid {
        inode.uid = u;
    }
    if let Some(g) = changes.gid {
        inode.gid = g;
    }
    if let Some(a) = changes.atime {
        inode.atime = crate::inode::timestamps::resolve_time_or_now(a, now);
    }
    if let Some(m) = changes.mtime {
        inode.mtime = crate::inode::timestamps::resolve_time_or_now(m, now);
    }
    inode.ctime = now;
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ondisk::serialization::Extent;

    fn test_inode() -> Inode {
        Inode {
            ino: 2,
            mode: crate::pal::posix::S_IFREG | 0o644,
            uid: 1000,
            gid: 1000,
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
    fn chmod_preserves_file_type_bits() {
        let mut inode = test_inode();
        apply_attr_changes(
            &mut inode,
            AttrChanges {
                mode: Some(0o600),
                ..Default::default()
            },
            100,
        );
        assert_eq!(
            inode.mode & crate::pal::posix::S_IFMT,
            crate::pal::posix::S_IFREG
        );
        assert_eq!(inode.mode & 0o7777, 0o600);
    }

    #[test]
    fn unset_fields_are_left_alone() {
        let mut inode = test_inode();
        inode.uid = 42;
        apply_attr_changes(
            &mut inode,
            AttrChanges {
                gid: Some(7),
                ..Default::default()
            },
            100,
        );
        assert_eq!(inode.uid, 42); // untouched
        assert_eq!(inode.gid, 7);
    }

    #[test]
    fn any_change_bumps_ctime() {
        let mut inode = test_inode();
        apply_attr_changes(
            &mut inode,
            AttrChanges {
                uid: Some(5),
                ..Default::default()
            },
            999,
        );
        assert_eq!(inode.ctime, 999);
    }
}
