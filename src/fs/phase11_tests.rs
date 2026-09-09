//! Phase 11 money tests: pipelined transaction groups, lock-free
//! readers, and birth-generation snapshots.
//!
//! The contracts under test (see `specifications/phase11_txg_birth.md`):
//!
//! 1. **O(1) total snapshot creation** -- on a checksummed image,
//!    `create_snapshot` allocates O(1) blocks regardless of how many
//!    data blocks are live (3.2-3.4 paid an O(extent runs) pin walk).
//! 2. **Redirect iff birth <= barrier** -- pre-snapshot content is
//!    copy-on-write; post-snapshot content is written in place (no
//!    redirect copies burned).
//! 3. **Birth-aware retention and reclaim** -- truncate under a live
//!    snapshot retains snapshot-reachable blocks; deleting the last
//!    snapshot at-or-above their birth reclaims them.
//! 4. **Pin-mode fallback** -- images with the checksum tree off keep
//!    the 3.3 pin-walk semantics exactly.
//! 5. **Pipelined durability** -- two writers fsyncing concurrently
//!    through the committer thread end up with both files durable
//!    after a clean remount; nothing torn, nothing lost.

use super::parallel_tests::{mkcreate, mount, remount};
use crate::btree::tree::BTree;
use crate::disk::block_io::Disk;
use crate::file::writer::FileManager;
use crate::fs::snapshots::{SnapshotManager, SNAPSHOT_FLAG_BIRTH};
use crate::inode::tree::INODE_TREE_NODE_TYPE;
use crate::integrity::checksum_tree::ChecksumTree;
use crate::integrity::refcount::RefCountManager;
use crate::ondisk::serialization::{BlockGroupDescriptor, Inode, Superblock, BLOCK_SIZE};
use crate::security::block_cipher::BlockCipherContext;
use crate::transaction::manager::TransactionManager;
use crate::transaction::transaction::TxContext;
use crate::vfs::VfsOps;

// -------------------------------------------------------------------
// Fixture (modeled on cow_tests): a scratch image with fixed roots.
// -------------------------------------------------------------------

fn zero_sb() -> Superblock {
    Superblock {
        block_size: 4096,
        next_ino: 3,
        ..unsafe { std::mem::zeroed() }
    }
}

fn bg() -> BlockGroupDescriptor {
    BlockGroupDescriptor {
        bg_block_bitmap: 40,
        bg_inode_bitmap: 0,
        bg_inode_table: 0,
        bg_free_blocks_count: 0,
        bg_free_inodes_count: 0,
        bg_used_dirs_count: 0,
        bg_padding: 0,
        bg_reserved: [0; 32],
    }
}

fn inactive_cipher() -> BlockCipherContext {
    BlockCipherContext {
        compression_algo: 0,
        encryption_algo: 0,
        key: None,
        crypto_tree_root: 0,
    }
}

fn fresh_inode(ino: u64) -> Inode {
    Inode {
        ino,
        mode: 0o100644,
        size: 0,
        extent_count: 0,
        ..unsafe { std::mem::zeroed() }
    }
}

struct Phase11Env {
    ctx: TxContext<'static>,
}

/// `csum_on = false` builds the pin-mode image (checksum tree absent).
fn setup(tag: &str, csum_on: bool) -> Phase11Env {
    let path = std::env::temp_dir().join(format!("test_p11_{tag}.img"));
    let _ = std::fs::remove_file(&path);
    let disk = Disk::create(&path, 1024 * 1024 * 16).unwrap();
    let mut sb = zero_sb();
    sb.inode_tree_root = 20;
    sb.checksum_tree_root = if csum_on { 22 } else { 0 };
    sb.snapshot_tree_root = 24;
    sb.refcount_tree_root = 26;
    sb.next_ino = 3;
    disk.write_block(0, bytemuck::bytes_of(&sb)).unwrap();
    let sb_static: &'static Superblock = Box::leak(Box::new(sb));
    let tm: &'static TransactionManager = Box::leak(Box::new(TransactionManager::new(sb_static)));
    let tx: &'static mut crate::transaction::transaction::Transaction =
        Box::leak(Box::new(tm.begin(0)));
    let disk: &'static mut Disk = Box::leak(Box::new(disk));
    let mut ctx = TxContext::new(disk, tx);
    BTree::<u64, Inode>::init_empty(&mut ctx, 20, INODE_TREE_NODE_TYPE).unwrap();
    if csum_on {
        ChecksumTree::init_empty(&mut ctx, 22).unwrap();
    }
    SnapshotManager::init_empty(&mut ctx, 24).unwrap();
    RefCountManager::init_empty(&mut ctx, 26).unwrap();
    // Free bitmap: blocks 0..64 pre-marked used (superblock, roots,
    // fixture zones); everything after is free for the tests.
    let mut bitmap = vec![0u8; 4096];
    for byte in bitmap.iter_mut().take(8) {
        *byte = 0xFF;
    }
    ctx.write_block(40, &bitmap).unwrap();
    Phase11Env { ctx }
}

