//! 3.6: snapshot replication -- the `lfs_replicate send | recv` stream
//! (the ZFS send/recv shape, sized for LionFS).
//!
//! **send** walks a SNAPSHOT's frozen view (frozen inode tree, frozen
//! per-inode extents, frozen checksum view -- every block read is
//! verified against the SNAPSHOT's own checksum tree, so the stream
//! carries exactly what the snapshot froze, bit for bit) and writes a
//! portable, self-describing stream file:
//!
//! ```text
//! Header  : magic "LFSS" u32, version u16=1, flags u16=0,
//!           snapshot_id u64, created u64, barrier u64, pool_uuid 16B
//! DirRec  : tag u8=2, path_len u16, path, mode u32, uid u32, gid u32,
//!           mtime i64
//! FileRec : tag u8=1, path_len u16, path, mode u32, uid u32, gid u32,
//!           mtime i64, size u64,
//!           chunks: (len u32, bytes)* terminated by len==0,
//!           sha256 u8[32]      -- digest of the file's plaintext bytes
//! End     : tag u8=0xFF, file_count u32, sha256 u8[32]
//!           -- digest over the per-file digests IN ORDER
//! ```
//!
//! All integers little-endian; paths are relative, `/`-separated.
//! Chunked + checksummed + manifest-terminated: the receiver can
//! verify every file and the manifest as a whole (the tar_stream
//! manifest protocol, carried to snapshots).
//!
//! **recv** replays a stream into a (fresh) image through the
//! ordinary POSIX write path -- the same VfsOps every client uses --
//! verifies every file's digest and the manifest, then records a
//! snapshot on the target so the received state is frozen exactly
//! like the source was. Crash mid-recv = a partially populated
//! image with no snapshot: simply recv again (files are overwritten).
//!
//! Honest 3.6 limits (recorded in `specifications/replication.md`):
//! whole-file records (no incremental delta against a parent snapshot
//! -- that is the obvious 3.7 feature and needs a parent-id field in
//! the header, already reserved via `flags`), no symlink records (the
//! engine does not store symlinks yet), compressed inodes travel as
//! their PLAINTEXT bytes (the receiver re-compresses under its own
//! policy -- cross-compression replication for free).

use std::io::{Error, ErrorKind, Read, Result, Write};

use crate::disk::block_io::Disk;
use crate::ondisk::serialization::{Inode, Superblock, BLOCK_SIZE};
use crate::security::kdf::derive_file_key;
use crate::transaction::manager::TransactionManager;
use crate::transaction::transaction::TxContext;

pub const STREAM_MAGIC: u32 = 0x5353_464C; // "LFSS" (little-endian)
pub const STREAM_VERSION: u16 = 1;
const TAG_FILE: u8 = 1;
const TAG_DIR: u8 = 2;
const TAG_END: u8 = 0xFF;

fn sha256(data: &[u8]) -> [u8; 32] {
    use sha2::{Digest, Sha256};
    let mut h = Sha256::new();
    h.update(data);
    h.finalize().into()
}

// -- send -------------------------------------------------------------------

/// Walks a snapshot's frozen view and emits the stream.
pub struct SendStream<'a> {
    disk: &'a Disk,
    sb: Superblock,
    snap: crate::fs::snapshots::SnapshotManager,
    record: crate::ondisk::serialization::SnapshotRecord,
    out: &'a mut dyn Write,
    files: Vec<[u8; 32]>,
}

impl<'a> SendStream<'a> {
    /// Open a snapshot for sending (offline image).
    pub fn open(disk: &'a Disk, sb: &Superblock, snapshot_id: u64, out: &'a mut dyn Write) -> Result<Self> {
        if sb.snapshot_tree_root == 0 {
            return Err(Error::new(ErrorKind::InvalidData, "image has no snapshot registry"));
        }
        let snap = crate::fs::snapshots::SnapshotManager::new(sb.snapshot_tree_root);
        let mut tx = TransactionManager::new(sb).begin(0);
        let mut ctx = TxContext::new(disk, &mut tx);
        let record = snap
            .get_snapshot(&mut ctx, snapshot_id)?
            .ok_or_else(|| Error::new(ErrorKind::NotFound, "snapshot id not found"))?;
        let header = Self::header_bytes(sb, &record);
        out.write_all(&header)?;
        Ok(Self { disk, sb: *sb, snap, record, out, files: Vec::new() })
    }

