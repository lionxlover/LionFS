//! A `std::io::Read`-compatible streaming wrapper around
//! `FileManager::read_file`, for callers (`tools::dump`, a future backup
//! tool) that want to treat a LionFS file as an ordinary `Read` source
//! (e.g. to pipe through `std::io::copy`) instead of managing offsets by
//! hand at every call site.

use crate::file::writer::FileManager;
use crate::ondisk::serialization::Inode;
use crate::security::block_cipher::BlockCipherContext;
use crate::transaction::transaction::TxContext;
use std::io::{Read, Result};

pub struct LfsFileReader<'a, 'b> {
    ctx: &'a mut TxContext<'b>,
    checksum_tree_root: u64,
    bad_blocks_root: u64,
    cctx: BlockCipherContext,
    inode: Inode,
    position: u64,
}

impl<'a, 'b> LfsFileReader<'a, 'b> {
    pub fn new(
        ctx: &'a mut TxContext<'b>,
        checksum_tree_root: u64,
        bad_blocks_root: u64,
        cctx: BlockCipherContext,
        inode: Inode,
    ) -> Self {
        Self {
            ctx,
            checksum_tree_root,
            bad_blocks_root,
            cctx,
            inode,
            position: 0,
        }
    }

    pub fn seek_to(&mut self, position: u64) {
        self.position = position;
    }
}

impl Read for LfsFileReader<'_, '_> {
    fn read(&mut self, buf: &mut [u8]) -> Result<usize> {
        let data = FileManager::read_file(
            self.ctx,
            self.checksum_tree_root,
            self.bad_blocks_root,
            &self.cctx,
            &mut self.inode,
            self.position,
            buf.len() as u64,
        )?;
        buf[..data.len()].copy_from_slice(&data);
        self.position += data.len() as u64;
        Ok(data.len())
    }
}
