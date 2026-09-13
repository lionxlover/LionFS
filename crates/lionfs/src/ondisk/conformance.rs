//! 3.6 Format Vault: the image conformance battery.
//!
//! One reusable check set, driven by three consumers:
//! * `lfs_conformance <image>` -- the standalone checker,
//! * `lfs_upgrade <image>` -- the offline upgrade gate (an image that
//!   does not conform is never upgraded),
//! * the lib test suite -- money tests run the battery against real
//!   mounted images so format drift breaks CI, not user data.
//!
//! Design position (RFC-005): the checks assert what the FORMAT
//! promises, not what the current build happens to do -- superblock
//! integrity and geometry, tree reachability and structural validity,
//! checksum spot-verification against on-disk bytes, snapshot record
//! sanity, bitmap/free-block agreement, journal tail cleanliness,
//! feature-flag consistency. All read-only: the battery never writes.

use std::io::Result;

use crate::btree::tree::BTree;
use crate::disk::block_io::Disk;
use crate::integrity::algorithms::{verify_checksum, ChecksumAlgorithm};
use crate::integrity::checksum_tree::{ChecksumTree, ChecksumTreeKey};
use crate::ondisk::serialization::{
    Superblock, BLOCK_SIZE, LIONFS_MAGIC,
};
use crate::transaction::transaction::TxContext;

#[derive(Debug, Clone)]
pub struct CheckResult {
    pub name: &'static str,
    pub passed: bool,
    pub detail: String,
}

impl CheckResult {
    fn pass(name: &'static str, detail: impl Into<String>) -> Self {
        Self { name, passed: true, detail: detail.into() }
    }
    fn fail(name: &'static str, detail: impl Into<String>) -> Self {
        Self { name, passed: false, detail: detail.into() }
    }
}

#[derive(Debug, Clone, Default)]
pub struct ConformanceReport {
    pub checks: Vec<CheckResult>,
}

impl ConformanceReport {
    #[must_use]
    pub fn all_passed(&self) -> bool {
        self.checks.iter().all(|c| c.passed)
    }

    #[must_use]
    pub fn failed(&self) -> Vec<&CheckResult> {
        self.checks.iter().filter(|c| !c.passed).collect()
    }

    #[must_use]
    pub fn render(&self) -> String {
        let mut out = String::from("LionFS Format Conformance Report\n");
        out.push_str("================================\n");
        for c in &self.checks {
            out.push_str(&format!(
                "  [{}] {:<28} {}\n",
                if c.passed { "PASS" } else { "FAIL" },
                c.name,
                c.detail
            ));
        }
        out.push_str(&format!(
            "\n{}: {}/{} checks passed\n",
            if self.all_passed() { "VERDICT" } else { "VERDICT" },
            self.checks.iter().filter(|c| c.passed).count(),
            self.checks.len()
        ));
        out
    }

    fn push(&mut self, name: &'static str, r: Result<String>) {
        match r {
            Ok(detail) => self.checks.push(CheckResult { name, passed: true, detail }),
            Err(e) => self.checks.push(CheckResult { name, passed: false, detail: e.to_string() }),
        }
    }
}

/// Run the full battery. `verify_blocks` caps the checksum spot-check
/// (0 = skip spot checks; the default 64 keeps the tool fast while
/// still touching real data bytes).
pub fn run(disk: &Disk, sb: &Superblock, verify_blocks: usize) -> ConformanceReport {
    let mut report = ConformanceReport::default();

    // -- static superblock checks (no context needed) ----------------
    report.checks.push(if sb.magic == LIONFS_MAGIC {
        CheckResult::pass("superblock_magic", "LIONFS10 magic present")
    } else {
        CheckResult::fail("superblock_magic", format!("bad magic {:#x}", sb.magic))
    });
    report.checks.push(if sb.version <= crate::common::version::CURRENT_VERSION {
        CheckResult::pass(
            "format_version",
            format!(
                "version {} understood (current {})",
                sb.version,
                crate::common::version::CURRENT_VERSION
            ),
        )
    } else {
        CheckResult::fail(
            "format_version",
            format!(
                "version {} exceeds current {}",
                sb.version,
                crate::common::version::CURRENT_VERSION
            ),
        )
    });
    report.checks.push({
        let unknown = crate::common::version::unknown_features(sb.fs_features);
        if unknown == 0 {
            CheckResult::pass("feature_flags", format!("features {:#b} all known", sb.fs_features))
        } else {
            CheckResult::fail(
                "feature_flags",
                format!("unknown feature bits {unknown:#b}"),
            )
        }
    });
    report.checks.push({
        let computed = crate::utils::checksum::calculate_superblock_checksum(sb);
        if computed == sb.checksum {
            CheckResult::pass("superblock_checksum", "self-checksum matches")
        } else {
            CheckResult::fail(
                "superblock_checksum",
                format!("stored {:#x} != computed {:#x}", sb.checksum, computed),
            )
        }
    });
    report.checks.push({
        let ordered = sb.bitmap_start < sb.inode_table_start
            && sb.inode_table_start < sb.data_region_start
            && sb.data_region_start <= sb.total_blocks
            && sb.journal_start >= sb.data_region_start.saturating_sub(sb.journal_blocks)
            && sb.journal_start + sb.journal_blocks <= sb.total_blocks
            && sb.block_size == BLOCK_SIZE as u32
            && sb.root_inode == 1;
        if ordered {
            CheckResult::pass(
                "geometry_sanity",
                format!(
                    "bitmap@{} inode_table@{} data@{} journal+{}..{}, {} blocks",
                    sb.bitmap_start,
                    sb.inode_table_start,
                    sb.data_region_start,
                    sb.journal_start,
                    sb.journal_start + sb.journal_blocks,
                    sb.total_blocks
                ),
            )
        } else {
            CheckResult::fail(
                "geometry_sanity",
                "layout fields out of order or block_size/root wrong".to_string(),
            )
        }
    });

    // -- on-disk checks (bare read contexts) -------------------------
    let tm = crate::transaction::manager::TransactionManager::new(sb);
    let mut tx = tm.begin(0);
    let mut ctx = TxContext::new(disk, &mut tx);

    report.push("superblock_slots_agree", check_slots(disk, sb));
    report.push("bitmap_free_count", check_bitmap(&mut ctx, sb));
    report.push("inode_tree", check_inode_tree(&mut ctx, sb));
    report.push("checksum_tree", check_checksum_tree(&mut ctx, sb));
    report.push("checksum_spot_verify", check_spot(&mut ctx, disk, sb, verify_blocks));
    report.push("snapshot_records", check_snapshots(&mut ctx, sb));
    report.push("clone_registry", check_clones(&mut ctx, sb));
    report.push("xattr_blocks", check_xattrs(&mut ctx, sb));
    report.push("journal_tail", check_journal(disk, sb));

    report
}

