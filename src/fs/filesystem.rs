#![allow(
    clippy::collapsible_if,
    clippy::manual_div_ceil,
    clippy::unnecessary_cast
)]

//! The portable core of the mounted filesystem: disk, superblock,
//! transaction manager, caches, and mount-time recovery. The operations
//! surface lives in [`super::vfs_impl`] (platform-neutral `VfsOps`);
//! platform bridges live in [`crate::vfs`]. This file contains no
//! fuser/libc code, so the core compiles on every platform the PAL
//! supports.

use crate::disk::block_io::Disk;
use crate::ondisk::serialization::{Inode, Superblock, BLOCK_SIZE};
use crate::transaction::manager::TransactionManager;
use crate::transaction::transaction::{Transaction, TxContext};
use std::sync::Arc;

use crate::cache::inode_cache::InodeCache;
use crate::security::block_cipher::BlockCipherContext;
use crate::security::keys::KeyManager;

pub struct LionFS {
    pub disk: Arc<Disk>,
    pub superblock: Superblock,
    pub tx_manager: TransactionManager,
    pub active_tx: Option<Transaction>,
    pub(crate) inode_cache: InodeCache,
    pub(crate) scrubber: crate::worker::scrubber::ScrubberWorker,
    pub(crate) image_path: String,
    pub(crate) key_manager: KeyManager,
}

impl LionFS {
    pub fn new(mut disk: Disk, image_path: String) -> std::io::Result<Self> {
        let mut buffer = [0u8; BLOCK_SIZE];
        let mut candidates: Vec<Option<[u8; BLOCK_SIZE]>> =
            Vec::with_capacity(crate::ondisk::superblock::CANDIDATE_LOCATIONS.len());
        for &loc in &crate::ondisk::superblock::CANDIDATE_LOCATIONS {
            if disk.read_block(loc, &mut buffer).is_ok() {
                candidates.push(Some(buffer));
            } else {
                candidates.push(None);
            }
        }

        let superblock = match crate::ondisk::superblock::pick_best(&candidates) {
            Some(sb) => sb,
            None => {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    "No valid superblock found or all checksums failed",
                ))
            }
        };

        // Recover journal if any
        let highest_tx =
            crate::recovery::recovery::RecoveryManager::recover(&mut disk, &superblock)?;

        let post_recovery = crate::recovery::verify::verify_post_recovery(&disk, &superblock);
        if !post_recovery.is_healthy() {
            eprintln!(
                "Warning: post-recovery verification found issues: {:?}",
                post_recovery.issues
            );
        }

        let tx_manager = TransactionManager::new(&superblock);

        if highest_tx
            > tx_manager
                .current_tx_id
                .load(std::sync::atomic::Ordering::SeqCst)
        {
            tx_manager
                .current_tx_id
                .store(highest_tx, std::sync::atomic::Ordering::SeqCst);
        }

        let inode_cache = InodeCache::new(10000);

        let scrubber = crate::worker::scrubber::ScrubberWorker::new();
        // Background workers are initialized but waiting

        Ok(Self {
            disk: Arc::new(disk),
            superblock,
            tx_manager,
            active_tx: None,
            inode_cache,
            scrubber,
            image_path,
            key_manager: KeyManager::new(),
        })
    }

    /// Builds the block-cipher context for `inode`, resolving its key (if
    /// any) through the key manager. Cheap when encryption/compression are
    /// both off (the common case): returns immediately without touching
    /// the key tree.
    ///
    /// Deliberately takes `key_manager`/`key_tree_root`/`crypto_tree_root`
    /// as separate parameters rather than `&mut self`: callers already hold
    /// a `TxContext` borrowed from `self.disk`/`self.active_tx`, and a
    /// `&mut self` method here would conflict with that outstanding borrow.
    /// Passing the specific fields it needs keeps the borrows disjoint.
    fn resolve_block_cipher_ctx(
        key_manager: &mut KeyManager,
        key_tree_root: u64,
        crypto_tree_root: u64,
        ctx: &mut TxContext,
        inode: &Inode,
    ) -> std::io::Result<BlockCipherContext> {
        let key = if inode.encryption_algo != 0 {
            key_manager.get_key(ctx, key_tree_root, inode.key_id)?
        } else {
            None
        };
        Ok(BlockCipherContext {
            compression_algo: inode.compression_algo,
            encryption_algo: inode.encryption_algo,
            key,
            crypto_tree_root,
        })
    }

    pub(crate) fn get_bg_desc(&self) -> crate::ondisk::serialization::BlockGroupDescriptor {
        crate::ondisk::serialization::BlockGroupDescriptor {
            bg_block_bitmap: self.superblock.bitmap_start,
            bg_inode_bitmap: 0,
            bg_inode_table: self.superblock.inode_table_start,
            bg_free_blocks_count: 0,
            bg_free_inodes_count: 0,
            bg_used_dirs_count: 0,
            bg_padding: 0,
            bg_reserved: [0; 32],
        }
    }

    pub(crate) fn get_inode(&mut self, ino: u64) -> std::io::Result<Inode> {
        if let Some(inode) = self.inode_cache.get(ino) {
            return Ok(inode);
        }
        let mut temp_tx = Transaction::new(0, 0);
        let tx = if let Some(ref mut act_tx) = self.active_tx {
            act_tx
        } else {
            &mut temp_tx
        };
        let mut ctx = TxContext::new(&self.disk, tx);
        let inode = crate::inode::manager::InodeManager::read_inode(
            &mut ctx,
            self.superblock.inode_tree_root,
            ino,
        )?;
        self.inode_cache.insert(ino, inode, false);
        Ok(inode)
    }
}