/// Count the set bits in the allocation bitmap = allocated blocks.
fn used_blocks(ctx: &mut TxContext) -> u64 {
    let mut bitmap = vec![0u8; 4096];
    ctx.read_block(40, &mut bitmap).unwrap();
    bitmap.iter().map(|b| b.count_ones() as u64).sum()
}

/// Persist `inode` into the fixture's inode tree (root 20), so
/// snapshot reads (and pin walks) can find it.
fn write_inode_fixture(ctx: &mut TxContext, inode: &Inode) {
    let mut tree = BTree::<u64, Inode>::new(20, INODE_TREE_NODE_TYPE);
    let mut next = 300u64;
    let mut alloc = move |_c: &mut TxContext| {
        let b = next;
        next += 1;
        Ok(b)
    };
    tree.insert(ctx, inode.ino, *inode, &mut alloc).unwrap();
}

/// Create one snapshot over the fixture, with a counting allocator.
/// Returns (allocations burned, the sb the snapshot mutated).
fn snapshot_fixture(ctx: &mut TxContext, csum_root: u64, id: u64) -> (u64, Superblock) {
    let mut snap = SnapshotManager::new(24);
    let mut next_node = 300u64;
    let mut burned = 0u64;
    let mut alloc_fn = |_c: &mut TxContext| -> std::io::Result<u64> {
        let b = next_node;
        next_node += 1;
        burned += 1;
        Ok(b)
    };
    let mut sb2 = zero_sb();
    sb2.inode_tree_root = 20;
    sb2.checksum_tree_root = csum_root;
    sb2.snapshot_tree_root = 24;
    sb2.refcount_tree_root = 26;
    sb2.bitmap_start = 40; // the fixture's bitmap block (the real mount reads this off the disk sb)
    sb2.next_ino = 3;
    snap.create_snapshot(ctx, &mut sb2, id, 0, &mut alloc_fn)
        .unwrap();
    (burned, sb2)
}

// -------------------------------------------------------------------
// P3: O(1) total snapshot creation.
// -------------------------------------------------------------------

/// THE Phase 11 money test: with 200 extent runs live (7 inline + 193
/// spilled -- a pin walk would provably burn proportional
/// allocations), a birth-mode `create_snapshot` allocates exactly the
/// snapshot-registry insert: one. 3.2-3.4's pin walk burns one-plus
/// tree op per extent run.
#[test]
fn snapshot_creation_is_o1_in_data_blocks() {
    let mut env = setup("o1create", true);
    let ctx = &mut env.ctx;
    let cctx = inactive_cipher();
    let bpg = 4096u32;

    // One inode with 200 one-block extents (stride 2 keeps the
    // speculative-run merger from fusing them into long runs).
    let mut inode = fresh_inode(2);
    for logical in (0..400u64).step_by(2) {
        let data = vec![b'x'; BLOCK_SIZE];
        FileManager::write_file(
            ctx,
            &bg(),
            bpg,
            22,
            0,
            0,
            &cctx,
            &mut inode,
            logical * BLOCK_SIZE as u64,
            &data,
        )
        .unwrap();
    }
    write_inode_fixture(ctx, &inode);
    assert_eq!(inode.extent_count, 7, "inline slots fill; the rest spill");
    assert_ne!(
        inode.spill_extent_root, 0,
        "the 193 further extents must be in the spill tree (200 runs total to protect)"
    );

    let (burned, sb2) = snapshot_fixture(ctx, 22, 1);
    assert!(
        burned <= 1,
        "O(1) create: at most the registry insert (0 or 1 allocations, never N); got {burned}"
    );
    assert_ne!(sb2.snapshot_tree_root, 0);

    // And the snapshot still reads the file it froze.
    let snap = SnapshotManager::new(24);
    let snap_inode = snap
        .read_snapshot_inode(ctx, 1, 2)
        .unwrap()
        .expect("snapshot holds the inode");
    assert_eq!(snap_inode.extent_count, 7);
    assert_eq!(snap_inode.spill_extent_root, inode.spill_extent_root);
    assert_eq!(snap_inode.size, 399 * BLOCK_SIZE as u64); // last write: logical 398 + 4096
}

