//! FUSE bridge: adapts a [`VfsOps`] implementor to `fuser::Filesystem`
//! (Linux kernel FUSE and macOS macFUSE).
//!
//! This file is the *entire* Unix platform glue: every other module in
//! the crate is platform-neutral, so porting the engine to a new mount
//! technology (WinFsp, NFS export, a library API) means writing one more
//! file like this one, never touching the core.
//!
//! Translation rules, kept mechanical on purpose:
//! * `reply.error(errno)` becomes `Err(VfsError::new(errno))`.
//! * `fuser::TimeOrNow` becomes `SystemTime` (Now is resolved at entry).
//! * The FUSE readdir offset protocol (entry carries next offset) maps
//!   to `VfsOps::readdir(ino, offset, max)` returning entries that
//!   already carry their `next_offset`.
//! * TTL comes from [`VfsOps::entry_ttl`].

use std::ffi::OsStr;
use std::time::SystemTime;

use fuser::{
    FileAttr, FileType, Filesystem, KernelConfig, ReplyAttr, ReplyCreate, ReplyData,
    ReplyDirectory, ReplyEmpty, ReplyEntry, ReplyStatfs, ReplyWrite, Request, TimeOrNow,
};

use super::{VfsAttr, VfsCreate, VfsError, VfsKind, VfsOps, VfsResult, VfsSetAttr};

/// Wraps a `VfsOps` implementor as a `fuser::Filesystem`.
pub struct FuseBridge<T: VfsOps> {
    pub inner: T,
}

impl<T: VfsOps> FuseBridge<T> {
    #[must_use]
    pub fn new(inner: T) -> Self {
        Self { inner }
    }

    /// Consumes the bridge, returning the inner VfsOps (unmount path).
    pub fn into_inner(self) -> T {
        self.inner
    }
}

fn to_file_attr(attr: &VfsAttr, blksize: u32) -> FileAttr {
    let kind = match attr.kind {
        VfsKind::RegularFile => FileType::RegularFile,
        VfsKind::Directory => FileType::Directory,
        VfsKind::Symlink => FileType::Symlink,
    };
    FileAttr {
        ino: attr.ino,
        size: attr.size,
        blocks: attr.blocks,
        atime: attr.atime,
        mtime: attr.mtime,
        ctime: attr.ctime,
        crtime: attr.ctime,
        kind,
        perm: (attr.perm & 0o7777) as u16,
        nlink: attr.nlink,
        uid: attr.uid,
        gid: attr.gid,
        rdev: 0,
        blksize,
        flags: attr.flags,
    }
}

fn os_str_to_string(s: &OsStr) -> String {
    s.to_string_lossy().into_owned()
}

fn now_or(t: TimeOrNow) -> SystemTime {
    match t {
        TimeOrNow::Now => SystemTime::now(),
        TimeOrNow::SpecificTime(time) => time,
    }
}

impl<T: VfsOps> Filesystem for FuseBridge<T> {
    fn init(&mut self, _req: &Request, _config: &mut KernelConfig) -> Result<(), libc::c_int> {
        self.inner.init();
        Ok(())
    }

    fn destroy(&mut self) {
        self.inner.destroy();
    }

    fn lookup(&mut self, _req: &Request, parent: u64, name: &OsStr, reply: ReplyEntry) {
        match self.inner.lookup(parent, &os_str_to_string(name)) {
            Ok(attr) => reply.entry(&self.inner.entry_ttl(), &to_file_attr(&attr, 4096), 0),
            Err(e) => reply.error(e.errno),
        }
    }