    fn header_bytes(sb: &Superblock, record: &crate::ondisk::serialization::SnapshotRecord) -> [u8; 48] {
        let mut h = [0u8; 48];
        h[0..4].copy_from_slice(&STREAM_MAGIC.to_le_bytes());
        h[4..6].copy_from_slice(&STREAM_VERSION.to_le_bytes());
        h[6..8].copy_from_slice(&0u16.to_le_bytes()); // flags
        h[8..16].copy_from_slice(&record.id.to_le_bytes());
        h[16..24].copy_from_slice(&record.creation_time.to_le_bytes());
        h[24..32].copy_from_slice(&record.generation.to_le_bytes());
        h[32..48].copy_from_slice(&sb.pool_uuid);
        h
    }

    /// Recursively serialize the snapshot's tree. `dir_path` is the
    /// '/'-separated prefix ("" at the root).
    pub fn send_all(&mut self) -> Result<usize> {
        let root = self
            .snapshot_inode(1)?
            .ok_or_else(|| Error::new(ErrorKind::InvalidData, "snapshot has no root inode"))?;
        self.send_dir(1, "", &root)?;
        // Manifest end record.
        let mut digest = [0u8; 32];
        let mut concat = Vec::with_capacity(self.files.len() * 32);
        for d in &self.files {
            concat.extend_from_slice(d);
        }
        digest.copy_from_slice(&sha256(&concat)[..32]);
        let mut end = Vec::with_capacity(1 + 4 + 32);
        end.push(TAG_END);
        end.extend_from_slice(&(self.files.len() as u32).to_le_bytes());
        end.extend_from_slice(&digest);
        self.out.write_all(&end)?;
        Ok(self.files.len())
    }

    fn snapshot_inode(&mut self, ino: u64) -> Result<Option<Inode>> {
        let id = self.record.id;
        let mut tx = TransactionManager::new(&self.sb).begin(0);
        let mut ctx = TxContext::new(self.disk, &mut tx);
        self.snap.read_snapshot_inode(&mut ctx, id, ino)
    }

    /// List a frozen directory's entries: the frozen inode's data
    /// blocks parsed as dir entries, checksums verified against the
    /// SNAPSHOT's frozen checksum view.
    fn snapshot_dir_entries(&mut self, dir: &Inode) -> Result<Vec<(u64, String, u8)>> {
        let _id = self.record.id;
        let mut tx = TransactionManager::new(&self.sb).begin(0);
        let mut ctx = TxContext::new(self.disk, &mut tx);
        let mut dir_copy = *dir;
        crate::directory::entries::DirManager::read_entries(
            &mut ctx,
            self.record.checksum_tree_root,
            self.record.bad_blocks_root,
            &mut dir_copy,
        )
        .map(|entries| entries.into_iter().map(|e| (e.ino, e.name, e.file_type)).collect())
    }

    fn send_dir(&mut self, dir_ino: u64, path: &str, dir: &Inode) -> Result<()> {
        let entries = self.snapshot_dir_entries(dir)?;
        for (ino, name, file_type) in entries {
            if name == "." || name == ".." {
                continue;
            }
            let child_path = if path.is_empty() { name.clone() } else { format!("{path}/{name}") };
            let child = self
                .snapshot_inode(ino)?
                .ok_or_else(|| Error::new(ErrorKind::InvalidData, "dir entry points at missing inode"))?;
            if child.mode == 0 {
                continue; // freed inode in the entry table
            }
            let is_dir = file_type == 2;
            if is_dir {
                self.emit_dir(&child_path, &child)?;
                self.send_dir(ino, &child_path, &child)?;
            } else {
                self.emit_file(&child_path, &child)?;
            }
            let _ = dir_ino;
        }
        Ok(())
    }

    fn emit_dir(&mut self, path: &str, inode: &Inode) -> Result<()> {
        let mut rec = Vec::with_capacity(1 + 2 + path.len() + 20);
        rec.push(TAG_DIR);
        rec.extend_from_slice(&(path.len() as u16).to_le_bytes());
        rec.extend_from_slice(path.as_bytes());
        rec.extend_from_slice(&inode.mode.to_le_bytes());
        rec.extend_from_slice(&inode.uid.to_le_bytes());
        rec.extend_from_slice(&inode.gid.to_le_bytes());
        rec.extend_from_slice(&inode.mtime.to_le_bytes());
        self.out.write_all(&rec)
    }