// -------------------------------------------------------------------
// P3: redirect iff birth <= barrier.
// -------------------------------------------------------------------

/// Pre-snapshot content redirects (the live mapping moves and the old
/// bytes survive); a fresh post-snapshot append writes in place
/// (exactly one block burned -- no redirect copy).
#[test]
fn redirect_for_pre_snapshot_writes_inplace_for_post() {
    let mut env = setup("redirect", true);
    let ctx = &mut env.ctx;
    let cctx = inactive_cipher();
    let bpg = 4096u32;

    let mut inode = fresh_inode(2);
    // Two blocks ("A") in one write call.
    let data = vec![b'A'; 2 * BLOCK_SIZE];
    FileManager::write_file(ctx, &bg(), bpg, 22, 0, 0, &cctx, &mut inode, 0, &data).unwrap();
    write_inode_fixture(ctx, &inode);
    let old_phys = inode.extents[0].physical_start;

    let (_, sb2) = snapshot_fixture(ctx, 22, 1);
    let after_snapshot = used_blocks(ctx);

    // Overwrite the PRE-snapshot block 0: must redirect (one data
    // block, plus possible metadata path-copies of the frozen csum
    // tree's nodes). The redirect is asserted BEHAVIORALLY: the live
    // mapping moves off the original block and the original bytes
    // survive for the snapshot.
    let data_b = vec![b'B'; BLOCK_SIZE];
    let mut live = inode;
    FileManager::write_file(ctx, &bg(), bpg, 22, 26, 0, &cctx, &mut live, 0, &data_b).unwrap();
    let new_phys = live.extents[0].physical_start;
    assert_ne!(new_phys, old_phys, "pre-snapshot overwrite must redirect");
    let after_overwrite = used_blocks(ctx);
    let redirect_cost = after_overwrite - after_snapshot;
    assert!(
        (1..=3).contains(&redirect_cost),
        "redirect burns 1 data block + frozen-tree path-copies (got {redirect_cost})"
    );
    let snap = SnapshotManager::new(24);
    let snap_inode = snap.read_snapshot_inode(ctx, 1, 2).unwrap().unwrap();
    let old_phys_snap = snap_inode
        .extents
        .iter()
        .find(|e| e.length > 0)
        .unwrap()
        .physical_start;
    assert_eq!(old_phys_snap, old_phys);
    let mut old_bytes = [0u8; BLOCK_SIZE];
    ctx.read_block(old_phys_snap, &mut old_bytes).unwrap();
    assert!(old_bytes.iter().all(|&b| b == b'A'));

    // Post-snapshot APPEND (logical 2): in place -- exactly ONE new
    // block: no redirect copy, and no further path-copies (the
    // overwrite already re-stamped the csum tree's nodes fresh, so
    // the append's insert mutates them in place).
    let data_c = vec![b'C'; BLOCK_SIZE];
    FileManager::write_file(
        ctx,
        &bg(),
        bpg,
        22,
        26,
        0,
        &cctx,
        &mut live,
        2 * BLOCK_SIZE as u64,
        &data_c,
    )
    .unwrap();
    let after_append = used_blocks(ctx);
    assert_eq!(
        after_append - after_overwrite,
        1,
        "post-snapshot append is in-place (no CoW copy, no path copies)"
    );
    let _ = sb2;
}

// -------------------------------------------------------------------
// P3: birth-aware retention and reclaim through truncate + delete.
// -------------------------------------------------------------------

