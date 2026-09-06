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

/// 3.6: bridge a VFS error into the io::Error world (replication,
/// tools) preserving the errno.
impl From<VfsError> for std::io::Error {
    fn from(e: VfsError) -> Self {
        std::io::Error::from_raw_os_error(e.errno)
    }
}

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
///
/// Phase 10: every operation except mount-lifecycle `init`/`destroy`
/// takes `&self` -- the implementor must be internally synchronized.
/// This is what lets a bridge (or a library consumer, or the Rust/C
/// API) drive N threads through ONE mounted filesystem: reads run
/// concurrently, buffered writes land in the write-back intake layer
/// concurrently, and metadata staging serializes on the implementor's
/// staging lock (the exact journaling-FS model). A `&mut self`
/// surface would force every bridge to hold one giant mutex, which is
/// the single-writer mount model 3.3 shipped and this release retires.
pub trait VfsOps {
    /// Mount-time initialization (start workers, scrubbers).
    fn init(&mut self);
    /// Unmount-time teardown (sync, stop workers).
    fn destroy(&mut self);

    fn lookup(&self, parent: u64, name: &str) -> VfsResult<VfsAttr>;
    fn getattr(&self, ino: u64) -> VfsResult<VfsAttr>;
    fn setattr(&self, ino: u64, attr: &VfsSetAttr) -> VfsResult<VfsAttr>;
    fn readdir(&self, ino: u64, offset: u64, max_entries: usize)
        -> VfsResult<Vec<VfsDirEntry>>;
    fn read(&self, ino: u64, offset: u64, size: u32) -> VfsResult<Vec<u8>>;
    fn write(&self, ino: u64, offset: u64, data: &[u8]) -> VfsResult<u32>;
    fn create(&self, parent: u64, name: &str, create: &VfsCreate) -> VfsResult<VfsAttr>;
    fn mkdir(&self, parent: u64, name: &str, create: &VfsCreate) -> VfsResult<VfsAttr>;
    fn unlink(&self, parent: u64, name: &str) -> VfsResult<()>;
    fn rmdir(&self, parent: u64, name: &str) -> VfsResult<()>;
    fn rename(&self, parent: u64, name: &str, newparent: u64, newname: &str) -> VfsResult<()>;
    fn fsync(&self, ino: u64, datasync: bool) -> VfsResult<()>;
    /// Flush at file close (the FUSE flush entry).
    fn flush(&self, ino: u64) -> VfsResult<()>;
    fn statfs(&self, ino: u64) -> VfsResult<VfsStatFs>;
    /// access(2): uid/gid are the *caller's*.
    fn access(&self, ino: u64, uid: u32, gid: u32, mask: i32) -> VfsResult<()>;
    /// Read a symlink target (Symlink inodes only).
    fn readlink(&self, ino: u64) -> VfsResult<String>;
    /// Create a symlink `name` in `parent` pointing at `target`.
    fn symlink(
        &self,
        parent: u64,
        name: &str,
        target: &str,
        uid: u32,
        gid: u32,
    ) -> VfsResult<VfsAttr>;

    // -- 3.6: extended attributes, POSIX ACLs, reflink ------------------
    //
    // Default implementations return ENOSYS so existing VfsOps
    // implementors (test mocks, library embedders) keep compiling and
    // bridges advertise the capability honestly until implemented.

    /// getxattr(2): read one extended attribute. `Ok(None)` = the
    /// attribute does not exist (ENOATTR/ENODATA for the caller).
    fn getxattr(&self, _ino: u64, _name: &str) -> VfsResult<Option<Vec<u8>>> {
        Err(VfsError::nosys())
    }
    /// setxattr(2): create/replace one extended attribute. `flags`
    /// carries the Linux XATTR_CREATE (0x1) / XATTR_REPLACE (0x2) bits.
    fn setxattr(&self, _ino: u64, _name: &str, _value: &[u8], _flags: i32) -> VfsResult<()> {
        Err(VfsError::nosys())
    }
    /// listxattr(2): every attribute name on the inode.
    fn listxattr(&self, _ino: u64) -> VfsResult<Vec<String>> {
        Err(VfsError::nosys())
    }
    /// removexattr(2): delete one extended attribute.
    fn removexattr(&self, _ino: u64, _name: &str) -> VfsResult<()> {
        Err(VfsError::nosys())
    }
    /// copy_file_range(2): copy (or, when the engine can, REFLINK) a
    /// range from `ino_in` to `ino_out`. Returns the number of bytes
    /// actually copied. A whole-file, zero-offset, empty-destination
    /// request on a checksummed image takes the shared-extent reflink
    /// path (Btrfs/APFS clone parity); anything else is an honest
    /// byte copy.
    #[allow(clippy::too_many_arguments)]
    fn copy_file_range(
        &self,
        _ino_in: u64,
        _offset_in: u64,
        _ino_out: u64,
        _offset_out: u64,
        _len: u64,
    ) -> VfsResult<u64> {
        Err(VfsError::nosys())
    }

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
