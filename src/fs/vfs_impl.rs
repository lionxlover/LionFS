//! `VfsOps` implementation for the core `LionFS` engine: the
//! platform-neutral operations surface (RFC-003). Every platform bridge
//! (FUSE on Linux/macOS, WinFsp on Windows) drives the file system
//! through exactly this impl, so semantics are identical everywhere by
//! construction rather than by porting discipline.
//!
//! Ported 1:1 from the 1.x `impl fuser::Filesystem for LionFS` -- the
//! method bodies are the same logic; the `reply.*` callback pattern
//! became `Result` returns and the libc errno constants became
//! `pal::posix` constants (the same ABI values).

use std::time::{SystemTime, UNIX_EPOCH};

use super::filesystem::LionFS;
use crate::ondisk::serialization::{Inode, BLOCK_SIZE};
use crate::pal::posix;
use crate::transaction::transaction::{Transaction, TxContext};
use crate::vfs::{
    VfsAttr, VfsCreate, VfsDirEntry, VfsError, VfsKind, VfsOps, VfsResult, VfsSetAttr, VfsStatFs,
};

/// Virtual control files exposed at the mount root (1.x behavior,
/// preserved: scrub status and health report as read-only files).
const SCRUB_INO: u64 = 999_999;
const HEALTH_INO: u64 = 999_998;

impl LionFS {
    fn to_vfs_attr(&self, inode: &Inode) -> VfsAttr {
        let kind = if posix::is_dir(inode.mode) {
            VfsKind::Directory
        } else {
            VfsKind::RegularFile
        };
        let t = |secs: i64| -> SystemTime {
            if secs >= 0 {
                UNIX_EPOCH + std::time::Duration::from_secs(secs as u64)
            } else {
                UNIX_EPOCH
            }
        };
        VfsAttr {
            ino: inode.ino,
            size: inode.size,
            blocks: inode.size.div_ceil(BLOCK_SIZE as u64),
            atime: t(inode.atime),
            mtime: t(inode.mtime),
            ctime: t(inode.ctime),
            kind,
            perm: inode.mode & 0o777,
            nlink: inode.links_count,
            uid: inode.uid,
            gid: inode.gid,
            blksize: BLOCK_SIZE as u32,
            flags: inode.flags,
        }
    }

    fn virtual_attr(ino: u64) -> VfsAttr {
        let now = SystemTime::now();
        VfsAttr {
            ino,
            size: 4096, // Virtual size
            blocks: 1,
            atime: now,
            mtime: now,
            ctime: now,
            kind: VfsKind::RegularFile,
            perm: 0o666,
            nlink: 1,
            uid: 1000, // lion
            gid: 1000,
            blksize: BLOCK_SIZE as u32,
            flags: 0,
        }
    }