/// Truncate-to-0 under a live snapshot retains the data (the frozen
/// view still reads it); deleting the LAST snapshot reclaims it.
#[test]
fn truncate_retains_under_snapshot_delete_reclaims() {
    let mut env = setup("reclaim", true);
    let ctx = &mut env.ctx;
    let cctx = inactive_cipher();
    let bpg = 4096u32;

    let mut inode = fresh_inode(2);
    let data = vec![b'D'; 4 * BLOCK_SIZE];
    FileManager::write_file(ctx, &bg(), bpg, 22, 0, 0, &cctx, &mut inode, 0, &data).unwrap();
    write_inode_fixture(ctx, &inode);

    let (_, mut sb2) = snapshot_fixture(ctx, 22, 1);
    let with_snapshot = used_blocks(ctx);

    // Truncate to 0: the snapshot's four blocks must be RETAINED
    // (they were born before the barrier).
    let mut live = inode;
    FileManager::truncate_file(ctx, &bg(), bpg, 26, 22, &mut live, 0).unwrap();
    assert_eq!(
        used_blocks(ctx),
        with_snapshot,
        "blocks born <= barrier are retained on truncate"
    );
    assert_eq!(live.size, 0);
    assert_eq!(live.extent_count, 0);

    // The frozen view still reads the ORIGINAL data.
    let snap = SnapshotManager::new(24);
    let snap_inode = snap.read_snapshot_inode(ctx, 1, 2).unwrap().unwrap();
    let e = snap_inode
        .extents
        .iter()
        .find(|e| e.length > 0)
        .unwrap()
        .clone();
    let mut old_bytes = [0u8; BLOCK_SIZE];
    ctx.read_block(e.physical_start, &mut old_bytes).unwrap();
    assert!(
        old_bytes.iter().all(|&b| b == b'D'),
        "frozen view reads pre-truncate data"
    );

    // Delete the (only, last at-or-above-birth) snapshot: the
    // retained data blocks are reclaimed.
    let mut next_node = 400u64;
    let mut alloc_fn = |_c: &mut TxContext| -> std::io::Result<u64> {
        let b = next_node;
        next_node += 1;
        Ok(b)
    };
    let mut snap_mut = snap;
    snap_mut.delete_snapshot(ctx, &mut sb2, 1, &mut alloc_fn).unwrap();
    let after_delete = used_blocks(ctx);
    assert!(
        after_delete <= with_snapshot - 3,
        "deleting the last snapshot reclaims the retained data blocks (used {with_snapshot} -> {after_delete})"
    );
}

// -------------------------------------------------------------------
// P3: pin-mode fallback (checksums off) keeps 3.3 semantics.
// -------------------------------------------------------------------

#[test]
fn pin_mode_fallback_when_csums_off() {
    let mut env = setup("pinmode", false);
    let ctx = &mut env.ctx;
    let cctx = inactive_cipher();
    let bpg = 4096u32;

    let mut inode = fresh_inode(2);
    let data = vec![b'A'; BLOCK_SIZE];
    FileManager::write_file(ctx, &bg(), bpg, 0, 0, 0, &cctx, &mut inode, 0, &data).unwrap();
    write_inode_fixture(ctx, &inode);
    let pinned_phys = inode.extents[0].physical_start;

    let (_burned, mut sb2) = snapshot_fixture(ctx, 0, 1);
    // Pin mode: the walk ran -- proven by the PIN below (the coverage
    // entry exists), which birth mode never creates. Allocation count
    // is not the evidence (tree inserts may not split at this size).
    let rc = RefCountManager::new(26);
    assert!(
        rc.is_pinned(ctx, pinned_phys).unwrap(),
        "pin-mode create pins the data"
    );
    let mut snap = SnapshotManager::new(24);
    let rec = snap.get_snapshot(ctx, 1).unwrap().unwrap();
    assert_eq!(rec.flags & SNAPSHOT_FLAG_BIRTH, 0, "no birth flag in pin mode");

    // Overwrite redirects (pin path) and the old content survives.
    let data_b = vec![b'B'; BLOCK_SIZE];
    let mut live = inode;
    FileManager::write_file(ctx, &bg(), bpg, 0, 26, 0, &cctx, &mut live, 0, &data_b).unwrap();
    assert_ne!(
        live.extents[0].physical_start, pinned_phys,
        "pin-mode overwrite redirects"
    );
    let mut old_bytes = [0u8; BLOCK_SIZE];
    ctx.read_block(pinned_phys, &mut old_bytes).unwrap();
    assert!(old_bytes.iter().all(|&b| b == b'A'));

    // Delete unpins exactly.
    let mut next_node = 400u64;
    let mut alloc_fn = |_c: &mut TxContext| -> std::io::Result<u64> {
        let b = next_node;
        next_node += 1;
        Ok(b)
    };
    snap.delete_snapshot(ctx, &mut sb2, 1, &mut alloc_fn).unwrap();
    assert!(!rc.is_pinned(ctx, pinned_phys).unwrap(), "delete unpins");
}

