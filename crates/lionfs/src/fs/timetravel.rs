//! Time travel for the local engine (LionFS 8.0 — the HFS merge).
//!
//! The 3.3 snapshot machinery freezes metadata roots and pins (or
//! birth-protects) data blocks, and [`SnapshotManager`] already exposes
//! per-snapshot reads of inodes, dir entries, and checksums. What it
//! never had — the flagship capability the merged HFS plane brings —
//! is **path-based time travel**: resolve `/a/b/c` *as it was at time
//! t*, read the file's bytes as of that moment, list a directory as of
//! that moment, and diff two snapshots path-wise.
//!
//! Semantics (mirroring the cluster plane's `dag::resolve(path, t)`):
//!
//! * the timeline is the snapshot sequence; a time `t` maps to the
//!   NEWEST snapshot whose `creation_time` (unix seconds, recorded by
//!   [`SnapshotManager::create_snapshot`]) is `<= t`;
//! * `t` before the first snapshot resolves to `NotFound` — there is
//!   no recorded past before the timeline starts (the cluster plane's
//!   full version history is finer-grained; here snapshots are the
//!   granularity);
//! * reads through a snapshot's frozen views are point-in-time STABLE
//!   by construction (frozen metadata CoW + pinned/birth-protected
//!   data): live writes after the snapshot never change what you read;
//! * compressed and encrypted inodes are outside snapshot coverage
//!   (the documented 3.3 limitation — same refusal as
//!   `lfs_snapshot verify`).
//!
//! All functions take a bare [`TxContext`] over an OPEN image plus the
//! image's [`Superblock`] — the same offline shape `lfs_snapshot`
//! uses, so the library, the `lfs_timetravel` tool, and the tests all
//! drive one code path.

use crate::directory::entries::DirManager;
use crate::file::writer::FileManager;
use crate::fs::snapshots::SnapshotManager;
use crate::ondisk::serialization::{Inode, SnapshotRecord, Superblock};
use crate::security::block_cipher::BlockCipherContext;
use crate::transaction::transaction::TxContext;
use std::collections::BTreeMap;
use std::io::{Error, ErrorKind, Result};
use std::time::{SystemTime, UNIX_EPOCH};

/// One point on the timeline.
#[derive(Clone, Copy, Debug)]
pub struct TimelineEntry {
    pub id: u64,
    /// Unix seconds at which the snapshot was taken.
    pub created_at_unix: u64,
    /// Node-stamp barrier the snapshot froze at.
    pub generation: u64,
}

/// A path resolved as of a moment in time.
#[derive(Clone, Debug)]
pub struct ResolvedAt {
    /// Snapshot whose frozen view served the resolution.
    pub snapshot_id: u64,
    /// The moment the request named (unix seconds).
    pub requested_at_unix: u64,
    pub ino: u64,
    /// FUSE-style file type byte (`DT_*`).
    pub file_type: u8,
    pub size: u64,
    /// Modification time of the inode AS SEEN by the snapshot.
    pub mtime_unix: i64,
    pub path: String,
}

/// Kind of change between two snapshots.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DiffKind {
    /// Present in `b`, absent in `a`.
    Added,
    /// Present in `a`, absent in `b`.
    Removed,
    /// Present in both with a different size or mtime.
    Modified,
}

impl DiffKind {
    pub fn as_symbol(self) -> &'static str {
        match self {
            DiffKind::Added => "+",
            DiffKind::Removed => "-",
            DiffKind::Modified => "~",
        }
    }
}

/// One path-level difference between two snapshots.
#[derive(Clone, Debug)]
pub struct PathDiff {
    pub path: String,
    pub kind: DiffKind,
    pub size_a: Option<u64>,
    pub size_b: Option<u64>,
}

impl PathDiff {
    pub fn kind_symbol(&self) -> &'static str {
        self.kind.as_symbol()
    }
}

fn tt_err(kind: ErrorKind, msg: impl Into<String>) -> Error {
    Error::new(kind, format!("timetravel: {}", msg.into()))
}

