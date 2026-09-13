use lionfs_core::debug::dump::{format_inode, format_superblock};
use lionfs_core::disk::block_io::Disk;
use lionfs_core::ondisk::serialization::{Superblock, BLOCK_SIZE};
use std::env;

fn main() {
    let args: Vec<String> = env::args().collect();
    if args.len() < 2 {
        eprintln!("Usage: lfs_dump <image_file> [inode_number]");
        std::process::exit(1);
    }

    let image_file = &args[1];
    let disk = Disk::open(image_file).expect("Failed to open image file");

    let mut buf = [0u8; BLOCK_SIZE];
    disk.read_block(0, &mut buf)
        .expect("Failed to read superblock");
    let sb: Superblock = *bytemuck::from_bytes(&buf);

    println!("=== LionFS Superblock ===");
    println!("{}", format_superblock(&sb));

    if let Some(ino_str) = args.get(2) {
        let ino: u64 = ino_str
            .parse()
            .expect("inode_number must be a positive integer");
        let tm = lionfs_core::transaction::manager::TransactionManager::new(&sb);
        let mut tx = tm.begin(0);
        let mut ctx = lionfs_core::transaction::transaction::TxContext::new(&disk, &mut tx);
        match lionfs_core::inode::manager::InodeManager::read_inode(
            &mut ctx,
            sb.inode_tree_root,
            ino,
        ) {
            Ok(inode) => {
                println!("\n=== Inode {ino} ===");
                println!("{}", format_inode(&inode));
            }
            Err(e) => {
                eprintln!("Failed to read inode {ino}: {e}");
                std::process::exit(1);
            }
        }
    }
}
