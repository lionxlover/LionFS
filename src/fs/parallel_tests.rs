//! Phase 10 money tests: parallel write intake, write-back durability
//! semantics, and the `&self` operations surface under real threads.
//!
//! Every test drives the REAL mount path (`LionFS::new` on a real
//! mkfs'd image, `VfsOps::create/write/read/fsync`), because the whole
//! point of Phase 10 is what concurrent CALLERS see -- not what the
//! internals do in isolation.
//!
//! The durability contract under test (identical to 3.3 where it
//! matters, restated for write-back):
//! * buffered writes are readable immediately (read-your-own-write);
//! * buffered writes survive a "crash" (object dropped, no destroy)
//!   only after the fsync that flushed + committed them;
//! * `destroy` (unmount) flushes + commits everything -- close(2)
//!   semantics;
//! * two threads writing two files never lose updates; two threads
//!   writing one file serialize on the per-inode gate and never
//!   interleave a torn page.

use crate::disk::block_io::Disk;
use crate::fs::filesystem::LionFS;
use crate::inode::tree::INODE_TREE_NODE_TYPE;
use crate::ondisk::serialization::{Inode, Superblock, BLOCK_SIZE};
use crate::transaction::manager::TransactionManager;
use crate::transaction::transaction::TxContext;
use crate::btree::tree::BTree;
use crate::vfs::{VfsCreate, VfsOps, VfsSetAttr};
use std::sync::Arc;
use std::time::Duration;