    fn emit_file(&mut self, path: &str, inode: &Inode) -> Result<()> {
        // Read the file's plaintext through the SNAPSHOT's checksum
        // view (every block verified as it is read).
        let data = {
            let id = self.record.id;
            let mut tx = TransactionManager::new(&self.sb).begin(0);
            let mut ctx = TxContext::new(self.disk, &mut tx);
            let inode_copy = *inode;
            // The frozen checksum tree needs a FROZEN handle --
            // `SnapshotManager::read_snapshot_csum` consults it per
            // block; the plain read path consults the LIVE tree, so we
            // verify per block ourselves instead of trusting read_file's
            // live-tree check. Read raw through the frozen extents:
            let mut out = Vec::with_capacity(inode_copy.size as usize);
            let blocks = inode_copy.size.div_ceil(BLOCK_SIZE as u64);
            for lb in 0..blocks {
                let phys = crate::file::writer::FileManager::resolve_physical_block(
                    &mut ctx, &inode_copy, lb,
                )?;
                if phys == 0 {
                    out.extend_from_slice(&[0u8; BLOCK_SIZE]);
                    continue;
                }
                let mut buf = [0u8; BLOCK_SIZE];
                ctx.read_block(phys, &mut buf)?;
                // Frozen-view verification.
                match self.snap.read_snapshot_csum(&mut ctx, id, inode_copy.ino, lb)? {
                    Some(v) => {
                        let algo = crate::integrity::algorithms::ChecksumAlgorithm::from_u8(v.algorithm_id);
                        if !crate::integrity::algorithms::verify_checksum(algo, &buf, &v.checksum_bytes) {
                            return Err(Error::new(
                                ErrorKind::InvalidData,
                                format!("snapshot block {lb} of ino {} failed frozen verification", inode_copy.ino),
                            ));
                        }
                    }
                    None => return Err(Error::new(
                        ErrorKind::InvalidData,
                        format!("snapshot has no checksum record for block {lb} of ino {}", inode_copy.ino),
                    )),
                }
                let start = (lb * BLOCK_SIZE as u64) as usize;
                let end = ((lb + 1) * BLOCK_SIZE as u64).min(inode_copy.size) as usize;
                // Encrypted inodes are refused honestly: the plaintext
                // lives behind the file's key material, which a cold
                // snapshot walk does not carry (documented 3.6 limit).
                if inode_copy.encryption_algo != 0 {
                    return Err(Error::new(
                        ErrorKind::Unsupported,
                        "encrypted inodes need their key material and cannot be sent without it (documented 3.6 limit)",
                    ));
                }
                out.extend_from_slice(&buf[..end - start]);
            }
            out.truncate(inode_copy.size as usize);
            out
        };

        let digest = sha256(&data);
        self.files.push(digest);

        let mut rec = Vec::with_capacity(1 + 2 + path.len() + 32 + 8 + 5 + 32);
        rec.push(TAG_FILE);
        rec.extend_from_slice(&(path.len() as u16).to_le_bytes());
        rec.extend_from_slice(path.as_bytes());
        rec.extend_from_slice(&inode.mode.to_le_bytes());
        rec.extend_from_slice(&inode.uid.to_le_bytes());
        rec.extend_from_slice(&inode.gid.to_le_bytes());
        rec.extend_from_slice(&inode.mtime.to_le_bytes());
        rec.extend_from_slice(&(data.len() as u64).to_le_bytes());
        // Chunks of 64 KiB.
        for chunk in data.chunks(64 * 1024) {
            rec.extend_from_slice(&(chunk.len() as u32).to_le_bytes());
            rec.extend_from_slice(chunk);
        }
        rec.extend_from_slice(&0u32.to_le_bytes());
        rec.extend_from_slice(&digest);
        self.out.write_all(&rec)
    }
}

// -- recv -------------------------------------------------------------------

/// One received file's outcome.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecvSummary {
    pub files: usize,
    pub dirs: usize,
    pub bytes: u64,
    pub snapshot_recorded: Option<u64>,
}

