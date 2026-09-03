//! Post-recovery structural verification: after
//! `recovery::recovery::RecoveryManager::recover` has replayed the
//! journal (or found nothing to replay), confirm the filesystem is
//! actually in a mountable state before `LionFS::new` hands it to FUSE --
//! catching the case where recovery "succeeded" (no I/O errors) but the
//! result still isn't structurally sound.

use crate::disk::block_io::Disk;
use crate::ondisk::serialization::Superblock;
use crate::ondisk::validation::validate_superblock;
use crate::transaction::manager::TransactionManager;
use crate::transaction::transaction::TxContext;

#[derive(Debug, Clone)]
pub struct RecoveryVerification {
    pub superblock_ok: bool,
    pub root_inode_readable: bool,
    pub issues: Vec<String>,
}

impl RecoveryVerification {
    pub fn is_healthy(&self) -> bool {
        self.superblock_ok && self.root_inode_readable
    }
}

pub fn verify_post_recovery(disk: &Disk, sb: &Superblock) -> RecoveryVerification {
    let mut issues = Vec::new();

    let sb_report = validate_superblock(sb);
    let superblock_ok = sb_report.is_clean();
    issues.extend(sb_report.errors);

    let root_inode_readable = {
        let tm = TransactionManager::new(sb);
        let mut tx = tm.begin(0);
        let mut ctx = TxContext::new(disk, &mut tx);
        crate::inode::manager::InodeManager::read_inode(&mut ctx, sb.inode_tree_root, sb.root_inode)
            .is_ok()
    };
    if !root_inode_readable {
        issues.push(format!(
            "root inode ({}) could not be read from inode_tree_root ({})",
            sb.root_inode, sb.inode_tree_root
        ));
    }

    RecoveryVerification {
        superblock_ok,
        root_inode_readable,
        issues,
    }
}
