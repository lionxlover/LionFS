use std::fs::{File, OpenOptions};
use std::io::{Error, ErrorKind, Result};
use std::path::Path;

use crate::ondisk::serialization::BLOCK_SIZE;

use crate::pool::raid::{RaidEngine, RaidProfile};

use rayon::prelude::*;
use std::sync::Arc;

// 2.0: positioned I/O goes through the platform abstraction layer
// (`pal::file`), which maps to pread/pwrite on unix and seek_read/
// seek_write on Windows. The 1.x code called FileExt directly, which
// welded the block layer to Linux/macOS.
use crate::pal::file::{pread_full, pwrite_full};

pub struct Disk {
    files: Vec<Arc<File>>,
    pub raid_engine: RaidEngine,
    /// Probed geometry per device (Phase 2). For plain image files this
    /// is (file length, 512) -- geometry probing only means something
    /// against real block devices, which is exactly why it is probed
    /// rather than assumed.
    pub geometries: Vec<crate::disk::geometry::DeviceGeometry>,
}

/// Probe a device's geometry and warn if its logical sector size does
/// not evenly divide LionFS's block size (Phase 2: device geometry is
/// now CHECKED at open time, not just implemented). A device whose
/// sectors don't divide BLOCK_SIZE forces the kernel into read-modify-
/// write for every LionFS block -- correct but slow; the warning says
/// so instead of silently eating the cost.
fn probe_and_validate(
    file: &File,
    index: usize,
    path_label: &str,
) -> std::io::Result<crate::disk::geometry::DeviceGeometry> {
    let geo = crate::disk::geometry::probe(file)?;
    match crate::disk::sectors::sectors_per_block(geo.logical_sector_size) {
        Some(_) => {}
        None => {
            eprintln!(
                "WARNING: device {} (path {}) reports a logical sector size of {} bytes, \
which does not evenly divide LionFS's {}-byte block size. Every block \
I/O will be a read-modify-write at the sector layer. Refusing is \
available by checking the return of Disk::geometry() at mount time.",
                index, path_label, geo.logical_sector_size, BLOCK_SIZE
            );
        }
    }
    Ok(geo)
}

/// `FileExt::read_at` returns the number of bytes actually read, which can
/// be *less* than `buf.len()` (e.g. reading near/past EOF on a truncated or
/// shrunk device) without that being an `Err` -- it's easy to accidentally
/// treat a short read as "success" and silently work with a
/// mostly-uninitialized buffer. Every read in this file goes through this
/// helper instead of calling `read_at` directly so that a short read is
/// always a real, explicit error (which is also what makes degraded-mode
/// reconstruction actually reachable, rather than reads from a
/// missing/truncated device spuriously "succeeding" with garbage).
fn read_full(file: &File, buf: &mut [u8], offset: u64) -> Result<()> {
    pread_full(file, buf, offset)
}

impl Disk {
    /// Opens a single-device (RAID `Single`) filesystem. For a multi-device
    /// pool, use `open_pool`.
    pub fn open<P: AsRef<Path>>(path: P) -> Result<Self> {
        let path_label = path.as_ref().to_string_lossy().to_string();
        let file = OpenOptions::new().read(true).write(true).open(path)?;
        // Phase 2: geometry is probed (and validated) on every open.
        let geo = probe_and_validate(&file, 0, &path_label)?;
        Ok(Self {
            files: vec![Arc::new(file)],
            raid_engine: RaidEngine::new(RaidProfile::Single, 0, 1),
            geometries: vec![geo],
        })
    }

