use crate::btree::tree::BTree;
use crate::ondisk::serialization::{SubvolumeRecord, Superblock};
use crate::transaction::transaction::TxContext;
use std::io::{Error, ErrorKind, Result};

pub const SUBVOLUME_TREE_NODE_TYPE: u32 = 10;

pub struct VolumeManager {
    tree: BTree<u64, SubvolumeRecord>,
}

impl VolumeManager {
    pub fn new(root_block: u64) -> Self {
        Self {
            tree: BTree::new(root_block, SUBVOLUME_TREE_NODE_TYPE),
        }
    }

    pub fn create_subvolume<F>(
        &mut self,
        ctx: &mut TxContext,
        sb: &mut Superblock,
        subvol_id: u64,
        parent_id: u64,
        allocate_block: &mut F,
    ) -> Result<()>
    where
        F: FnMut(&mut TxContext) -> Result<u64>,
    {
        if self.tree.lookup(ctx, &subvol_id)?.is_some() {
            return Err(Error::new(
                ErrorKind::AlreadyExists,
                "Subvolume ID already exists",
            ));
        }

        let record = SubvolumeRecord {
            id: subvol_id,
            parent_id,
            inode_tree_root: 0,
            dir_tree_root: 0,
            extent_tree_root: 0,
            flags: 0,
            reserved: [0; 4],
        };

        self.tree.insert(ctx, subvol_id, record, allocate_block)?;
        sb.subvolume_tree_root = self.tree.root_block;

        Ok(())
    }

    pub fn get_subvolume(
        &self,
        ctx: &mut TxContext,
        subvol_id: u64,
    ) -> Result<Option<SubvolumeRecord>> {
        self.tree.lookup(ctx, &subvol_id)
    }

    pub fn delete_subvolume<F>(
        &mut self,
        ctx: &mut TxContext,
        sb: &mut Superblock,
        subvol_id: u64,
        _allocate_block: &mut F,
    ) -> Result<()>
    where
        F: FnMut(&mut TxContext) -> Result<u64>,
    {
        let removed = self.tree.remove(ctx, &subvol_id)?;
        if !removed {
            return Err(Error::new(ErrorKind::NotFound, "Subvolume not found"));
        }
        sb.subvolume_tree_root = self.tree.root_block;
        Ok(())
    }
}