/// Compact single-device mkfs for tests: mirrors `tools/mkfs/main.rs`
/// (superblock + reserved tree roots + bitmap + root inode + "."
/// and ".." entries + one committed transaction).
pub(crate) fn mkfs_image(path: &std::path::Path, size_mb: u64, compress: bool) {
    let total_blocks = (size_mb * 1024 * 1024) / BLOCK_SIZE as u64;
    let bitmap_blocks = total_blocks.div_ceil(BLOCK_SIZE as u64 * 8);
    let inode_count: u64 = 1024;
    let inodes_per_block = BLOCK_SIZE as u64 / std::mem::size_of::<Inode>() as u64;
    let inode_blocks = inode_count.div_ceil(inodes_per_block);
    let bitmap_start = 1u64;
    let inode_table_start = bitmap_start + bitmap_blocks;
    let data_region_start = inode_table_start + inode_blocks;
    let journal_start = data_region_start;
    let journal_blocks = 4096u64;

    // Phase 11 fix (found live by the pipelined durability money
    // test): `data_region_start` must point AFTER the journal -- the
    // bitmap marks 0..data_region_start used, so pointing it at the
    // journal START left the journal's 4096 blocks allocatable: the
    // allocator handed them to file data and tree nodes, and the
    // journal's own writes clobbered them (live zero pages, lost
    // extents, torn replay tails). This now matches tools/mkfs.
    let data_after_journal = journal_start + journal_blocks;
    let mut sb = Superblock {
        block_size: BLOCK_SIZE as u32,
        total_blocks,
        free_blocks: total_blocks
            - (journal_start + journal_blocks)
            - crate::ondisk::superblock::CANDIDATE_LOCATIONS
                .iter()
                .filter(|&&l| l > 0 && l < total_blocks)
                .count() as u64,
        inode_count,
        root_inode: 1,
        bitmap_start,
        inode_table_start,
        data_region_start: data_after_journal,
        generation: 1,
        journal_start,
        journal_blocks,
        secondary_sb_1: 0,
        secondary_sb_2: 0,
        block_group_count: 1,
        blocks_per_group: total_blocks as u32,
        inode_tree_root: 12,
        checksum_tree_root: 13,
        bad_blocks_root: 14,
        key_tree_root: 15,
        crypto_tree_root: 16,
        dedupe_tree_root: 17,
        snapshot_tree_root: 18,
        next_ino: 2,
        default_compression: if compress {
            crate::common::constants::COMPRESSION_ZSTD
        } else {
            0
        },
        default_encryption: 0,
        raid_profile: 0,
        chunk_size: 0,
        node_generation: 0,
        ..unsafe { std::mem::zeroed() }
    };
    sb.magic = crate::ondisk::serialization::LIONFS_MAGIC;
    sb.version = crate::common::version::CURRENT_VERSION;
    sb.checksum = crate::utils::checksum::calculate_superblock_checksum(&sb);

    let mut disk = Disk::create(path, size_mb * 1024 * 1024).unwrap();
    disk.write_block(0, bytemuck::bytes_of(&sb)).unwrap();

    // Bitmap: metadata + journal marked used, PLUS the in-range
    // superblock slot blocks (Phase 11 layout-collision fix -- see
    // tools/mkfs/main.rs for the full story).
    let mut bitmap_buf = [0u8; BLOCK_SIZE];
    for i in 0..sb.data_region_start {
        bitmap_buf[(i / 8) as usize] |= 1 << (i % 8);
    }
    for &slot in crate::ondisk::superblock::CANDIDATE_LOCATIONS.iter() {
        if slot > 0 && slot < total_blocks {
            bitmap_buf[(slot / 8) as usize] |= 1 << (slot % 8);
        }
    }
    disk.write_block(bitmap_start, &bitmap_buf).unwrap();
    for i in 1..bitmap_blocks {
        disk.write_block(bitmap_start + i, &[0u8; BLOCK_SIZE]).unwrap();
    }
    for i in 0..inode_blocks {
        disk.write_block(inode_table_start + i, &[0u8; BLOCK_SIZE]).unwrap();
    }

    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64;
    let root_inode = Inode {
        ino: 1,
        mode: crate::pal::posix::S_IFDIR | 0o755,
        uid: 1000,
        gid: 1000,
        links_count: 2,
        size: 0,
        extent_count: 0,
        compression_algo: 0,
        encryption_algo: 0,
        key_id: 0,
        ctime: now,
        mtime: now,
        atime: now,
        ..unsafe { std::mem::zeroed() }
    };

    let tm = TransactionManager::new(&sb);
    let mut tx = tm.begin(0);
    {
        let mut ctx = TxContext::new(&disk, &mut tx);
        BTree::<u64, Inode>::init_empty(&mut ctx, sb.inode_tree_root, INODE_TREE_NODE_TYPE)
            .unwrap();
        let mut tree = BTree::<u64, Inode>::new(sb.inode_tree_root, INODE_TREE_NODE_TYPE);
        let mut mock_allocator = |_ctx: &mut TxContext| -> std::io::Result<u64> { Ok(20) };
        tree.insert(&mut ctx, 1, root_inode, &mut mock_allocator).unwrap();
        crate::integrity::checksum_tree::ChecksumTree::init_empty(&mut ctx, sb.checksum_tree_root)
            .unwrap();
        crate::integrity::bad_blocks::BadBlockManager::init_empty(&mut ctx, sb.bad_blocks_root)
            .unwrap();
        crate::security::keys::KeyTree::init_empty(&mut ctx, sb.key_tree_root).unwrap();
        crate::security::block_cipher::BlockTransformTree::init_empty(
            &mut ctx,
            sb.crypto_tree_root,
        )
        .unwrap();
        crate::fs::dedupe::DedupeTree::init_empty(&mut ctx, sb.dedupe_tree_root).unwrap();
        crate::fs::snapshots::SnapshotManager::init_empty(&mut ctx, sb.snapshot_tree_root)
            .unwrap();
    }
    tm.commit(&disk, &sb, &tx).unwrap();
    disk.sync().unwrap();
}

pub(crate) fn test_path(tag: &str) -> std::path::PathBuf {
    std::env::temp_dir().join(format!("test_par10_{tag}.img"))
}

pub(crate) fn mount(tag: &str, size_mb: u64) -> LionFS {
    let path = test_path(tag);
    let _ = std::fs::remove_file(&path);
    mkfs_image(&path, size_mb, false);
    LionFS::new(
        Disk::open(&path).expect("open fresh image"),
        path.to_string_lossy().into_owned(),
    )
    .expect("mount")
}