    /// Opens every device in `paths` (in order -- device order matters for
    /// RAID address mapping and must match how the pool was created) under
    /// the given RAID profile and chunk size.
    pub fn open_pool<P: AsRef<Path>>(
        paths: &[P],
        profile: RaidProfile,
        chunk_size_blocks: u32,
    ) -> Result<Self> {
        if paths.len() < profile.min_devices() {
            return Err(Error::new(
                ErrorKind::InvalidInput,
                format!(
                    "RAID profile {:?} needs at least {} devices, got {}",
                    profile,
                    profile.min_devices(),
                    paths.len()
                ),
            ));
        }
        let mut files = Vec::with_capacity(paths.len());
        let mut geometries = Vec::with_capacity(paths.len());
        for (i, p) in paths.iter().enumerate() {
            let file = OpenOptions::new().read(true).write(true).open(p)?;
            let geo = probe_and_validate(&file, i, &p.as_ref().to_string_lossy())?;
            files.push(Arc::new(file));
            geometries.push(geo);
        }
        let num_devices = files.len();
        Ok(Self {
            files,
            raid_engine: RaidEngine::new(profile, chunk_size_blocks, num_devices),
            geometries,
        })
    }

    pub fn create<P: AsRef<Path>>(path: P, size_bytes: u64) -> Result<Self> {
        let path_label = path.as_ref().to_string_lossy().to_string();
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(true)
            .open(path)?;
        file.set_len(size_bytes)?;
        let geo = probe_and_validate(&file, 0, &path_label)?;
        Ok(Self {
            files: vec![Arc::new(file)],
            raid_engine: RaidEngine::new(RaidProfile::Single, 0, 1),
            geometries: vec![geo],
        })
    }

    /// Creates and sizes every device in `paths`, each `size_per_device_bytes`
    /// long, and opens the pool under the given RAID profile.
    pub fn create_pool<P: AsRef<Path>>(
        paths: &[P],
        size_per_device_bytes: u64,
        profile: RaidProfile,
        chunk_size_blocks: u32,
    ) -> Result<Self> {
        if paths.len() < profile.min_devices() {
            return Err(Error::new(
                ErrorKind::InvalidInput,
                format!(
                    "RAID profile {:?} needs at least {} devices, got {}",
                    profile,
                    profile.min_devices(),
                    paths.len()
                ),
            ));
        }
        let mut files = Vec::with_capacity(paths.len());
        let mut geometries = Vec::with_capacity(paths.len());
        for (i, p) in paths.iter().enumerate() {
            let file = OpenOptions::new()
                .read(true)
                .write(true)
                .create(true)
                .truncate(true)
                .open(p)?;
            file.set_len(size_per_device_bytes)?;
            let geo = probe_and_validate(&file, i, &p.as_ref().to_string_lossy())?;
            files.push(Arc::new(file));
            geometries.push(geo);
        }
        let num_devices = files.len();
        Ok(Self {
            files,
            raid_engine: RaidEngine::new(profile, chunk_size_blocks, num_devices),
            geometries,
        })
    }

    pub fn device_count(&self) -> usize {
        self.files.len()
    }

    /// Probed geometry of device `index` (Phase 2). Image files report
    /// (length, 512); real block devices report their ioctl-queried
    /// geometry.
    pub fn geometry(&self, index: usize) -> Option<&crate::disk::geometry::DeviceGeometry> {
        self.geometries.get(index)
    }

    /// Raw device-level read bypassing RAID mapping (diagnostics and
    /// tests that need to inspect what a specific device actually
    /// holds, e.g. verifying parity blocks directly).
    pub fn read_block_direct(
        &self,
        device: usize,
        physical_block: u64,
        buf: &mut [u8],
    ) -> Result<()> {
        read_full(&self.files[device], buf, physical_block * BLOCK_SIZE as u64)
    }

    pub fn read_block(&self, block_num: u64, buf: &mut [u8]) -> Result<()> {
        if block_num == 0 {
            // The superblock bypasses RAID mapping entirely (see write_block)
            // so it can always be found at a fixed, predictable location
            // without first knowing the pool's RAID profile -- which is a
            // field inside the superblock itself.
            return read_full(&self.files[0], buf, 0);
        }
        let maps = self.raid_engine.map_read(block_num);
        let (dev_idx, physical_block) = maps[0];

        read_full(
            &self.files[dev_idx],
            buf,
            physical_block * BLOCK_SIZE as u64,
        )
    }