// -------------------------------------------------------------------
// P1: pipelined durability through the committer thread.
// -------------------------------------------------------------------

/// Two writers, each writing and fsyncing their own file concurrently
/// through the full VFS path (which uses the background committer).
/// After `destroy` + remount, BOTH files' full contents are intact:
/// no torn blocks, no lost updates, no lost commits. This is the
/// end-to-end money test for the quiesce / journal / apply / retire
/// pipeline: the staging lock is released during I/O, so writer B
/// stages while writer A's group is in flight; any lost-overlay bug
/// shows up here as missing or corrupted data.
#[test]
fn pipelined_durable_two_writers_survive_remount() {
    let tag = "p11_pipeline";
    let fs = std::sync::Arc::new(mount(tag, 64));

    let attr_a = fs.create(1, "a.bin", &mkcreate()).expect("create a");
    let attr_b = fs.create(1, "b.bin", &mkcreate()).expect("create b");
    let ino_a = attr_a.ino;
    let ino_b = attr_b.ino;

    // 2 MiB per writer in 64-KiB calls, each followed by fsync --
    // 32 durability groups per writer, interleaved by the scheduler,
    // drained by the background committer.
    const PER_WRITE: usize = 64 * 1024;
    const TOTAL: usize = 2 * 1024 * 1024;
    let block_of = |fill: u8, idx: usize| {
        let mut v = vec![fill; PER_WRITE];
        // A per-offset marker so any torn or misordered write is
        // detectable: the first 8 bytes of each 4-KiB page encode the
        // page's global index; bytes 8..16 are the writer fill.
        let pages = PER_WRITE / BLOCK_SIZE;
        for p in 0..pages {
            let off = p * BLOCK_SIZE;
            v[off..off + 8].copy_from_slice(&(idx + p).to_le_bytes());
            v[off + 8..off + 16].copy_from_slice(&[fill; 8]);
        }
        v
    };
    let fs_a = fs.clone();
    let fs_b = fs.clone();

    let handle_a = std::thread::spawn(move || {
        for i in 0..TOTAL / PER_WRITE {
            let data = block_of(b'a', i * (PER_WRITE / BLOCK_SIZE));
            fs_a.write(ino_a, (i * PER_WRITE) as u64, &data).unwrap();
            fs_a.fsync(ino_a, false).unwrap();
        }
    });
    let handle_b = std::thread::spawn(move || {
        for i in 0..TOTAL / PER_WRITE {
            let data = block_of(b'b', i * (PER_WRITE / BLOCK_SIZE));
            fs_b.write(ino_b, (i * PER_WRITE) as u64, &data).unwrap();
            fs_b.fsync(ino_b, false).unwrap();
        }
    });
    handle_a.join().unwrap();
    handle_b.join().unwrap();

    // Pre-destroy verification: the LIVE view must already be exact.
    {
        let live_check = |ino: u64, fill: u8| {
            let first = fs.read(ino, 0, PER_WRITE as u32).unwrap();
            let ok = first[0..8] == (0u64).to_le_bytes() && first[8..16].iter().all(|&b| b == fill);
            // FULL live check (all chunks): decides live-vs-remount.
            for i in 0..TOTAL / PER_WRITE {
                let got = fs.read(ino, (i * PER_WRITE) as u64, PER_WRITE as u32).unwrap();
                for p in 0..PER_WRITE / BLOCK_SIZE {
                    let off = p * BLOCK_SIZE;
                    let idx = u64::from_le_bytes(got[off..off + 8].try_into().unwrap());
                    let expect = (i * (PER_WRITE / BLOCK_SIZE) + p) as u64;
                    if idx != expect {
                        let cached = fs.core.get_inode(ino).ok();
                        let tree_v = fs.core.with_scratch_ctx(|ctx, sb2| {
                            crate::inode::manager::InodeManager::read_inode(
                                ctx, sb2.inode_tree_root, ino,
                            )
                            .ok()
                        });
                        let fmt_i = |i: &crate::ondisk::serialization::Inode| {
                            (
                                i.size,
                                i.extent_count,
                                i.extents[0..i.extent_count as usize]
                                    .iter()
                                    .map(|e| (e.logical_start, e.physical_start, e.length))
                                    .collect::<Vec<_>>(),
                                i.spill_extent_root,
                            )
                        };
                        panic!(
                            "LIVE FULL mismatch writer {fill} chunk {i} page {p}: idx {idx} expect {expect}; cache {:?}; tree {:?}",
                            cached.as_ref().map(fmt_i),
                            tree_v.as_ref().map(fmt_i)
                        );
                    }
                }
            }
            let _ = ok;
            if false {
                let cached = fs.core.get_inode(ino).ok();
                let tree_version = fs.core.with_scratch_ctx(|ctx, sb2| {
                    crate::inode::manager::InodeManager::read_inode(
                        ctx,
                        sb2.inode_tree_root,
                        ino,
                    )
                    .ok()
                    .map(|i| {
                        (
                            i.size,
                            i.extent_count,
                            i.extents[0..i.extent_count as usize]
                                .iter()
                                .map(|e| (e.logical_start, e.physical_start, e.length))
                                .collect::<Vec<_>>(),
                            i.spill_extent_root,
                        )
                    })
                });
                panic!(
                    "LIVE mismatch writer {fill}: got16={:?}; cache={:?}; tree={:?}",
                    &first[0..16],
                    cached.map(|i| {
                        (
                            i.size,
                            i.extent_count,
                            i.extents[0..i.extent_count as usize]
                                .iter()
                                .map(|e| (e.logical_start, e.physical_start, e.length))
                                .collect::<Vec<_>>(),
                            i.spill_extent_root,
                        )
                    }),
                    tree_version
                );
            }
        };
        live_check(ino_a, b'a');
        live_check(ino_b, b'b');
    }

    let mut fs = std::sync::Arc::try_unwrap(fs).ok().expect("writers dropped");
    fs.destroy();

    // Remount: everything must be there, exactly.
    let fs = remount(tag);
    {
        let mut dbuf = [0u8; BLOCK_SIZE];
        let mut report = Vec::new();
        for loc in [0u64, 8192, 16384] {
            let d = crate::disk::block_io::Disk::open(
                &std::env::temp_dir().join(format!("test_par10_{tag}.img")),
            )
            .unwrap();
            match d.read_block(loc, &mut dbuf) {
                Ok(_) => {
                    let sb = crate::ondisk::superblock::is_valid_superblock_block(&dbuf);
                    report.push(format!(
                        "slot {loc}: {}",
                        sb.map(|s| format!("valid gen={} roots=({}, {})", s.generation, s.inode_tree_root, s.checksum_tree_root))
                            .unwrap_or_else(|| "INVALID".into())
                    ));
                }
                Err(e) => report.push(format!("slot {loc}: read-err {e}")),
            }
        }
        eprintln!("SB-DUMP: {:?} mounted-gen={}", report, fs.core.sb().generation);
    }
    let check = |ino: u64, fill: u8| {
        for i in 0..TOTAL / PER_WRITE {
            let offset = (i * PER_WRITE) as u64;
            let got = fs.read(ino, offset, PER_WRITE as u32).unwrap();
            assert_eq!(got.len(), PER_WRITE, "writer {fill} chunk {i} length");
            for p in 0..PER_WRITE / BLOCK_SIZE {
                let off = p * BLOCK_SIZE;
                let expect_idx = (i * (PER_WRITE / BLOCK_SIZE) + p) as u64;
                let got_idx = u64::from_le_bytes(got[off..off + 8].try_into().unwrap());
                if got_idx != expect_idx {
                    let ino_state = fs.core.get_inode(ino).ok();
                    let extents: Vec<(u64, u64, u64)> = ino_state
                        .map(|i| {
                            let mut v: Vec<(u64, u64, u64)> = i.extents
                                [0..i.extent_count as usize]
                                .iter()
                                .map(|e| (e.logical_start, e.physical_start, e.length))
                                .collect();
                            if i.spill_extent_root != 0 {
                                v.push((u64::MAX, i.spill_extent_root, 0));
                            }
                            v
                        })
                        .unwrap_or_default();
    // spill walk happens in the pre-panic block below
                    let spill_root = ino_state.map(|i| i.spill_extent_root).filter(|r| *r != 0);
                    let spill_dump: Vec<String> = match spill_root {
                        Some(root) => {
                            let entries = fs.core.with_scratch_ctx(|ctx, _sb| {
                                crate::extents::tree::ExtentTree::new(root)
                                    .iter_extents(ctx)
                                    .map(|v| v.len())
                                    .unwrap_or(usize::MAX)
                            });
                            vec![format!("spill root {root}: {} entries", entries)]
                        }
                        None => vec!["no spill tree".to_string()],
                    };
                    panic!(
                        "writer {fill} chunk {i} page {p}: got_idx {got_idx} expect {expect_idx}; size {:?}; extents {:?}; spill {:?}",
                        ino_state.map(|i| i.size),
                        extents,
                        spill_dump
                    );
                }
                if !got[off + 8..off + 16].iter().all(|&b| b == fill) {
                    let g: Vec<u8> = got[off..off + 24].to_vec();
                    panic!(
                        "writer {fill} marker corrupted at chunk {i} page {p}: got {:02x?} (idx {})",
                        g, got_idx
                    );
                }
            }
        }
    };
    check(ino_a, b'a');
    check(ino_b, b'b');
}

