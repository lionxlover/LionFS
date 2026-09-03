use bytemuck::{Pod, Zeroable};

pub const BLOCK_SIZE: usize = 4096;
pub const LIONFS_MAGIC: u64 = 0x4C494F4E46533130; // "LIONFS10"
pub const MAX_INLINE_EXTENTS: usize = 7;
pub const DEVICE_TREE_NODE_TYPE: u8 = 10;

#[repr(C)]
#[derive(Debug, Clone, Copy, Pod, Zeroable)]
pub struct Superblock {
    pub magic: u64,
    pub version: u32,
    pub block_size: u32,
    pub total_blocks: u64,
    pub free_blocks: u64,
    pub inode_count: u64,
    pub root_inode: u64,
    pub flags: u32,
    pub padding1: u32,
    pub bitmap_start: u64,
    pub inode_table_start: u64,
    pub data_region_start: u64,
    pub generation: u64,
    pub checksum: u32,
    pub padding_csum: u32,
    pub journal_start: u64,
    pub journal_blocks: u64,
    pub secondary_sb_1: u64,
    pub secondary_sb_2: u64,
    pub block_group_count: u32,
    pub blocks_per_group: u32,
    pub inode_tree_root: u64,
    pub dir_tree_root: u64,
    pub extent_tree_root: u64,
    pub freespace_tree_root: u64,
    pub next_ino: u64,
    pub checksum_tree_root: u64,
    pub bad_blocks_root: u64,
    pub snapshot_tree_root: u64,
    pub clone_tree_root: u64,
    pub refcount_tree_root: u64,
    pub subvolume_tree_root: u64,
    pub space_map_root: u64,
    pub last_snapshot_generation: u64,
    // Phase 7
    pub dedupe_tree_root: u64,
    pub key_tree_root: u64,
    pub fs_features: u64, // Flags for compression, encryption, dedupe enabled
    pub default_compression: u8,
    pub default_encryption: u8,
    pub padding_phase7: [u8; 6],
    // Phase 8
    pub device_tree_root: u64,
    pub pool_uuid: [u8; 16],
    pub raid_profile: u8,
    pub padding_raid: [u8; 3],
    pub chunk_size: u32,
    // Phase 9: root of the per-block transform metadata tree (nonce/tag for
    // encrypted blocks, on-disk length for compressed blocks). See
    // `security::block_cipher::BlockTransformTree`. 0 = not initialized.
    pub crypto_tree_root: u64,
    pub padding2: [u8; BLOCK_SIZE - 312],
}

#[repr(C)]
#[derive(Debug, Clone, Copy, Pod, Zeroable)]
pub struct BlockGroupDescriptor {
    pub bg_block_bitmap: u64, // Block number for the block bitmap
    pub bg_inode_bitmap: u64, // Block number for the inode bitmap (if separate)
    pub bg_inode_table: u64,  // Block number of inode table start for this group
    pub bg_free_blocks_count: u32,
    pub bg_free_inodes_count: u32,
    pub bg_used_dirs_count: u32,
    pub bg_padding: u32,
    pub bg_reserved: [u8; 32], // Total descriptor size = 64 bytes
}

pub const JOURNAL_MAGIC: u64 = 0x4A4F55524E414C31; // "JOURNAL1"

#[repr(C)]
#[derive(Debug, Clone, Copy, Pod, Zeroable)]
pub struct JournalHeader {
    pub magic: u64,
    pub version: u32,
    pub entry_count: u32,
    pub tx_id: u64,
    pub timestamp: u64,
    pub checksum: u32,
    pub padding_csum: u32,
    pub padding: [u8; BLOCK_SIZE - 40], // Header is a full block
}

#[repr(C)]
#[derive(Debug, Clone, Copy, Pod, Zeroable)]
pub struct JournalRecordHeader {
    pub tx_id: u64,
    pub physical_block: u64, // Where this block belongs
    pub checksum: u32,       // Checksum of the 4096-byte data block that follows this record header
    pub padding: u32,
    // The actual 4096-byte data block is written immediately after this header (which we will pad to 1 block or just write inline).
    // Actually, to make it simple: each journaled block takes exactly 2 blocks in the journal:
    // 1 block for JournalRecordHeader (with padding) + 1 block for the Data itself.
    // Let's just make JournalRecordHeader exactly 4096 bytes.
    pub padding2: [u8; BLOCK_SIZE - 24],
}