pub(crate) fn remount(tag: &str) -> LionFS {
    let path = test_path(tag);
    LionFS::new(
        Disk::open(&path).expect("reopen image"),
        path.to_string_lossy().into_owned(),
    )
    .expect("remount")
}

pub(crate) fn mkcreate() -> VfsCreate {
    VfsCreate {
        mode: 0o100644,
        uid: 1000,
        gid: 1000,
    }
}

fn pattern(len: usize, seed: u8) -> Vec<u8> {
    // wrapping arithmetic: deterministic, overflow-free in debug builds.
    (0..len)
        .map(|i| i.wrapping_mul(7).wrapping_add(seed as usize * 13) as u8)
        .collect()
}

/// THE Phase 10 money test: N threads, N files, one shared `Arc<LionFS>`
/// -- the single mount that 3.3 could only drive one writer at a time.
/// No lost updates, byte-exact content, durable after fsync.
#[test]
fn concurrent_writes_two_files_no_lost_update() {
    let tag = "two_files";
    let fs = Arc::new(mount(tag, 64));
    const THREADS: usize = 4;
    const PER_FILE: u64 = 256 * 1024;

    let mut handles = Vec::new();
    for t in 0..THREADS {
        let fs = Arc::clone(&fs);
        handles.push(std::thread::spawn(move || {
            let name = format!("t{t}.bin");
            let attr = fs.create(1, &name, &mkcreate()).expect("create");
            let data = pattern(PER_FILE as usize, t as u8);
            let mut off = 0u64;
            while off < PER_FILE {
                let n = fs
                    .write(attr.ino, off, &data[off as usize..(off + 4096) as usize])
                    .expect("write");
                assert_eq!(n as u64, 4096);
                off += 4096;
            }
            fs.fsync(attr.ino, false).expect("fsync");
            attr.ino
        }));
    }
    let inos: Vec<u64> = handles.into_iter().map(|h| h.join().unwrap()).collect();
    assert_eq!(inos.len(), THREADS);

    // Byte-exact verification on the same mount (cache/disk merge)...
    for (t, &ino) in inos.iter().enumerate() {
        let expect = pattern(PER_FILE as usize, t as u8);
        let got = fs.read(ino, 0, PER_FILE as u32).expect("read");
        assert_eq!(got, expect, "thread {t} content mismatch pre-remount");
    }
    drop(fs);

    // ...and after a clean unmount + remount (durable path).
    let fs = remount(tag);
    for (t, &ino) in inos.iter().enumerate() {
        let expect = pattern(PER_FILE as usize, t as u8);
        let got = fs.read(ino, 0, PER_FILE as u32).expect("read after remount");
        assert_eq!(got, expect, "thread {t} content lost across remount");
    }
}

/// Same file, two threads, disjoint page ranges, interleaved: the
/// per-inode gate must serialize page writes so the merged content is
/// exactly "thread A's pages + thread B's pages".
#[test]
fn same_file_interleaved_disjoint_pages() {
    let tag = "same_file";
    let fs = Arc::new(mount(tag, 64));
    let attr = fs.create(1, "shared.bin", &mkcreate()).expect("create");
    let ino = attr.ino;
    const PAGES: u64 = 64;

    let a = Arc::clone(&fs);
    let b = Arc::clone(&fs);
    let (ja, jb) = (
        std::thread::spawn(move || {
            for p in 0..PAGES {
                let data = pattern(4096, 1);
                assert_eq!(a.write(ino, p * 2 * 4096, &data).unwrap(), 4096);
            }
        }),
        std::thread::spawn(move || {
            for p in 0..PAGES {
                let data = pattern(4096, 2);
                assert_eq!(b.write(ino, (p * 2 + 1) * 4096, &data).unwrap(), 4096);
            }
        }),
    );
    ja.join().unwrap();
    jb.join().unwrap();
    fs.fsync(ino, false).unwrap();

    let total = (PAGES * 2 * 4096) as u32;
    let got = fs.read(ino, 0, total).expect("read");
    assert_eq!(got.len(), total as usize);
    for p in 0..PAGES * 2 {
        let expect = pattern(4096, if p % 2 == 0 { 1 } else { 2 });
        assert_eq!(
            &got[(p * 4096) as usize..((p + 1) * 4096) as usize],
            &expect[..],
            "page {p} torn or lost"
        );
    }
    drop(fs);

    let fs = remount(tag);
    let got = fs.read(ino, 0, total).expect("read after remount");
    for p in 0..PAGES * 2 {
        let expect = pattern(4096, if p % 2 == 0 { 1 } else { 2 });
        assert_eq!(&got[(p * 4096) as usize..((p + 1) * 4096) as usize], &expect[..]);
    }
}