/// Replay a send-stream into `ops` (a mounted target image),
/// verifying every digest and the manifest, then freezing the result
/// as a snapshot.
pub fn recv_stream(
    reader: &mut dyn Read,
    ops: &dyn crate::vfs::VfsOps,
    sb: &Superblock,
    disk: &Disk,
    record_snapshot: bool,
) -> Result<RecvSummary> {
    // Header.
    let mut header = [0u8; 48];
    reader.read_exact(&mut header)?;
    let magic = u32::from_le_bytes(header[0..4].try_into().expect("4 bytes"));
    if magic != STREAM_MAGIC {
        return Err(Error::new(ErrorKind::InvalidData, "not an LFSS stream"));
    }
    let version = u16::from_le_bytes(header[4..6].try_into().expect("2 bytes"));
    if version != STREAM_VERSION {
        return Err(Error::new(ErrorKind::InvalidData, "unknown stream version"));
    }
    let source_snapshot = u64::from_le_bytes(header[8..16].try_into().expect("8 bytes"));

    let mut files = 0usize;
    let mut dirs = 0usize;
    let mut bytes = 0u64;
    let mut digests: Vec<[u8; 32]> = Vec::new();

    loop {
        let mut tag = [0u8; 1];
        reader.read_exact(&mut tag)?;
        match tag[0] {
            TAG_DIR => {
                let (path, _mode, _uid, _gid, _mtime) = read_rec_head(reader)?;
                mkdir_all(ops, &path)?;
                dirs += 1;
            }
            TAG_FILE => {
                let (path, _mode, _uid, _gid, mtime) = read_rec_head(reader)?;
                let mut size_buf = [0u8; 8];
                reader.read_exact(&mut size_buf)?;
                let size = u64::from_le_bytes(size_buf);
                // Chunks until 0.
                let mut data = Vec::with_capacity(size as usize);
                loop {
                    let mut len_buf = [0u8; 4];
                    reader.read_exact(&mut len_buf)?;
                    let len = u32::from_le_bytes(len_buf);
                    if len == 0 {
                        break;
                    }
                    let mut chunk = vec![0u8; len as usize];
                    reader.read_exact(&mut chunk)?;
                    data.extend_from_slice(&chunk);
                }
                let mut digest = [0u8; 32];
                reader.read_exact(&mut digest)?;
                let computed = sha256(&data);
                if computed != digest {
                    return Err(Error::new(
                        ErrorKind::InvalidData,
                        format!("digest mismatch for {path}"),
                    ));
                }
                if data.len() as u64 != size {
                    return Err(Error::new(
                        ErrorKind::InvalidData,
                        format!("size mismatch for {path}"),
                    ));
                }
                // Create parent dirs + file, write through VfsOps.
                let (parent_ino, name) = split_parent(ops, &path)?;
                let attr = match ops.lookup(parent_ino, &name) {
                    Ok(a) => a,
                    Err(_) => ops.create(
                        parent_ino,
                        &name,
                        &crate::vfs::VfsCreate { mode: 0o100644, uid: 0, gid: 0 },
                    )?,
                };
                let mut off = 0u64;
                for chunk in data.chunks(1024 * 1024) {
                    let written = ops.write(attr.ino, off, chunk)?;
                    off += written as u64;
                }
                ops.fsync(attr.ino, true)?;
                let _ = mtime; // (setattr utimens rides on VfsSetAttr; applied below)
                let _ = ops.setattr(
                    attr.ino,
                    &crate::vfs::VfsSetAttr {
                        size: None,
                        mode: Some(_mode & 0o7777),
                        uid: Some(_uid),
                        gid: Some(_gid),
                        atime: None,
                        mtime: std::time::UNIX_EPOCH
                            .checked_add(std::time::Duration::from_secs(mtime.max(0) as u64)),
                    },
                );
                files += 1;
                bytes += size;
                digests.push(digest);
            }
            TAG_END => {
                let mut count_buf = [0u8; 4];
                reader.read_exact(&mut count_buf)?;
                let count = u32::from_le_bytes(count_buf) as usize;
                let mut digest = [0u8; 32];
                reader.read_exact(&mut digest)?;
                if count != digests.len() {
                    return Err(Error::new(
                        ErrorKind::InvalidData,
                        format!("manifest count {count} != received {}", digests.len()),
                    ));
                }
                let mut concat = Vec::with_capacity(digests.len() * 32);
                for d in &digests {
                    concat.extend_from_slice(d);
                }
                if sha256(&concat)[..32] != digest[..32] {
                    return Err(Error::new(ErrorKind::InvalidData, "manifest digest mismatch"));
                }
                break;
            }
            other => {
                return Err(Error::new(
                    ErrorKind::InvalidData,
                    format!("unknown stream tag {other}"),
                ));
            }
        }
    }

    // Freeze the received state as a snapshot (the "recv" contract).
    let mut recorded = None;
    if record_snapshot && files + dirs > 0 {
        recorded = Some(record_recv_snapshot(sb, disk, source_snapshot)?);
    }
    Ok(RecvSummary { files, dirs, bytes, snapshot_recorded: recorded })
}

