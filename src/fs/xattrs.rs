//! 3.6 Extended attributes + POSIX ACLs: the `XattrTree` and the
//! high-level xattr operations the VFS layer drives.
//!
//! Design (recorded in `specifications/xattrs_acl.md`):
//!
//! * **Storage**: one `BTree<u64 /*ino*/, XattrRecord>` (node type 13,
//!   rooted at `Superblock::xattr_tree_root`); the record points at
//!   the inode's 4 KiB "LXAT" block (`ondisk::xattr`). One record per
//!   inode WITH xattrs -- inodes without xattrs cost nothing.
//! * **Freezing**: node type 13 is a frozen tree (`btree::is_frozen_tree`),
//!   so path-copy CoW applies while a snapshot is live, exactly like
//!   the dir-name / checksum trees. `SnapshotRecord.reserved[0]` carries
//!   the frozen xattr-tree root so a snapshot's xattr view is readable
//!   and verifiable (`read_snapshot_xattr`).
//! * **Root plumbing**: the B-tree publishes root moves through
//!   `ctx.set_root_cell(XATTR_TREE_NODE_TYPE, ..)`; `SharedCore::commit_tx`
//!   syncs that cell into `Superblock::xattr_tree_root` at commit (the
//!   same mechanism as the inode/dir/checksum trees). First
//!   initialization (tree root was 0) allocates a fresh root block and
//!   stamps `FS_FEATURE_XATTR` -- both inside the staging closure, the
//!   established `next_ino` pattern.
//! * **ACLs**: `system.posix_acl_access` / `system.posix_acl_default`
//!   entries carry the POSIX 1003.1e wire format
//!   (`security::posix_acl`); setting one re-derives the inode mode
//!   bits, and `access()` evaluates the ACL when present.

use std::io::{Error, ErrorKind, Result};

use crate::btree::tree::BTree;
use crate::ondisk::serialization::BLOCK_SIZE;
use crate::ondisk::xattr::{
    self, classify_name, write_block, XattrEntry, XATTR_CREATE, XATTR_REPLACE,
};
use crate::transaction::transaction::TxContext;

/// B-tree node type for the xattr tree (next free after the Phase 9-11
/// types 1..=12).
pub const XATTR_TREE_NODE_TYPE: u32 = 13;

/// Per-inode xattr record: the physical block holding this inode's
/// "LXAT" block plus its birth stamp (the Format Vault generation
/// convention -- every on-disk pointer carries its birth so GC and
/// snapshot reclaim can classify it; 3.6 only writes it).
#[repr(C)]
#[derive(Debug, Clone, Copy, bytemuck::Pod, bytemuck::Zeroable)]
pub struct XattrRecord {
    pub block: u64,
    pub generation: u64,
    pub reserved: [u64; 2],
}

/// The xattr tree handle.
pub struct XattrManager {
    tree: BTree<u64, XattrRecord>,
}

impl XattrManager {
    #[must_use]
    pub fn new(root_block: u64) -> Self {
        Self { tree: BTree::new(root_block, XATTR_TREE_NODE_TYPE) }
    }

    /// Initialize an empty tree at a freshly allocated root block.
    pub fn init_empty(ctx: &mut TxContext, root_block: u64) -> Result<()> {
        BTree::<u64, XattrRecord>::init_empty(ctx, root_block, XATTR_TREE_NODE_TYPE)
    }

    /// Resolve the inode's xattr block (None = the inode has no
    /// xattrs). Accepts root 0 (tree not initialized) as empty.
    pub fn block_of(&self, ctx: &mut TxContext, ino: u64) -> Result<Option<u64>> {
        if self.tree.root_block == 0 {
            return Ok(None);
        }
        Ok(self.tree.lookup(ctx, &ino)?.map(|r| r.block))
    }

