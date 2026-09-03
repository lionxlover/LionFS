use crate::file::writer::FileManager;
use crate::ondisk::serialization::{BlockGroupDescriptor, DirEntryHeader, Inode};
use crate::security::block_cipher::BlockCipherContext;
use crate::transaction::transaction::TxContext;
use std::io::{Error, ErrorKind, Result};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DirEntry {
    pub ino: u64,
    pub name: String,
    pub file_type: u8,
}

pub struct DirManager;

impl DirManager {
    pub fn read_entries(
        ctx: &mut TxContext,
        checksum_tree_root: u64,
        bad_blocks_root: u64,
        inode: &mut Inode,
    ) -> Result<Vec<DirEntry>> {
        // Directory metadata is never encrypted/compressed; pass a
        // pass-through cipher context.
        let cctx = BlockCipherContext::none();
        let data = FileManager::read_file(
            ctx,
            checksum_tree_root,
            bad_blocks_root,
            &cctx,
            inode,
            0,
            inode.size,
        )?;
        let mut entries = Vec::new();
        let mut offset = 0;

        while offset < data.len() {
            if offset + 16 > data.len() {
                break;
            }

            let header_bytes = &data[offset..offset + 16];
            let header: DirEntryHeader = bytemuck::pod_read_unaligned(header_bytes);

            if header.rec_len == 0 {
                break; // Corrupted or end of list
            }

            if header.ino != 0 {
                let name_start = offset + 16;
                let name_end = name_start + header.name_len as usize;

                if name_end <= data.len() {
                    let name = String::from_utf8_lossy(&data[name_start..name_end]).into_owned();
                    entries.push(DirEntry {
                        ino: header.ino,
                        name,
                        file_type: header.file_type,
                    });
                }
            }

            offset += header.rec_len as usize;
        }

        Ok(entries)
    }

