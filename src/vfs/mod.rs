//! # Platform-Neutral VFS Operations (RFC-003)
//!
//! The 1.x code implemented `fuser::Filesystem` directly on the core
//! `LionFS` type, which welded the whole engine to Linux FUSE. The 2.0
//! shape: [`VfsOps`] is the *only* operations surface the core exposes;
//! platform bridges implement it outward:
//!
//! * **Unix**: [`fuse_bridge`] (Linux kernel FUSE, macOS macFUSE via
//!   fuser) adapts `VfsOps` to the fuser trait.
//! * **Windows**: the WinFsp bridge (RFC-003 §"Windows bridge") adapts
//!   `VfsOps` to `FSP_FILE_SYSTEM_INTERFACE`; see
//!   `docs/platform_support.md` for the binding plan (the core ships
//!   compile-clean without it; the bridge is opt-in via the WinFsp
//!   runtime, which cannot be linked from this repo's CI).
//!
//! The trait's method set mirrors the FUSE ABI surface deliberately --
//! name-for-name where possible -- so the bridge is a thin translation
//! layer with no semantic reinterpretation. Errors are errno numbers
//! from [`crate::pal::posix`] (the Linux ABI values, which are also the
//! FUSE wire values; see that module for why they are constants here
//! rather than libc references).

// The FUSE bridge only compiles where fuser does (Linux kernel FUSE,
// macOS macFUSE, FreeBSD fusefs). Windows mounts through the WinFsp
// bridge instead (docs/platform_support.md).
#[cfg(unix)]
pub mod fuse_bridge;

#[cfg(unix)]
pub use fuse_bridge::FuseBridge;

use std::fmt;
use std::time::{Duration, SystemTime};

/// VFS-level error: an errno code (see `pal::posix`) plus context.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct VfsError {
    pub errno: i32,
}

impl VfsError {
    #[must_use]
    pub fn new(errno: i32) -> Self {
        Self { errno }
    }

    #[must_use]
    pub fn noent() -> Self {
        Self::new(crate::pal::posix::ENOENT)
    }

    #[must_use]
    pub fn io() -> Self {
        Self::new(crate::pal::posix::EIO)
    }

    #[must_use]
    pub fn nosys() -> Self {
        Self::new(crate::pal::posix::ENOSYS)
    }

    #[must_use]
    pub fn perm() -> Self {
        Self::new(crate::pal::posix::EPERM)
    }

    #[must_use]
    pub fn from_io(err: &std::io::Error) -> Self {
        Self::new(crate::pal::posix::io_error_to_errno(err))
    }
}

impl fmt::Display for VfsError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let name = match self.errno {
            crate::pal::posix::ENOENT => "ENOENT",
            crate::pal::posix::EIO => "EIO",
            crate::pal::posix::EACCES => "EACCES",
            crate::pal::posix::EPERM => "EPERM",
            crate::pal::posix::EINVAL => "EINVAL",
            crate::pal::posix::ENOSPC => "ENOSPC",
            crate::pal::posix::EEXIST => "EEXIST",
            crate::pal::posix::ENOSYS => "ENOSYS",
            crate::pal::posix::ENOTDIR => "ENOTDIR",
            crate::pal::posix::EISDIR => "EISDIR",
            _ => "errno",
        };
        write!(f, "{name}({})", self.errno)
    }
}

impl std::error::Error for VfsError {}

pub type VfsResult<T> = Result<T, VfsError>;

/// File kind (platform-neutral; the bridge maps to fuser::FileType /
/// WinFsp file attributes).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VfsKind {
    RegularFile,
    Directory,
    Symlink,
}

impl VfsKind {
    #[must_use]
    pub fn from_mode(mode: u32) -> Self {
        use crate::pal::posix as p;
        if p::is_dir(mode) {
            Self::Directory
        } else if p::is_lnk(mode) {
            Self::Symlink
        } else {
            Self::RegularFile
        }
    }
}

/// File attributes, the neutral shape of stat(2).
#[derive(Debug, Clone, Copy)]
pub struct VfsAttr {
    pub ino: u64,
    pub size: u64,
    /// 512-byte blocks allocated (stat(2) convention).
    pub blocks: u64,
    pub atime: SystemTime,
    pub mtime: SystemTime,
    pub ctime: SystemTime,
    pub kind: VfsKind,
    pub perm: u32,
    pub nlink: u32,
    pub uid: u32,
    pub gid: u32,
    /// Preferred I/O size (the fs block size).
    pub blksize: u32,
    pub flags: u32,
}