/// The full snapshot timeline, oldest first.
pub fn timeline(ctx: &mut TxContext, sb: &Superblock) -> Result<Vec<TimelineEntry>> {
    if sb.snapshot_tree_root == 0 {
        return Err(tt_err(
            ErrorKind::NotFound,
            "image has no snapshot tree (mkfs did not initialize one)",
        ));
    }
    let snap = SnapshotManager::new(sb.snapshot_tree_root);
    let mut out: Vec<TimelineEntry> = snap
        .list_snapshots(ctx)?
        .into_iter()
        .map(|r| TimelineEntry {
            id: r.id,
            created_at_unix: r.creation_time,
            generation: r.generation,
        })
        .collect();
    out.sort_by_key(|e| (e.created_at_unix, e.id));
    Ok(out)
}

/// The NEWEST snapshot whose creation time is `<= t` (unix seconds).
///
/// Tie semantics: creation times have SECOND granularity, so several
/// snapshots can share a timestamp. Among those sharing the boundary
/// second, the EARLIEST-taken (smallest id) wins — a snapshot taken
/// later within the same second may already reflect writes that
/// happened after `t` sub-second, so the first one is the conservative
/// point-in-time answer.
pub fn snapshot_at_or_before(
    ctx: &mut TxContext,
    sb: &Superblock,
    t_unix: u64,
) -> Result<SnapshotRecord> {
    let line = timeline(ctx, sb)?;
    if !line.is_empty() && line[0].created_at_unix > t_unix {
        return Err(tt_err(
            ErrorKind::NotFound,
            format!(
                "no snapshot at or before {t_unix}; timeline starts at {} \
                 (snapshot {})",
                line[0].created_at_unix, line[0].id
            ),
        ));
    }
    // Ascending walk; strictly-greater creation time replaces the
    // candidate, so same-second ties keep the earliest (smallest id).
    let mut chosen: Option<TimelineEntry> = None;
    for entry in &line {
        if entry.created_at_unix <= t_unix {
            match &mut chosen {
                Some(c) if c.created_at_unix < entry.created_at_unix => *c = *entry,
                None => chosen = Some(*entry),
                _ => {}
            }
        }
    }
    let pick = chosen.ok_or_else(|| {
        tt_err(ErrorKind::NotFound, format!("no snapshot at or before {t_unix}"))
    })?;
    let snap = SnapshotManager::new(sb.snapshot_tree_root);
    snap.get_snapshot(ctx, pick.id)?
        .ok_or_else(|| tt_err(ErrorKind::NotFound, format!("snapshot {} vanished", pick.id)))
}

/// Normalize an absolute path into components: strip a leading `/`,
/// drop trailing slashes, reject `.`, `..`, and empty components.
fn components_of(path: &str) -> Result<Vec<String>> {
    let trimmed = path.trim_start_matches('/');
    if trimmed.is_empty() {
        return Ok(Vec::new()); // the root itself
    }
    let mut out = Vec::new();
    for comp in trimmed.split('/') {
        match comp {
            "" => {
                return Err(tt_err(
                    ErrorKind::InvalidInput,
                    format!("empty path component in {path:?}"),
                ))
            }
            "." | ".." => {
                return Err(tt_err(
                    ErrorKind::InvalidInput,
                    format!("{comp:?} components are not supported in time-travel paths"),
                ))
            }
            c => out.push(c.to_string()),
        }
    }
    Ok(out)
}

/// Resolve `path` inside ONE snapshot's frozen view. Returns the inode.
fn resolve_in_snapshot(
    ctx: &mut TxContext,
    record: &SnapshotRecord,
    path: &str,
) -> Result<Inode> {
    let comps = components_of(path)?;
    let mut current: Inode =
        match SnapshotManager::read_inode_in_record(ctx, record, 1)? {
            // FUSE root inode
            Some(i) => i,
            None => {
                return Err(tt_err(
                    ErrorKind::NotFound,
                    format!("root inode is absent from snapshot {}'s view", record.id),
                ))
            }
        };
    for comp in comps {
        if !is_directory(&current) {
            return Err(tt_err(
                ErrorKind::NotADirectory,
                format!("component {comp:?} under a non-directory on snapshot {}", record.id),
            ));
        }
        let entries =
            DirManager::read_entries(ctx, record.checksum_tree_root, record.bad_blocks_root, &mut current)?;
        match entries.into_iter().find(|e| e.name == comp) {
            Some(entry) => {
                current = match SnapshotManager::read_inode_in_record(
                    ctx, record, entry.ino,
                )? {
                    Some(i) => i,
                    None => {
                        return Err(tt_err(
                            ErrorKind::NotFound,
                            format!(
                                "inode {} ({comp:?}) is absent from snapshot {}'s view",
                                entry.ino, record.id
                            ),
                        ))
                    }
                };
            }
            None => {
                return Err(tt_err(
                    ErrorKind::NotFound,
                    format!("{comp:?} does not exist in snapshot {}'s view", record.id),
                ))
            }
        }
    }
    Ok(current)
}

