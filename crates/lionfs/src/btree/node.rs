use crate::ondisk::serialization::BLOCK_SIZE;
use bytemuck::{Pod, Zeroable};

pub const BTREE_MAGIC: u64 = 0x4254524545313030; // "BTREE100"

#[repr(C)]
#[derive(Debug, Clone, Copy, Pod, Zeroable)]
pub struct BTreeNodeHeader {
    pub magic: u64,
    pub node_type: u32, // 0 = Leaf, 1 = Internal, etc. (Can be used for tree-specific types)
    pub level: u16,     // 0 = Leaf, >0 = Internal
    pub item_count: u16, // Number of items in this node
    pub checksum: u32,
    pub padding_align: u32,
    pub generation: u64,
    pub parent_block: u64,
    pub next_leaf: u64, // Only used if level == 0
    pub prev_leaf: u64, // Only used if level == 0
    pub padding: [u8; 8],
}

// BTreeNodeHeader is 64 bytes.
// Payload space: 4096 - 64 = 4032 bytes.
pub const BTREE_PAYLOAD_SIZE: usize = BLOCK_SIZE - 64;

#[repr(C)]
#[derive(Debug, Clone, Copy, Pod, Zeroable)]
pub struct BTreeNodeData {
    pub header: BTreeNodeHeader,
    pub payload: [u8; BTREE_PAYLOAD_SIZE],
}

impl BTreeNodeData {
    pub fn new(level: u16, node_type: u32) -> Self {
        Self {
            header: BTreeNodeHeader {
                magic: BTREE_MAGIC,
                node_type,
                level,
                item_count: 0,
                checksum: 0,
                padding_align: 0,
                generation: 0,
                parent_block: 0,
                next_leaf: 0,
                prev_leaf: 0,
                padding: [0; 8],
            },
            payload: [0; BTREE_PAYLOAD_SIZE],
        }
    }
}