    fn getattr(&mut self, _req: &Request, ino: u64, reply: ReplyAttr) {
        match self.inner.getattr(ino) {
            Ok(attr) => reply.attr(&self.inner.entry_ttl(), &to_file_attr(&attr, 4096)),
            Err(e) => reply.error(e.errno),
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn setattr(
        &mut self,
        _req: &Request,
        ino: u64,
        mode: Option<u32>,
        uid: Option<u32>,
        gid: Option<u32>,
        size: Option<u64>,
        atime: Option<TimeOrNow>,
        mtime: Option<TimeOrNow>,
        _ctime: Option<SystemTime>,
        _fh: Option<u64>,
        _crtime: Option<SystemTime>,
        _chgtime: Option<SystemTime>,
        _bkuptime: Option<SystemTime>,
        _flags: Option<u32>,
        reply: ReplyAttr,
    ) {
        let set = VfsSetAttr {
            mode,
            uid,
            gid,
            size,
            atime: atime.map(now_or),
            mtime: mtime.map(now_or),
        };
        match self.inner.setattr(ino, &set) {
            Ok(attr) => reply.attr(&self.inner.entry_ttl(), &to_file_attr(&attr, 4096)),
            Err(e) => reply.error(e.errno),
        }
    }

    fn readdir(
        &mut self,
        _req: &Request,
        ino: u64,
        _fh: u64,
        offset: i64,
        mut reply: ReplyDirectory,
    ) {
        if offset < 0 {
            reply.error(libc::EINVAL);
            return;
        }
        // The FUSE protocol feeds the offset of the last-served entry
        // back in; entries carry next_offset. Fetch a window of entries
        // starting at the requested offset.
        match self.inner.readdir(ino, offset as u64, 256) {
            Ok(entries) => {
                for entry in entries {
                    let kind = match entry.kind {
                        VfsKind::RegularFile => FileType::RegularFile,
                        VfsKind::Directory => FileType::Directory,
                        VfsKind::Symlink => FileType::Symlink,
                    };
                    if reply.add(entry.ino, entry.next_offset as i64, kind, entry.name) {
                        break; // Buffer full: next readdir starts at next_offset.
                    }
                }
                reply.ok();
            }
            Err(e) => reply.error(e.errno),
        }
    }

    fn read(
        &mut self,
        _req: &Request,
        ino: u64,
        _fh: u64,
        offset: i64,
        size: u32,
        _flags: i32,
        _lock_owner: Option<u64>,
        reply: ReplyData,
    ) {
        if offset < 0 {
            reply.error(libc::EINVAL);
            return;
        }
        match self.inner.read(ino, offset as u64, size) {
            Ok(data) => reply.data(&data),
            Err(e) => reply.error(e.errno),
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn write(
        &mut self,
        _req: &Request,
        ino: u64,
        _fh: u64,
        offset: i64,
        data: &[u8],
        _write_flags: u32,
        _flags: i32,
        _lock_owner: Option<u64>,
        reply: ReplyWrite,
    ) {
        if offset < 0 {
            reply.error(libc::EINVAL);
            return;
        }
        match self.inner.write(ino, offset as u64, data) {
            Ok(n) => reply.written(n),
            Err(e) => reply.error(e.errno),
        }
    }

    fn create(
        &mut self,
        req: &Request,
        parent: u64,
        name: &OsStr,
        mode: u32,
        _umask: u32,
        _flags: i32,
        reply: ReplyCreate,
    ) {
        let create = VfsCreate {
            mode,
            uid: req.uid(),
            gid: req.gid(),
        };
        match self.inner.create(parent, &os_str_to_string(name), &create) {
            Ok(attr) => reply.created(&self.inner.entry_ttl(), &to_file_attr(&attr, 4096), 0, 0, 0),
            Err(e) => reply.error(e.errno),
        }
    }

    fn mkdir(
        &mut self,
        req: &Request,
        parent: u64,
        name: &OsStr,
        mode: u32,
        _umask: u32,
        reply: ReplyEntry,
    ) {
        let create = VfsCreate {
            mode,
            uid: req.uid(),
            gid: req.gid(),
        };
        match self.inner.mkdir(parent, &os_str_to_string(name), &create) {
            Ok(attr) => reply.entry(&self.inner.entry_ttl(), &to_file_attr(&attr, 4096), 0),
            Err(e) => reply.error(e.errno),
        }
    }

    fn unlink(&mut self, _req: &Request, parent: u64, name: &OsStr, reply: ReplyEmpty) {
        match self.inner.unlink(parent, &os_str_to_string(name)) {
            Ok(()) => reply.ok(),
            Err(e) => reply.error(e.errno),
        }
    }

    fn rmdir(&mut self, _req: &Request, parent: u64, name: &OsStr, reply: ReplyEmpty) {
        match self.inner.rmdir(parent, &os_str_to_string(name)) {
            Ok(()) => reply.ok(),
            Err(e) => reply.error(e.errno),
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn rename(
        &mut self,
        _req: &Request,
        parent: u64,
        name: &OsStr,
        newparent: u64,
        newname: &OsStr,
        _flags: u32,
        reply: ReplyEmpty,
    ) {
        match self.inner.rename(
            parent,
            &os_str_to_string(name),
            newparent,
            &os_str_to_string(newname),
        ) {
            Ok(()) => reply.ok(),
            Err(e) => reply.error(e.errno),
        }
    }

    fn flush(&mut self, _req: &Request, ino: u64, _fh: u64, _lock_owner: u64, reply: ReplyEmpty) {
        match self.inner.flush(ino) {
            Ok(()) => reply.ok(),
            Err(e) => reply.error(e.errno),
        }
    }

    fn fsync(&mut self, _req: &Request, ino: u64, _fh: u64, datasync: bool, reply: ReplyEmpty) {
        match self.inner.fsync(ino, datasync) {
            Ok(()) => reply.ok(),
            Err(e) => reply.error(e.errno),
        }
    }

    fn statfs(&mut self, _req: &Request, ino: u64, reply: ReplyStatfs) {
        match self.inner.statfs(ino) {
            Ok(s) => reply.statfs(
                s.total_blocks,
                s.free_blocks,
                s.avail_blocks,
                s.total_inodes,
                s.free_inodes,
                s.block_size,
                s.max_name_len,
                s.block_size,
            ),
            Err(e) => reply.error(e.errno),
        }
    }

    fn access(&mut self, req: &Request, ino: u64, mask: i32, reply: ReplyEmpty) {
        match self.inner.access(ino, req.uid(), req.gid(), mask) {
            Ok(()) => reply.ok(),
            Err(e) => reply.error(e.errno),
        }
    }

    fn readlink(&mut self, _req: &Request, ino: u64, reply: ReplyData) {
        match self.inner.readlink(ino) {
            Ok(target) => reply.data(target.as_bytes()),
            Err(e) => reply.error(e.errno),
        }
    }

    fn symlink(
        &mut self,
        req: &Request,
        parent: u64,
        name: &OsStr,
        target: &std::path::Path,
        reply: ReplyEntry,
    ) {
        let target_str = target.to_string_lossy().into_owned();
        match self.inner.symlink(
            parent,
            &os_str_to_string(name),
            &target_str,
            req.uid(),
            req.gid(),
        ) {
            Ok(attr) => reply.entry(&self.inner.entry_ttl(), &to_file_attr(&attr, 4096), 0),
            Err(e) => reply.error(e.errno),
        }
    }

    // -- 3.6: extended attributes, POSIX ACLs, reflink -------------------
    //
    // The xattr reply protocol: `size == 0` is the size probe (the
    // caller allocates its buffer from our answer), a non-zero `size`
    // must either receive the value or ERANGE.

    fn setxattr(
        &mut self,
        _req: &Request,
        ino: u64,
        name: &OsStr,
        value: &[u8],
        flags: i32,
        _position: u32,
        reply: ReplyEmpty,
    ) {
        match self.inner.setxattr(ino, &os_str_to_string(name), value, flags) {
            Ok(()) => reply.ok(),
            Err(e) => reply.error(e.errno),
        }
    }

    fn getxattr(
        &mut self,
        _req: &Request,
        ino: u64,
        name: &OsStr,
        size: u32,
        reply: fuser::ReplyXattr,
    ) {
        match self.inner.getxattr(ino, &os_str_to_string(name)) {
            Ok(Some(value)) => {
                if size == 0 {
                    reply.size(value.len() as u32);
                } else if (value.len() as u32) <= size {
                    reply.data(&value);
                } else {
                    reply.error(libc::ERANGE);
                }
            }
            Ok(None) => reply.error(libc::ENODATA),
            Err(e) => reply.error(e.errno),
        }
    }

    fn listxattr(&mut self, _req: &Request, ino: u64, size: u32, reply: fuser::ReplyXattr) {
        match self.inner.listxattr(ino) {
            Ok(names) => {
                // The FUSE wire format: NUL-separated names.
                let mut buf = Vec::with_capacity(names.iter().map(|n| n.len() + 1).sum());
                for n in &names {
                    buf.extend_from_slice(n.as_bytes());
                    buf.push(0);
                }
                if size == 0 {
                    reply.size(buf.len() as u32);
                } else if (buf.len() as u32) <= size {
                    reply.data(&buf);
                } else {
                    reply.error(libc::ERANGE);
                }
            }
            Err(e) => reply.error(e.errno),
        }
    }

    fn removexattr(&mut self, _req: &Request, ino: u64, name: &OsStr, reply: ReplyEmpty) {
        match self.inner.removexattr(ino, &os_str_to_string(name)) {
            Ok(()) => reply.ok(),
            Err(e) => reply.error(e.errno),
        }
    }

    fn copy_file_range(
        &mut self,
        _req: &Request,
        ino_in: u64,
        _fh_in: u64,
        offset_in: i64,
        ino_out: u64,
        _fh_out: u64,
        offset_out: i64,
        len: u64,
        _flags: u32,
        reply: ReplyWrite,
    ) {
        if offset_in < 0 || offset_out < 0 {
            reply.error(libc::EINVAL);
            return;
        }
        match self
            .inner
            .copy_file_range(ino_in, offset_in as u64, ino_out, offset_out as u64, len)
        {
            Ok(n) => reply.written(n.min(u32::MAX as u64) as u32),
            Err(e) => reply.error(e.errno),
        }
    }
}

/// Mounts a `VfsOps` at `mountpoint` through fuser, blocking until
/// unmount (the CLI path).
pub fn mount<T: VfsOps + 'static>(
    ops: T,
    mountpoint: &std::path::Path,
    options: &[fuser::MountOption],
) -> VfsResult<()> {
    let bridge = FuseBridge::new(ops);
    fuser::mount2(bridge, mountpoint, options).map_err(|e| VfsError::from_io(&e))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pal::posix;
    use crate::vfs::VfsDirEntry;
    use std::time::Duration;

    /// A minimal in-memory VfsOps for bridge translation tests.
    /// Phase 10: the VfsOps surface is `&self`, so the mock keeps its
    /// mutable content in a `RefCell` (interior mutability), exactly
    /// like a real implementor keeps it behind locks.
    struct MemVfs {
        content: std::cell::RefCell<Vec<u8>>,
    }

    impl MemVfs {
        fn attr(&self) -> VfsAttr {
            VfsAttr {
                ino: 1,
                size: self.content.borrow().len() as u64,
                blocks: self.content.borrow().len().div_ceil(512) as u64,
                atime: SystemTime::UNIX_EPOCH,
                mtime: SystemTime::UNIX_EPOCH,
                ctime: SystemTime::UNIX_EPOCH,
                kind: VfsKind::RegularFile,
                perm: 0o644,
                nlink: 1,
                uid: 0,
                gid: 0,
                blksize: 4096,
                flags: 0,
            }
        }
    }

    impl VfsOps for MemVfs {
        fn init(&mut self) {}
        fn destroy(&mut self) {}
        fn lookup(&self, parent: u64, name: &str) -> VfsResult<VfsAttr> {
            if parent == 1 && name == "file" {
                Ok(self.attr())
            } else {
                Err(VfsError::noent())
            }
        }
        fn getattr(&self, ino: u64) -> VfsResult<VfsAttr> {
            if ino == 1 {
                Ok(self.attr())
            } else {
                Err(VfsError::noent())
            }
        }
        fn setattr(&self, ino: u64, _a: &VfsSetAttr) -> VfsResult<VfsAttr> {
            self.getattr(ino)
        }
        fn readdir(&self, ino: u64, offset: u64, _m: usize) -> VfsResult<Vec<VfsDirEntry>> {
            if ino == 1 && offset == 0 {
                Ok(vec![VfsDirEntry {
                    ino: 1,
                    kind: VfsKind::RegularFile,
                    name: "file".into(),
                    next_offset: 1,
                }])
            } else if ino == 1 {
                Ok(vec![])
            } else {
                Err(VfsError::noent())
            }
        }
        fn read(&self, ino: u64, offset: u64, size: u32) -> VfsResult<Vec<u8>> {
            if ino != 1 {
                return Err(VfsError::noent());
            }
            let c = self.content.borrow();
            let start = (offset as usize).min(c.len());
            let end = (start + size as usize).min(c.len());
            Ok(c[start..end].to_vec())
        }
        fn write(&self, ino: u64, offset: u64, data: &[u8]) -> VfsResult<u32> {
            if ino != 1 {
                return Err(VfsError::noent());
            }
            let mut c = self.content.borrow_mut();
            let end = offset as usize + data.len();
            if end > c.len() {
                c.resize(end, 0);
            }
            c[offset as usize..end].copy_from_slice(data);
            Ok(data.len() as u32)
        }
        fn create(&self, _p: u64, _n: &str, _c: &VfsCreate) -> VfsResult<VfsAttr> {
            Err(VfsError::nosys())
        }
        fn mkdir(&self, _p: u64, _n: &str, _c: &VfsCreate) -> VfsResult<VfsAttr> {
            Err(VfsError::nosys())
        }
        fn unlink(&self, _p: u64, _n: &str) -> VfsResult<()> {
            Err(VfsError::nosys())
        }
        fn rmdir(&self, _p: u64, _n: &str) -> VfsResult<()> {
            Err(VfsError::nosys())
        }
        fn rename(&self, _p: u64, _n: &str, _np: u64, _nn: &str) -> VfsResult<()> {
            Err(VfsError::nosys())
        }
        fn fsync(&self, _i: u64, _d: bool) -> VfsResult<()> {
            Ok(())
        }
        fn flush(&self, _i: u64) -> VfsResult<()> {
            Ok(())
        }
        fn statfs(&self, _i: u64) -> VfsResult<crate::vfs::VfsStatFs> {
            Ok(crate::vfs::VfsStatFs::default())
        }
        fn access(&self, _i: u64, _u: u32, _g: u32, mask: i32) -> VfsResult<()> {
            // Owner rw: mask 1 (F_OK=0 ok, R_OK=4 ok, W_OK=2 ok).
            if mask == libc::X_OK {
                return Err(VfsError::new(posix::EACCES));
            }
            Ok(())
        }
        fn readlink(&self, _i: u64) -> VfsResult<String> {
            Err(VfsError::nosys())
        }
        fn symlink(&self, _p: u64, _n: &str, _t: &str, _u: u32, _g: u32) -> VfsResult<VfsAttr> {
            Err(VfsError::nosys())
        }
        fn entry_ttl(&self) -> Duration {
            Duration::from_millis(500)
        }
    }

    #[test]
    fn bridge_constructs_over_vfs_ops() {
        let bridge = FuseBridge::new(MemVfs {
            content: std::cell::RefCell::new(vec![1, 2, 3]),
        });
        assert_eq!(bridge.inner.entry_ttl(), Duration::from_millis(500));
        // The Filesystem impl is exercised through the fuser types,
        // which require a live kernel session to drive; the construction
        // and ownership transfer is what compiles here, plus:
        let inner = bridge.into_inner();
        assert_eq!(*inner.content.borrow(), vec![1, 2, 3]);
    }
}
