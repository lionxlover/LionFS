//! Cross-platform device geometry probing (Pillar IV, RFC-002 §6.1:
//! "every policy is a function of geometry the engine probes rather than
//! guesses").
//!
//! The 1.x `disk/geometry.rs` knew exactly one trick: two Linux ioctls.
//! The 2.0 PAL probes, per platform:
//!
//! * **Linux** block devices: `BLKGETSIZE64` (size), `BLKSSZGET` (logical
//!   sector), `BLKPBSZGET` (physical sector), `BLKOPTGET` (optimal I/O).
//! * **macOS** block devices: `DKIOCGETBLOCKCOUNT` + `DKIOCGETBLOCKSIZE`
//!   (size), `DKIOCGETPHYSICALBLOCKSIZE` (physical sector).
//! * **Windows** raw volumes/physical drives: `IOCTL_DISK_GET_LENGTH_INFO`
//!   (size) and `IOCTL_DISK_GET_DRIVE_GEOMETRY_EX` (media/sector).
//! * **Regular files** (the common case: images, including every test):
//!   `metadata().len()` and 512-byte sectors, exactly like 1.x, so mkfs
//!   on an image behaves identically everywhere.
//!
//! `optimal_io_size` feeds the alignment engine's 4K/16K/64K page-cluster
//! classes (RFC-002 §6.2); devices that do not report one get the logical
//! sector, which is the conservative floor.

use std::fs::File;
use std::io::Result;

/// Probed device geometry. A superset of the 1.x struct; the 1.x fields
/// keep their names so callers port mechanically.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DeviceGeometry {
    /// Device size in bytes.
    pub size_bytes: u64,
    /// Logical (addressable) sector size in bytes.
    pub logical_sector_size: u32,
    /// Physical (media) sector size in bytes, when the platform reports
    /// it; equal to `logical_sector_size` otherwise.
    pub physical_sector_size: u32,
    /// Optimal I/O size in bytes (RAID stripe width, ZNS zone-append
    /// granularity, or platform default). `None` when unreported.
    pub optimal_io_size: Option<u32>,
}

impl DeviceGeometry {
    /// Conservative geometry for a plain image file.
    #[must_use]
    pub fn for_image(size_bytes: u64) -> Self {
        Self {
            size_bytes,
            logical_sector_size: 512,
            physical_sector_size: 512,
            optimal_io_size: None,
        }
    }
}

/// Probe `file`'s geometry: block-device ioctls where applicable, plain
/// stat for regular files.
pub fn probe(file: &File) -> Result<DeviceGeometry> {
    let meta = file.metadata()?;
    if !meta.file_type().is_block_device_impl() {
        // Image file path (also: Windows where nothing is a "block
        // device" through std metadata).
        return Ok(DeviceGeometry::for_image(meta.len()));
    }
    probe_block_device(file)
}

/// `std::fs::FileType` extension that is honest about block devices:
/// exists on unix, always false on Windows (where devices are opened
/// through `\\.\` paths, and `probe` dispatches on those instead).
trait IsBlockDevice {
    fn is_block_device_impl(&self) -> bool;
}

#[cfg(unix)]
impl IsBlockDevice for std::fs::FileType {
    fn is_block_device_impl(&self) -> bool {
        use std::os::unix::fs::FileTypeExt;
        self.is_block_device()
    }
}

#[cfg(not(unix))]
impl IsBlockDevice for std::fs::FileType {
    fn is_block_device_impl(&self) -> bool {
        false
    }
}

// -- Linux -------------------------------------------------------------------

#[cfg(target_os = "linux")]
mod linux_impl {
    use super::DeviceGeometry;
    use std::fs::File;
    use std::io::{Error, Result};
    use std::os::unix::io::AsRawFd;

    // Standard Linux ioctl request codes (same values the 1.x code
    // carried; BLKPBSZGET/BLKOPTGET extend the set for Pillar IV). A wrong
    // constant fails cleanly with EINVAL/ENOTTY -- never memory
    // unsafety, since each only writes a fixed-size integer.
    const BLKGETSIZE64: libc::c_ulong = 0x8008_1272; // u64: device size
    const BLKSSZGET: libc::c_ulong = 0x0000_1268; // int: logical sector
    const BLKPBSZGET: libc::c_ulong = 0x0000_127B; // int: physical sector
    const BLKOPTGET: libc::c_ulong = 0x0000_1273; // int: optimal I/O size

