//! Errno codes and `S_IF*` file-type bits, independent of `libc`.
//!
//! The 1.x code sprinkled `libc::S_IFDIR`, `libc::ENOENT`, ... through the
//! inode/metadata/VFS layers, which (a) tied the whole core to unix and
//! (b) left the FUSE reply codes implicit. The values below are the
//! **Linux/x86-64 ABI values**, which is exactly what the FUSE kernel
//! protocol expects to receive, and they are identical (for every code we
//! use) on the macOS and Windows CRTs where they matter. Defining them
//! here makes the wire ABI an explicit, documented contract instead of an
//! accident of `libc`.
//!
//! These are *not* used to interpret `std::io::Error::raw_os_error()`
//! values (on Windows those are Win32 error codes, not errnos); use
//! [`io_error_to_errno`] for that direction.

use std::io::Error;

// -- errno (Linux ABI = FUSE wire ABI) ---------------------------------------

pub const EPERM: i32 = 1; // Operation not permitted
pub const ENOENT: i32 = 2; // No such file or directory
pub const EIO: i32 = 5; // I/O error
pub const EACCES: i32 = 13; // Permission denied
pub const EEXIST: i32 = 17; // File exists
pub const ENOTDIR: i32 = 20; // Not a directory
pub const EISDIR: i32 = 21; // Is a directory
pub const EINVAL: i32 = 22; // Invalid argument
pub const ENOSPC: i32 = 28; // No space left on device
pub const EROFS: i32 = 30; // Read-only file system
pub const ENOSYS: i32 = 38; // Function not implemented
pub const ENOTEMPTY: i32 = 39; // Directory not empty
pub const EDQUOT: i32 = 122; // Quota exceeded
pub const EBADE: i32 = 52; // Invalid exchange (LionFS: unsupported media op)

// -- file type bits ----------------------------------------------------------

pub const S_IFMT: u32 = 0o170_000; // bit mask for the file type
pub const S_IFSOCK: u32 = 0o140_000;
pub const S_IFLNK: u32 = 0o120_000;
pub const S_IFREG: u32 = 0o100_000;
pub const S_IFBLK: u32 = 0o060_000;
pub const S_IFDIR: u32 = 0o040_000;
pub const S_IFCHR: u32 = 0o020_000;
pub const S_IFIFO: u32 = 0o010_000;

/// Extracts the type from a full mode word.
#[inline]
#[must_use]
pub fn file_type_of(mode: u32) -> u32 {
    mode & S_IFMT
}

/// Does `mode` describe a directory?
#[inline]
#[must_use]
pub fn is_dir(mode: u32) -> bool {
    file_type_of(mode) == S_IFDIR
}

/// Does `mode` describe a regular file?
#[inline]
#[must_use]
pub fn is_reg(mode: u32) -> bool {
    file_type_of(mode) == S_IFREG
}

/// Does `mode` describe a symlink?
#[inline]
#[must_use]
pub fn is_lnk(mode: u32) -> bool {
    file_type_of(mode) == S_IFLNK
}

/// FUSE wire type code for a directory entry (`0` file would be a bad
/// encoding: the 1.x directory format already stores 1 = file, 2 = dir;
/// these helpers keep that mapping explicit).
#[inline]
#[must_use]
pub fn dirent_type(mode: u32) -> u8 {
    if is_dir(mode) {
        2
    } else {
        1
    }
}

/// Maps a `std::io::Error` to the errno the VFS layer should report.
///
/// On unix this preserves the raw OS errno when there is one. On Windows
/// a handful of common Win32 codes are mapped (the same set MSVC's CRT
/// maps); anything unmapped becomes `EIO` -- never a guess beyond that.
#[must_use]
pub fn io_error_to_errno(err: &Error) -> i32 {
    match err.raw_os_error() {
        Some(code) => code,
        None => match err.kind() {
            std::io::ErrorKind::NotFound => ENOENT,
            std::io::ErrorKind::PermissionDenied => EACCES,
            std::io::ErrorKind::AlreadyExists => EEXIST,
            std::io::ErrorKind::DirectoryNotEmpty => ENOTEMPTY,
            std::io::ErrorKind::NotADirectory => ENOTDIR,
            std::io::ErrorKind::IsADirectory => EISDIR,
            std::io::ErrorKind::InvalidInput => EINVAL,
            std::io::ErrorKind::ReadOnlyFilesystem => EROFS,
            _ => EIO,
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn file_type_predicates() {
        assert!(is_dir(S_IFDIR | 0o755));
        assert!(is_reg(S_IFREG | 0o644));
        assert!(is_lnk(S_IFLNK | 0o777));
        assert!(!is_dir(S_IFREG | 0o755));
    }

    #[test]
    fn dirent_type_matches_1x_encoding() {
        assert_eq!(dirent_type(S_IFREG), 1);
        assert_eq!(dirent_type(S_IFDIR), 2);
    }

    #[test]
    fn kind_fallback_on_windows_relevant_errors() {
        let e = std::io::Error::new(std::io::ErrorKind::NotFound, "missing");
        assert_eq!(io_error_to_errno(&e), ENOENT);
        let e = std::io::Error::new(std::io::ErrorKind::PermissionDenied, "denied");
        assert_eq!(io_error_to_errno(&e), EACCES);
    }

    #[cfg(unix)]
    #[test]
    fn os_errno_preserved_on_unix() {
        let e = std::io::Error::from_raw_os_error(2);
        assert_eq!(io_error_to_errno(&e), ENOENT);
    }
}