/// Partial-block writes at unaligned offsets merge correctly with
/// committed content (page-level RMW), and a buffered read sees them
/// before any flush.
#[test]
fn partial_write_rmw_and_read_your_write() {
    let tag = "rmw";
    let fs = mount(tag, 48);
    let attr = fs.create(1, "rmw.bin", &mkcreate()).expect("create");
    let ino = attr.ino;

    // Commit a base block first.
    let base = pattern(4096, 9);
    assert_eq!(fs.write(ino, 0, &base).unwrap(), 4096);
    fs.fsync(ino, false).unwrap();

    // Partial overwrite at an unaligned offset, still buffered.
    let patch = vec![0xA5u8; 100];
    assert_eq!(fs.write(ino, 100, &patch).unwrap(), 100);

    // Read-your-own-buffered-write BEFORE any flush.
    let got = fs.read(ino, 0, 4096).expect("read");
    assert_eq!(got.len(), 4096);
    assert_eq!(&got[0..100], &base[0..100]);
    assert_eq!(&got[100..200], &patch[..]);
    assert_eq!(&got[200..], &base[200..]);

    // And after fsync + remount (durable).
    fs.fsync(ino, false).unwrap();
    drop(fs);
    let fs = remount(tag);
    let got = fs.read(ino, 0, 4096).expect("read after remount");
    assert_eq!(&got[100..200], &patch[..]);
    assert_eq!(&got[200..], &base[200..]);
}

/// The durability contract: fsync'd buffered writes survive a crash
/// (object dropped without destroy); unflushed buffered writes do not
/// -- exactly the 3.3 semantics for an uncommitted transaction.
#[test]
fn fsync_durable_unflushed_lost() {
    let tag = "durability";
    let fs = mount(tag, 48);
    let keep = fs.create(1, "keep.bin", &mkcreate()).expect("create");
    let lose = fs.create(1, "lose.bin", &mkcreate()).expect("create");
    let data = pattern(8192, 3);

    assert_eq!(fs.write(keep.ino, 0, &data).unwrap(), 8192);
    fs.fsync(keep.ino, false).unwrap(); // flushed + committed + synced
    assert_eq!(fs.write(lose.ino, 0, &data).unwrap(), 8192); // buffered only

    // "Crash": drop the mount without destroy (no flush, no commit).
    drop(fs);

    let fs = remount(tag);
    let got = fs.read(keep.ino, 0, 8192).expect("read keep");
    assert_eq!(got, data, "fsync'd data must survive");
    let gone = fs.read(lose.ino, 0, 8192).expect("read lose");
    assert!(
        gone.is_empty() || gone.iter().all(|&b| b == 0),
        "unflushed buffered data must NOT survive a crash (write-back contract)"
    );
}

/// close(2) semantics: `destroy` (unmount) is a full barrier.
#[test]
fn destroy_flushes_all_buffered_writes() {
    let tag = "destroy";
    let mut fs = mount(tag, 48);
    for i in 0..3 {
        let attr = fs
            .create(1, &format!("d{i}.bin"), &mkcreate())
            .expect("create");
        let data = pattern(4096 + i * 100, i as u8);
        assert_eq!(fs.write(attr.ino, 0, &data).unwrap(), data.len() as u32);
    }
    fs.destroy(); // flush + commit + sync, no explicit fsyncs anywhere

    let fs = remount(tag);
    for i in 0..3 {
        let found = fs.lookup(1, &format!("d{i}.bin")).expect("lookup");
        let data = pattern(4096 + i * 100, i as u8);
        let got = fs.read(found.ino, 0, data.len() as u32).expect("read");
        assert_eq!(got, data, "destroy must have flushed file {i}");
    }
}