    /// Reads a block the normal way; on I/O error, and only for profiles
    /// that can tolerate a device loss (RAID1/5/6/10), attempts to
    /// reconstruct it from redundancy instead of propagating the error.
    /// This is what makes RAID's redundancy actually load-bearing for
    /// reads, not just something computed and stored on writes.
    pub fn read_block_resilient(&self, block_num: u64, buf: &mut [u8]) -> Result<()> {
        match self.read_block(block_num, buf) {
            Ok(()) => Ok(()),
            Err(e) => self.reconstruct_block(block_num, buf).map_err(|_| e),
        }
    }

    fn reconstruct_block(&self, block_num: u64, buf: &mut [u8]) -> Result<()> {
        let layout = self.raid_engine.layout(block_num);
        match self.raid_engine.profile {
            RaidProfile::Raid1 | RaidProfile::Raid10 => {
                // Any surviving mirror will do.
                for &dev in &layout.data_devs {
                    let mut b = vec![0u8; buf.len()];
                    if read_full(
                        &self.files[dev],
                        &mut b,
                        layout.phys_block * BLOCK_SIZE as u64,
                    )
                    .is_ok()
                    {
                        buf.copy_from_slice(&b);
                        return Ok(());
                    }
                }
                Err(Error::new(
                    ErrorKind::Other,
                    "no surviving mirror could be read",
                ))
            }
            RaidProfile::Raid5 | RaidProfile::Raid6 => {
                // Single-failure case: reconstruct via P alone (works
                // identically for RAID5 and RAID6, since P is a plain XOR
                // either way). Recovering when a *second* device has also
                // failed needs `rebuild_double_from_parity` with both
                // missing columns identified -- that requires knowing
                // which device failed, not just that one read failed, so
                // it's handled by the scrubber/pool health monitor rather
                // than here.
                let mut surviving = Vec::new();
                for &(dev, _) in &layout.other_data {
                    let mut b = vec![0u8; buf.len()];
                    read_full(
                        &self.files[dev],
                        &mut b,
                        layout.phys_block * BLOCK_SIZE as u64,
                    )?;
                    surviving.push(b);
                }
                let mut p = vec![0u8; buf.len()];
                read_full(
                    &self.files[layout.parity_devs[0]],
                    &mut p,
                    layout.phys_block * BLOCK_SIZE as u64,
                )?;
                let refs: Vec<&[u8]> = surviving.iter().map(|v| v.as_slice()).collect();
                let rebuilt = crate::pool::raid::rebuild_single_from_parity(&p, &refs, buf.len());
                buf.copy_from_slice(&rebuilt);
                Ok(())
            }
            _ => Err(Error::new(
                ErrorKind::Other,
                "this RAID profile has no redundancy to reconstruct from",
            )),
        }
    }

    pub fn write_block(&self, block_num: u64, buf: &[u8]) -> Result<()> {
        if block_num == 0 {
            // Written identically to every device (not just device 0) so
            // that any single surviving device is enough to identify the
            // pool and its RAID profile.
            for f in &self.files {
                pwrite_full(f, buf, 0)?;
            }
            return Ok(());
        }
        if self.raid_engine.is_parity_profile() {
            return self.write_block_parity(block_num, buf, false);
        }
        let maps = self.raid_engine.map_write(block_num);
        for (dev_idx, physical_block) in maps {
            pwrite_full(
                &self.files[dev_idx],
                buf,
                physical_block * BLOCK_SIZE as u64,
            )?;
        }
        Ok(())
    }

