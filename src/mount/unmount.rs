//! Unmounting from a separate process/invocation than the one that
//! mounted -- `api::handle`'s registry only works within the process that
//! called `lfs_mount_fuse` (it's just an in-memory map), so a standalone
//! `unmount-lfs <mountpoint>`-style CLI tool needs a different mechanism:
//! asking the OS to unmount the path, the same way the standard `umount`
//! command does for any FUSE filesystem.

use std::io::{Error, ErrorKind, Result};
use std::process::Command;

/// Unmounts `mount_point` by invoking `fusermount -u` (falling back to
/// `fusermount3 -u`, present on some newer distributions) -- the standard,
/// unprivileged way to unmount a user-space FUSE filesystem on Linux,
/// rather than calling the raw `umount2` syscall directly (which
/// typically requires root for a non-lazy unmount).
pub fn unmount(mount_point: &str) -> Result<()> {
    for cmd in ["fusermount", "fusermount3"] {
        match Command::new(cmd).arg("-u").arg(mount_point).status() {
            Ok(status) if status.success() => return Ok(()),
            Ok(status) => {
                return Err(Error::new(
                    ErrorKind::Other,
                    format!("{cmd} -u {mount_point} exited with {status}"),
                ))
            }
            Err(e) if e.kind() == ErrorKind::NotFound => continue, // try the next candidate
            Err(e) => return Err(e),
        }
    }
    Err(Error::new(
        ErrorKind::NotFound,
        "neither fusermount nor fusermount3 was found on PATH",
    ))
}

/// Force-unmounts, detaching the mount point immediately even if it's
/// still busy (open file handles, a shell `cd`'d into it) -- for cleanup
/// scenarios where waiting for every user to finish isn't acceptable.
/// Corresponds to `umount -l` / `fusermount -uz`.
pub fn force_unmount(mount_point: &str) -> Result<()> {
    for cmd in ["fusermount", "fusermount3"] {
        match Command::new(cmd).arg("-uz").arg(mount_point).status() {
            Ok(status) if status.success() => return Ok(()),
            Ok(status) => {
                return Err(Error::new(
                    ErrorKind::Other,
                    format!("{cmd} -uz {mount_point} exited with {status}"),
                ))
            }
            Err(e) if e.kind() == ErrorKind::NotFound => continue,
            Err(e) => return Err(e),
        }
    }
    Err(Error::new(
        ErrorKind::NotFound,
        "neither fusermount nor fusermount3 was found on PATH",
    ))
}
