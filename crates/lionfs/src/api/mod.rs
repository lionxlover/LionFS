//! C-compatible API export layer for LionFS.
//! Exposes stable endpoints for external utilities to mount, unmount, and query LionFS in userspace.

pub mod builder;
pub mod handle;
pub mod options;

use crate::api::builder::LionFsBuilder;
use crate::api::options::LfsOptions;
use std::ffi::{c_char, CStr};

use crate::pal::posix::{EINVAL, EIO};

#[repr(C)]
pub struct LfsApiStatus {
    pub success: bool,
    pub error_code: i32,
    /// Valid only when `success` is true: a handle for a later
    /// `lfs_unmount` call. Always 0 (an intentionally invalid sentinel)
    /// when `success` is false.
    pub mount_handle: u64,
}

fn failure(error_code: i32) -> LfsApiStatus {
    LfsApiStatus {
        success: false,
        error_code,
        mount_handle: 0,
    }
}

/// Converts a C string pointer into an owned `String`, safely: rejects
/// null pointers and non-UTF-8 content rather than assuming either can't
/// happen. Every FFI entry point below goes through this rather than
/// calling `CStr::from_ptr` directly at the call site, so there is exactly
/// one place that does the unsafe pointer dereference and its safety
/// preconditions (pointer is null-terminated, valid for reads, not
/// mutated concurrently -- the standard `CStr::from_ptr` contract, which
/// callers of a C API are expected to uphold same as for any other C
/// string-accepting function) are documented once.
///
/// # Safety
/// `ptr` must either be null or point to a valid, null-terminated C
/// string that remains valid and unmodified for the duration of this call.
unsafe fn c_str_to_string(ptr: *const c_char) -> Option<String> {
    if ptr.is_null() {
        return None;
    }
    CStr::from_ptr(ptr).to_str().ok().map(|s| s.to_string())
}

#[no_mangle]
pub extern "C" fn lfs_version() -> *const c_char {
    concat!(env!("CARGO_PKG_VERSION"), "\0").as_ptr().cast()
}

/// Mounts the filesystem at `device_path` onto `mount_point` in a
/// background thread and returns immediately; the mount stays active
/// until `lfs_unmount` is called with the returned handle (or the process
/// exits). Both arguments must be non-null, null-terminated, valid UTF-8
/// C strings.
///
/// # Safety
/// `device_path` and `mount_point` must each be null or a valid,
/// null-terminated C string valid for the duration of this call.
#[no_mangle]
#[cfg(unix)]
pub unsafe extern "C" fn lfs_mount_fuse(
    device_path: *const c_char,
    mount_point: *const c_char,
) -> LfsApiStatus {
    let device_path = match c_str_to_string(device_path) {
        Some(s) => s,
        None => return failure(EINVAL),
    };
    let mount_point = match c_str_to_string(mount_point) {
        Some(s) => s,
        None => return failure(EINVAL),
    };

    let fs = match LionFsBuilder::new(LfsOptions::new(device_path)).build() {
        Ok(fs) => fs,
        Err(e) => return failure(e.raw_os_error().unwrap_or(EIO)),
    };

    let options = crate::mount::options::build_mount_options(&crate::common::config::MountConfig {
        read_only: false,
        ..Default::default()
    });

    match fuser::spawn_mount2(crate::mount::mount::fuse_bridge(fs), &mount_point, &options) {
        Ok(session) => {
            let handle = crate::api::handle::register(session);
            LfsApiStatus {
                success: true,
                error_code: 0,
                mount_handle: handle,
            }
        }
        Err(e) => failure(e.raw_os_error().unwrap_or(EIO)),
    }
}

/// Unmounts a filesystem previously mounted via `lfs_mount_fuse`. Returns
/// a status with `success == true` iff `mount_handle` referred to an
/// active mount.
#[no_mangle]
pub extern "C" fn lfs_unmount(mount_handle: u64) -> LfsApiStatus {
    if crate::api::handle::unmount(mount_handle) {
        LfsApiStatus {
            success: true,
            error_code: 0,
            mount_handle,
        }
    } else {
        failure(EINVAL)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_ffi_version() {
        let ptr = lfs_version();
        assert!(!ptr.is_null());
    }

    #[test]
    fn mount_with_null_paths_fails_cleanly_not_a_crash() {
        let status = unsafe { lfs_mount_fuse(std::ptr::null(), std::ptr::null()) };
        assert!(!status.success);
        assert_eq!(status.mount_handle, 0);
    }

    #[test]
    fn unmount_of_unknown_handle_fails_cleanly() {
        let status = lfs_unmount(999_999);
        assert!(!status.success);
    }
}