    /// Borrows a transaction context: the active one if a write is in
    /// flight, else a scratch read-only one. An associated function with
    /// explicit arguments (not a `&mut self` method) so closures can
    /// capture the remaining `self` fields disjointly -- edition-2021
    /// closure captures make `self.superblock`, `self.key_manager`, and
    /// `&mut self.active_tx` coexist without whole-self borrows.
    fn with_ctx<R>(
        disk: &std::sync::Arc<crate::disk::block_io::Disk>,
        active_tx: &mut Option<Transaction>,
        f: impl FnOnce(&mut TxContext<'_>) -> R,
    ) -> R {
        let mut temp_tx = Transaction::new(0, 0);
        let tx: &mut Transaction = match active_tx.as_mut() {
            Some(act_tx) => act_tx,
            None => &mut temp_tx,
        };
        let mut ctx = TxContext::new(disk, tx);
        f(&mut ctx)
    }
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn e(errno: i32) -> VfsError {
    VfsError::new(errno)
}

impl VfsOps for LionFS {
    fn init(&mut self) {
        let bg = self.get_bg_desc();
        self.scrubber
            .start(self.superblock, bg, self.image_path.clone());
    }

    fn destroy(&mut self) {
        self.scrubber.stop();
        if let Err(err) = self.disk.sync() {
            eprintln!("Failed to sync disk on unmount: {err}");
        }
    }

    fn lookup(&mut self, parent: u64, name: &str) -> VfsResult<VfsAttr> {
        if parent == 1 {
            if name == ".lfs_scrub" {
                return Ok(Self::virtual_attr(SCRUB_INO));
            } else if name == ".lfs_health" {
                return Ok(Self::virtual_attr(HEALTH_INO));
            }
        }

        let found: Option<Inode> = Self::with_ctx(&self.disk, &mut self.active_tx, |ctx| {
            if let Ok(mut parent_inode) = crate::inode::manager::InodeManager::read_inode(
                ctx,
                self.superblock.inode_tree_root,
                parent,
            ) {
                if let Ok(entries) = crate::directory::entries::DirManager::read_entries(
                    ctx,
                    self.superblock.checksum_tree_root,
                    self.superblock.bad_blocks_root,
                    &mut parent_inode,
                ) {
                    for entry in entries {
                        if entry.name == name {
                            if let Ok(inode) = crate::inode::manager::InodeManager::read_inode(
                                ctx,
                                self.superblock.inode_tree_root,
                                entry.ino,
                            ) {
                                return Some(inode);
                            }
                        }
                    }
                }
            }
            None
        });
        match found {
            Some(inode) => Ok(self.to_vfs_attr(&inode)),
            None => Err(e(posix::ENOENT)),
        }
    }

    fn getattr(&mut self, ino: u64) -> VfsResult<VfsAttr> {
        if ino == SCRUB_INO || ino == HEALTH_INO {
            return Ok(Self::virtual_attr(ino));
        }
        self.get_inode(ino)
            .map(|inode| self.to_vfs_attr(&inode))
            .map_err(|_| e(posix::ENOENT))
    }

    fn setattr(&mut self, ino: u64, attr: &VfsSetAttr) -> VfsResult<VfsAttr> {
        if ino == SCRUB_INO || ino == HEALTH_INO {
            return Ok(Self::virtual_attr(ino));
        }

        let now = now_secs() as i64;
        let mut result_inode = None;

        {
            let bg_desc = self.get_bg_desc();
            let blocks_per_group = self.superblock.blocks_per_group;
            let inode_tree_root = self.superblock.inode_tree_root;
            if self.active_tx.is_none() {
                self.active_tx = Some(self.tx_manager.begin(now as u64));
            }
            let tx = self.active_tx.as_mut().expect("just began");
            let mut ctx = TxContext::new(&self.disk, tx);

            if let Ok(mut inode) =
                crate::inode::manager::InodeManager::read_inode(&mut ctx, inode_tree_root, ino)
            {
                if let Some(new_size) = attr.size {
                    let _ = crate::file::writer::FileManager::truncate_file(
                        &mut ctx,
                        &bg_desc,
                        blocks_per_group,
                        &mut inode,
                        new_size,
                    );
                }
                crate::fs::metadata::apply_attr_changes(
                    &mut inode,
                    crate::fs::metadata::AttrChanges {
                        mode: attr.mode,
                        uid: attr.uid,
                        gid: attr.gid,
                        atime: attr
                            .atime
                            .map(|t| crate::fs::metadata::TimeOrNow::At(secs_of(t))),
                        mtime: attr
                            .mtime
                            .map(|t| crate::fs::metadata::TimeOrNow::At(secs_of(t))),
                    },
                    now,
                );

                if crate::inode::manager::InodeManager::write_inode_with_allocator(
                    &mut ctx,
                    inode_tree_root,
                    &inode,
                    |c| {
                        crate::allocator::bitmap::Allocator::allocate_extents(
                            c,
                            &bg_desc,
                            blocks_per_group,
                            1,
                        )
                    },
                )
                .is_ok()
                {
                    result_inode = Some(inode);
                }
            }
        }

        if let Some(final_inode) = result_inode {
            if let Some(tx) = self.active_tx.take() {
                let _ = self.tx_manager.commit(&self.disk, &self.superblock, &tx);
            }
            self.inode_cache.insert(ino, final_inode, false);
            Ok(self.to_vfs_attr(&final_inode))
        } else {
            Err(e(posix::ENOENT))
        }
    }

    fn readdir(
        &mut self,
        ino: u64,
        offset: u64,
        max_entries: usize,
    ) -> VfsResult<Vec<VfsDirEntry>> {
        let out = Self::with_ctx(&self.disk, &mut self.active_tx, |ctx| {
            if let Ok(mut inode) = crate::inode::manager::InodeManager::read_inode(
                ctx,
                self.superblock.inode_tree_root,
                ino,
            ) {
                if let Ok(entries) = crate::directory::entries::DirManager::read_entries(
                    ctx,
                    self.superblock.checksum_tree_root,
                    self.superblock.bad_blocks_root,
                    &mut inode,
                ) {
                    let mut dir_entries = vec![
                        (inode.ino, VfsKind::Directory, ".".to_string()),
                        // Simplified parent for Phase 1 (1.x behavior).
                        (inode.ino, VfsKind::Directory, "..".to_string()),
                    ];
                    for entry in entries {
                        let kind = if entry.file_type == 2 {
                            VfsKind::Directory
                        } else {
                            VfsKind::RegularFile
                        };
                        dir_entries.push((entry.ino, kind, entry.name));
                    }
                    if ino == 1 {
                        dir_entries.push((
                            SCRUB_INO,
                            VfsKind::RegularFile,
                            ".lfs_scrub".to_string(),
                        ));
                        dir_entries.push((
                            HEALTH_INO,
                            VfsKind::RegularFile,
                            ".lfs_health".to_string(),
                        ));
                    }
                    // The FUSE readdir offset protocol: each entry carries
                    // its 1-based next offset; callers start at the offset
                    // of the last entry they consumed.
                    return Some(
                        dir_entries
                            .into_iter()
                            .enumerate()
                            .skip(offset as usize)
                            .take(max_entries)
                            .map(|(i, (ino, kind, name))| VfsDirEntry {
                                ino,
                                kind,
                                name,
                                next_offset: (i + 1) as u64,
                            })
                            .collect::<Vec<_>>(),
                    );
                }
            }
            None
        });
        out.ok_or_else(|| e(posix::ENOENT))
    }

    fn read(&mut self, ino: u64, offset: u64, size: u32) -> VfsResult<Vec<u8>> {
        let data = Self::with_ctx(&self.disk, &mut self.active_tx, |ctx| {
            if ino == SCRUB_INO || ino == HEALTH_INO {
                let data = if ino == SCRUB_INO {
                    self.scrubber.get_status().into_bytes()
                } else {
                    crate::integrity::bad_blocks::BadBlockManager::get_health_report(
                        ctx,
                        self.superblock.bad_blocks_root,
                    )
                    .into_bytes()
                };
                let off = offset as usize;
                let slice = if off >= data.len() {
                    &data[0..0]
                } else {
                    let end = (off + size as usize).min(data.len());
                    &data[off..end]
                };
                return Some(slice.to_vec());
            }

            let key_tree_root = self.superblock.key_tree_root;
            let crypto_tree_root = self.superblock.crypto_tree_root;
            if let Ok(mut inode) = crate::inode::manager::InodeManager::read_inode(
                ctx,
                self.superblock.inode_tree_root,
                ino,
            ) {
                // Resolve the block-cipher context (cheap when
                // encryption/compression are both off).
                let key = if inode.encryption_algo != 0 {
                    self.key_manager
                        .get_key(ctx, key_tree_root, inode.key_id)
                        .ok()
                        .flatten()
                } else {
                    None
                };
                let cctx = crate::security::block_cipher::BlockCipherContext {
                    compression_algo: inode.compression_algo,
                    encryption_algo: inode.encryption_algo,
                    key,
                    crypto_tree_root,
                };
                if let Ok(data) = crate::file::writer::FileManager::read_file(
                    ctx,
                    self.superblock.checksum_tree_root,
                    self.superblock.bad_blocks_root,
                    &cctx,
                    &mut inode,
                    offset,
                    u64::from(size),
                ) {
                    return Some(data);
                }
            }
            None
        });
        data.ok_or_else(|| e(posix::EIO))
    }

    fn write(&mut self, ino: u64, offset: u64, data: &[u8]) -> VfsResult<u32> {
        if ino == SCRUB_INO {
            if let Ok(cmd) = std::str::from_utf8(data) {
                self.scrubber.handle_command(cmd.trim());
            }
            return Ok(data.len() as u32);
        }
        if ino == HEALTH_INO {
            return Err(e(posix::EPERM));
        }

        let now = now_secs();
        let mut success = false;
        let mut commit_now = false;

        {
            let bg_desc = self.get_bg_desc();
            let blocks_per_group = self.superblock.blocks_per_group;
            let inode_tree_root = self.superblock.inode_tree_root;
            let key_tree_root = self.superblock.key_tree_root;
            let crypto_tree_root = self.superblock.crypto_tree_root;
            if self.active_tx.is_none() {
                self.active_tx = Some(self.tx_manager.begin(now));
            }
            let tx = self.active_tx.as_mut().expect("just began");
            let mut ctx = TxContext::new(&self.disk, tx);
            if let Ok(mut inode) =
                crate::inode::manager::InodeManager::read_inode(&mut ctx, inode_tree_root, ino)
            {
                // Resolve the cipher context: fresh key material for
                // encrypted files (the 1.x resolve_block_cipher_ctx
                // logic, inlined for the borrow structure).
                let key = if inode.encryption_algo != 0 {
                    self.key_manager
                        .get_key(&mut ctx, key_tree_root, inode.key_id)
                        .ok()
                        .flatten()
                } else {
                    None
                };
                let cctx = crate::security::block_cipher::BlockCipherContext {
                    compression_algo: inode.compression_algo,
                    encryption_algo: inode.encryption_algo,
                    key,
                    crypto_tree_root,
                };
                if crate::file::writer::FileManager::write_file(
                    &mut ctx,
                    &bg_desc,
                    blocks_per_group,
                    self.superblock.checksum_tree_root,
                    &cctx,
                    &mut inode,
                    offset,
                    data,
                )
                .is_ok()
                {
                    inode.mtime = now as i64;
                    // Use the real allocator (not the guaranteed-to-error
                    // dummy) so this doesn't start silently failing once
                    // the inode tree grows past its first leaf node.
                    let _ = crate::inode::manager::InodeManager::write_inode_with_allocator(
                        &mut ctx,
                        inode_tree_root,
                        &inode,
                        |c| {
                            crate::allocator::bitmap::Allocator::allocate_extents(
                                c,
                                &bg_desc,
                                blocks_per_group,
                                1,
                            )
                        },
                    );
                    success = true;
                }
            }
            if let Some(tx) = &self.active_tx {
                if tx.dirty_blocks.len() > 1024 {
                    commit_now = true;
                }
            }
        }

        if success {
            if commit_now {
                if let Some(tx) = self.active_tx.take() {
                    let _ = self.tx_manager.commit(&self.disk, &self.superblock, &tx);
                }
            }
            Ok(data.len() as u32)
        } else {
            Err(e(posix::EIO))
        }
    }

    fn flush(&mut self, _ino: u64) -> VfsResult<()> {
        if let Some(tx) = self.active_tx.take() {
            let _ = self.tx_manager.commit(&self.disk, &self.superblock, &tx);
        }
        Ok(())
    }

    fn fsync(&mut self, _ino: u64, _datasync: bool) -> VfsResult<()> {
        if let Some(tx) = self.active_tx.take() {
            let _ = self.tx_manager.commit(&self.disk, &self.superblock, &tx);
        }
        let _ = self.disk.sync();
        Ok(())
    }

    fn create(&mut self, parent: u64, name: &str, create: &VfsCreate) -> VfsResult<VfsAttr> {
        let now = now_secs();

        // Resolve compression/encryption defaults and, if encryption is
        // on by default, generate a fresh key up front -- before any
        // TxContext borrow is in play.
        let compression_algo = self.superblock.default_compression;
        let encryption_algo = self.superblock.default_encryption;
        let key_tree_root = self.superblock.key_tree_root;
        let new_key = if encryption_algo != 0 {
            self.key_manager.generate_key(encryption_algo).ok()
        } else {
            None
        };
        let key_id = new_key.map(|(id, _)| id).unwrap_or(0);

        let mut final_inode = None;
        {
            let bg_desc = self.get_bg_desc();
            let blocks_per_group = self.superblock.blocks_per_group;
            if self.active_tx.is_none() {
                self.active_tx = Some(self.tx_manager.begin(now));
            }
            let tx = self.active_tx.as_mut().expect("just began");
            let mut ctx = TxContext::new(&self.disk, tx);
            if let Ok(mut parent_inode) = crate::inode::manager::InodeManager::read_inode(
                &mut ctx,
                self.superblock.inode_tree_root,
                parent,
            ) {
                if let Ok(new_ino) =
                    crate::inode::manager::InodeManager::allocate_inode(&mut self.superblock)
                {
                    let new_inode = Inode {
                        ino: new_ino,
                        mode: create.mode | posix::S_IFREG,
                        uid: create.uid,
                        gid: create.gid,
                        links_count: 1,
                        flags: 0,
                        padding1: 0,
                        size: 0,
                        ctime: now as i64,
                        mtime: now as i64,
                        atime: now as i64,
                        extent_count: 0,
                        compression_algo,
                        encryption_algo,
                        key_id,
                        extents: [crate::ondisk::serialization::Extent {
                            logical_start: 0,
                            physical_start: 0,
                            length: 0,
                        }; 7],
                        checksum: 0,
                        spill_pad_head: [0; 4],
                        spill_extent_root: 0,
                    };

                    if crate::inode::manager::InodeManager::write_inode_with_allocator(
                        &mut ctx,
                        self.superblock.inode_tree_root,
                        &new_inode,
                        |c| {
                            crate::allocator::bitmap::Allocator::allocate_extents(
                                c,
                                &bg_desc,
                                blocks_per_group,
                                1,
                            )
                        },
                    )
                    .is_ok()
                    {
                        if key_id != 0 {
                            // Persist the freshly generated key so it's
                            // still there after a remount, not just for
                            // this mount's in-memory cache.
                            let _ =
                                self.key_manager
                                    .persist(&mut ctx, key_tree_root, key_id, |c| {
                                        crate::allocator::bitmap::Allocator::allocate_extents(
                                            c,
                                            &bg_desc,
                                            blocks_per_group,
                                            1,
                                        )
                                    });
                        }
                        if crate::directory::entries::DirManager::add_entry(
                            &mut ctx,
                            &bg_desc,
                            self.superblock.blocks_per_group,
                            self.superblock.checksum_tree_root,
                            self.superblock.bad_blocks_root,
                            &mut parent_inode,
                            name,
                            new_ino,
                            posix::dirent_type(create.mode | posix::S_IFREG),
                        )
                        .is_ok()
                        {
                            parent_inode.mtime = now as i64;
                            let _ = crate::inode::manager::InodeManager::write_inode_with_allocator(
                                &mut ctx,
                                self.superblock.inode_tree_root,
                                &parent_inode,
                                |c| {
                                    crate::allocator::bitmap::Allocator::allocate_extents(
                                        c,
                                        &bg_desc,
                                        blocks_per_group,
                                        1,
                                    )
                                },
                            );
                            final_inode = Some(new_inode);
                        }
                    }
                }
            }
        }

        match final_inode {
            Some(inode) => {
                if let Some(tx) = self.active_tx.take() {
                    let _ = self.tx_manager.commit(&self.disk, &self.superblock, &tx);
                }
                Ok(self.to_vfs_attr(&inode))
            }
            None => Err(e(posix::EIO)),
        }
    }

    fn mkdir(&mut self, parent: u64, name: &str, create: &VfsCreate) -> VfsResult<VfsAttr> {
        let now = now_secs();
        let mut final_inode = None;

        {
            let bg_desc = self.get_bg_desc();
            if self.active_tx.is_none() {
                self.active_tx = Some(self.tx_manager.begin(now));
            }
            let tx = self.active_tx.as_mut().expect("just began");
            let mut ctx = TxContext::new(&self.disk, tx);
            if let Ok(mut parent_inode) = crate::inode::manager::InodeManager::read_inode(
                &mut ctx,
                self.superblock.inode_tree_root,
                parent,
            ) {
                if let Ok(new_ino) =
                    crate::inode::manager::InodeManager::allocate_inode(&mut self.superblock)
                {
                    let mut new_inode = Inode {
                        ino: new_ino,
                        mode: create.mode | posix::S_IFDIR,
                        uid: create.uid,
                        gid: create.gid,
                        links_count: 2,
                        flags: 0,
                        padding1: 0,
                        size: 0,
                        ctime: now as i64,
                        mtime: now as i64,
                        atime: now as i64,
                        extent_count: 0,
                        compression_algo: 0,
                        encryption_algo: 0,
                        key_id: 0,
                        extents: [crate::ondisk::serialization::Extent {
                            logical_start: 0,
                            physical_start: 0,
                            length: 0,
                        }; 7],
                        checksum: 0,
                        spill_pad_head: [0; 4],
                        spill_extent_root: 0,
                    };

                    let blocks_per_group = self.superblock.blocks_per_group;
                    if crate::inode::manager::InodeManager::write_inode_with_allocator(
                        &mut ctx,
                        self.superblock.inode_tree_root,
                        &new_inode,
                        |c| {
                            crate::allocator::bitmap::Allocator::allocate_extents(
                                c,
                                &bg_desc,
                                blocks_per_group,
                                1,
                            )
                        },
                    )
                    .is_ok()
                    {
                        if crate::directory::entries::DirManager::add_entry(
                            &mut ctx,
                            &bg_desc,
                            self.superblock.blocks_per_group,
                            self.superblock.checksum_tree_root,
                            self.superblock.bad_blocks_root,
                            &mut parent_inode,
                            name,
                            new_ino,
                            posix::dirent_type(create.mode | posix::S_IFDIR),
                        )
                        .is_ok()
                        {
                            parent_inode.mtime = now as i64;
                            let _ = crate::inode::manager::InodeManager::write_inode_with_allocator(
                                &mut ctx,
                                self.superblock.inode_tree_root,
                                &parent_inode,
                                |c| {
                                    crate::allocator::bitmap::Allocator::allocate_extents(
                                        c,
                                        &bg_desc,
                                        blocks_per_group,
                                        1,
                                    )
                                },
                            );

                            // Also add . and .. to the new directory.
                            let _ = crate::directory::entries::DirManager::add_entry(
                                &mut ctx,
                                &bg_desc,
                                self.superblock.blocks_per_group,
                                self.superblock.checksum_tree_root,
                                self.superblock.bad_blocks_root,
                                &mut new_inode,
                                ".",
                                new_ino,
                                2,
                            );
                            let _ = crate::directory::entries::DirManager::add_entry(
                                &mut ctx,
                                &bg_desc,
                                self.superblock.blocks_per_group,
                                self.superblock.checksum_tree_root,
                                self.superblock.bad_blocks_root,
                                &mut new_inode,
                                "..",
                                parent,
                                2,
                            );

                            final_inode = Some(new_inode);
                        }
                    }
                }
            }
        }

        match final_inode {
            Some(inode) => {
                if let Some(tx) = self.active_tx.take() {
                    let _ = self.tx_manager.commit(&self.disk, &self.superblock, &tx);
                }
                Ok(self.to_vfs_attr(&inode))
            }
            None => Err(e(posix::EIO)),
        }
    }

    fn unlink(&mut self, parent: u64, name: &str) -> VfsResult<()> {
        let now = now_secs();
        let mut success = false;

        {
            let bg_desc = self.get_bg_desc();
            let blocks_per_group = self.superblock.blocks_per_group;
            if self.active_tx.is_none() {
                self.active_tx = Some(self.tx_manager.begin(now));
            }
            let tx = self.active_tx.as_mut().expect("just began");
            let mut ctx = TxContext::new(&self.disk, tx);
            if let Ok(mut parent_inode) = crate::inode::manager::InodeManager::read_inode(
                &mut ctx,
                self.superblock.inode_tree_root,
                parent,
            ) {
                if let Ok(Some(target_ino)) = crate::directory::entries::DirManager::remove_entry(
                    &mut ctx,
                    &bg_desc,
                    self.superblock.blocks_per_group,
                    self.superblock.checksum_tree_root,
                    self.superblock.bad_blocks_root,
                    &mut parent_inode,
                    name,
                ) {
                    if let Ok(mut target_inode) = crate::inode::manager::InodeManager::read_inode(
                        &mut ctx,
                        self.superblock.inode_tree_root,
                        target_ino,
                    ) {
                        target_inode.links_count -= 1;
                        if target_inode.links_count == 0 {
                            target_inode.mode = 0; // free inode
                        }
                        let _ = crate::inode::manager::InodeManager::write_inode_with_allocator(
                            &mut ctx,
                            self.superblock.inode_tree_root,
                            &target_inode,
                            |c| {
                                crate::allocator::bitmap::Allocator::allocate_extents(
                                    c,
                                    &bg_desc,
                                    blocks_per_group,
                                    1,
                                )
                            },
                        );

                        parent_inode.mtime = now as i64;
                        let _ = crate::inode::manager::InodeManager::write_inode_with_allocator(
                            &mut ctx,
                            self.superblock.inode_tree_root,
                            &parent_inode,
                            |c| {
                                crate::allocator::bitmap::Allocator::allocate_extents(
                                    c,
                                    &bg_desc,
                                    blocks_per_group,
                                    1,
                                )
                            },
                        );
                        success = true;
                    }
                }
            }
        }

        if success {
            if let Some(tx) = self.active_tx.take() {
                let _ = self.tx_manager.commit(&self.disk, &self.superblock, &tx);
            }
            Ok(())
        } else {
            Err(e(posix::ENOENT))
        }
    }

    fn rmdir(&mut self, parent: u64, name: &str) -> VfsResult<()> {
        // Directory removal goes through unlink semantics plus a
        // not-empty guard: look up the target first.
        let target = Self::with_ctx(&self.disk, &mut self.active_tx, |ctx| {
            let mut parent_inode = match crate::inode::manager::InodeManager::read_inode(
                ctx,
                self.superblock.inode_tree_root,
                parent,
            ) {
                Ok(p) => p,
                Err(_) => return Err(e(posix::ENOENT)),
            };
            match crate::directory::entries::DirManager::read_entries(
                ctx,
                self.superblock.checksum_tree_root,
                self.superblock.bad_blocks_root,
                &mut parent_inode,
            ) {
                Ok(entries) => {
                    match entries.iter().find(|en| en.name == name) {
                        Some(entry) => {
                            if entry.file_type != 2 {
                                return Err(e(posix::ENOTDIR));
                            }
                            // Not-empty check: entries beyond . and ..
                            // (read_entries returns only the real ones).
                            if !entries.is_empty() {
                                return Err(e(posix::ENOTEMPTY));
                            }
                            Ok(())
                        }
                        None => Err(e(posix::ENOENT)),
                    }
                }
                Err(_) => Err(e(posix::EIO)),
            }
        });
        target?;
        // Empty: fall through to the unlink path (same 1.x semantics).
        self.unlink(parent, name)
    }

    fn rename(&mut self, parent: u64, name: &str, newparent: u64, newname: &str) -> VfsResult<()> {
        let now = now_secs();
        let mut success = false;

        {
            let bg_desc = self.get_bg_desc();
            let blocks_per_group = self.superblock.blocks_per_group;
            let checksum_tree_root = self.superblock.checksum_tree_root;
            let bad_blocks_root = self.superblock.bad_blocks_root;
            let inode_tree_root = self.superblock.inode_tree_root;
            if self.active_tx.is_none() {
                self.active_tx = Some(self.tx_manager.begin(now));
            }
            let tx = self.active_tx.as_mut().expect("just began");
            let mut ctx = TxContext::new(&self.disk, tx);
            if let Ok(mut p_inode) =
                crate::inode::manager::InodeManager::read_inode(&mut ctx, inode_tree_root, parent)
            {
                if let Ok(Some(target_ino)) = crate::directory::entries::DirManager::remove_entry(
                    &mut ctx,
                    &bg_desc,
                    blocks_per_group,
                    checksum_tree_root,
                    bad_blocks_root,
                    &mut p_inode,
                    name,
                ) {
                    if let Ok(target_inode) = crate::inode::manager::InodeManager::read_inode(
                        &mut ctx,
                        inode_tree_root,
                        target_ino,
                    ) {
                        let file_type = posix::dirent_type(target_inode.mode);

                        // Same-directory rename reuses p_inode for both
                        // sides; cross-directory rename loads the
                        // destination parent separately and updates both.
                        let add_result = if parent == newparent {
                            crate::directory::entries::DirManager::add_entry(
                                &mut ctx,
                                &bg_desc,
                                blocks_per_group,
                                checksum_tree_root,
                                bad_blocks_root,
                                &mut p_inode,
                                newname,
                                target_ino,
                                file_type,
                            )
                        } else if let Ok(mut np_inode) =
                            crate::inode::manager::InodeManager::read_inode(
                                &mut ctx,
                                inode_tree_root,
                                newparent,
                            )
                        {
                            let r = crate::directory::entries::DirManager::add_entry(
                                &mut ctx,
                                &bg_desc,
                                blocks_per_group,
                                checksum_tree_root,
                                bad_blocks_root,
                                &mut np_inode,
                                newname,
                                target_ino,
                                file_type,
                            );
                            if r.is_ok() {
                                np_inode.mtime = now as i64;
                                let _ =
                                    crate::inode::manager::InodeManager::write_inode_with_allocator(
                                        &mut ctx,
                                        inode_tree_root,
                                        &np_inode,
                                        |c| {
                                            crate::allocator::bitmap::Allocator::allocate_extents(
                                                c,
                                                &bg_desc,
                                                blocks_per_group,
                                                1,
                                            )
                                        },
                                    );
                            }
                            r
                        } else {
                            Err(std::io::Error::new(
                                std::io::ErrorKind::NotFound,
                                "destination parent inode not found",
                            ))
                        };

                        if add_result.is_ok() {
                            p_inode.mtime = now as i64;
                            let _ = crate::inode::manager::InodeManager::write_inode_with_allocator(
                                &mut ctx,
                                inode_tree_root,
                                &p_inode,
                                |c| {
                                    crate::allocator::bitmap::Allocator::allocate_extents(
                                        c,
                                        &bg_desc,
                                        blocks_per_group,
                                        1,
                                    )
                                },
                            );
                            success = true;
                        } else {
                            // The entry was already removed from the source
                            // directory; since it couldn't be re-added at
                            // the destination, put it back rather than
                            // leaving the inode orphaned.
                            let _ = crate::directory::entries::DirManager::add_entry(
                                &mut ctx,
                                &bg_desc,
                                blocks_per_group,
                                checksum_tree_root,
                                bad_blocks_root,
                                &mut p_inode,
                                name,
                                target_ino,
                                file_type,
                            );
                        }
                    }
                }
            }
        }

        if success {
            if let Some(tx) = self.active_tx.take() {
                let _ = self.tx_manager.commit(&self.disk, &self.superblock, &tx);
            }
            Ok(())
        } else {
            Err(e(posix::ENOENT))
        }
    }

    fn statfs(&mut self, _ino: u64) -> VfsResult<VfsStatFs> {
        let stats = crate::fs::stat::compute_stats(&self.superblock);
        Ok(VfsStatFs {
            total_blocks: stats.total_blocks,
            free_blocks: stats.free_blocks,
            avail_blocks: stats.free_blocks,
            total_inodes: stats.total_inodes,
            free_inodes: stats.free_inodes_estimate,
            block_size: stats.block_size,
            max_name_len: stats.max_name_len,
        })
    }

    fn access(&mut self, ino: u64, uid: u32, gid: u32, mask: i32) -> VfsResult<()> {
        if ino == SCRUB_INO || ino == HEALTH_INO {
            return Ok(());
        }
        match self.get_inode(ino) {
            Ok(inode) => {
                if crate::inode::permissions::check_access(&inode, uid, gid, mask) {
                    Ok(())
                } else {
                    Err(e(posix::EACCES))
                }
            }
            Err(_) => Err(e(posix::ENOENT)),
        }
    }

    fn readlink(&mut self, _ino: u64) -> VfsResult<String> {
        // Symlinks are not yet first-class in the on-disk format (the
        // 1.x status documented this); the bridge surfaces ENOSYS.
        Err(VfsError::nosys())
    }

    fn symlink(
        &mut self,
        _parent: u64,
        _name: &str,
        _target: &str,
        _uid: u32,
        _gid: u32,
    ) -> VfsResult<VfsAttr> {
        Err(VfsError::nosys())
    }
}

fn secs_of(t: SystemTime) -> i64 {
    t.duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn vfs_error_mapping_is_stable() {
        assert_eq!(e(posix::ENOENT).errno, 2);
        assert_eq!(e(posix::EIO).errno, 5);
    }

    #[test]
    fn virtual_inodes_are_stable_constants() {
        // The control-file inodes must never change: they are visible in
        // mounted filesystems.
        assert_eq!(SCRUB_INO, 999_999);
        assert_eq!(HEALTH_INO, 999_998);
    }
}
