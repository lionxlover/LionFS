use crate::btree::tree::BTree;
use crate::transaction::transaction::TxContext;
use bytemuck::{Pod, Zeroable};
use std::io::{Error, ErrorKind, Result};

pub const DIR_TREE_NODE_TYPE: u32 = 2;

#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct DirTreeValue {
    pub ino: u64,
    pub file_type: u8,
    pub name_len: u8,
    pub name: [u8; 254],
}
unsafe impl Zeroable for DirTreeValue {}
unsafe impl Pod for DirTreeValue {}

pub struct DirectoryTree {
    pub btree: BTree<u64, DirTreeValue>,
}

fn hash_name(name: &str) -> u64 {
    let mut hash: u64 = 14695981039346656037;
    for b in name.bytes() {
        hash ^= b as u64;
        hash = hash.wrapping_mul(1099511628211);
    }
    hash
}

impl DirectoryTree {
    pub fn new(root_block: u64) -> Self {
        Self {
            btree: BTree::new(root_block, DIR_TREE_NODE_TYPE),
        }
    }

    pub fn init_empty(ctx: &mut TxContext, root_block: u64) -> Result<()> {
        BTree::<u64, DirTreeValue>::init_empty(ctx, root_block, DIR_TREE_NODE_TYPE)
    }

    pub fn lookup(&self, ctx: &mut TxContext, name: &str) -> Result<Option<DirTreeValue>> {
        let key = hash_name(name);
        // Note: In a production filesystem we must handle hash collisions.
        // For Phase 4 prototype, we assume FNV-1a has negligible collisions for small dirs.
        self.btree.lookup(ctx, &key)
    }

    pub fn insert<F>(
        &mut self,
        ctx: &mut TxContext,
        name: &str,
        ino: u64,
        file_type: u8,
        allocate_block: F,
    ) -> Result<()>
    where
        F: FnMut(&mut TxContext) -> Result<u64>,
    {
        let key = hash_name(name);

        if name.len() > 254 {
            return Err(Error::new(ErrorKind::InvalidInput, "Filename too long"));
        }

        let mut value = DirTreeValue {
            ino,
            file_type,
            name_len: name.len() as u8,
            name: [0; 254],
        };
        value.name[..name.len()].copy_from_slice(name.as_bytes());

        self.btree.insert(ctx, key, value, allocate_block)
    }
}
