//! Phase 12 (3.6) money tests: xattrs + ACLs, reflink, the wired
//! self-heal scrub, Format-Vault conformance, and snapshot send/recv.
//!
//! Conventions follow `parallel_tests.rs` (real images, real mounts,
//! destroy+remount for durability) with one new fixture: a three-
//! device RAID5 pool (`mkfs_pool`), because the scrubber's heal path
//! needs real parity to reconstruct from.

use std::io::Write;
use std::sync::Arc;

use super::parallel_tests::{mkcreate, mount, remount, test_path};
use crate::ondisk::serialization::{Superblock, BLOCK_SIZE, LIONFS_MAGIC};
use crate::vfs::{VfsCreate, VfsError, VfsOps};


/// Tear-down helper: works for both a plain `LionFS` and a
/// single-owner `Arc<LionFS>` (tests keep the mount in an Arc while
/// driving it from several code paths).
trait DestroyNow {
    fn destroy_now(self);
}
impl DestroyNow for LionFS {
    fn destroy_now(mut self) {
        LionFS::destroy(&mut self);
    }
}
impl DestroyNow for Arc<LionFS> {
    fn destroy_now(self) {
        match Arc::try_unwrap(self) {
            Ok(mut fs) => LionFS::destroy(&mut fs),
            Err(_) => panic!("destroy_now on a shared mount"),
        }
    }
}

fn destroy_any<T: DestroyNow>(t: T) {
    t.destroy_now();
}

fn errno_of(r: Result<(), VfsError>) -> i32 {
    r.err().map(|e| e.errno).unwrap_or(0)
}

// -- xattrs ---------------------------------------------------------------

#[test]
fn xattr_roundtrip_persists_across_remount() {
    let fs = Arc::new(mount("p12_xattr_persist", 64));
    let attr = fs.create(1, "f.txt", &mkcreate()).expect("create");
    let ino = attr.ino;

    fs.setxattr( ino, "user.mime_type", b"text/plain", 0).expect("setxattr");
    assert_eq!(
        fs.getxattr( ino, "user.mime_type").expect("getxattr"),
        Some(b"text/plain".to_vec())
    );
    // Read-your-own-write before durability, too.
    let listed = fs.listxattr( ino).expect("listxattr");
    assert!(listed.contains(&"user.mime_type".to_string()));

    let fs = Arc::try_unwrap(fs).ok().expect("sole owner");
    destroy_any(fs);

    let fs = remount("p12_xattr_persist");
    assert_eq!(
        fs.getxattr( ino, "user.mime_type").expect("getxattr after remount"),
        Some(b"text/plain".to_vec())
    );
    assert!(fs.listxattr( ino)
        .expect("list")
        .contains(&"user.mime_type".to_string()));
    // Replace + remove.
    fs.setxattr( ino, "user.mime_type", b"image/png", 0).expect("replace");
    assert_eq!(
        fs.getxattr( ino, "user.mime_type").expect("get"),
        Some(b"image/png".to_vec())
    );
    fs.removexattr( ino, "user.mime_type").expect("remove");
    assert_eq!(fs.getxattr( ino, "user.mime_type").expect("get"), None);
    let fs = fs;
    destroy_any(fs);
}

#[test]
fn xattr_flags_and_namespace_rules() {
    let fs = Arc::new(mount("p12_xattr_flags", 64));
    let attr = fs.create(1, "f.txt", &mkcreate()).expect("create");
    let ino = attr.ino;

    // XATTR_CREATE then CREATE again -> EEXIST.
    fs.setxattr( ino, "user.a", b"1", crate::ondisk::xattr::XATTR_CREATE).expect("create");
    assert_eq!(
        errno_of(fs.setxattr(
            ino,
            "user.a",
            b"2",
            crate::ondisk::xattr::XATTR_CREATE
        )),
        crate::pal::posix::EEXIST
    );
    // XATTR_REPLACE on a missing name -> ENODATA.
    assert_eq!(
        errno_of(fs.setxattr(
            ino,
            "user.missing",
            b"2",
            crate::ondisk::xattr::XATTR_REPLACE
        )),
        crate::pal::posix::ENODATA
    );
    // Unknown namespace -> EINVAL.
    assert_eq!(
        errno_of(fs.setxattr( ino, "notanamespace.x", b"2", 0)),
        crate::pal::posix::EINVAL
    );
    // Oversize value -> ENOSPC (one block of capacity).
    let big = vec![7u8; crate::ondisk::xattr::XATTR_CAPACITY + 1];
    assert_eq!(
        errno_of(fs.setxattr( ino, "user.big", &big, 0)),
        crate::pal::posix::ENOSPC
    );
    // Missing name on get -> None; on remove -> ENODATA.
    assert_eq!(fs.getxattr( ino, "user.nope").expect("get"), None);
    assert_eq!(
        errno_of(fs.removexattr( ino, "user.nope")),
        crate::pal::posix::ENODATA
    );
    let fs = fs;
    destroy_any(fs);
}

// -- ACLs ------------------------------------------------------------------