/// getattr reflects buffered writes immediately (shadow size), and the
/// committed size catches up after flush.
#[test]
fn getattr_sees_shadow_size() {
    let tag = "shadow";
    let fs = mount(tag, 48);
    let attr = fs.create(1, "s.bin", &mkcreate()).expect("create");
    assert_eq!(fs.getattr(attr.ino).unwrap().size, 0);

    let data = pattern(10 * 4096, 5);
    fs.write(attr.ino, 0, &data).unwrap();
    assert_eq!(fs.getattr(attr.ino).unwrap().size, data.len() as u64);

    fs.fsync(attr.ino, false).unwrap();
    assert_eq!(fs.getattr(attr.ino).unwrap().size, data.len() as u64);
    drop(fs);
    let fs = remount(tag);
    assert_eq!(fs.getattr(attr.ino).unwrap().size, data.len() as u64);
}

/// Truncate after buffered writes: flush-first ordering means nothing
/// is lost or resurrected.
#[test]
fn truncate_after_buffered_writes() {
    let tag = "trunc";
    let fs = mount(tag, 48);
    let attr = fs.create(1, "t.bin", &mkcreate()).expect("create");
    let data = pattern(3 * 4096, 6);
    fs.write(attr.ino, 0, &data).unwrap();

    let shrunken = VfsSetAttr {
        size: Some(4096),
        ..Default::default()
    };
    let after = fs.setattr(attr.ino, &shrunken).expect("setattr");
    assert_eq!(after.size, 4096);

    let got = fs.read(attr.ino, 0, 4096).expect("read");
    assert_eq!(got, &data[..4096]);
    // Beyond the new EOF: nothing.
    assert!(fs.read(attr.ino, 4096, 4096).unwrap().is_empty());
    drop(fs);

    let fs = remount(tag);
    assert_eq!(fs.getattr(attr.ino).unwrap().size, 4096);
    assert_eq!(fs.read(attr.ino, 0, 4096).unwrap(), &data[..4096]);
}

/// Unlink drops the inode's buffered pages: a new file of the same
/// name never sees stale bytes.
#[test]
fn unlink_drops_unsynced_pages() {
    let tag = "unlink";
    let fs = mount(tag, 48);
    let attr = fs
        .create(1, "u.bin", &mkcreate())
        .expect("create");
    fs.write(attr.ino, 0, &pattern(4096, 7)).unwrap();
    fs.unlink(1, "u.bin").expect("unlink");

    let fresh = fs.create(1, "u.bin", &mkcreate()).expect("recreate");
    let got = fs.read(fresh.ino, 0, 4096).expect("read fresh");
    assert!(got.is_empty(), "stale pages leaked through unlink");
}

