//! A device handle that remembers its own path, index, and open file --
//! `disk::block_io::Disk` currently stores just `Vec<Arc<File>>` internally
//! and discards the original paths after opening, which is fine for
//! normal I/O but loses information a future "which device failed" error
//! message or device-replacement operation would want. This is a
//! standalone building block for that, not a change to `Disk` itself
//! (swapping `Disk`'s internal representation is exactly the kind of
//! change best made on its own, separately from the RAID/encryption work
//! already done to it in this pass).
//!
//! Distinct from `pool::device::DeviceRecord`, which is on-disk metadata
//! (persisted device state for pool health tracking); this is a purely
//! in-memory, session-local handle.

use std::fs::File;
use std::io::Result;
use std::path::{Path, PathBuf};
use std::sync::Arc;

#[derive(Clone)]
pub struct DeviceHandle {
    pub index: usize,
    pub path: PathBuf,
    pub file: Arc<File>,
}

impl DeviceHandle {
    pub fn open(index: usize, path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref().to_path_buf();
        let file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(&path)?;
        Ok(Self {
            index,
            path,
            file: Arc::new(file),
        })
    }
}

/// Opens every path in order, tagging each with its position -- the same
/// order `pool::raid::RaidEngine` expects device indices to correspond to.
pub fn open_all(paths: &[impl AsRef<Path>]) -> Result<Vec<DeviceHandle>> {
    paths
        .iter()
        .enumerate()
        .map(|(i, p)| DeviceHandle::open(i, p))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn opens_and_tags_devices_in_order() {
        let dir =
            std::env::temp_dir().join(format!("lionfs_devhandle_test_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let paths: Vec<_> = (0..3)
            .map(|i| {
                let p = dir.join(format!("dev{i}.img"));
                std::fs::File::create(&p).unwrap().set_len(1024).unwrap();
                p
            })
            .collect();

        let handles = open_all(&paths).unwrap();
        assert_eq!(handles.len(), 3);
        for (i, h) in handles.iter().enumerate() {
            assert_eq!(h.index, i);
            assert_eq!(h.path, paths[i]);
        }

        let _ = std::fs::remove_dir_all(&dir);
    }
}