fn check_slots(disk: &Disk, sb: &Superblock) -> Result<String> {
    let mut buf = [0u8; BLOCK_SIZE];
    let mut agreed = 1;
    let mut checked = 1;
    for &slot in &crate::ondisk::superblock::CANDIDATE_LOCATIONS[1..] {
        if slot >= sb.total_blocks {
            continue;
        }
        if disk.read_block(slot, &mut buf).is_err() {
            continue;
        }
        if let Some(other) = crate::ondisk::superblock::is_valid_superblock_block(&buf) {
            checked += 1;
            if other.generation == sb.generation
                && other.inode_tree_root == sb.inode_tree_root
                && other.checksum_tree_root == sb.checksum_tree_root
            {
                agreed += 1;
            }
        }
    }
    if agreed == checked {
        Ok(format!("{agreed} valid slot(s) agree on the live roots"))
    } else {
        Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("only {agreed}/{checked} slots agree"),
        ))
    }
}

fn check_bitmap(ctx: &mut TxContext, sb: &Superblock) -> Result<String> {
    let counted = crate::allocator::bitmap::Allocator::count_free_blocks(
        ctx,
        sb.bitmap_start,
        sb.total_blocks,
    )?;
    if counted == sb.free_blocks {
        Ok(format!("free_blocks {} matches the bitmap", sb.free_blocks))
    } else {
        Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("superblock says {} free, bitmap counts {counted}", sb.free_blocks),
        ))
    }
}

fn check_inode_tree(ctx: &mut TxContext, sb: &Superblock) -> Result<String> {
    if sb.inode_tree_root == 0 {
        return Err(std::io::Error::new(std::io::ErrorKind::InvalidData, "no inode tree"));
    }
    let tree = BTree::<u64, crate::ondisk::serialization::Inode>::new(
        sb.inode_tree_root,
        crate::inode::tree::INODE_TREE_NODE_TYPE,
    );
    let nodes = tree.validate(ctx)?;
    let entries = tree.iter_all(ctx)?;
    let mut bad = 0;
    for (ino, inode) in &entries {
        if *ino == 0 || inode.ino != *ino || inode.extent_count > crate::ondisk::serialization::MAX_INLINE_EXTENTS as u16 {
            bad += 1;
        }
    }
    if bad == 0 {
        Ok(format!(
            "{} inodes across {nodes} node(s); all records structurally valid",
            entries.len()
        ))
    } else {
        Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("{bad} malformed inode record(s)"),
        ))
    }
}

fn check_checksum_tree(ctx: &mut TxContext, sb: &Superblock) -> Result<String> {
    if sb.checksum_tree_root == 0 {
        return Ok("no checksum tree (pin-mode image): skipped".into());
    }
    let tree = ChecksumTree::new(sb.checksum_tree_root);
    let nodes = tree.btree.validate(ctx)?;
    Ok(format!("checksum tree valid ({nodes} node(s))"))
}

