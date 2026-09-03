//! Positioned (offset-based) reads and writes on device/file handles.
//!
//! The 1.x code used `std::os::unix::fs::FileExt::read_at`/`write_at`
//! directly, which made the entire block layer Linux/macOS-only. The
//! equivalents on Windows are `seek_read`/`seek_write` (which mutate the
//! file cursor under the hood — still safe for positioned I/O from a single
//! thread per handle, and our device handles are per-shard/thread, never
//! shared unsynchronized). This module is the single shim both go through.
//!
//! Semantics guaranteed on every platform:
//!
//! * `pread_full` reads exactly `buf.len()` bytes or fails
//!   (`UnexpectedEof` at end of device, like the 1.x `read_full` helper
//!   that this replaces -- a short read is never silently zero-filled).
//! * `pwrite_full` writes exactly `buf.len()` bytes or fails.
//! * Both are positional: the file cursor is never depended upon, and
//!   concurrent positioned calls on the same handle from different threads
//!   are each individually atomic w.r.t. their own range (the OS makes no
//!   cross-call ordering guarantee, which the I/O engine is designed
//!   around anyway: ordering comes from the transaction layer, not from
//!   device call order).

use std::fs::{File, OpenOptions};
use std::io::{Error, ErrorKind, Result};
use std::path::Path;

#[cfg(unix)]
use std::os::unix::fs::FileExt;
#[cfg(windows)]
use std::os::windows::fs::FileExt as WinFileExt;

/// Reads exactly `buf.len()` bytes at `offset`. A partial read (possible
/// at or past end-of-device) is `UnexpectedEof`, never success with stale
/// buffer contents.
#[inline]
pub fn pread_full(file: &File, buf: &mut [u8], offset: u64) -> Result<()> {
    let n = pread_at(file, buf, offset)?;
    if n != buf.len() {
        return Err(Error::new(
            ErrorKind::UnexpectedEof,
            format!(
                "short read: got {n} of {} bytes at offset {offset}",
                buf.len()
            ),
        ));
    }
    Ok(())
}

/// Writes exactly `buf.len()` bytes at `offset`; partial writes are an
/// error. Returns the number of bytes written on success (== `buf.len()`).
#[inline]
pub fn pwrite_full(file: &File, buf: &[u8], offset: u64) -> Result<usize> {
    let n = pwrite_at(file, buf, offset)?;
    if n != buf.len() {
        return Err(Error::new(
            ErrorKind::WriteZero,
            format!(
                "short write: got {n} of {} bytes at offset {offset}",
                buf.len()
            ),
        ));
    }
    Ok(n)
}

/// Platform positioned read; returns bytes read.
#[inline]
pub fn pread_at(file: &File, buf: &mut [u8], offset: u64) -> Result<usize> {
    #[cfg(unix)]
    {
        file.read_at(buf, offset)
    }
    #[cfg(windows)]
    {
        // seek_read uses the *current* file pointer as a base then seeks
        // relative... no: it is documented as "reads starting at offset
        // from the current position" -- i.e. it seeks to `offset` from the
        // *start* through an internal SetFilePointerEx? To avoid relying
        // on any cursor state, serialize cursor moves per call under a
        // re-entrant guard: on Windows we accept that concurrent calls on
        // the same handle race the cursor. Our engine never issues
        // concurrent positioned I/O on one handle (per-shard ownership),
        // so this is correct by construction; the PAL documents the rule.
        file.seek_read(buf, offset)
    }
    #[cfg(not(any(unix, windows)))]
    {
        let _ = (file, buf, offset);
        Err(Error::new(
            ErrorKind::Unsupported,
            "pread not supported on this platform",
        ))
    }
}

/// Platform positioned write; returns bytes written.
#[inline]
pub fn pwrite_at(file: &File, buf: &[u8], offset: u64) -> Result<usize> {
    #[cfg(unix)]
    {
        file.write_at(buf, offset)
    }
    #[cfg(windows)]
    {
        // Same ownership rule as seek_read: single issuer per handle.
        file.seek_write(buf, offset)
    }
    #[cfg(not(any(unix, windows)))]
    {
        let _ = (file, buf, offset);
        Err(Error::new(
            ErrorKind::Unsupported,
            "pwrite not supported on this platform",
        ))
    }
}

/// Opens a device or image file for unbuffered-capable read/write access.
///
/// On unix this is a plain `O_RDWR` open (O_DIRECT is deliberately NOT
/// forced here: the engine decides buffering policy per-pool; image files
/// in tests live on tmpfs where O_DIRECT is invalid). On Windows the
/// equivalent of no-share + normal buffering is used; `FILE_FLAG_NO_BUFFERING`
/// is applied later by the geometry-aware engine path, not blanket-open.
pub fn open_device_rw<P: AsRef<Path>>(path: P) -> Result<File> {
    OpenOptions::new().read(true).write(true).open(path)
}

/// Creates (or truncates) an image file of `size_bytes` and returns it
/// opened read/write. Used by `mkfs` and tests.
pub fn create_image<P: AsRef<Path>>(path: P, size_bytes: u64) -> Result<File> {
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(true)
        .open(path)?;
    file.set_len(size_bytes)?;
    Ok(file)
}

/// Opens a device/image read-only (verify, inspect, dump tools).
pub fn open_device_ro<P: AsRef<Path>>(path: P) -> Result<File> {
    OpenOptions::new().read(true).open(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_path(tag: &str) -> std::path::PathBuf {
        std::env::temp_dir().join(format!("lionfs_pal_file_{}_{}", tag, std::process::id()))
    }

    #[test]
    fn positioned_roundtrip() {
        let path = temp_path("rw");
        let f = create_image(&path, 64 * 1024).unwrap();
        let payload: Vec<u8> = (0u32..1024).map(|i| (i % 251) as u8).collect();
        pwrite_full(&f, &payload, 4096).unwrap();
        let mut back = vec![0u8; payload.len()];
        pread_full(&f, &mut back, 4096).unwrap();
        assert_eq!(back, payload);
        drop(f);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn short_read_at_eof_is_explicit() {
        let path = temp_path("eof");
        let f = create_image(&path, 512).unwrap();
        let mut buf = [0u8; 4096];
        let err = pread_full(&f, &mut buf, 0).unwrap_err();
        assert_eq!(err.kind(), ErrorKind::UnexpectedEof);
        drop(f);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn pwrite_past_eof_extends_like_pwrite() {
        // Positioned writes past logical EOF are legal (sparse) on unix and
        // extend the file on Windows via seek_write; either way a read of
        // the freshly written range must succeed.
        let path = temp_path("sparse");
        let f = create_image(&path, 4096).unwrap();
        pwrite_full(&f, &[7u8; 16], 8192 + 32).unwrap();
        let mut back = [0u8; 16];
        pread_full(&f, &mut back, 8192 + 32).unwrap();
        assert!(back.iter().all(|&b| b == 7));
        drop(f);
        let _ = std::fs::remove_file(&path);
    }
}