fn acl_access_bytes(entries: &[(u16, u16, Option<u32>)]) -> Vec<u8> {
    use crate::security::posix_acl::*;
    let acl = PosixAcl {
        entries: entries
            .iter()
            .map(|(tag, perm, id)| AclEntry { tag: *tag, perm: *perm, id: *id })
            .collect(),
    };
    acl.encode()
}

#[test]
fn acl_access_evaluation_overrides_mode_bits() {
    use crate::security::posix_acl::*;
    let fs = Arc::new(mount("p12_acl_access", 64));
    // Owner root(0):group(0), mode 0o640 -> a "nobody" (uid 9) caller
    // gets nothing from the mode bits.
    let attr = fs
        .create(
            1,
            "secret.txt",
            &VfsCreate { mode: 0o100640, uid: 0, gid: 0 },
        )
        .expect("create");
    let ino = attr.ino;
    assert!(fs.access( ino, 9, 9, 4).is_err(), "mode bits deny uid 9");

    // Grant uid 9 read via a named USER entry.
    let bytes = acl_access_bytes(&[
        (ACL_USER_OBJ, 0o6, None),
        (ACL_USER, 0o4, Some(9)),
        (ACL_GROUP_OBJ, 0o4, None),
        (ACL_MASK, 0o4, None),
        (ACL_OTHER, 0o0, None),
    ]);
    fs.setxattr( ino, ACL_ACCESS_XATTR, &bytes, 0).expect("set acl");

    // ACL evaluation: uid 9 now reads, still cannot write; owner rw.
    assert!(fs.access( ino, 9, 9, 4).is_ok(), "named ACL entry grants read");
    assert!(fs.access( ino, 9, 9, 2).is_err(), "ACL denies write");
    assert!(fs.access( ino, 0, 0, 4 | 2).is_ok(), "owner keeps rw");
    // The mode bits became ACL-derived (group class = GROUP_OBJ & MASK).
    let mode = fs.getattr( ino).expect("getattr").perm;
    assert_eq!(mode & 0o777, 0o640, "mode = owner 6 | (4&4)<<3 | 0");

    let fs = fs;
    destroy_any(fs);
}

#[test]
fn acl_chmod_keeps_named_entries_and_resyncs_mode() {
    use crate::security::posix_acl::*;
    let fs = Arc::new(mount("p12_acl_chmod", 64));
    let attr = fs.create(1, "f.txt", &mkcreate()).expect("create");
    let ino = attr.ino;
    let bytes = acl_access_bytes(&[
        (ACL_USER_OBJ, 0o6, None),
        (ACL_USER, 0o7, Some(42)),
        (ACL_GROUP_OBJ, 0o6, None),
        (ACL_MASK, 0o6, None),
        (ACL_OTHER, 0o0, None),
    ]);
    fs.setxattr( ino, ACL_ACCESS_XATTR, &bytes, 0).expect("set acl");

    // chmod 0o640: MASK becomes 4 (group bits), named entry stays 7.
    fs.setattr( ino, &crate::vfs::VfsSetAttr { mode: Some(0o640), ..Default::default() })
        .expect("chmod");
    let stored = fs.getxattr( ino, ACL_ACCESS_XATTR)
        .expect("get acl")
        .expect("acl present");
    let acl = PosixAcl::decode(&stored).expect("decode");
    assert_eq!(acl.find(ACL_MASK).unwrap().perm, 0o4, "MASK took the mode group bits");
    assert_eq!(acl.named_by_id(ACL_USER, 42).unwrap().perm, 0o7, "named entry survives chmod");
    let mode = fs.getattr( ino).expect("getattr").perm;
    assert_eq!(mode & 0o777, 0o640);
    let fs = fs;
    destroy_any(fs);
}

#[test]
fn acl_default_inheritance_on_mkdir() {
    use crate::security::posix_acl::*;
    let fs = Arc::new(mount("p12_acl_default", 64));
    // Parent dir with a default ACL granting uid 7 rw.
    let root = fs.getattr( 1).expect("root");
    let def = acl_access_bytes(&[
        (ACL_USER_OBJ, 0o7, None),
        (ACL_USER, 0o6, Some(7)),
        (ACL_GROUP_OBJ, 0o5, None),
        (ACL_MASK, 0o6, None),
        (ACL_OTHER, 0o0, None),
    ]);
    fs.setxattr( 1, ACL_DEFAULT_XATTR, &def, 0).expect("set default acl");

    let dir = fs
        .mkdir(
            1,
            "d",
            &VfsCreate { mode: crate::pal::posix::S_IFDIR | 0o750, uid: 0, gid: 0 },
        )
        .expect("mkdir");

    // Child access ACL: uid 7 entry intersected with create mode.
    let child_acl = fs.getxattr( dir.ino, ACL_ACCESS_XATTR)
        .expect("get")
        .expect("inherited access acl present");
    let acl = PosixAcl::decode(&child_acl).expect("decode");
    assert_eq!(acl.named_by_id(ACL_USER, 7).unwrap().perm, 0o6, "named entry inherited");
    // Child default ACL copied (it is a directory).
    assert!(
        fs.getxattr( dir.ino, ACL_DEFAULT_XATTR)
            .expect("get")
            .is_some(),
        "default acl copied to child dir"
    );
    // Mode derived from the inherited ACL: group class =
    // GROUP_OBJ(5 after intersect) & MASK(6) = 4 -> 0o740.
    let mode = fs.getattr( dir.ino).expect("getattr").perm;
    assert_eq!(mode & 0o777, 0o740);
    let _ = root;
    let fs = fs;
    destroy_any(fs);
}