fn is_directory(inode: &Inode) -> bool {
    inode.mode & 0o170000 == 0o040000
}

/// Resolve `path` as of unix time `t`. Reading the returned inode's
/// data is [`read_file_at`]; the metadata itself is in
/// [`ResolvedAt::size`] / [`ResolvedAt::mtime_unix`].
pub fn resolve_path_at(
    ctx: &mut TxContext,
    sb: &Superblock,
    path: &str,
    t_unix: u64,
) -> Result<ResolvedAt> {
    let record = snapshot_at_or_before(ctx, sb, t_unix)?;
    let inode = resolve_in_snapshot(ctx, &record, path)?;
    let comps = components_of(path)?;
    let file_type = if is_directory(&inode) {
        4 // DT_DIR
    } else if inode.mode & 0o170000 == 0o120000 {
        10 // DT_LNK
    } else {
        8 // DT_REG
    };
    Ok(ResolvedAt {
        snapshot_id: record.id,
        requested_at_unix: t_unix,
        ino: inode.ino,
        file_type,
        size: inode.size,
        mtime_unix: inode.mtime,
        path: format!("/{}", comps.join("/")),
    })
}

/// Read a file's BYTES as of unix time `t`.
///
/// Refuses compressed and encrypted inodes (the documented 3.3
/// snapshot-coverage limitation).
pub fn read_file_at(
    ctx: &mut TxContext,
    sb: &Superblock,
    path: &str,
    t_unix: u64,
) -> Result<Vec<u8>> {
    let record = snapshot_at_or_before(ctx, sb, t_unix)?;
    let mut inode = resolve_in_snapshot(ctx, &record, path)?;
    if is_directory(&inode) {
        return Err(tt_err(
            ErrorKind::IsADirectory,
            format!("{path:?} is a directory in snapshot {}", record.id),
        ));
    }
    if inode.compression_algo != 0 {
        return Err(tt_err(
            ErrorKind::Unsupported,
            "compressed inodes are outside snapshot coverage (3.3 limitation)",
        ));
    }
    if inode.encryption_algo != 0 {
        return Err(tt_err(
            ErrorKind::Unsupported,
            "encrypted inodes are outside snapshot coverage (3.3 limitation)",
        ));
    }
    let cctx = BlockCipherContext::none();
    let size = inode.size;
    FileManager::read_file(
        ctx,
        record.checksum_tree_root,
        record.bad_blocks_root,
        &cctx,
        &mut inode,
        0,
        size,
    )
}

/// List a directory as of unix time `t`: `(name, DT_* type, ino)`.
pub fn list_dir_at(
    ctx: &mut TxContext,
    sb: &Superblock,
    path: &str,
    t_unix: u64,
) -> Result<Vec<(String, u8, u64)>> {
    let record = snapshot_at_or_before(ctx, sb, t_unix)?;
    let mut inode = resolve_in_snapshot(ctx, &record, path)?;
    if !is_directory(&inode) {
        return Err(tt_err(
            ErrorKind::NotADirectory,
            format!("{path:?} is not a directory in snapshot {}", record.id),
        ));
    }
    let entries =
        DirManager::read_entries(ctx, record.checksum_tree_root, record.bad_blocks_root, &mut inode)?;
    Ok(entries
        .into_iter()
        .map(|e| (e.name, e.file_type, e.ino))
        .collect())
}