    /// Publish (or update) the inode's xattr block pointer.
    pub fn set_block<F>(&mut self, ctx: &mut TxContext, ino: u64, block: u64, allocate: &mut F) -> Result<()>
    where
        F: FnMut(&mut TxContext) -> Result<u64>,
    {
        let rec = XattrRecord {
            block,
            generation: crate::btree::tree::node_gen_stamp(),
            reserved: [0; 2],
        };
        self.tree.insert(ctx, ino, rec, allocate)
    }

    /// Remove the inode's xattr record entirely. Returns true if a
    /// record existed. (Callers free the data block themselves.)
    pub fn remove_record<F>(&mut self, ctx: &mut TxContext, ino: u64, allocate: &mut F) -> Result<bool>
    where
        F: FnMut(&mut TxContext) -> Result<u64>,
    {
        if self.tree.root_block == 0 {
            return Ok(false);
        }
        self.tree.remove_with_alloc(ctx, &ino, allocate)
    }

    /// The (possibly moved) tree root after mutations.
    #[must_use]
    pub fn root_block(&self) -> u64 {
        self.tree.root_block
    }
}

/// Read one extended attribute. `Ok(None)` = the attribute does not
/// exist (ENOATTR for the caller). A missing tree or missing inode
/// record is also "no attributes", not an error.
pub fn get_xattr(ctx: &mut TxContext, xattr_tree_root: u64, ino: u64, name: &str) -> Result<Option<Vec<u8>>> {
    let mgr = XattrManager::new(xattr_tree_root);
    let Some(block) = mgr.block_of(ctx, ino)? else {
        return Ok(None);
    };
    let mut buf = [0u8; BLOCK_SIZE];
    ctx.read_block(block, &mut buf)?;
    let entries = xattr::parse_block(&buf)?;
    Ok(entries.into_iter().find(|e| e.name == name).map(|e| e.value))
}

/// List all attribute names for an inode (the raw name list; the VFS
/// layer NUL-separates it for listxattr(2)).
pub fn list_xattrs(ctx: &mut TxContext, xattr_tree_root: u64, ino: u64) -> Result<Vec<String>> {
    let mgr = XattrManager::new(xattr_tree_root);
    let Some(block) = mgr.block_of(ctx, ino)? else {
        return Ok(Vec::new());
    };
    let mut buf = [0u8; BLOCK_SIZE];
    ctx.read_block(block, &mut buf)?;
    Ok(xattr::parse_block(&buf)?.into_iter().map(|e| e.name).collect())
}

