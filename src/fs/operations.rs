//! Compound, block-level file operations built from the lower-level
//! primitives elsewhere (as opposed to `fs::clones`, which does
//! metadata/refcount-only CoW cloning) -- a real byte-for-byte copy of one
//! file's contents into another, for tools/scenarios that specifically
//! want a materialized duplicate rather than a copy-on-write clone (e.g.
//! `tools::backup`-style use, or restoring from a clone into a
//! independent file).

use crate::file::writer::FileManager;
use crate::ondisk::serialization::{BlockGroupDescriptor, Inode};
use crate::security::block_cipher::BlockCipherContext;
use crate::transaction::transaction::TxContext;
use std::io::Result;

/// Copies up to `chunk_size` bytes at a time from `src` to `dst`,
/// preserving `dst`'s own compression/encryption settings (`dst_cctx`) --
/// the copy re-encodes through the destination's settings rather than
/// blindly duplicating the source's on-disk bytes, so e.g. copying an
/// encrypted file into a not-encrypted destination does the right thing.
#[allow(clippy::too_many_arguments)]
pub fn copy_file_contents(
    ctx: &mut TxContext,
    bg_desc: &BlockGroupDescriptor,
    blocks_per_group: u32,
    checksum_tree_root: u64,
    bad_blocks_root: u64,
    src_cctx: &BlockCipherContext,
    dst_cctx: &BlockCipherContext,
    src: &mut Inode,
    dst: &mut Inode,
    chunk_size: u64,
) -> Result<u64> {
    let mut offset = 0u64;
    let mut total = 0u64;
    while offset < src.size {
        let to_read = chunk_size.min(src.size - offset);
        let data = FileManager::read_file(
            ctx,
            checksum_tree_root,
            bad_blocks_root,
            src_cctx,
            src,
            offset,
            to_read,
        )?;
        if data.is_empty() {
            break;
        }
        FileManager::write_file(
            ctx,
            bg_desc,
            blocks_per_group,
            checksum_tree_root,
            dst_cctx,
            dst,
            offset,
            &data,
        )?;
        total += data.len() as u64;
        offset += data.len() as u64;
    }
    Ok(total)
}