    pub fn add_entry(
        ctx: &mut TxContext,
        bg_desc: &BlockGroupDescriptor,
        blocks_per_group: u32,
        checksum_tree_root: u64,
        bad_blocks_root: u64,
        inode: &mut Inode,
        name: &str,
        target_ino: u64,
        file_type: u8,
    ) -> Result<()> {
        let name_bytes = name.as_bytes();
        if name_bytes.len() > 255 {
            return Err(Error::new(ErrorKind::InvalidInput, "Name too long"));
        }

        let required_len = (16 + name_bytes.len() as u16 + 7) & !7; // 8-byte aligned

        // Let's read the whole directory to find a spot or append
        let cctx = BlockCipherContext::none();
        let data = FileManager::read_file(
            ctx,
            checksum_tree_root,
            bad_blocks_root,
            &cctx,
            inode,
            0,
            inode.size,
        )?;
        let mut offset = 0;

        while offset < data.len() {
            if offset + 16 > data.len() {
                break;
            }
            let header_bytes = &data[offset..offset + 16];
            let mut header: DirEntryHeader = bytemuck::pod_read_unaligned(header_bytes);

            if header.rec_len == 0 {
                return Err(Error::new(
                    ErrorKind::InvalidData,
                    "Corrupt directory (rec_len=0)",
                ));
            }

            if header.ino == 0 {
                // Free entry slot, can we fit?
                if header.rec_len >= required_len {
                    // Fit!
                    header.ino = target_ino;
                    header.name_len = name_bytes.len() as u8;
                    header.file_type = file_type;

                    let mut entry_bytes = vec![0u8; header.rec_len as usize];
                    entry_bytes[0..16].copy_from_slice(bytemuck::bytes_of(&header));
                    entry_bytes[16..16 + name_bytes.len()].copy_from_slice(name_bytes);

                    FileManager::write_file(
                        ctx,
                        bg_desc,
                        blocks_per_group,
                        checksum_tree_root,
                        &cctx,
                        inode,
                        offset as u64,
                        &entry_bytes,
                    )?;
                    return Ok(());
                }
            } else {
                // Used entry slot
                let current_required = (16 + header.name_len as u16 + 7) & !7;
                let remainder = header.rec_len - current_required;

                if remainder >= required_len {
                    // Split this entry
                    header.rec_len = current_required;

                    let mut current_bytes = vec![0u8; current_required as usize];
                    current_bytes[0..16].copy_from_slice(bytemuck::bytes_of(&header));
                    let name_end = 16 + header.name_len as usize;
                    current_bytes[16..name_end]
                        .copy_from_slice(&data[offset + 16..offset + name_end]);
                    FileManager::write_file(
                        ctx,
                        bg_desc,
                        blocks_per_group,
                        checksum_tree_root,
                        &cctx,
                        inode,
                        offset as u64,
                        &current_bytes,
                    )?;

                    // New entry
                    let new_header = DirEntryHeader {
                        ino: target_ino,
                        rec_len: remainder,
                        name_len: name_bytes.len() as u8,
                        file_type,
                        padding: 0,
                    };

                    let mut new_bytes = vec![0u8; remainder as usize];
                    new_bytes[0..16].copy_from_slice(bytemuck::bytes_of(&new_header));
                    new_bytes[16..16 + name_bytes.len()].copy_from_slice(name_bytes);

                    FileManager::write_file(
                        ctx,
                        bg_desc,
                        blocks_per_group,
                        checksum_tree_root,
                        &cctx,
                        inode,
                        (offset + current_required as usize) as u64,
                        &new_bytes,
                    )?;
                    return Ok(());
                }
            }

            offset += header.rec_len as usize;
        }

        // Append at the end
        let new_header = DirEntryHeader {
            ino: target_ino,
            rec_len: required_len,
            name_len: name_bytes.len() as u8,
            file_type,
            padding: 0,
        };

        let mut new_bytes = vec![0u8; required_len as usize];
        new_bytes[0..16].copy_from_slice(bytemuck::bytes_of(&new_header));
        new_bytes[16..16 + name_bytes.len()].copy_from_slice(name_bytes);

        FileManager::write_file(
            ctx,
            bg_desc,
            blocks_per_group,
            checksum_tree_root,
            &cctx,
            inode,
            inode.size,
            &new_bytes,
        )?;
        Ok(())
    }

    pub fn remove_entry(
        ctx: &mut TxContext,
        bg_desc: &BlockGroupDescriptor,
        blocks_per_group: u32,
        checksum_tree_root: u64,
        bad_blocks_root: u64,
        inode: &mut Inode,
        name: &str,
    ) -> Result<Option<u64>> {
        let name_bytes = name.as_bytes();
        let cctx = BlockCipherContext::none();
        let data = FileManager::read_file(
            ctx,
            checksum_tree_root,
            bad_blocks_root,
            &cctx,
            inode,
            0,
            inode.size,
        )?;
        let mut offset = 0;

        while offset < data.len() {
            if offset + 16 > data.len() {
                break;
            }
            let header_bytes = &data[offset..offset + 16];
            let mut header: DirEntryHeader = bytemuck::pod_read_unaligned(header_bytes);

            if header.rec_len == 0 {
                break;
            }

            if header.ino != 0 && header.name_len as usize == name_bytes.len() {
                let name_start = offset + 16;
                let name_end = name_start + header.name_len as usize;

                if &data[name_start..name_end] == name_bytes {
                    let target_ino = header.ino;
                    // Mark as free by setting ino to 0
                    header.ino = 0;

                    let mut entry_bytes = vec![0u8; header.rec_len as usize];
                    entry_bytes[0..16].copy_from_slice(bytemuck::bytes_of(&header));

                    FileManager::write_file(
                        ctx,
                        bg_desc,
                        blocks_per_group,
                        checksum_tree_root,
                        &cctx,
                        inode,
                        offset as u64,
                        &entry_bytes,
                    )?;
                    return Ok(Some(target_ino));
                }
            }
            offset += header.rec_len as usize;
        }

        Ok(None)
    }
}