/// One recursive namespace walk entry.
#[derive(Clone, Debug)]
struct WalkEntry {
    /// Present for future inode-level diffing; the path-level diff
    /// compares size/mtime only.
    #[allow(dead_code)]
    ino: u64,
    #[allow(dead_code)]
    file_type: u8,
    size: u64,
    mtime_unix: i64,
}

/// Recursively walk a snapshot's namespace from inode `dir` into `out`
/// keyed by absolute path. Depth is bounded by `MAX_WALK_DEPTH`.
const MAX_WALK_DEPTH: usize = 64;

fn walk_snapshot(
    ctx: &mut TxContext,
    record: &SnapshotRecord,
    dir_ino: u64,
    prefix: &str,
    depth: usize,
    out: &mut BTreeMap<String, WalkEntry>,
) -> Result<()> {
    if depth > MAX_WALK_DEPTH {
        return Err(tt_err(
            ErrorKind::InvalidData,
            format!("namespace deeper than {MAX_WALK_DEPTH} levels under {prefix:?}"),
        ));
    }
    let mut dir_inode = match SnapshotManager::read_inode_in_record(ctx, record, dir_ino)? {
        Some(i) => i,
        None => return Ok(()), // dangling entry: skip, diff reports nothing
    };
    let entries =
        DirManager::read_entries(ctx, record.checksum_tree_root, record.bad_blocks_root, &mut dir_inode)?;
    for entry in entries {
        if entry.name == "." || entry.name == ".." {
            continue;
        }
        let child_path = format!("{prefix}/{}", entry.name);
        let child = SnapshotManager::read_inode_in_record(ctx, record, entry.ino)?;
        let (size, mtime) = child
            .as_ref()
            .map(|c| (c.size, c.mtime))
            .unwrap_or((0, 0));
        out.insert(
            child_path.clone(),
            WalkEntry {
                ino: entry.ino,
                file_type: entry.file_type,
                size,
                mtime_unix: mtime,
            },
        );
        // Recurse into subdirectories.
        if entry.file_type == 4 {
            walk_snapshot(ctx, record, entry.ino, &child_path, depth + 1, out)?;
        }
    }
    Ok(())
}

/// Path-level diff between two snapshots (changes FROM `a` TO `b`).
///
/// A path is **Modified** when its size or mtime differs between the
/// frozen views; byte-level comparison is the cluster plane's
/// content-hash diff, out of scope for the frozen-tree walk.
pub fn diff(
    ctx: &mut TxContext,
    sb: &Superblock,
    a: u64,
    b: u64,
) -> Result<Vec<PathDiff>> {
    if sb.snapshot_tree_root == 0 {
        return Err(tt_err(ErrorKind::NotFound, "image has no snapshot tree"));
    }
    let snap = SnapshotManager::new(sb.snapshot_tree_root);
    let rec_a = snap
        .get_snapshot(ctx, a)?
        .ok_or_else(|| tt_err(ErrorKind::NotFound, format!("snapshot {a} not found")))?;
    let rec_b = snap
        .get_snapshot(ctx, b)?
        .ok_or_else(|| tt_err(ErrorKind::NotFound, format!("snapshot {b} not found")))?;

    let mut left = BTreeMap::new();
    let mut right = BTreeMap::new();
    walk_snapshot(ctx, &rec_a, 1, "", 0, &mut left)?;
    walk_snapshot(ctx, &rec_b, 1, "", 0, &mut right)?;

    let mut out = Vec::new();
    let mut paths: Vec<&String> = left.keys().chain(right.keys()).collect();
    paths.sort();
    paths.dedup();
    for p in paths {
        let pa = left.get(p);
        let pb = right.get(p);
        match (pa, pb) {
            (Some(x), Some(y)) => {
                if x.size != y.size || x.mtime_unix != y.mtime_unix {
                    out.push(PathDiff {
                        path: p.clone(),
                        kind: DiffKind::Modified,
                        size_a: Some(x.size),
                        size_b: Some(y.size),
                    });
                }
            }
            (Some(x), None) => out.push(PathDiff {
                path: p.clone(),
                kind: DiffKind::Removed,
                size_a: Some(x.size),
                size_b: None,
            }),
            (None, Some(y)) => out.push(PathDiff {
                path: p.clone(),
                kind: DiffKind::Added,
                size_a: None,
                size_b: Some(y.size),
            }),
            (None, None) => unreachable!("keys come from one of the maps"),
        }
    }
    Ok(out)
}