// -- reflink ---------------------------------------------------------------

#[test]
fn reflink_shares_blocks_and_redirects_writes() {
    let fs = Arc::new(mount("p12_reflink", 64));
    let src = fs.create(1, "src.bin", &mkcreate()).expect("create");
    let payload: Vec<u8> = (0..32)
        .flat_map(|b| (0..BLOCK_SIZE).map(move |i| ((b + i) & 0xFF) as u8))
        .collect();
    assert_eq!(payload.len(), 32 * BLOCK_SIZE);
    fs.write(src.ino, 0, &payload).expect("write");
    fs.fsync(src.ino, true).expect("fsync");

    let statfs_before = fs.statfs(1).expect("statfs").free_blocks;

    let dst = fs.create(1, "dst.bin", &mkcreate()).expect("create dst");
    let copied = fs.copy_file_range( src.ino, 0, dst.ino, 0, u64::MAX).expect("reflink");
    assert_eq!(copied, payload.len() as u64);

    // Both read the same bytes.
    assert_eq!(fs.read( dst.ino, 0, payload.len() as u32).expect("read dst"), payload);

    // Space accounting: sharing means ~no new allocation (metadata
    // only; allow a few blocks of tree growth).
    let statfs_after = fs.statfs(1).expect("statfs").free_blocks;
    assert!(
        statfs_before.saturating_sub(statfs_after) < 8,
        "reflink allocated { } blocks; sharing failed",
        statfs_before - statfs_after
    );

    // Overwrite the CLONE's middle block: the pin forces a redirect,
    // so the SOURCE must be untouched.
    let mut new_mid = vec![0xAAu8; BLOCK_SIZE];
    new_mid[0] = 0xBB;
    fs.write(dst.ino, 8 * BLOCK_SIZE as u64, &new_mid).expect("overwrite dst");
    fs.fsync(dst.ino, true).expect("fsync dst");

    let src_expect = payload;
    let got_src = fs.read( src.ino, 8 * BLOCK_SIZE as u64, BLOCK_SIZE as u32).expect("read src");
    assert_eq!(got_src, src_expect[8 * BLOCK_SIZE..9 * BLOCK_SIZE], "source corrupted by clone write");

    let fs = Arc::try_unwrap(fs).ok().expect("sole owner");
    destroy_any(fs);

    // Durability: remount, both sides keep their own views.
    let fs = remount("p12_reflink");
    let got_src = fs.read( src.ino, 8 * BLOCK_SIZE as u64, BLOCK_SIZE as u32).expect("read src");
    assert_eq!(got_src, src_expect[8 * BLOCK_SIZE..9 * BLOCK_SIZE]);
    let got_dst = fs.read( dst.ino, 8 * BLOCK_SIZE as u64, BLOCK_SIZE as u32).expect("read dst");
    assert_eq!(got_dst, new_mid);
    let fs = fs;
    destroy_any(fs);
}

#[test]
fn reflink_registry_record_and_feature_flag_persist() {
    let fs = Arc::new(mount("p12_reflink_reg", 64));
    let src = fs.create(1, "a.bin", &mkcreate()).expect("create");
    fs.write(src.ino, 0, b"hello clone registry").expect("write");
    fs.fsync(src.ino, true).expect("fsync");
    let dst = fs.create(1, "b.bin", &mkcreate()).expect("create");
    fs.copy_file_range( src.ino, 0, dst.ino, 0, u64::MAX).expect("reflink");
    let features = fs.core.sb().fs_features;
    assert_eq!(features & crate::common::version::FS_FEATURE_REFLINK, crate::common::version::FS_FEATURE_REFLINK);
    let clone_root = fs.core.sb().clone_tree_root;
    assert!(clone_root != 0);
    let fs = fs;
    destroy_any(fs);

    // The registry survives remount with the record intact.
    let fs = remount("p12_reflink_reg");
    let sb = fs.core.sb();
    assert_eq!(sb.clone_tree_root, clone_root);
    let mut tx = fs.core.tx_manager.begin(0);
    let mut ctx = crate::transaction::transaction::TxContext::new(&fs.core.disk, &mut tx);
    let tree = crate::btree::tree::BTree::<u64, crate::ondisk::serialization::CloneRecord>::new(
        sb.clone_tree_root,
        crate::fs::clones::CLONE_TREE_NODE_TYPE,
    );
    let rec = tree.lookup(&mut ctx, &dst.ino).expect("lookup").expect("clone record present");
    assert_eq!(rec.source_id, src.ino);
    drop(ctx);
    let fs = fs;
    destroy_any(fs);
}