/// A directory entry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VfsDirEntry {
    pub ino: u64,
    pub kind: VfsKind,
    pub name: String,
    /// The 1-based index for readdir offset protocol (matches the FUSE
    /// readdir offset contract; the bridge uses it verbatim).
    pub next_offset: u64,
}

/// statfs(2) shape.
#[derive(Debug, Clone, Copy, Default)]
pub struct VfsStatFs {
    pub total_blocks: u64,
    pub free_blocks: u64,
    pub avail_blocks: u64,
    pub total_inodes: u64,
    pub free_inodes: u64,
    pub block_size: u32,
    pub max_name_len: u32,
}

/// setattr fields (None = leave unchanged).
#[derive(Debug, Clone, Copy, Default)]
pub struct VfsSetAttr {
    pub mode: Option<u32>,
    pub uid: Option<u32>,
    pub gid: Option<u32>,
    pub size: Option<u64>,
    pub atime: Option<SystemTime>,
    pub mtime: Option<SystemTime>,
}

/// Creation parameters.
#[derive(Debug, Clone, Copy)]
pub struct VfsCreate {
    pub mode: u32,
    pub uid: u32,
    pub gid: u32,
}

/// The operations surface. One method per FUSE ABI entry point (plus
/// `init`/`destroy`); bridges translate, nothing more.
pub trait VfsOps {
    /// Mount-time initialization (start workers, scrubbers).
    fn init(&mut self);
    /// Unmount-time teardown (sync, stop workers).
    fn destroy(&mut self);

    fn lookup(&mut self, parent: u64, name: &str) -> VfsResult<VfsAttr>;
    fn getattr(&mut self, ino: u64) -> VfsResult<VfsAttr>;
    fn setattr(&mut self, ino: u64, attr: &VfsSetAttr) -> VfsResult<VfsAttr>;
    fn readdir(&mut self, ino: u64, offset: u64, max_entries: usize)
        -> VfsResult<Vec<VfsDirEntry>>;
    fn read(&mut self, ino: u64, offset: u64, size: u32) -> VfsResult<Vec<u8>>;
    fn write(&mut self, ino: u64, offset: u64, data: &[u8]) -> VfsResult<u32>;
    fn create(&mut self, parent: u64, name: &str, create: &VfsCreate) -> VfsResult<VfsAttr>;
    fn mkdir(&mut self, parent: u64, name: &str, create: &VfsCreate) -> VfsResult<VfsAttr>;
    fn unlink(&mut self, parent: u64, name: &str) -> VfsResult<()>;
    fn rmdir(&mut self, parent: u64, name: &str) -> VfsResult<()>;
    fn rename(&mut self, parent: u64, name: &str, newparent: u64, newname: &str) -> VfsResult<()>;
    fn fsync(&mut self, ino: u64, datasync: bool) -> VfsResult<()>;
    /// Flush at file close (the FUSE flush entry).
    fn flush(&mut self, ino: u64) -> VfsResult<()>;
    fn statfs(&mut self, ino: u64) -> VfsResult<VfsStatFs>;
    /// access(2): uid/gid are the *caller's*.
    fn access(&mut self, ino: u64, uid: u32, gid: u32, mask: i32) -> VfsResult<()>;
    /// Read a symlink target (Symlink inodes only).
    fn readlink(&mut self, ino: u64) -> VfsResult<String>;
    /// Create a symlink `name` in `parent` pointing at `target`.
    fn symlink(
        &mut self,
        parent: u64,
        name: &str,
        target: &str,
        uid: u32,
        gid: u32,
    ) -> VfsResult<VfsAttr>;
    /// Time-to-live for positive/negative dentries (bridges feed this to
    /// the kernel to keep RCU path caching effective).
    fn entry_ttl(&self) -> Duration {
        Duration::from_secs(1)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn error_names_render() {
        assert_eq!(VfsError::noent().to_string(), "ENOENT(2)");
        assert_eq!(VfsError::io().to_string(), "EIO(5)");
        assert_eq!(VfsError::new(999).to_string(), "errno(999)");
    }

    #[test]
    fn kind_from_mode() {
        assert_eq!(
            VfsKind::from_mode(crate::pal::posix::S_IFREG | 0o644),
            VfsKind::RegularFile
        );
        assert_eq!(
            VfsKind::from_mode(crate::pal::posix::S_IFDIR | 0o755),
            VfsKind::Directory
        );
        assert_eq!(
            VfsKind::from_mode(crate::pal::posix::S_IFLNK | 0o777),
            VfsKind::Symlink
        );
    }

    #[test]
    fn io_error_mapping() {
        let e = std::io::Error::new(std::io::ErrorKind::NotFound, "x");
        assert_eq!(VfsError::from_io(&e).errno, crate::pal::posix::ENOENT);
    }
}