/// Current unix seconds (timeline tooling convenience).
pub fn now_unix() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::allocator::bitmap::Allocator;
    use crate::btree::tree::node_gen_current;
    use crate::disk::block_io::Disk;
    use crate::fs::parallel_tests::{mount, remount, test_path};
    use crate::fs::snapshots::SnapshotManager;
    use crate::ondisk::serialization::{
        BlockGroupDescriptor, BLOCK_SIZE, LIONFS_MAGIC,
    };
    use crate::ondisk::superblock as sbio;
    use crate::transaction::manager::TransactionManager;
    use crate::transaction::transaction::TxContext;
    use crate::vfs::{VfsCreate, VfsOps};
    use std::path::Path;
    use std::time::Duration;

    /// creation_time has second granularity; boundaries the test
    /// distinguishes must land in distinct seconds.
    fn next_second() {
        std::thread::sleep(Duration::from_millis(1050));
    }

    /// Offline snapshot creation — exactly the `lfs_snapshot create`
    /// tool path (bare context over an unmounted image). Returns the
    /// snapshot's recorded creation time (unix seconds).
    fn take_snapshot(image: &Path, id: u64) -> u64 {
        let mut disk = Disk::open(image).expect("open image");
        let mut buf = [0u8; BLOCK_SIZE];
        disk.read_block(0, &mut buf).expect("read sb");
        let mut sb: Superblock =
            *bytemuck::from_bytes(&buf[..std::mem::size_of::<Superblock>()]);
        assert_eq!(sb.magic, LIONFS_MAGIC, "not a LionFS image");
        disk.live_barrier
            .fetch_max(sb.last_snapshot_generation, std::sync::atomic::Ordering::AcqRel);

        let tm = TransactionManager::new(&sb);
        let mut tx = tm.begin(0);
        let bg = BlockGroupDescriptor {
            bg_block_bitmap: sb.bitmap_start,
            bg_inode_bitmap: 0,
            bg_inode_table: sb.inode_table_start,
            bg_free_blocks_count: 0,
            bg_free_inodes_count: 0,
            bg_used_dirs_count: 0,
            bg_padding: 0,
            bg_reserved: [0; 32],
        };
        let bpg = sb.blocks_per_group;
        let mut snap = SnapshotManager::new(sb.snapshot_tree_root);
        let tx_id = tx.id;
        let mut ctx = TxContext::new(&disk, &mut tx);
        snap.create_snapshot(
            &mut ctx,
            &mut sb,
            id,
            0,
            &mut |c| Allocator::allocate_extents_meta(c, &bg, bpg, 1),
        )
        .expect("create snapshot");
        // Read back the creation time before the context goes away.
        let record = snap
            .get_snapshot(&mut ctx, id)
            .expect("fetch record")
            .expect("record present");
        let created = record.creation_time;
        drop(ctx);
        tm.commit(&disk, &sb, &tx).expect("commit");
        sb.node_generation = node_gen_current();
        sbio::write_all_slots(&mut disk, &sb, tx_id).expect("persist sb");
        created
    }

    /// Open the image bare (tool-style) and run `f` with a TxContext +
    /// superblock.
    fn with_bare_ctx<R>(image: &Path, f: impl FnOnce(&mut TxContext, &Superblock) -> R) -> R {
        let disk = Disk::open(image).expect("open image");
        let mut buf = [0u8; BLOCK_SIZE];
        disk.read_block(0, &mut buf).expect("read sb");
        let sb: Superblock =
            *bytemuck::from_bytes(&buf[..std::mem::size_of::<Superblock>()]);
        let tm = TransactionManager::new(&sb);
        let mut tx = tm.begin(0);
        let mut ctx = TxContext::new(&disk, &mut tx);
        f(&mut ctx, &sb)
    }

    /// THE money test: what `read_file_at` returns must be the bytes
    /// frozen at snapshot time, not the live current bytes — i.e. the
    /// HFS time-travel semantics landed on the local engine's frozen
    /// snapshot trees.
    #[test]
    fn read_file_at_travels_across_versions() {
        let tag = "tt_versions";
        let path = test_path(tag);
        let _ = std::fs::remove_file(&path);

        // v1 on disk, clean unmount, snapshot 1.
        {
            let mut fs = mount(tag, 64);
            let ino = fs.create(1, "doc.txt", &VfsCreate { mode: 0o100644, uid: 1000, gid: 1000 }).expect("create").ino;
            fs.write(ino, 0, b"version one").expect("write v1");
            fs.fsync(ino, true).expect("fsync v1");
            fs.destroy();
        }
        let t1 = take_snapshot(&path, 1);

        // v2 (different length so size travels too), clean unmount,
        // snapshot 2. Sleep past the second boundary so t1 != t2
        // (creation_time is unix SECONDS).
        next_second();
        {
            let mut fs = remount(tag);
            let attr = fs.lookup(1, "doc.txt").expect("lookup");
            fs.write(attr.ino, 0, b"version two (longer)").expect("write v2");
            fs.fsync(attr.ino, true).expect("fsync v2");
            fs.destroy();
        }
        let t2 = take_snapshot(&path, 2);
        assert!(t2 >= t1);

        // Before the timeline starts: NotFound.
        with_bare_ctx(&path, |ctx, sb| {
            let err = read_file_at(ctx, sb, "/doc.txt", t1.saturating_sub(3600)).unwrap_err();
            assert_eq!(err.kind(), std::io::ErrorKind::NotFound);
        });

        // At t1: v1. At t2: v2. Point-in-time stability, path-based.
        with_bare_ctx(&path, |ctx, sb| {
            assert_eq!(
                read_file_at(ctx, sb, "/doc.txt", t1).expect("read at t1"),
                b"version one"
            );
            assert_eq!(
                read_file_at(ctx, sb, "/doc.txt", t2).expect("read at t2"),
                b"version two (longer)"
            );
            // Time BETWEEN the snapshots still resolves to the older one.
            let mid = t1 + (t2 - t1) / 2;
            assert_eq!(
                read_file_at(ctx, sb, "/doc.txt", mid).expect("read mid"),
                b"version one"
            );
            // Far future: the newest snapshot.
            assert_eq!(
                read_file_at(ctx, sb, "/doc.txt", t2 + 86400).expect("read future"),
                b"version two (longer)"
            );
        });

        // The LIVE file is still v2 — snapshots never mutated it.
        {
            let fs = remount(tag);
            let attr = fs.lookup(1, "doc.txt").expect("lookup");
            let live = fs.read(attr.ino, 0, 1024).expect("live read");
            assert_eq!(&live[..20], b"version two (longer)");
        }
    }

    #[test]
    fn resolve_path_at_reports_frozen_metadata() {
        let tag = "tt_resolve";
        let path = test_path(tag);
        let _ = std::fs::remove_file(&path);
        {
            let mut fs = mount(tag, 64);
            let ino = fs.create(1, "data.bin", &VfsCreate { mode: 0o100644, uid: 1000, gid: 1000 }).expect("create").ino;
            let payload: Vec<u8> = (0..3 * BLOCK_SIZE as u32)
                .map(|i| (i * 29 & 0xFF) as u8)
                .collect();
            fs.write(ino, 0, &payload).expect("write");
            fs.fsync(ino, true).expect("fsync");
            fs.destroy();
        }
        let t1 = take_snapshot(&path, 7);

        with_bare_ctx(&path, |ctx, sb| {
            let r = resolve_path_at(ctx, sb, "/data.bin", t1).expect("resolve");
            assert_eq!(r.snapshot_id, 7);
            assert_eq!(r.ino, 2, "first created inode after root");
            assert_eq!(r.size, (3 * BLOCK_SIZE) as u64);
            assert_eq!(r.file_type, 8); // DT_REG
            assert_eq!(r.path, "/data.bin");

            // Unknown path at a valid time: NotFound.
            let err = resolve_path_at(ctx, sb, "/nope.bin", t1).unwrap_err();
            assert_eq!(err.kind(), std::io::ErrorKind::NotFound);
            // Bad component separators: InvalidInput.
            assert_eq!(
                resolve_path_at(ctx, sb, "/a//b", t1).unwrap_err().kind(),
                std::io::ErrorKind::InvalidInput
            );
        });
    }

    #[test]
    fn list_dir_at_sees_deletions_only_later() {
        let tag = "tt_ls";
        let path = test_path(tag);
        let _ = std::fs::remove_file(&path);
        {
            let mut fs = mount(tag, 64);
            let mk = VfsCreate { mode: 0o100644, uid: 1000, gid: 1000 };
            let keep = fs.create(1, "keep.txt", &mk).expect("create").ino;
            let gone = fs.create(1, "gone.txt", &mk).expect("create").ino;
            fs.write(keep, 0, b"kept").expect("write");
            fs.write(gone, 0, b"deleted later").expect("write");
            fs.fsync(keep, true).expect("fsync keep");
            fs.fsync(gone, true).expect("fsync gone");
            fs.destroy();
        }
        let t1 = take_snapshot(&path, 1);
        next_second();
        {
            let mut fs = remount(tag);
            let attr = fs.lookup(1, "gone.txt").expect("lookup");
            fs.unlink(1, "gone.txt").expect("unlink");
            let _ = attr; // inode number captured before unlink for debugging
            fs.destroy();
        }
        let t2 = take_snapshot(&path, 2);

        with_bare_ctx(&path, |ctx, sb| {
            let at_t1: Vec<String> = list_dir_at(ctx, sb, "/", t1)
                .expect("ls t1")
                .into_iter()
                .map(|(n, _, _)| n)
                .collect();
            assert!(at_t1.contains(&"keep.txt".to_string()));
            assert!(at_t1.contains(&"gone.txt".to_string()), "gone.txt must still exist at t1");

            let at_t2: Vec<String> = list_dir_at(ctx, sb, "/", t2)
                .expect("ls t2")
                .into_iter()
                .map(|(n, _, _)| n)
                .collect();
            assert!(at_t2.contains(&"keep.txt".to_string()));
            assert!(
                !at_t2.contains(&"gone.txt".to_string()),
                "gone.txt must be absent at t2"
            );
        });
    }

    #[test]
    fn diff_reports_added_removed_modified() {
        let tag = "tt_diff";
        let path = test_path(tag);
        let _ = std::fs::remove_file(&path);
        {
            let mut fs = mount(tag, 64);
            let mk = VfsCreate { mode: 0o100644, uid: 1000, gid: 1000 };
            let a = fs.create(1, "modified.txt", &mk).expect("create").ino;
            let _r = fs.create(1, "removed.txt", &mk).expect("create");
            fs.mkdir(1, "sub", &VfsCreate { mode: 0o040755, uid: 1000, gid: 1000 })
                .expect("mkdir");
            let c = fs.create(1, "sub/child.txt", &mk).expect("create sub").ino;
            fs.write(a, 0, b"old contents").expect("write");
            fs.write(c, 0, b"child v1").expect("write child");
            fs.fsync(a, true).expect("fsync a");
            fs.fsync(c, true).expect("fsync c");
            fs.destroy();
        }
        let _t1 = take_snapshot(&path, 1);
        next_second();
        {
            let mut fs = remount(tag);
            let attr = fs.lookup(1, "modified.txt").expect("lookup");
            fs.write(attr.ino, 0, b"brand new contents (longer)").expect("overwrite");
            fs.fsync(attr.ino, true).expect("fsync");
            fs.unlink(1, "removed.txt").expect("unlink removed");
            let mk = VfsCreate { mode: 0o100644, uid: 1000, gid: 1000 };
            let n = fs.create(1, "added.txt", &mk).expect("create added").ino;
            fs.write(n, 0, b"new file").expect("write added");
            fs.fsync(n, true).expect("fsync added");
            fs.destroy();
        }
        let _t2 = take_snapshot(&path, 2);

        with_bare_ctx(&path, |ctx, sb| {
            let d = diff(ctx, sb, 1, 2).expect("diff");
            let find = |p: &str| d.iter().find(|x| x.path == p).expect("diff entry");

            let m = find("/modified.txt");
            assert_eq!(m.kind, DiffKind::Modified);
            assert_eq!(m.size_a, Some(12));
            assert_eq!(m.size_b, Some(27));

            let r = find("/removed.txt");
            assert_eq!(r.kind, DiffKind::Removed);
            assert_eq!(r.size_b, None);

            let a = find("/added.txt");
            assert_eq!(a.kind, DiffKind::Added);
            assert_eq!(a.size_a, None);
            assert_eq!(a.size_b, Some(8));

            // Untouched paths are absent from the diff.
            assert!(d.iter().all(|x| x.path != "/sub/child.txt"));

            // Snapshot-order sanity: reversing swaps Added/Removed.
            let rev = diff(ctx, sb, 2, 1).expect("reverse diff");
            let ra = rev.iter().find(|x| x.path == "/removed.txt").unwrap();
            assert_eq!(ra.kind, DiffKind::Added);
        });
    }

    #[test]
    fn timeline_is_sorted_and_complete() {
        let tag = "tt_line";
        let path = test_path(tag);
        let _ = std::fs::remove_file(&path);
        {
            let mut fs = mount(tag, 32);
            let ino = fs.create(1, "f.txt", &VfsCreate { mode: 0o100644, uid: 1000, gid: 1000 }).expect("create").ino;
            fs.write(ino, 0, b"x").expect("write");
            fs.fsync(ino, true).expect("fsync");
            fs.destroy();
        }
        let t1 = take_snapshot(&path, 1);
        next_second();
        let t2 = take_snapshot(&path, 2);
        next_second();
        let t3 = take_snapshot(&path, 3);

        with_bare_ctx(&path, |ctx, sb| {
            let line = timeline(ctx, sb).expect("timeline");
            assert_eq!(line.len(), 3);
            let ids: Vec<u64> = line.iter().map(|e| e.id).collect();
            assert_eq!(ids, vec![1, 2, 3]);
            let times: Vec<u64> = line.iter().map(|e| e.created_at_unix).collect();
            assert!(times.windows(2).all(|w| w[0] <= w[1]));
            assert_eq!(times[0], t1);
            assert_eq!(times[2], t3);
            // All three map to distinct picks at their own timestamps.
            assert_eq!(snapshot_at_or_before(ctx, sb, t1).unwrap().id, 1);
            assert_eq!(snapshot_at_or_before(ctx, sb, t2).unwrap().id, 2);
            assert_eq!(snapshot_at_or_before(ctx, sb, t3).unwrap().id, 3);
        });
    }

    /// Writing AFTER a snapshot must never change what that snapshot
    /// reads (the write-redirect guarantee, now proven through the
    /// path-based time-travel surface).
    #[test]
    fn live_writes_never_mutate_the_past() {
        let tag = "tt_immutable";
        let path = test_path(tag);
        let _ = std::fs::remove_file(&path);
        {
            let mut fs = mount(tag, 64);
            let ino = fs.create(1, "past.txt", &VfsCreate { mode: 0o100644, uid: 1000, gid: 1000 }).expect("create").ino;
            fs.write(ino, 0, b"the past is immutable").expect("write");
            fs.fsync(ino, true).expect("fsync");
            fs.destroy();
        }
        let t1 = take_snapshot(&path, 42);

        // Overwrite the live file TWICE after the snapshot.
        {
            let mut fs = remount(tag);
            let attr = fs.lookup(1, "past.txt").expect("lookup");
            for round in 0..2u8 {
                let payload = format!("attempt {round} to rewrite history {}", round * 7);
                fs.write(attr.ino, 0, payload.as_bytes()).expect("rewrite");
                fs.fsync(attr.ino, true).expect("fsync");
            }
            fs.destroy();
        }

        with_bare_ctx(&path, |ctx, sb| {
            assert_eq!(
                read_file_at(ctx, sb, "/past.txt", t1).expect("past read"),
                b"the past is immutable"
            );
        });
    }
}