// -- self-heal scrub on a real RAID5 pool ----------------------------------

/// mkfs over a 3-device RAID5 pool (the tool's sequence, test-sized).
fn mkfs_pool(paths: &[std::path::PathBuf], size_mb: u64) -> Superblock {
    use crate::pool::raid::RaidProfile;
    // The mkfs tool's usable-blocks arithmetic for RAID5 (rounded
    // down to whole stripe rows, conservative margin).
    let per_device = size_mb * 1024 * 1024 / BLOCK_SIZE as u64;
    let chunk = 32u64;
    let raw = per_device * (3 - 1);
    let row_width = chunk * 2;
    let total = ((raw - row_width) / row_width) * row_width;
    let _create_check = Disk::create_pool(paths, size_mb * 1024 * 1024, RaidProfile::Raid5, 32)
        .expect("create pool");
    let bitmap_start = 1u64;
    let inode_count = 1024u64;
    let inode_blocks = inode_count * 256 / BLOCK_SIZE as u64 + 1;
    let inode_table_start = bitmap_start + 1;
    let data_region_start = inode_table_start + inode_blocks;
    let journal_start = data_region_start;
    let journal_blocks = 256u64;
    let data_start = journal_start + journal_blocks;
    let mut sb = Superblock {
        magic: LIONFS_MAGIC,
        version: crate::common::version::CURRENT_VERSION,
        block_size: BLOCK_SIZE as u32,
        total_blocks: total,
        free_blocks: total
            - data_start
            - crate::ondisk::superblock::CANDIDATE_LOCATIONS
                .iter()
                .filter(|&&l| l > 0 && l < total)
                .count() as u64,
        inode_count,
        root_inode: 1,
        flags: 0,
        padding1: 0,
        bitmap_start,
        inode_table_start,
        data_region_start: data_start,
        generation: 1,
        checksum: 0,
        padding_csum: 0,
        journal_start,
        journal_blocks,
        secondary_sb_1: 8192,
        secondary_sb_2: 16384,
        block_group_count: 1,
        blocks_per_group: total as u32,
        inode_tree_root: 12,
        dir_tree_root: 0,
        extent_tree_root: 0,
        freespace_tree_root: 0,
        next_ino: 2,
        checksum_tree_root: 13,
        bad_blocks_root: 14,
        snapshot_tree_root: 18,
        clone_tree_root: 0,
        refcount_tree_root: 0,
        subvolume_tree_root: 0,
        space_map_root: 0,
        last_snapshot_generation: 0,
        dedupe_tree_root: 17,
        key_tree_root: 15,
        fs_features: 0,
        default_compression: 0,
        default_encryption: 0,
        padding_phase7: [0; 6],
        device_tree_root: 0,
        pool_uuid: [0; 16],
        raid_profile: RaidProfile::Raid5 as u8,
        padding_raid: [0; 3],
        chunk_size: 32,
        crypto_tree_root: 16,
        node_generation: 0,
        xattr_tree_root: 0,
        key_envelope_block: 0,
        padding2: [0; BLOCK_SIZE - 336],
    };
    sb.checksum = crate::utils::checksum::calculate_superblock_checksum(&sb);
    let disk = Disk::open_pool(paths, RaidProfile::Raid5, 32).expect("reopen pool");
    disk.write_block(0, bytemuck::bytes_of(&sb)).expect("write sb");
    // Secondary slots (the real mkfs sequence): on RAID5 the primary
    // slot shares device-0 physical block 0 with row-0 parity, so
    // metadata writes clobber it and the SECONDARIES are what mount
    // reads back. Reserve + write both.
    disk.write_block(8192, bytemuck::bytes_of(&sb)).expect("write sb slot 1");
    if total > 16384 {
        disk.write_block(16384, bytemuck::bytes_of(&sb)).expect("write sb slot 2");
    }

    // Bitmap: metadata + journal + slots used.
    let mut bitmap = [0u8; BLOCK_SIZE];
    for i in 0..data_start {
        bitmap[(i / 8) as usize] |= 1 << (i % 8);
    }
    for &slot in crate::ondisk::superblock::CANDIDATE_LOCATIONS.iter() {
        if slot > 0 && slot < total {
            bitmap[(slot / 8) as usize] |= 1 << (slot % 8);
        }
    }
    disk.write_block(bitmap_start, &bitmap).expect("bitmap");
    for i in 0..inode_blocks {
        disk.write_block(inode_table_start + i, &[0; BLOCK_SIZE]).expect("inode table");
    }

    // Root inode + tree init (the mkfs sequence).
    let tm = crate::transaction::manager::TransactionManager::new(&sb);
    let mut tx = tm.begin(0);
    {
        let mut ctx = crate::transaction::transaction::TxContext::new(&disk, &mut tx);
        let root = crate::ondisk::serialization::Inode::new_dir(1, 0o755, 0, 0, 0);
        crate::btree::tree::BTree::<u64, crate::ondisk::serialization::Inode>::init_empty(
            &mut ctx, 12, crate::inode::tree::INODE_TREE_NODE_TYPE,
        )
        .unwrap();
        let mut tree =
            crate::btree::tree::BTree::<u64, crate::ondisk::serialization::Inode>::new(
                12,
                crate::inode::tree::INODE_TREE_NODE_TYPE,
            );
        let mut alloc = |c: &mut crate::transaction::transaction::TxContext| {
            crate::allocator::bitmap::Allocator::allocate_extents_meta(
                c,
                &crate::ondisk::serialization::BlockGroupDescriptor {
                    bg_block_bitmap: bitmap_start,
                    bg_inode_bitmap: 0,
                    bg_inode_table: inode_table_start,
                    bg_free_blocks_count: 0,
                    bg_free_inodes_count: 0,
                    bg_used_dirs_count: 0,
                    bg_padding: 0,
                    bg_reserved: [0; 32],
                },
                total as u32,
                1,
            )
        };
        tree.insert(&mut ctx, 1, root, &mut alloc).unwrap();
        crate::integrity::checksum_tree::ChecksumTree::init_empty(&mut ctx, 13).unwrap();
        crate::integrity::bad_blocks::BadBlockManager::init_empty(&mut ctx, 14).unwrap();
        crate::fs::snapshots::SnapshotManager::init_empty(&mut ctx, 18).unwrap();
    }
    tm.commit(&disk, &sb, &tx).unwrap();
    disk.sync().unwrap();
    sb
}