    /// Write path for JOURNAL REPLAY (Phase 3): identical to
    /// `write_block` except parity profiles always take the
    /// full-row-recompute branch. The incremental path is not idempotent
    /// under replay of a partially-applied transaction (old-data on disk
    /// may already equal the new data while parity is still the previous
    /// row's), and replay correctness depends on apply being idempotent.
    /// Recovery therefore always pays the full recompute.
    pub fn write_block_recovery(&self, block_num: u64, buf: &[u8]) -> Result<()> {
        if block_num == 0 {
            return self.write_block(block_num, buf);
        }
        if self.raid_engine.is_parity_profile() {
            return self.write_block_parity(block_num, buf, true);
        }
        self.write_block(block_num, buf)
    }

    /// Whether LFS_PARITY_FULL=1 is set (debug/diagnostic escape hatch
    /// that forces the full-row-recompute parity path, e.g. for
    /// benchmarking both paths with an identical harness or for
    /// isolating a suspected incremental-path issue in the field).
    /// Cached after first read.
    fn parity_full_forced() -> bool {
        use std::sync::OnceLock;
        static FORCED: OnceLock<bool> = OnceLock::new();
        *FORCED.get_or_init(|| {
            std::env::var("LFS_PARITY_FULL")
                .map(|v| v != "0")
                .unwrap_or(false)
        })
    }

