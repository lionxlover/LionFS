//! A fluent builder for opening a `Disk` and constructing `LionFS` from
//! `api::options::LfsOptions`, so callers (the C API, a future embedding
//! API) don't need to know about `pool::raid::RaidProfile`/`Disk::open_pool`
//! plumbing directly.

use crate::api::options::LfsOptions;
use crate::disk::block_io::Disk;
use crate::fs::filesystem::LionFS;
use crate::ondisk::serialization::{Superblock, BLOCK_SIZE};
use crate::pool::raid::RaidProfile;
use std::io::Result;

pub struct LionFsBuilder {
    options: LfsOptions,
}

impl LionFsBuilder {
    pub fn new(options: LfsOptions) -> Self {
        Self { options }
    }

    /// Opens the device(s) described by `options` -- bootstrapping the
    /// RAID profile from the superblock the same way
    /// `userspace::cli::mount` does, if there's more than one device path
    /// involved -- and constructs `LionFS` on top.
    pub fn build(self) -> Result<LionFS> {
        let disk = if self.options.extra_devices.is_empty() {
            Disk::open(&self.options.device_path)?
        } else {
            let bootstrap = Disk::open(&self.options.device_path)?;
            let mut sb_buf = [0u8; BLOCK_SIZE];
            bootstrap.read_block(0, &mut sb_buf)?;
            let sb: Superblock = *bytemuck::from_bytes(&sb_buf);
            drop(bootstrap);

            let profile = RaidProfile::from_u8(sb.raid_profile);
            let mut all_devices = vec![self.options.device_path.clone()];
            all_devices.extend(self.options.extra_devices.clone());
            Disk::open_pool(&all_devices, profile, sb.chunk_size)?
        };

        LionFS::new(disk, self.options.device_path.clone())
    }
}