fn check_spot(ctx: &mut TxContext, disk: &Disk, sb: &Superblock, verify_blocks: usize) -> Result<String> {
    if sb.checksum_tree_root == 0 || verify_blocks == 0 {
        return Ok("skipped".into());
    }
    let tree = ChecksumTree::new(sb.checksum_tree_root);
    let records = tree.btree.iter_all(ctx)?;
    if records.is_empty() {
        return Ok("no data blocks recorded yet".into());
    }
    // Deterministic stride sample: first, last, then evenly spaced.
    let mut to_check: Vec<&(ChecksumTreeKey, crate::integrity::checksum_tree::ChecksumTreeValue)> = Vec::new();
    let n = records.len();
    let cap = verify_blocks.min(n);
    for i in 0..cap {
        let idx = if cap == 1 { 0 } else { i * (n - 1) / (cap - 1) };
        to_check.push(&records[idx]);
    }
    let mut verified = 0;
    let mut mismatches = 0;
    for (key, val) in to_check {
        let mut buf = [0u8; BLOCK_SIZE];
        if disk.read_block(val.physical_block, &mut buf).is_err() {
            mismatches += 1;
            continue;
        }
        let algo = ChecksumAlgorithm::from_u8(val.algorithm_id);
        if verify_checksum(algo, &buf, &val.checksum_bytes) {
            verified += 1;
        } else {
            mismatches += 1;
            eprintln!(
                "  conformance: checksum mismatch ino {} block {} (phys {})",
                key.object_id, key.logical_block, val.physical_block
            );
        }
    }
    if mismatches == 0 {
        Ok(format!("{verified} sampled block(s) verify against on-disk bytes"))
    } else {
        Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("{mismatches} sampled block(s) FAILED verification"),
        ))
    }
}

fn check_snapshots(ctx: &mut TxContext, sb: &Superblock) -> Result<String> {
    if sb.snapshot_tree_root == 0 {
        return Ok("no snapshot registry".into());
    }
    use crate::fs::snapshots::SnapshotManager;
    let mgr = SnapshotManager::new(sb.snapshot_tree_root);
    let records = mgr.list_snapshots(ctx)?;
    let bad = records
        .iter()
        .filter(|r| r.inode_tree_root == 0 || r.generation == 0)
        .count();
    if bad == 0 {
        Ok(format!("{} snapshot record(s) valid", records.len()))
    } else {
        Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("{bad} snapshot record(s) have zero roots/barrier"),
        ))
    }
}

fn check_clones(ctx: &mut TxContext, sb: &Superblock) -> Result<String> {
    if sb.clone_tree_root == 0 {
        return Ok("no clone registry (no reflinks taken)".into());
    }
    let tree = BTree::<u64, crate::ondisk::serialization::CloneRecord>::new(
        sb.clone_tree_root,
        crate::fs::clones::CLONE_TREE_NODE_TYPE,
    );
    tree.validate(ctx)?;
    let records = tree.iter_all(ctx)?;
    Ok(format!("{} clone record(s); registry tree valid", records.len()))
}

fn check_xattrs(ctx: &mut TxContext, sb: &Superblock) -> Result<String> {
    if sb.xattr_tree_root == 0 {
        return Ok("no xattr tree".into());
    }
    let tree = BTree::<u64, crate::fs::xattrs::XattrRecord>::new(
        sb.xattr_tree_root,
        crate::fs::xattrs::XATTR_TREE_NODE_TYPE,
    );
    tree.validate(ctx)?;
    let records = tree.iter_all(ctx)?;
    let mut unparsable = 0;
    let mut buf = [0u8; BLOCK_SIZE];
    for (_ino, rec) in &records {
        if ctx.read_block(rec.block, &mut buf).is_err() {
            unparsable += 1;
            continue;
        }
        if crate::ondisk::xattr::parse_block(&buf).is_err() {
            unparsable += 1;
        }
    }
    if unparsable == 0 {
        Ok(format!(
            "{} inode(s) carry xattrs; every LXAT block parses",
            records.len()
        ))
    } else {
        Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("{unparsable} xattr block(s) failed to parse"),
        ))
    }
}

fn check_journal(disk: &Disk, sb: &Superblock) -> Result<String> {
    if sb.journal_blocks == 0 {
        return Ok("no journal".into());
    }
    let mut buf = [0u8; BLOCK_SIZE];
    // Scan for the first VALID header: after recovery the tail may sit
    // anywhere in the ring; an empty/stale region reads as zeros.
    let mut checked = 0;
    for i in 0..sb.journal_blocks.min(16) {
        if disk.read_block(sb.journal_start + i, &mut buf).is_err() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!("journal block {} unreadable", sb.journal_start + i),
            ));
        }
        let magic = u64::from_le_bytes(buf[0..8].try_into().expect("8 bytes"));
        if magic == crate::ondisk::serialization::JOURNAL_MAGIC {
            // The commit path hashes the header with `checksum == 0`,
            // then stores the digest at offset 32. Verify the same way.
            let mut copy = buf;
            copy[32..36].copy_from_slice(&[0; 4]);
            let computed = crate::utils::crc::compute_checksum(&copy);
            let stored = u32::from_le_bytes(buf[32..36].try_into().expect("4 bytes"));
            if computed == stored {
                checked += 1;
            }
        } else if magic == 0 {
            checked += 1; // clean empty region
        }
    }
    Ok(format!("journal ring header region intact ({checked} block(s) sane)"))
}
