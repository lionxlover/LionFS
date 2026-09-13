//! Durability primitives: the fsync/fdatasync family.
//!
//! Durability semantics differ more between platforms than any other PAL
//! surface, and getting them silently wrong is how file systems corrupt.
//! The mapping is:
//!
//! | Call | Linux | macOS | Windows |
//! |------|-------|-------|---------|
//! | `sync_data(file)` | `fdatasync(2)` (flush data + the metadata needed to retrieve it) | `fcntl(F_FULLFSYNC)` -- the only true "bytes on media" barrier on APFS/HFS+; plain `fsync` is merely a page-cache flush there | `FlushFileBuffers` |
//! | `sync_file(file)` | `fsync(2)` (data + all metadata) | `fsync(2)` (documented weaker than F_FULLFSYNC; we still offer it for parity) | `FlushFileBuffers` |
//!
//! The I/O engine's commit path (LFS-RFC-002 §5.1 step 4) uses
//! `sync_data` + the ring's FUA: that is the "flush device cache" step of
//! the commit ordering. `sync_file` exists for superblock rewrites, where
//! file *size* changes (metadata) must also be durable.

use std::fs::File;
use std::io::Result;

/// Flush file *data* (and retrieval metadata) to stable storage.
///
/// * Linux: `fdatasync` -- skips flushing mtime-only metadata updates,
///   which is exactly what a journal append wants.
/// * macOS: `F_FULLFSYNC`. This is deliberate and load-bearing: on APFS,
///   `fsync` does NOT guarantee persistence to the disk (SSD write caches
///   may still hold the data). The RFC's crash-consistency model (intent
///   journal precedes data; commit record closes it) only holds if the
///   "fdatasync" barrier is a real barrier, so macOS pays the cost.
///   Falls back to `fsync` if the volume does not support F_FULLFSYNC
///   (network filesystems and some eFUSE volumes return EOPNOTSUPP).
/// * Windows: `FlushFileBuffers` (equivalent of fsync; there is no
///   cheaper data-only variant).
#[inline]
pub fn sync_data(file: &File) -> Result<()> {
    #[cfg(target_os = "linux")]
    {
        // SAFETY: fd is a valid open descriptor owned by `file` for the
        // duration of the call. fdatasync has no memory-safety concerns.
        let ret = unsafe { libc::fdatasync(file.as_raw_fd_()) };
        if ret != 0 {
            return Err(std::io::Error::last_os_error());
        }
        Ok(())
    }
    #[cfg(target_os = "macos")]
    {
        // F_FULLFSYNC = 51 (non-public constant, stable since 10.4).
        // SAFETY: valid fd; fcntl with F_FULLFSYNC takes no pointer arg.
        const F_FULLFSYNC: i32 = 51;
        let ret = unsafe { libc::fcntl(file.as_raw_fd_(), F_FULLFSYNC) };
        if ret != 0 {
            let err = std::io::Error::last_os_error();
            // EOPNOTSUPP/ENOTSUP on volumes without full-fsync support:
            // plain fsync is the best available barrier there, which is
            // the documented macOS behavior, not a silent degradation.
            #[allow(clippy::match_like_matches_macro)]
            match err.raw_os_error() {
                Some(libc::ENOTSUP) | Some(libc::EOPNOTSUPP) => file.sync_all(),
                _ => Err(err),
            }
        } else {
            Ok(())
        }
    }
    #[cfg(all(unix, not(any(target_os = "linux", target_os = "macos"))))]
    {
        file.sync_all()
    }
    #[cfg(windows)]
    {
        flush_file_buffers(file)
    }
    #[cfg(not(any(unix, windows)))]
    {
        file.sync_all()
    }
}

/// Flush data + all metadata (incl. size) to stable storage -- superblock
/// and file-size changing paths.
#[inline]
pub fn sync_file(file: &File) -> Result<()> {
    #[cfg(unix)]
    {
        file.sync_all()
    }
    #[cfg(windows)]
    {
        flush_file_buffers(file)
    }
    #[cfg(not(any(unix, windows)))]
    {
        file.sync_all()
    }
}

// -- platform plumbing -------------------------------------------------------

#[cfg(unix)]
use std::os::unix::io::AsRawFd;

/// Small internal trait so the cfg(unix) import stays out of the public
/// docs; `file.as_raw_fd_()` reads clearly next to the FFI call it feeds.
#[cfg(unix)]
trait RawFd {
    fn as_raw_fd_(&self) -> i32;
}

#[cfg(unix)]
impl RawFd for File {
    fn as_raw_fd_(&self) -> i32 {
        self.as_raw_fd()
    }
}

#[cfg(windows)]
mod winffi {
    extern "system" {
        pub fn FlushFileBuffers(handle: *mut core::ffi::c_void) -> i32;
    }
}

#[cfg(windows)]
#[inline]
fn flush_file_buffers(file: &File) -> Result<()> {
    use std::os::windows::io::AsRawHandle;
    // RawHandle is `*mut c_void` already; FlushFileBuffers takes that
    // directly. SAFETY: handle is owned by `file` for the call;
    // FlushFileBuffers has no memory-safety concerns.
    let ret = unsafe { winffi::FlushFileBuffers(file.as_raw_handle()) };
    if ret == 0 {
        Err(std::io::Error::last_os_error())
    } else {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn data_sync_roundtrip_on_a_temp_image() {
        let dir = std::env::temp_dir().join(format!("lionfs_pal_sync_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("img.bin");
        let f = crate::pal::create_image(&path, 64 * 1024).unwrap();
        crate::pal::pwrite_full(&f, &[1u8; 512], 4096).unwrap();
        sync_data(&f).unwrap();
        sync_file(&f).unwrap();
        drop(f);
        // After both barriers, the bytes must be visible to a new handle.
        let g = crate::pal::open_device_rw(&path).unwrap();
        let mut back = [0u8; 512];
        crate::pal::pread_full(&g, &mut back, 4096).unwrap();
        assert!(back.iter().all(|&b| b == 1));
        drop(g);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