    /// RAID5/6 write path (Phase 3): by default an INCREMENTAL
    /// read-modify-write -- read the one old data block and the old
    /// parity block(s), XOR the delta into parity -- instead of reading
    /// the entire rest of the stripe row. For a 4-device RAID5 that is
    /// 2 reads instead of 2 reads (no win); for 6+-device pools and
    /// RAID6 it is strictly fewer reads (6-dev RAID5: 2 instead of 4;
    /// 6-dev RAID6: 3 instead of 3... plus no re-serialization). See
    /// `pool::raid::update_raid5_parity_incremental` for the math and
    /// the equivalence tests that prove it matches a full recompute.
    ///
    /// `force_full` selects the original full-row-recompute path (used
    /// by journal replay and as a fallback when the old data/parity
    /// cannot be read, e.g. on a degraded pool).
    fn write_block_parity(&self, block_num: u64, buf: &[u8], force_full: bool) -> Result<()> {
        let layout = self.raid_engine.layout(block_num);
        // Phase 2 alignment accounting: a parity write is
        // "partial-chunk" when the written range does not cover a whole
        // chunk starting at a chunk boundary -- such writes force the
        // full-row reads below. Single-block writes with an 8+-block
        // chunk are always partial; batched full-chunk writes are not.
        {
            use std::sync::atomic::Ordering;
            let chunk = self.raid_engine.chunk_size_blocks as u64;
            let blocks_written = (buf.len() as u64).div_ceil(BLOCK_SIZE as u64);
            let offset_in_chunk = block_num % chunk;
            let aligned = offset_in_chunk == 0 && blocks_written >= chunk;
            crate::debug::stats::PARITY_WRITES_TOTAL.fetch_add(1, Ordering::Relaxed);
            if !aligned {
                crate::debug::stats::PARITY_WRITES_PARTIAL_CHUNK.fetch_add(1, Ordering::Relaxed);
            }
        }
        // ------------------------------------------------------------------
        // Phase 3: incremental read-modify-write path. Reads: 1 old data
        // block + 1-2 old parity blocks (RAID5/6), instead of every other
        // data block in the row. The delta (old ^ new) is XORed into P
        // directly and, for RAID6, into Q scaled by the column's GF(256)
        // coefficient -- the same math as the full recompute (proven by
        // pool::raid's 200-round equivalence tests). Falls back to the
        // full path when any old read fails (degraded pool) or when the
        // caller (journal replay) requires idempotent apply.
        // ------------------------------------------------------------------
        let force_full = force_full || Self::parity_full_forced();
        if !force_full {
            let data_dev = layout.data_devs[0];
            let read_off = layout.phys_block * BLOCK_SIZE as u64;

            let mut old_data = vec![0u8; buf.len()];
            let mut p_ok = true;
            if read_full(&self.files[data_dev], &mut old_data, read_off).is_err() {
                p_ok = false;
            }

            if p_ok {
                crate::debug::stats::PARITY_INCREMENTAL_UPDATES
                    .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                match self.raid_engine.profile {
                    RaidProfile::Raid5 => {
                        let mut old_p = vec![0u8; buf.len()];
                        if read_full(&self.files[layout.parity_devs[0]], &mut old_p, read_off)
                            .is_ok()
                        {
                            let new_p = crate::pool::raid::update_raid5_parity_incremental(
                                &old_data, buf, &old_p,
                            );
                            pwrite_full(&self.files[data_dev], buf, read_off)?;
                            pwrite_full(&self.files[layout.parity_devs[0]], &new_p, read_off)?;
                            return Ok(());
                        }
                    }
                    RaidProfile::Raid6 => {
                        let mut old_p = vec![0u8; buf.len()];
                        let mut old_q = vec![0u8; buf.len()];
                        if read_full(&self.files[layout.parity_devs[0]], &mut old_p, read_off)
                            .is_ok()
                            && read_full(&self.files[layout.parity_devs[1]], &mut old_q, read_off)
                                .is_ok()
                        {
                            let (new_p, new_q) = crate::pool::raid::update_raid6_parity_incremental(
                                layout.column,
                                &old_data,
                                buf,
                                &old_p,
                                &old_q,
                            );
                            pwrite_full(&self.files[data_dev], buf, read_off)?;
                            pwrite_full(&self.files[layout.parity_devs[0]], &new_p, read_off)?;
                            pwrite_full(&self.files[layout.parity_devs[1]], &new_q, read_off)?;
                            return Ok(());
                        }
                    }
                    _ => unreachable!("write_block_parity only called for parity profiles"),
                }
                // Old parity unreadable (degraded): fall through to the
                // full recompute below.
                crate::debug::stats::PARITY_INCREMENTAL_FALLBACKS
                    .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            }
        }

        // Full-row-recompute path (original behavior; also the recovery
        // / degraded fallback).
        let mut other_bufs: Vec<Vec<u8>> = Vec::with_capacity(layout.other_data.len());
        for &(dev, _col) in &layout.other_data {
            let mut b = vec![0u8; buf.len()];
            read_full(
                &self.files[dev],
                &mut b,
                layout.phys_block * BLOCK_SIZE as u64,
            )?;
            other_bufs.push(b);
            crate::debug::stats::PARITY_ROW_READS
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        }

        let data_dev = layout.data_devs[0];
        pwrite_full(
            &self.files[data_dev],
            buf,
            layout.phys_block * BLOCK_SIZE as u64,
        )?;

        match self.raid_engine.profile {
            RaidProfile::Raid5 => {
                let mut refs: Vec<&[u8]> = vec![buf];
                refs.extend(other_bufs.iter().map(|v| v.as_slice()));
                let p = crate::pool::raid::compute_raid5_parity(&refs, buf.len());
                pwrite_full(
                    &self.files[layout.parity_devs[0]],
                    &p,
                    layout.phys_block * BLOCK_SIZE as u64,
                )?;
            }
            RaidProfile::Raid6 => {
                let mut cols: Vec<(usize, &[u8])> = vec![(layout.column, buf)];
                for ((_, col), b) in layout.other_data.iter().zip(other_bufs.iter()) {
                    cols.push((*col, b.as_slice()));
                }
                let (p, q) = crate::pool::raid::compute_raid6_parity(&cols, buf.len());
                pwrite_full(
                    &self.files[layout.parity_devs[0]],
                    &p,
                    layout.phys_block * BLOCK_SIZE as u64,
                )?;
                pwrite_full(
                    &self.files[layout.parity_devs[1]],
                    &q,
                    layout.phys_block * BLOCK_SIZE as u64,
                )?;
            }
            _ => unreachable!("write_block_parity only called for parity profiles"),
        }
        Ok(())
    }