/// Set (create/replace) one extended attribute. Handles the whole
/// lifecycle: tree initialization on first use, block read-modify-
/// write with fresh-block CoW.
///
/// `flags` carries the Linux XATTR_CREATE / XATTR_REPLACE bits.
///
/// Returns the (possibly new) tree root PLUS the list of stale data
/// blocks to free -- the CALLER frees them with
/// `Allocator::free_extents` inside the same staging transaction (it
/// owns the block-group descriptor), so the journal makes the update
/// and the reclaim crash-atomic.
#[allow(clippy::too_many_arguments)]
pub fn set_xattr<F>(
    ctx: &mut TxContext,
    xattr_tree_root: u64,
    ino: u64,
    name: &str,
    value: &[u8],
    flags: i32,
    is_symlink: bool,
    allocate: &mut F,
) -> Result<(u64, Vec<u64>)>
where
    F: FnMut(&mut TxContext) -> Result<u64>,
{
    let ns = classify_name(name)?;
    // Linux namespace rules: user.* is not allowed on symlinks at all;
    // on directories it is fine.
    if ns == xattr::NS_USER && is_symlink {
        return Err(Error::new(ErrorKind::Unsupported, "user xattrs not allowed on symlinks"));
    }
    if value.len() > u16::MAX as usize {
        return Err(Error::new(ErrorKind::InvalidInput, "xattr value too large"));
    }

    let mut mgr = XattrManager::new(xattr_tree_root);

    // Read-modify-write the inode's xattr block.
    let existing_entries: Vec<XattrEntry> = match mgr.block_of(ctx, ino)? {
        Some(block) => {
            let mut buf = [0u8; BLOCK_SIZE];
            ctx.read_block(block, &mut buf)?;
            xattr::parse_block(&buf)?
        }
        None => Vec::new(),
    };
    let old_block = mgr.block_of(ctx, ino)?;

    let exists = existing_entries.iter().any(|e| e.name == name);
    if flags & XATTR_CREATE != 0 && exists {
        return Err(Error::new(ErrorKind::AlreadyExists, "xattr exists (XATTR_CREATE)"));
    }
    if flags & XATTR_REPLACE != 0 && !exists {
        // POSIX xattr(7): ENODATA (a.k.a. ENOATTR), not ENOENT.
        return Err(Error::from_raw_os_error(crate::pal::posix::ENODATA));
    }

    let mut entries = existing_entries;
    if exists {
        for e in entries.iter_mut() {
            if e.name == name {
                e.value = value.to_vec();
                e.ns_flags = ns;
            }
        }
    } else {
        entries.push(XattrEntry { name: name.to_string(), value: value.to_vec(), ns_flags: ns });
    }
    entries.sort_by(|a, b| a.name.cmp(&b.name));

    let new_buf = write_block(&entries)?;
    let new_block = allocate(ctx)?;
    ctx.write_block(new_block, &new_buf)?;

    // Point the tree record at the fresh block (initializing the tree
    // on first use happens in the caller: root 0 -> allocated root).
    mgr.set_block(ctx, ino, new_block, allocate)?;

    let mut to_free = Vec::new();
    if let Some(old) = old_block {
        if old != new_block {
            to_free.push(old);
        }
    }
    Ok((mgr.root_block(), to_free))
}

/// Remove one extended attribute. Returns (existed, tree root, stale
/// blocks to free). Frees the inode's xattr block when the last
/// attribute goes away, and removes the tree record with it.
pub fn remove_xattr<F>(
    ctx: &mut TxContext,
    xattr_tree_root: u64,
    ino: u64,
    name: &str,
    allocate: &mut F,
) -> Result<(bool, u64, Vec<u64>)>
where
    F: FnMut(&mut TxContext) -> Result<u64>,
{
    let mut mgr = XattrManager::new(xattr_tree_root);
    let Some(block) = mgr.block_of(ctx, ino)? else {
        return Ok((false, mgr.root_block(), Vec::new()));
    };
    let mut buf = [0u8; BLOCK_SIZE];
    ctx.read_block(block, &mut buf)?;
    let entries = xattr::parse_block(&buf)?;
    let before = entries.len();
    let remaining: Vec<XattrEntry> = entries.into_iter().filter(|e| e.name != name).collect();
    if remaining.len() == before {
        // POSIX xattr(7): ENODATA (a.k.a. ENOATTR), not ENOENT.
        return Err(Error::from_raw_os_error(crate::pal::posix::ENODATA));
    }

    let to_free = vec![block];
    if remaining.is_empty() {
        mgr.remove_record(ctx, ino, allocate)?;
    } else {
        let new_buf = write_block(&remaining)?;
        let new_block = allocate(ctx)?;
        ctx.write_block(new_block, &new_buf)?;
        mgr.set_block(ctx, ino, new_block, allocate)?;
    }
    Ok((true, mgr.root_block(), to_free))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(name: &str, value: &[u8]) -> XattrEntry {
        XattrEntry {
            name: name.to_string(),
            value: value.to_vec(),
            ns_flags: xattr::NS_USER,
        }
    }

    #[test]
    fn record_is_pod_sized() {
        // 8 + 8 + 16 = 32 bytes; key(8) + value(32) = 40 per KV pair in
        // a 4032-byte leaf => ~100 inodes-with-xattrs per leaf node.
        assert_eq!(std::mem::size_of::<XattrRecord>(), 32);
        let _ = entry("user.a", b"b");
    }
}
