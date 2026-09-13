//! Shared constants that aren't tied to the on-disk format specifically
//! (for those, see `ondisk::serialization`, the canonical source for
//! `BLOCK_SIZE`/`LIONFS_MAGIC`/etc. -- this module re-exports rather than
//! duplicates them, so there's exactly one definition to keep in sync).

pub use crate::ondisk::serialization::{BLOCK_SIZE, LIONFS_MAGIC, MAX_INLINE_EXTENTS};

/// Reserved inode numbers, matching the convention already used by
/// `Superblock::root_inode` and `InodeTree::allocate_inode` (which starts
/// handing out numbers at 2).
pub const ROOT_INODE: u64 = 1;
pub const FIRST_ALLOCATABLE_INODE: u64 = 2;

/// Maximum length of a single path component, matching the value already
/// reported by `LionFS::statfs`.
pub const MAX_NAME_LEN: usize = 255;

/// Encryption/compression algorithm ids, matching
/// `security::encryption::{Aes256Gcm, ChaCha20Poly1305}` and
/// `fs::compression::{Lz4, Zstd, Deflate}`. Centralized here so new code
/// doesn't have to remember (or risk mismatching) the magic numbers.
pub const ALGO_NONE: u8 = 0;
pub const ENCRYPTION_AES_256_GCM: u8 = 1;
pub const ENCRYPTION_CHACHA20_POLY1305: u8 = 2;
pub const COMPRESSION_LZ4: u8 = 1;
pub const COMPRESSION_ZSTD: u8 = 2;
pub const COMPRESSION_DEFLATE: u8 = 3;
