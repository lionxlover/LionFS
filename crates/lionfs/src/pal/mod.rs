//! # Platform Abstraction Layer (PAL)
//!
//! LionFS 2.0 runs on Linux, macOS, and Windows from one code base. This
//! module is the single place where platform differences are visible; every
//! other module in the crate talks to the operating system through it (or
//! through `std`, which is already portable). See `docs/platform_support.md`
//! and `docs/rfc/LFS-RFC-003-cross-platform.md` for the design contract.
//!
//! What lives here:
//!
//! | File | Abstracts | Backends |
//! |------|-----------|----------|
//! | [`platform`] | OS identity, page size, CPU count, capability probe | compile-time target + runtime detection |
//! | [`file`] | positioned reads/writes on a raw device handle | `FileExt::read_at`/`write_at` (unix), `seek_read`/`seek_write` (Windows) |
//! | [`sync`] | durability flavors | `fdatasync` (Linux), `F_FULLFSYNC` (macOS), `FlushFileBuffers` (Windows) |
//! | [`posix`] | errno codes and `S_IF*` mode bits used by the VFS layer | fixed ABI values (Linux ABI = FUSE wire ABI) |
//! | [`geometry`] | device geometry probing | Linux `BLKGETSIZE64`/`BLKSSZGET`, macOS `DKIOC*`, Windows `IOCTL_DISK_*`, stat-fallback for image files |
//! | [`random`] | OS CSPRNG | `/dev/urandom` (unix), `ProcessPrng` (Windows 10+, with `RtlGenRandom` fallback) |
//! | [`waker`] | cross-thread wakeup for the I/O engine | `eventfd` (Linux), pipe (other unix), `SleepConditionVariableSRW` style (Windows) |
//!
//! Design rules, in priority order:
//!
//! 1. **The Windows build pulls in zero external crates.** Everything
//!    Windows-specific is raw `extern "system"` FFI against `kernel32` /
//!    `bcryptprimitives`, so `cargo build` on a stock Windows host with only
//!    the Rust toolchain works.
//! 2. **No silent emulation.** Where a platform cannot provide a primitive
//!    (e.g. `fdatasync` on macOS), the PAL provides the closest *documented*
//!    equivalent and says so in the function docs, or returns an error.
//! 3. **Fast paths stay fast.** PAL wrappers are `#[inline]` thin shims over
//!    the platform call, not trait objects; there is exactly one dynamic
//!    dispatch anywhere in the I/O path (the engine backend, chosen once at
//!    mount).
//! 4. **Everything is testable.** Each backend has unit tests that exercise
//!    it on its own platform; cross-platform behavior is covered by
//!    `tests/pal_tests.rs` which runs on all three CI operating systems.

pub mod file;
pub mod geometry;
pub mod platform;
pub mod posix;
pub mod random;
pub mod sync;
pub mod waker;

pub use file::{create_image, open_device_rw, pread_full, pwrite_at, pwrite_full};
pub use geometry::DeviceGeometry;
pub use platform::{current_platform, os_version_string, page_size, Platform};
pub use random::fill_random;
pub use sync::{sync_data, sync_file};

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn platform_is_identified() {
        // Compile-time target must match the runtime probe on all CI OSes.
        let p = current_platform();
        #[cfg(target_os = "linux")]
        assert_eq!(p, Platform::Linux);
        #[cfg(target_os = "macos")]
        assert_eq!(p, Platform::MacOs);
        #[cfg(target_os = "windows")]
        assert_eq!(p, Platform::Windows);
    }

    #[test]
    fn page_size_is_sane() {
        let ps = page_size();
        assert!(ps.is_power_of_two());
        assert!(ps >= 4096);
    }

    #[test]
    fn version_string_is_nonempty() {
        assert!(!os_version_string().is_empty());
    }
}