use crate::disk::block_io::Disk;

#[test]
fn scrub_heals_raid5_bitrot_and_verifies() {
    let dir = std::env::temp_dir();
    let paths: Vec<std::path::PathBuf> = (0..3)
        .map(|i| dir.join(format!("test_p12_raid5_dev{i}.img")))
        .collect();
    for p in &paths {
        let _ = std::fs::remove_file(p);
    }
    let sb_template = mkfs_pool(&paths, 32);

    // Write a file with a known 3-block payload through a real mount.
    let payload: Vec<u8> = (0..3)
        .flat_map(|b| (0..BLOCK_SIZE).map(move |i| ((b * 31 + i * 7) & 0xFF) as u8))
        .collect();
    let disk = Disk::open_pool(&paths, crate::pool::raid::RaidProfile::Raid5, 32)
        .expect("open pool");
    let fs = LionFS::new(disk, paths[0].display().to_string()).expect("mount");
    assert_eq!(fs.core.sb().raid_profile, sb_template.raid_profile);
    let file_ino = fs.create( 1, "data.bin", &mkcreate()).expect("create").ino;
    fs.write( file_ino, 0, &payload).expect("write");
    fs.fsync( file_ino, true).expect("fsync");

    // Keep the mount LIVE: a clean remount would replay the journal
    // (the WAL doing its job) and restore the pre-corruption bytes
    // before any sweep could see them. Bit-rot is injected under the
    // live mount through a raw device write instead.
    let phys_lba = fs.core.with_scratch_ctx(|ctx, sb| {
        let inode = crate::inode::manager::InodeManager::read_inode(ctx, sb.inode_tree_root, file_ino)?;
        crate::file::writer::FileManager::resolve_physical_block(ctx, &inode, 0)
    })
    .expect("resolve block 0");
    {
        let corrupt_handle =
            Disk::open_pool(&paths, crate::pool::raid::RaidProfile::Raid5, 32).unwrap();
        let layout = corrupt_handle.raid_engine.layout(phys_lba);
        let dev = layout.data_devs[0];
        let mut buf = [0u8; BLOCK_SIZE];
        corrupt_handle
            .read_block_direct(dev, layout.phys_block, &mut buf)
            .unwrap();
        buf[17] ^= 0x80; // flip one bit
        corrupt_handle
            .write_block_direct(dev, layout.phys_block, &buf)
            .unwrap();
    }

    // The application read now REFUSES (checksum mismatch): bit-rot is
    // live and detected end to end.
    assert!(
        fs.read( file_ino, 0, payload.len() as u32).is_err(),
        "corrupted block must fail verification before the scrub"
    );

    // One synchronous sweep: verify, reconstruct from parity, rewrite.
    let report = crate::worker::scrubber::scrub_sweep_rate(&fs.core, 0);
    assert!(report.blocks_scanned >= 3, "sweep scanned the file's blocks");
    assert_eq!(report.errors_found, 1, "exactly one corrupt block found");
    assert_eq!(report.errors_repaired, 1, "the corrupt block was repaired");
    assert_eq!(report.blocks_lost, 0);

    // The application now reads its original bytes back.
    let got = fs.read( file_ino, 0, payload.len() as u32).expect("read healed");
    assert_eq!(got, payload, "post-heal bytes equal the original payload");
    destroy_any(fs);
}

fn read_pool_sb(disk: &Disk) -> Superblock {
    // Same discovery rule as the mount path: best VALID slot wins
    // (on RAID5 the primary at physical 0 shares its slot with row-0
    // parity and is clobbered by the first metadata writes).
    let mut buf = [0u8; BLOCK_SIZE];
    let mut best: Option<Superblock> = None;
    for &slot in crate::ondisk::superblock::CANDIDATE_LOCATIONS.iter() {
        if disk.read_block(slot, &mut buf).is_ok() {
            if let Some(sb) = crate::ondisk::superblock::is_valid_superblock_block(&buf) {
                best = match best {
                    Some(b) if b.generation >= sb.generation => Some(b),
                    _ => Some(sb),
                };
            }
        }
    }
    best.expect("no valid superblock slot")
}

