use lionfs_core::disk::block_io::Disk;
use lionfs_core::ondisk::serialization::{Superblock, BLOCK_SIZE};
use std::env;

fn main() {
    let args: Vec<String> = env::args().collect();
    if args.len() < 2 {
        eprintln!("Usage: debug-lfs <image_file>");
        std::process::exit(1);
    }

    let image_file = &args[1];
    let disk = Disk::open(image_file).expect("Failed to open image file");

    let mut buf = [0u8; BLOCK_SIZE];
    disk.read_block(0, &mut buf)
        .expect("Failed to read superblock");

    let sb: Superblock = *bytemuck::from_bytes(&buf);

    println!("=== LionFS Superblock ===");
    println!("Magic: {:#018x}", sb.magic);
    println!("Version: {}", sb.version);
    println!("Block Size: {}", sb.block_size);
    println!("Total Blocks: {}", sb.total_blocks);
    println!("Free Blocks: {}", sb.free_blocks);
    println!("Inode Count: {}", sb.inode_count);
    println!("Root Inode: {}", sb.root_inode);
    println!("Bitmap Start Block: {}", sb.bitmap_start);
    println!("Inode Table Start Block: {}", sb.inode_table_start);
    println!("Data Region Start Block: {}", sb.data_region_start);
}