#[repr(C)]
#[derive(Debug, Clone, Copy, Pod, Zeroable)]
pub struct JournalFooter {
    pub magic: u64, // Same magic or "JCOMMIT1"
    pub tx_id: u64,
    pub total_records: u32,
    pub checksum: u32,
    pub padding: [u8; BLOCK_SIZE - 24], // Full block
}

#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Pod, Zeroable)]
pub struct Extent {
    pub logical_start: u64,
    pub physical_start: u64,
    pub length: u64,
}

// Phase 6 Records

#[repr(C)]
#[derive(Debug, Clone, Copy, Pod, Zeroable)]
pub struct SnapshotRecord {
    pub id: u64,
    pub parent_id: u64,
    pub creation_time: u64,
    pub generation: u64,
    pub inode_tree_root: u64,
    pub dir_tree_root: u64,
    pub extent_tree_root: u64,
    pub checksum_tree_root: u64,
    pub bad_blocks_root: u64,
    pub flags: u32,
    pub padding: u32,
    pub reserved: [u64; 4],
}

#[repr(C)]
#[derive(Debug, Clone, Copy, Pod, Zeroable)]
pub struct CloneRecord {
    pub id: u64,
    pub source_id: u64,
    pub generation: u64,
    pub shared_extents: u64,
    pub reserved: [u64; 4],
}

#[repr(C)]
#[derive(Debug, Clone, Copy, Pod, Zeroable)]
pub struct SubvolumeRecord {
    pub id: u64,
    pub parent_id: u64,
    pub inode_tree_root: u64,
    pub dir_tree_root: u64,
    pub extent_tree_root: u64,
    pub flags: u64,
    pub reserved: [u64; 4],
}

#[repr(C)]
#[derive(Debug, Clone, Copy, Pod, Zeroable)]
pub struct Inode {
    pub ino: u64,
    pub mode: u32,
    pub uid: u32,
    pub gid: u32,
    pub links_count: u32,
    pub flags: u32,
    pub padding1: u32,
    pub size: u64,
    pub ctime: i64,
    pub mtime: i64,
    pub atime: i64,
    pub extent_count: u16,
    pub compression_algo: u8,
    pub encryption_algo: u8,
    pub key_id: u32,
    pub extents: [Extent; MAX_INLINE_EXTENTS],
    pub checksum: u32,
    // Phase 0 extent spooling: when the 7 inline extent slots fill up,
    // further extents spill into a per-inode ExtentTree (a B-tree keyed
    // by logical_start). These two fields occupy what used to be zero
    // padding (bytes 244..256 of the 256-byte Inode). Field order
    // matters: the 4-byte pad keeps `spill_extent_root` 8-byte aligned
    // without implicit struct padding, and the byte offsets match the
    // old `padding4` region exactly, so images written by older code
    // (which always zeroed that padding) read back with
    // spill_extent_root == 0 and behave exactly as before -- the change
    // is read/write compatible in both directions.
    //
    // Phase 4 (format v2): for COMPRESSED inodes (compression_algo != 0)
    // this same field roots the per-inode ClusterTree instead (cluster
    // index -> variable-length physical extent). The two meanings are
    // mutually exclusive by construction: a compressed inode never has
    // inline or spilled extents (all data lives in clusters), and the
    // cluster path never runs for an uncompressed inode.
    pub spill_pad_head: [u8; 4],
    pub spill_extent_root: u64,
}

#[repr(C)]
#[derive(Debug, Clone, Copy, Pod, Zeroable)]
pub struct DirEntryHeader {
    pub ino: u64,
    pub rec_len: u16,
    pub name_len: u8,
    pub file_type: u8,
    pub padding: u32,
}