use crate::fs::filesystem::LionFS;

// -- Format Vault conformance ---------------------------------------------

#[test]
fn conformance_battery_passes_on_a_populated_image() {
    let fs = Arc::new(mount("p12_conformance", 64));
    let a = fs.create(1, "a.txt", &mkcreate()).expect("create");
    fs.write(a.ino, 0, b"conformance corpus").expect("write");
    let b = fs.create(1, "b.bin", &mkcreate()).expect("create");
    fs.write(b.ino, 0, &vec![5u8; 2 * BLOCK_SIZE]).expect("write");
    fs.fsync( b.ino, true).expect("fsync");
    fs.setxattr( a.ino, "user.tag", b"keeper", 0).expect("xattr");
    let c = fs.create(1, "c.bin", &mkcreate()).expect("create");
    fs.copy_file_range( b.ino, 0, c.ino, 0, u64::MAX).expect("reflink");
    let fs = fs;
    destroy_any(fs);

    // Offline snapshot for the registry check.
    {
        let disk = Disk::open(&test_path("p12_conformance")).unwrap();
        let mut sb = read_pool_sb(&disk);
        disk.live_barrier.fetch_max(sb.last_snapshot_generation, std::sync::atomic::Ordering::AcqRel);
        let tm = crate::transaction::manager::TransactionManager::new(&sb);
        let mut tx = tm.begin(0);
        let bg = crate::ondisk::serialization::BlockGroupDescriptor {
            bg_block_bitmap: sb.bitmap_start,
            bg_inode_bitmap: 0,
            bg_inode_table: sb.inode_table_start,
            bg_free_blocks_count: 0,
            bg_free_inodes_count: 0,
            bg_used_dirs_count: 0,
            bg_padding: 0,
            bg_reserved: [0; 32],
        };
        let mut snap = crate::fs::snapshots::SnapshotManager::new(sb.snapshot_tree_root);
        let tx_id = tx.id;
        let bpg = sb.blocks_per_group;
        let mut ctx = crate::transaction::transaction::TxContext::new(&disk, &mut tx);
        snap.create_snapshot(&mut ctx, &mut sb, 77, 0, &mut |c| {
            crate::allocator::bitmap::Allocator::allocate_extents_meta(c, &bg, bpg, 1)
        })
        .unwrap();
        drop(ctx);
        tm.commit(&disk, &sb, &tx).unwrap();
        if tx.alloc_delta != 0 {
            sb.free_blocks = (sb.free_blocks as i64 - tx.alloc_delta).max(0) as u64;
        }
        sb.node_generation = crate::btree::tree::node_gen_current();
        crate::ondisk::superblock::write_all_slots(&disk, &sb, tx_id).unwrap();
    }

    let disk = Disk::open(&test_path("p12_conformance")).unwrap();
    let sb = read_pool_sb(&disk);
    let report = crate::ondisk::conformance::run(&disk, &sb, 64);
    assert!(report.all_passed(), "conformance failures: {:?}", report.failed());
    assert!(report.checks.len() >= 10, "the battery ran its checks");
}

#[test]
fn mount_gate_refuses_unknown_feature_bits() {
    let fs = Arc::new(mount("p12_gate", 64));
    let fs = fs;
    destroy_any(fs);

    // Set an UNKNOWN bit directly in every superblock slot.
    let path = test_path("p12_gate");
    let disk = Disk::open(&path).unwrap();
    let mut sb = read_pool_sb(&disk);
    sb.fs_features |= 1 << 40;
    sb.checksum = crate::utils::checksum::calculate_superblock_checksum(&sb);
    for &slot in crate::ondisk::superblock::CANDIDATE_LOCATIONS.iter() {
        if slot < sb.total_blocks {
            disk.write_block(slot, bytemuck::bytes_of(&sb)).unwrap();
        }
    }
    drop(disk);

    // The core gate refuses (format vault contract).
    let disk = Disk::open(&path).unwrap();
    let err = LionFS::new(disk, path.display().to_string()).err().expect("mount refused");
    assert!(err.to_string().contains("unknown feature bits"), "gate message: {err}");
}

// -- send / recv -----------------------------------------------------------