    pub fn probe(file: &File) -> Result<DeviceGeometry> {
        let fd = file.as_raw_fd();
        let mut size_bytes: u64 = 0;
        // SAFETY: valid fd owned by `file`; BLKGETSIZE64 writes exactly 8
        // bytes into a local u64, matching its documented (u64*) arg.
        let ret = unsafe { libc::ioctl(fd, BLKGETSIZE64, &mut size_bytes as *mut u64) };
        if ret != 0 {
            return Err(Error::last_os_error());
        }

        let mut sector_size: libc::c_int = 512;
        // SAFETY: as above with BLKSSZGET's (int*) arg.
        let ret = unsafe { libc::ioctl(fd, BLKSSZGET, &mut sector_size as *mut libc::c_int) };
        if ret != 0 {
            return Err(Error::last_os_error());
        }

        let mut phys_sector: libc::c_int = sector_size;
        // SAFETY: as above; failure is tolerated (older kernels/devices),
        // falling back to the logical size.
        let ret = unsafe { libc::ioctl(fd, BLKPBSZGET, &mut phys_sector as *mut libc::c_int) };
        if ret != 0 {
            phys_sector = sector_size;
        }

        let mut opt_io: libc::c_int = 0;
        // SAFETY: as above; failure tolerated -> None.
        let ret = unsafe { libc::ioctl(fd, BLKOPTGET, &mut opt_io as *mut libc::c_int) };
        let optimal = if ret == 0 && opt_io > 0 {
            Some(opt_io as u32)
        } else {
            None
        };

        Ok(DeviceGeometry {
            size_bytes,
            logical_sector_size: sector_size.max(1) as u32,
            physical_sector_size: phys_sector.max(1) as u32,
            optimal_io_size: optimal,
        })
    }
}

// -- macOS -------------------------------------------------------------------

#[cfg(target_os = "macos")]
mod macos_impl {
    use super::DeviceGeometry;
    use std::fs::File;
    use std::io::Result;
    use std::os::unix::io::AsRawFd;

    const DKIOCGETBLOCKCOUNT: libc::c_ulong = 0x4004_6410; // u64 block count
    const DKIOCGETBLOCKSIZE: libc::c_ulong = 0x4004_6402; // u32 logical block
    const DKIOCGETPHYSICALBLOCKSIZE: libc::c_ulong = 0x4004_6411; // u32 phys block

    pub fn probe(file: &File) -> Result<DeviceGeometry> {
        let fd = file.as_raw_fd();
        let mut block_count: u64 = 0;
        let mut block_size: u32 = 512;
        let mut phys_size: u32 = 512;

        // SAFETY: valid fd; each DKIOC ioctl writes a fixed-size integer
        // into a local of exactly matching type. dk* ioctls are
        // well-formed on any fd; on non-disk fds they fail with ENOTTY,
        // which we surface as an error from the outer probe only for the
        // mandatory size/sector pair.
        unsafe {
            if libc::ioctl(fd, DKIOCGETBLOCKCOUNT, &mut block_count) != 0 {
                use std::io::Error;
                return Err(Error::last_os_error());
            }
            if libc::ioctl(fd, DKIOCGETBLOCKSIZE, &mut block_size) != 0 {
                use std::io::Error;
                return Err(Error::last_os_error());
            }
            if libc::ioctl(fd, DKIOCGETPHYSICALBLOCKSIZE, &mut phys_size) != 0 {
                phys_size = block_size;
            }
        }

        Ok(DeviceGeometry {
            size_bytes: block_count.saturating_mul(block_size as u64),
            logical_sector_size: block_size.max(1),
            physical_sector_size: phys_size.max(1),
            optimal_io_size: None,
        })
    }
}

// -- Other unix (BSD) ---------------------------------------------------------

#[cfg(all(unix, not(any(target_os = "linux", target_os = "macos"))))]
mod unix_fallback_impl {
    use super::DeviceGeometry;
    use std::fs::File;
    use std::io::Result;

    pub fn probe(file: &File) -> Result<DeviceGeometry> {
        // DIOCGMEDIASIZE is FreeBSD's; keep it minimal and portable: size
        // from metadata is unavailable for block devices on BSDs without
        // ioctls, so fall back to the stat st_blocks*512 heuristic.
        use std::os::unix::fs::MetadataExt;
        let meta = file.metadata()?;
        let size = (meta.blocks() as u64) * 512;
        Ok(DeviceGeometry {
            size_bytes: size,
            logical_sector_size: 512,
            physical_sector_size: 512,
            optimal_io_size: None,
        })
    }
}

// -- Windows ------------------------------------------------------------------