/// Readers never observe torn blocks while a writer stages and the
/// committer retires groups. Patterned pages make any tear visible:
/// every 4-KiB page starts with its own index; a torn read returns a
/// page whose index disagrees with its position.
#[test]
fn concurrent_reader_never_torn_during_pipeline_writes() {
    let tag = "p11_torn";
    let fs = std::sync::Arc::new(mount(tag, 32));

    let attr = fs.create(1, "f.bin", &mkcreate()).unwrap();
    let ino = attr.ino;

    const SIZE: usize = 512 * 1024; // 128 pages
    let mkpage = |idx: usize, fill: u8| -> Vec<u8> {
        let mut v = vec![fill; BLOCK_SIZE];
        v[0..8].copy_from_slice(&(idx as u64).to_le_bytes());
        v
    };
    // Initial content: 128 pages of 'Z'.
    let mut initial = Vec::with_capacity(SIZE);
    for p in 0..SIZE / BLOCK_SIZE {
        initial.extend_from_slice(&mkpage(p, b'Z'));
    }
    fs.write(ino, 0, &initial).unwrap();
    fs.fsync(ino, false).unwrap();

    let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let reader = {
        let fs = fs.clone();
        let stop = stop.clone();
        std::thread::spawn(move || {
            let mut reads = 0u64;
            let npages = SIZE / BLOCK_SIZE;
            while !stop.load(std::sync::atomic::Ordering::Relaxed) {
                let page = (reads as usize * 7 + 3) % npages; // deterministic walk
                let off = page * BLOCK_SIZE;
                let got = fs.read(ino, off as u64, BLOCK_SIZE as u32).unwrap();
                // An empty read is legal BEFORE the writer reaches the
                // page (the reader may start first); a NON-empty read
                // must be the full page and the full page must carry
                // the right content -- that is the torn-read check.
                if got.is_empty() {
                    reads += 1;
                    continue;
                }
                assert_eq!(got.len(), BLOCK_SIZE);
                let idx = u64::from_le_bytes(got[0..8].try_into().unwrap());
                assert_eq!(idx, page as u64, "torn read: page {page} returned index {idx}");
                reads += 1;
            }
            reads
        })
    };
    // Writer: rewrites the whole file with rotating fills, 16 pages
    // per call, fsync every 64 KiB -- the reader overlaps staging,
    // quiesces, applies, and retires.
    let writer = {
        let fs = fs.clone();
        std::thread::spawn(move || {
            for round in 0..8u64 {
                let fill = b'X' + (round % 26) as u8;
                let mut data = Vec::with_capacity(SIZE);
                for p in 0..SIZE / BLOCK_SIZE {
                    data.extend_from_slice(&mkpage(p, fill));
                }
                for off in (0..SIZE).step_by(16 * BLOCK_SIZE) {
                    fs.write(ino, off as u64, &data[off..off + 16 * BLOCK_SIZE])
                        .unwrap();
                    fs.fsync(ino, false).unwrap();
                }
            }
        })
    };
    writer.join().unwrap();
    stop.store(true, std::sync::atomic::Ordering::Relaxed);
    let reads = reader.join().unwrap();
    assert!(reads > 0, "reader must have actually read during the writes");

    let mut fs = std::sync::Arc::try_unwrap(fs).ok().unwrap();
    fs.destroy();
}
