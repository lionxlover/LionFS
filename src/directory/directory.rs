//! Higher-level directory operations built on top of
//! `directory::entries::DirManager`'s single-entry primitives -- currently
//! just "initialize a fresh directory with its conventional `.`/`..`
//! entries," which was previously done by hand at the one call site that
//! creates directories (`fs::filesystem::mkdir`).

use crate::directory::entries::DirManager;
use crate::ondisk::serialization::{BlockGroupDescriptor, Inode};
use crate::transaction::transaction::TxContext;
use std::io::Result;

pub const FILE_TYPE_REGULAR: u8 = 1;
pub const FILE_TYPE_DIRECTORY: u8 = 2;

/// Populates a freshly created, otherwise-empty directory inode with the
/// standard `.` (self) and `..` (parent) entries.
pub fn init_empty_directory(
    ctx: &mut TxContext,
    bg_desc: &BlockGroupDescriptor,
    blocks_per_group: u32,
    checksum_tree_root: u64,
    bad_blocks_root: u64,
    dir_inode: &mut Inode,
    dir_ino: u64,
    parent_ino: u64,
) -> Result<()> {
    DirManager::add_entry(
        ctx,
        bg_desc,
        blocks_per_group,
        checksum_tree_root,
        bad_blocks_root,
        dir_inode,
        ".",
        dir_ino,
        FILE_TYPE_DIRECTORY,
    )?;
    DirManager::add_entry(
        ctx,
        bg_desc,
        blocks_per_group,
        checksum_tree_root,
        bad_blocks_root,
        dir_inode,
        "..",
        parent_ino,
        FILE_TYPE_DIRECTORY,
    )?;
    Ok(())
}

/// Whether a directory has anything in it besides `.`/`..` -- what
/// `rmdir`/`unlink` on a directory needs to check before allowing removal
/// (LionFS doesn't currently expose a separate `rmdir` FUSE handler at
/// all, so nothing calls this yet; it's here ready for when that's added).
pub fn is_empty_directory(entries: &[crate::directory::entries::DirEntry]) -> bool {
    entries.iter().all(|e| e.name == "." || e.name == "..")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::directory::entries::DirEntry;

    #[test]
    fn empty_directory_detection() {
        let only_dots = vec![
            DirEntry {
                ino: 5,
                name: ".".to_string(),
                file_type: FILE_TYPE_DIRECTORY,
            },
            DirEntry {
                ino: 1,
                name: "..".to_string(),
                file_type: FILE_TYPE_DIRECTORY,
            },
        ];
        assert!(is_empty_directory(&only_dots));

        let mut with_file = only_dots.clone();
        with_file.push(DirEntry {
            ino: 9,
            name: "a.txt".to_string(),
            file_type: FILE_TYPE_REGULAR,
        });
        assert!(!is_empty_directory(&with_file));
    }
}
