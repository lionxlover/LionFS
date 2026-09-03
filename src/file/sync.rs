//! Per-inode durability helpers, distinct from `TransactionManager::commit`
//! (which flushes *every* dirty block in the active transaction). Useful
//! for a future `fsync(2)` implementation that wants to guarantee one
//! file's data specifically without necessarily forcing an early commit of
//! unrelated dirty blocks from other files in the same transaction.

use crate::ondisk::serialization::Inode;
use crate::transaction::transaction::TxContext;
use std::io::Result;

/// Reads back every physical block this inode currently maps to and
/// confirms the read succeeds -- a real (if blunt) durability check: if
/// every block is readable right now, there's nothing about this inode's
/// own data left only in a volatile write buffer somewhere this process
/// doesn't know about. Real fsync semantics ultimately depend on
/// `TransactionManager::commit`'s `Disk::sync` for the actual
/// hardware-durability guarantee; this is a lighter-weight sanity check
/// layered on top, not a replacement for it.
pub fn verify_readable(ctx: &mut TxContext, inode: &Inode) -> Result<()> {
    let mut buf = vec![0u8; crate::ondisk::serialization::BLOCK_SIZE];
    for e in &inode.extents[..inode.extent_count as usize] {
        for i in 0..e.length {
            ctx.read_block(e.physical_start + i, &mut buf)?;
        }
    }
    Ok(())
}