    pub fn write_blocks_parallel(&self, blocks: &[(u64, &[u8])]) -> Result<()> {
        let errors: Vec<_> = blocks
            .par_iter()
            .filter_map(|(block_num, buf)| self.write_block(*block_num, buf).err())
            .collect();

        if let Some(err) = errors.into_iter().next() {
            return Err(err);
        }
        Ok(())
    }

    pub fn sync(&self) -> Result<()> {
        for file in &self.files {
            file.sync_all()?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_paths(n: usize, tag: &str) -> Vec<std::path::PathBuf> {
        let dir =
            std::env::temp_dir().join(format!("lionfs_disk_test_{tag}_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        (0..n).map(|i| dir.join(format!("dev{i}.img"))).collect()
    }

    #[test]
    fn raid5_write_then_read_back() {
        let paths = temp_paths(4, "raid5");
        let disk = Disk::create_pool(&paths, 1024 * 1024, RaidProfile::Raid5, 8).unwrap();
        let data = vec![0xAB; BLOCK_SIZE];
        // Block 0 is special-cased (see write_block) to bypass RAID mapping
        // entirely so the superblock is always discoverable without first
        // knowing the pool's RAID profile; use a different block here so
        // this test actually exercises striping + parity.
        disk.write_block(20, &data).unwrap();
        let mut back = vec![0u8; BLOCK_SIZE];
        disk.read_block(20, &mut back).unwrap();
        assert_eq!(back, data);
        for p in &paths {
            let _ = std::fs::remove_file(p);
        }
    }

    #[test]
    fn raid5_reconstructs_after_simulated_device_loss() {
        let paths = temp_paths(4, "raid5_rebuild");
        let disk = Disk::create_pool(&paths, 1024 * 1024, RaidProfile::Raid5, 8).unwrap();
        let data = vec![0x42; BLOCK_SIZE];
        disk.write_block(20, &data).unwrap();

        let layout = disk.raid_engine.layout(20);
        let victim_dev = layout.data_devs[0];
        // Simulate that device's block being unreadable/corrupted by
        // truncating its backing file so reads past EOF fail with an error.
        drop(disk);
        let f = OpenOptions::new()
            .write(true)
            .open(&paths[victim_dev])
            .unwrap();
        f.set_len(0).unwrap();

        let disk2 = Disk::open_pool(&paths, RaidProfile::Raid5, 8).unwrap();
        let mut back = vec![0u8; BLOCK_SIZE];
        disk2.read_block_resilient(20, &mut back).unwrap();
        assert_eq!(back, data);
        for p in &paths {
            let _ = std::fs::remove_file(p);
        }
    }

    #[test]
    fn raid6_write_then_read_back() {
        let paths = temp_paths(5, "raid6");
        let disk = Disk::create_pool(&paths, 1024 * 1024, RaidProfile::Raid6, 8).unwrap();
        let data = vec![0x77; BLOCK_SIZE];
        disk.write_block(3, &data).unwrap();
        let mut back = vec![0u8; BLOCK_SIZE];
        disk.read_block(3, &mut back).unwrap();
        assert_eq!(back, data);
        for p in &paths {
            let _ = std::fs::remove_file(p);
        }
    }

    #[test]
    fn raid1_mirrors_survive_one_device_loss() {
        let paths = temp_paths(2, "raid1");
        let disk = Disk::create_pool(&paths, 1024 * 1024, RaidProfile::Raid1, 0).unwrap();
        let data = vec![0x9F; BLOCK_SIZE];
        disk.write_block(5, &data).unwrap();
        drop(disk);
        let f = OpenOptions::new().write(true).open(&paths[0]).unwrap();
        f.set_len(0).unwrap();
        let disk2 = Disk::open_pool(&paths, RaidProfile::Raid1, 0).unwrap();
        let mut back = vec![0u8; BLOCK_SIZE];
        disk2.read_block_resilient(5, &mut back).unwrap();
        assert_eq!(back, data);
        for p in &paths {
            let _ = std::fs::remove_file(p);
        }
    }
}