#[test]
fn send_recv_roundtrip_recreates_the_tree_on_a_fresh_image() {
    // Source: populate + snapshot offline (the tool flow).
    let src_tag = "p12_send_src";
    {
        let fs = Arc::new(mount(src_tag, 64));
        let docs = fs
            .mkdir(
                1,
                "docs",
                &VfsCreate { mode: crate::pal::posix::S_IFDIR | 0o755, uid: 0, gid: 0 },
            )
            .expect("mkdir");
        let f1 = fs.create(1, "readme.md", &mkcreate()).expect("create");
        fs.write(f1.ino, 0, b"# LionFS send/recv roundtrip").expect("write");
        let f2 = fs.create(docs.ino, "spec.bin", &mkcreate()).expect("create");
        fs.write(f2.ino, 0, &vec![9u8; BLOCK_SIZE + 1000]).expect("write");
        fs.fsync( f1.ino, true).expect("fsync");
        fs.fsync( f2.ino, true).expect("fsync");
        let fs = fs;
        destroy_any(fs);
    }
    {
        let disk = Disk::open(&test_path(src_tag)).unwrap();
        let mut sb = read_pool_sb(&disk);
        disk.live_barrier
            .fetch_max(sb.last_snapshot_generation, std::sync::atomic::Ordering::AcqRel);
        let tm = crate::transaction::manager::TransactionManager::new(&sb);
        let mut tx = tm.begin(0);
        let bg = crate::ondisk::serialization::BlockGroupDescriptor {
            bg_block_bitmap: sb.bitmap_start,
            bg_inode_bitmap: 0,
            bg_inode_table: sb.inode_table_start,
            bg_free_blocks_count: 0,
            bg_free_inodes_count: 0,
            bg_used_dirs_count: 0,
            bg_padding: 0,
            bg_reserved: [0; 32],
        };
        let mut snap = crate::fs::snapshots::SnapshotManager::new(sb.snapshot_tree_root);
        let tx_id = tx.id;
        let bpg = sb.blocks_per_group;
        let mut ctx = crate::transaction::transaction::TxContext::new(&disk, &mut tx);
        snap.create_snapshot(&mut ctx, &mut sb, 5, 0, &mut |c| {
            crate::allocator::bitmap::Allocator::allocate_extents_meta(c, &bg, bpg, 1)
        })
        .unwrap();
        drop(ctx);
        tm.commit(&disk, &sb, &tx).unwrap();
        if tx.alloc_delta != 0 {
            sb.free_blocks = (sb.free_blocks as i64 - tx.alloc_delta).max(0) as u64;
        }
        sb.node_generation = crate::btree::tree::node_gen_current();
        crate::ondisk::superblock::write_all_slots(&disk, &sb, tx_id).unwrap();
    }

    // send -> stream file.
    let stream_path = std::env::temp_dir().join("test_p12_stream.bin");
    {
        let disk = Disk::open(&test_path(src_tag)).unwrap();
        let sb = read_pool_sb(&disk);
        let file = std::fs::File::create(&stream_path).unwrap();
        let mut writer = std::io::BufWriter::new(file);
        let mut stream = crate::fs::replication::SendStream::open(&disk, &sb, 5, &mut writer)
            .expect("open stream");
        let count = stream.send_all().expect("send");
        assert_eq!(count, 2, "two files serialized");
        writer.flush().unwrap();
    }

    // recv into a FRESH image.
    let dst_tag = "p12_recv_dst";
    {
        let _ = std::fs::remove_file(test_path(dst_tag));
        let _ = parallel_mkfs(&test_path(dst_tag), 64);
        let fs = Arc::new(mount(dst_tag, 64));
        let sb = fs.core.sb();
        let mut reader = std::io::BufReader::new(
            std::fs::File::open(&stream_path).expect("open stream"),
        );
        let summary = crate::fs::replication::recv_stream(
            &mut reader,
            &*fs as &dyn crate::vfs::VfsOps,
            &sb,
            &fs.core.disk,
            true,
        )
        .expect("recv");
        assert_eq!(summary.files, 2);
        assert_eq!(summary.dirs, 1);
        assert_eq!(summary.snapshot_recorded, Some(5));
        // Byte-for-byte verification through the target's read path.
        let rm = fs.lookup( 1, "readme.md").expect("readme present");
        assert_eq!(
            fs.read( rm.ino, 0, 64).expect("read"),
            b"# LionFS send/recv roundtrip".to_vec()
        );
        let spec_ino = {
            let docs = fs.lookup( 1, "docs").expect("docs");
            fs.lookup( docs.ino, "spec.bin").expect("spec").ino
        };
        let spec = fs.read( spec_ino, 0, (BLOCK_SIZE + 1000) as u32).expect("read");
        assert_eq!(spec.len(), BLOCK_SIZE + 1000);
        assert!(spec.iter().all(|&b| b == 9));
        let fs = fs;
        destroy_any(fs);
    }

    // The recorded snapshot verifies its own frozen view.
    let fs = remount(dst_tag);
    let sb = fs.core.sb();
    let mut tx = fs.core.tx_manager.begin(0);
    let mut ctx = crate::transaction::transaction::TxContext::new(&fs.core.disk, &mut tx);
    let snap = crate::fs::snapshots::SnapshotManager::new(sb.snapshot_tree_root);
    let rec = snap.get_snapshot(&mut ctx, 5).expect("get").expect("recorded");
    assert!(rec.generation > 0);
    drop(ctx);
    let fs = fs;
    destroy_any(fs);
}

