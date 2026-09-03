//! Ties bootstrap (RAID profile detection), `api::builder::LionFsBuilder`,
//! and `mount::options::build_mount_options` together into the one
//! function `userspace::cli::mount` and `api::mod::lfs_mount_fuse` both
//! conceptually perform -- provided as a reusable entry point so a future
//! third caller (e.g. a `tools::mount` utility separate from the primary
//! `userspace::cli::mount` binary) doesn't have to re-assemble the same
//! sequence a third time.

use crate::api::builder::LionFsBuilder;
use crate::api::options::LfsOptions;
use crate::common::config::MountConfig;
use crate::fs::filesystem::LionFS;
use crate::mount::options::build_mount_options;
use std::io::Result;

pub struct PreparedMount {
    pub fs: LionFS,
    /// FUSE mount options (unix only; the Windows/WinFsp path builds its
    /// own option set from the same MountConfig).
    #[cfg(unix)]
    pub fuse_options: Vec<fuser::MountOption>,
}

pub fn prepare(options: LfsOptions, config: &MountConfig) -> Result<PreparedMount> {
    let fs = LionFsBuilder::new(options).build()?;
    #[cfg(unix)]
    let fuse_options = build_mount_options(config);
    #[cfg(not(unix))]
    let _ = config;
    Ok(PreparedMount {
        fs,
        #[cfg(unix)]
        fuse_options,
    })
}

/// Wraps a mounted `LionFS` in the platform-neutral FUSE bridge. The
/// 2.0 mount path never exposes the raw engine to fuser: every platform
/// goes through `VfsOps`.
#[cfg(unix)]
pub fn fuse_bridge(fs: LionFS) -> crate::vfs::fuse_bridge::FuseBridge<LionFS> {
    crate::vfs::fuse_bridge::FuseBridge::new(fs)
}

/// Prepares and then blocks, serving the filesystem until unmounted --
/// what a simple CLI tool wants. For a non-blocking mount (the C API
/// wants this so it can return to its caller), use `prepare` and
/// `fuser::spawn_mount2` directly instead, as `api::mod::lfs_mount_fuse`
/// does.
/// Serves the filesystem until unmount. Unix: through the FUSE bridge;
/// other platforms: currently returns Unsupported (the WinFsp bridge is
/// the RFC-003 deliverable for Windows, tracked in
/// docs/platform_support.md).
#[cfg(unix)]
pub fn mount_and_serve(options: LfsOptions, config: &MountConfig, mount_point: &str) -> Result<()> {
    let prepared = prepare(options, config)?;
    let bridge = fuse_bridge(prepared.fs);
    fuser::mount2(bridge, mount_point, &prepared.fuse_options)
}

#[cfg(not(unix))]
pub fn mount_and_serve(
    _options: LfsOptions,
    _config: &MountConfig,
    _mount_point: &str,
) -> Result<()> {
    Err(std::io::Error::new(
        std::io::ErrorKind::Unsupported,
        "mount_and_serve requires a platform bridge (unix/FUSE today; WinFsp per RFC-003)",
    ))
}