/// Concurrent readers during active buffered writes: readers see a
/// consistent prefix, never a torn page, without blocking the writer.
#[test]
fn readers_and_writer_overlap() {
    let tag = "overlap";
    let fs = Arc::new(mount(tag, 64));
    let attr = fs.create(1, "o.bin", &mkcreate()).expect("create");
    let ino = attr.ino;
    const ROUNDS: u64 = 64;

    let writer = {
        let fs = Arc::clone(&fs);
        std::thread::spawn(move || {
            for r in 0..ROUNDS {
                let data = pattern(4096, (r % 250) as u8 + 1);
                assert_eq!(fs.write(ino, r * 4096, &data).unwrap(), 4096);
            }
            fs.fsync(ino, false).unwrap();
        })
    };
    // Readers hammer the growing region while the writer works.
    let readers: Vec<_> = (0..3)
        .map(|_| {
            let fs = Arc::clone(&fs);
            std::thread::spawn(move || loop {
                let size = fs.getattr(ino).unwrap().size;
                if size == 0 {
                    std::thread::sleep(Duration::from_micros(200));
                    continue;
                }
                // Any page the read returns must be a VALID pattern page
                // (never half-written): spot-check the first byte against
                // the page's own byte pattern.
                let got = fs.read(ino, 0, size as u32).unwrap();
                let pages = got.len() / 4096;
                if pages == 0 {
                    continue;
                }
                // The writer fills pages 0..k densely and sequentially, so
                // every full page the read returns must be exactly round
                // p's pattern -- a torn page (interleaved half-writes)
                // would break this.
                for p in 0..pages {
                    let page = &got[p * 4096..(p + 1) * 4096];
                    let expect = pattern(4096, (p % 250) as u8 + 1);
                    if page != &expect[..] {
                        let first16: Vec<u8> = page[0..16].to_vec();
                        let expect16: Vec<u8> = expect[0..16].to_vec();
                        let cached = fs.core.page_cache.get_pages(ino, &[p as u64]);
                        let cached_first: Vec<u8> = cached
                            .first()
                            .map(|(_, pg)| pg[0..8].to_vec())
                            .unwrap_or_default();
                        panic!(
                            "torn page at {p}: got {first16:?} expect {expect16:?} (pages={pages}, size={size}); cache-hit {} cached8 {:?}",
                            !cached.is_empty(),
                            cached_first
                        );
                    }
                }
                if size >= ROUNDS * 4096 {
                    break;
                }
            })
        })
        .collect();
    writer.join().unwrap();
    for r in readers {
        r.join().unwrap();
    }
    let final_data = fs.read(ino, 0, (ROUNDS * 4096) as u32).unwrap();
    for r in 0..ROUNDS {
        assert_eq!(
            &final_data[(r * 4096) as usize..((r + 1) * 4096) as usize],
            &pattern(4096, (r % 250) as u8 + 1)[..]
        );
    }
}

/// Compressed inodes take the 3.3 write-through path (no page cache):
/// writes still work, read-back still decodes, fsync still durables.
#[test]
fn compressed_inode_write_through_still_works() {
    let tag = "compressed";
    let path = test_path(tag);
    let _ = std::fs::remove_file(&path);
    mkfs_image(&path, 64, true);
    let fs = LionFS::new(
        Disk::open(&path).expect("open"),
        path.to_string_lossy().into_owned(),
    )
    .expect("mount");

    let attr = fs.create(1, "c.bin", &mkcreate()).expect("create");
    // Compressible data (runs of one byte).
    let mut data = vec![0u8; 128 * 1024];
    for (i, b) in data.iter_mut().enumerate() {
        *b = (i / 4096) as u8;
    }
    let mut off = 0;
    while off < data.len() {
        let end = (off + 4096).min(data.len());
        assert_eq!(fs.write(attr.ino, off as u64, &data[off..end]).unwrap() as usize, end - off);
        off = end;
    }
    fs.fsync(attr.ino, false).unwrap();

    let got = fs.read(attr.ino, 0, data.len() as u32).expect("read");
    assert_eq!(got, data);
    drop(fs);

    let fs = remount(tag);
    let got = fs.read(attr.ino, 0, data.len() as u32).expect("read after remount");
    assert_eq!(got, data);
}

// appended to parallel_tests.rs temporarily
#[test]
fn sequential_create_write_read_lookup() {
    let tag = "seqrepro";
    let fs = mount(tag, 64);
    for t in 0..4u64 {
        let name = format!("t{t}.bin");
        let attr = fs.create(1, &name, &mkcreate()).expect("create");
        eprintln!("created {name} ino={}", attr.ino);
        let data = pattern(8192, t as u8);
        let n = fs.write(attr.ino, 0, &data).expect("write");
        assert_eq!(n, 8192);
        let got = fs.read(attr.ino, 0, 8192).expect("read");
        assert_eq!(got, data);
    }
    // re-read all inodes after commits
    for t in 0..4u64 {
        let name = format!("t{t}.bin");
        let found = fs.lookup(1, &name).expect("lookup");
        eprintln!("lookup {name} -> ino={}", found.ino);
        let got = fs.read(found.ino, 0, 8192).expect("read2");
        assert_eq!(got.len(), 8192);
    }
}