fn parallel_mkfs(path: &std::path::Path, size_mb: u64) -> std::io::Result<()> {
    // Same sequence as parallel_tests::mkfs_image, exposed for the
    // recv fixture.
    let _ = std::fs::remove_file(path);
    let disk = Disk::create(path, size_mb * 1024 * 1024)?;
    let total_blocks = size_mb * 1024 * 1024 / BLOCK_SIZE as u64;
    let inode_count = 1024u64;
    let inode_blocks = inode_count * 256 / BLOCK_SIZE as u64 + 1;
    let bitmap_start = 1u64;
    let inode_table_start = bitmap_start + 1;
    let data_region_start = inode_table_start + inode_blocks;
    let journal_start = data_region_start;
    let journal_blocks = 256u64;
    let data_start = journal_start + journal_blocks;
    let mut sb = Superblock {
        magic: LIONFS_MAGIC,
        version: crate::common::version::CURRENT_VERSION,
        block_size: BLOCK_SIZE as u32,
        total_blocks,
        free_blocks: total_blocks
            - data_start
            - crate::ondisk::superblock::CANDIDATE_LOCATIONS
                .iter()
                .filter(|&&l| l > 0 && l < total_blocks)
                .count() as u64,
        inode_count,
        root_inode: 1,
        flags: 0,
        padding1: 0,
        bitmap_start,
        inode_table_start,
        data_region_start: data_start,
        generation: 1,
        checksum: 0,
        padding_csum: 0,
        journal_start,
        journal_blocks,
        secondary_sb_1: 0,
        secondary_sb_2: 0,
        block_group_count: 1,
        blocks_per_group: total_blocks as u32,
        inode_tree_root: 12,
        dir_tree_root: 0,
        extent_tree_root: 0,
        freespace_tree_root: 0,
        next_ino: 2,
        checksum_tree_root: 13,
        bad_blocks_root: 14,
        snapshot_tree_root: 18,
        clone_tree_root: 0,
        refcount_tree_root: 0,
        subvolume_tree_root: 0,
        space_map_root: 0,
        last_snapshot_generation: 0,
        dedupe_tree_root: 17,
        key_tree_root: 15,
        fs_features: 0,
        default_compression: 0,
        default_encryption: 0,
        padding_phase7: [0; 6],
        device_tree_root: 0,
        pool_uuid: [0; 16],
        raid_profile: 0,
        padding_raid: [0; 3],
        chunk_size: 0,
        crypto_tree_root: 16,
        node_generation: 0,
        xattr_tree_root: 0,
        key_envelope_block: 0,
        padding2: [0; BLOCK_SIZE - 336],
    };
    sb.checksum = crate::utils::checksum::calculate_superblock_checksum(&sb);
    disk.write_block(0, bytemuck::bytes_of(&sb))?;
    let mut bitmap = [0u8; BLOCK_SIZE];
    for i in 0..data_start {
        bitmap[(i / 8) as usize] |= 1 << (i % 8);
    }
    for &slot in crate::ondisk::superblock::CANDIDATE_LOCATIONS.iter() {
        if slot > 0 && slot < total_blocks {
            bitmap[(slot / 8) as usize] |= 1 << (slot % 8);
        }
    }
    disk.write_block(bitmap_start, &bitmap)?;
    for i in 0..inode_blocks {
        disk.write_block(inode_table_start + i, &[0; BLOCK_SIZE])?;
    }
    let tm = crate::transaction::manager::TransactionManager::new(&sb);
    let mut tx = tm.begin(0);
    {
        let mut ctx = crate::transaction::transaction::TxContext::new(&disk, &mut tx);
        let root = crate::ondisk::serialization::Inode::new_dir(1, 0o755, 0, 0, 0);
        crate::btree::tree::BTree::<u64, crate::ondisk::serialization::Inode>::init_empty(
            &mut ctx, 12, crate::inode::tree::INODE_TREE_NODE_TYPE,
        )?;
        let mut tree =
            crate::btree::tree::BTree::<u64, crate::ondisk::serialization::Inode>::new(
                12,
                crate::inode::tree::INODE_TREE_NODE_TYPE,
            );
        let bg = crate::ondisk::serialization::BlockGroupDescriptor {
            bg_block_bitmap: bitmap_start,
            bg_inode_bitmap: 0,
            bg_inode_table: inode_table_start,
            bg_free_blocks_count: 0,
            bg_free_inodes_count: 0,
            bg_used_dirs_count: 0,
            bg_padding: 0,
            bg_reserved: [0; 32],
        };
        let mut alloc = |c: &mut crate::transaction::transaction::TxContext| {
            crate::allocator::bitmap::Allocator::allocate_extents_meta(
                c,
                &bg,
                total_blocks as u32,
                1,
            )
        };
        tree.insert(&mut ctx, 1, root, &mut alloc)?;
        crate::integrity::checksum_tree::ChecksumTree::init_empty(&mut ctx, 13)?;
        crate::integrity::bad_blocks::BadBlockManager::init_empty(&mut ctx, 14)?;
        crate::fs::snapshots::SnapshotManager::init_empty(&mut ctx, 18)?;
    }
    tm.commit(&disk, &sb, &tx)?;
    disk.sync()?;
    Ok(())
}