fn read_rec_head(reader: &mut dyn Read) -> Result<(String, u32, u32, u32, i64)> {
    let mut len_buf = [0u8; 2];
    reader.read_exact(&mut len_buf)?;
    let path_len = u16::from_le_bytes(len_buf) as usize;
    let mut path_buf = vec![0u8; path_len];
    reader.read_exact(&mut path_buf)?;
    let mut fixed = [0u8; 20];
    reader.read_exact(&mut fixed)?;
    let mode = u32::from_le_bytes(fixed[0..4].try_into().unwrap());
    let uid = u32::from_le_bytes(fixed[4..8].try_into().unwrap());
    let gid = u32::from_le_bytes(fixed[8..12].try_into().unwrap());
    let mtime = i64::from_le_bytes(fixed[12..20].try_into().unwrap());
    Ok((String::from_utf8_lossy(&path_buf).into_owned(), mode, uid, gid, mtime))
}

fn split_parent(ops: &dyn crate::vfs::VfsOps, path: &str) -> Result<(u64, String)> {
    let parts: Vec<&str> = path.split('/').filter(|p| !p.is_empty()).collect();
    let (name,) = match parts.split_last() {
        Some((n, _)) => ((*n).to_string(),),
        None => return Err(Error::new(ErrorKind::InvalidData, "empty path in stream")),
    };
    let mut cur = 1u64;
    for p in &parts[..parts.len().saturating_sub(1)] {
        mkdir_all(ops, &parts[..parts.len().saturating_sub(1)].join("/"))?;
        let attr = ops.lookup(cur, p)?;
        cur = attr.ino;
    }
    Ok((cur, name))
}

fn mkdir_all(ops: &dyn crate::vfs::VfsOps, path: &str) -> Result<u64> {
    let mut cur = 1u64;
    for part in path.split('/').filter(|p| !p.is_empty()) {
        match ops.lookup(cur, part) {
            Ok(attr) => cur = attr.ino,
            Err(_) => {
                let attr = ops.mkdir(
                    cur,
                    part,
                    &crate::vfs::VfsCreate {
                        mode: crate::pal::posix::S_IFDIR | 0o755,
                        uid: 0,
                        gid: 0,
                    },
                )?;
                cur = attr.ino;
            }
        }
    }
    Ok(cur)
}

fn record_recv_snapshot(sb: &Superblock, disk: &Disk, snapshot_id: u64) -> Result<u64> {
    // Bare-context snapshot creation on the target (the lfs_snapshot
    // create pattern): freeze the just-received tree as snapshot
    // `snapshot_id`. Disk writes are &self (interior files), so a
    // shared handle is fine -- the caller guarantees single-writer.
    let target_sb = read_live_sb(disk)?;
    let tm = TransactionManager::new(&target_sb);
    let mut tx = tm.begin(0);
    let bg = crate::ondisk::serialization::BlockGroupDescriptor {
        bg_block_bitmap: target_sb.bitmap_start,
        bg_inode_bitmap: 0,
        bg_inode_table: target_sb.inode_table_start,
        bg_free_blocks_count: 0,
        bg_free_inodes_count: 0,
        bg_used_dirs_count: 0,
        bg_padding: 0,
        bg_reserved: [0; 32],
    };
    let mut snap = crate::fs::snapshots::SnapshotManager::new(target_sb.snapshot_tree_root);
    let tx_id = tx.id;
    let mut sb_mut = target_sb;
    let mut ctx = TxContext::new(disk, &mut tx);
    snap.create_snapshot(&mut ctx, &mut sb_mut, snapshot_id, 0, &mut |c| {
        crate::allocator::bitmap::Allocator::allocate_extents_meta(
            c,
            &bg,
            target_sb.blocks_per_group,
            1,
        )
    })?;
    drop(ctx);
    tm.commit(disk, &target_sb, &tx)?;
    sb_mut.node_generation = crate::btree::tree::node_gen_current();
    crate::ondisk::superblock::write_all_slots(disk, &sb_mut, tx_id)?;
    let _ = sb;
    Ok(snapshot_id)
}

fn read_live_sb(disk: &Disk) -> Result<Superblock> {
    let mut buf = [0u8; BLOCK_SIZE];
    disk.read_block(0, &mut buf)?;
    Ok(*bytemuck::from_bytes(&buf[..std::mem::size_of::<Superblock>()]))
}

/// Unused-today hook the incremental (parent-delta) sender will need:
/// a file key derived for stream authentication. Kept so the format
/// docs can reference it.
#[must_use]
pub fn stream_auth_key(master: &[u8; 32], snapshot_id: u64) -> [u8; 32] {
    derive_file_key(master, u64::MAX - snapshot_id)
}