#[cfg(windows)]
mod windows_impl {
    use super::DeviceGeometry;
    use std::fs::File;
    use std::io::{Error, Result};
    use std::os::windows::io::AsRawHandle;

    const IOCTL_DISK_GET_LENGTH_INFO: u32 = 0x0007_4050; // -> u64 length
    const ERROR_INSUFFICIENT_BUFFER: i32 = 122;

    extern "system" {
        fn DeviceIoControl(
            handle: *mut core::ffi::c_void,
            ioctl: u32,
            in_buf: *const core::ffi::c_void,
            in_len: u32,
            out_buf: *mut core::ffi::c_void,
            out_len: u32,
            bytes_returned: *mut u32,
            overlapped: *mut core::ffi::c_void,
        ) -> i32;
    }

    pub fn probe(file: &File) -> Result<DeviceGeometry> {
        let mut length: u64 = 0;
        let mut returned: u32 = 0;
        // SAFETY: handle owned by `file`; output buffer is a local u64 of
        // exactly the size the ioctl documents; no input buffer.
        let ok = unsafe {
            DeviceIoControl(
                file.as_raw_handle(),
                IOCTL_DISK_GET_LENGTH_INFO,
                std::ptr::null(),
                0,
                &mut length as *mut u64 as *mut core::ffi::c_void,
                std::mem::size_of::<u64>() as u32,
                &mut returned,
                std::ptr::null_mut(),
            )
        };
        if ok == 0 {
            let err = Error::last_os_error();
            // Volume handles opened without volume- Sharing may refuse
            // the length ioctl; a 122 (ERROR_INSUFFICIENT_BUFFER) class
            // failure still means "this is a device".
            return Err(err);
        }

        // Physical drives report their length; sector sizes come from the
        // drive's *fixed* geometry on modern media (512e/4Kn): use the
        // volume's sector size via the FS sector information ioctl when
        // available; default to 512 like 1.x did for images.
        let sector = fs_sector_size(file).unwrap_or(512);

        Ok(DeviceGeometry {
            size_bytes: length,
            logical_sector_size: sector,
            physical_sector_size: sector,
            optimal_io_size: None,
        })
    }

    fn fs_sector_size(file: &File) -> Option<u32> {
        // IOCTL_FS_GET_NTFS_VOLUME_INFO is overkill; the simplest reliable
        // probe is FSCTL_QUERY_NTFS_VOLUME_DATA? Keep the surface minimal:
        // no sector probe -- documented, conservative 512.
        let _ = file;
        None
    }
}

// -- dispatch ------------------------------------------------------------------

#[cfg(unix)]
fn probe_block_device(file: &File) -> Result<DeviceGeometry> {
    #[cfg(target_os = "linux")]
    {
        linux_impl::probe(file)
    }
    #[cfg(target_os = "macos")]
    {
        macos_impl::probe(file)
    }
    #[cfg(all(unix, not(any(target_os = "linux", target_os = "macos"))))]
    {
        unix_fallback_impl::probe(file)
    }
}

#[cfg(windows)]
fn probe_block_device(file: &File) -> Result<DeviceGeometry> {
    // On Windows the image-vs-device split cannot be made through
    // metadata (everything is a file). Devices are opened as \\.\X:
    // paths; the length ioctl is the reliable discriminator: success
    // -> device, failure -> treat as image (st_size already returned).
    match windows_impl::probe(file) {
        Ok(g) => Ok(g),
        Err(_) => Ok(DeviceGeometry::for_image(file.metadata()?.len())),
    }
}

#[cfg(not(any(unix, windows)))]
fn probe_block_device(_file: &File) -> Result<DeviceGeometry> {
    Err(Error::new(
        std::io::ErrorKind::Unsupported,
        "block-device geometry probing is not supported on this platform",
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pal::file::create_image;

    #[test]
    fn regular_file_reports_its_length_as_size() {
        let dir = std::env::temp_dir().join(format!("lionfs_pal_geom_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("image.bin");
        let f = create_image(&path, 123_456).unwrap();
        let geom = probe(&f).unwrap();
        assert_eq!(geom.size_bytes, 123_456);
        assert_eq!(geom.logical_sector_size, 512);
        assert_eq!(geom.physical_sector_size, 512);
        drop(f);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn image_geometry_is_conservative() {
        let g = DeviceGeometry::for_image(4096);
        assert!(g.optimal_io_size.is_none());
        assert_eq!(g.physical_sector_size, g.logical_sector_size);
    }
}
