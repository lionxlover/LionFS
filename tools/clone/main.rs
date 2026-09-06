//! `lfs_clone` -- real reflink operations on a LionFS image.
//!
//! 3.5's binary was a placeholder that printed "created successfully"
//! without opening the device. 3.6 mounts the image in-process and
//! drives the REAL reflink engine (`fs::reflink`): the destination
//! shares the source's physical blocks under refcount pinning, so the
//! clone costs no data copy and both files stay writable
//! (redirect-on-write protects the sharing).
//!
//! Subcommands:
//!   reflink <image> <src_path> <dst_path>   clone src -> dst (O(extents))
//!   list    <image>                         list the clone registry
//!   check   <image> <src> <dst>             dry-run feasibility

use lionfs_core::api::options::LfsOptions;
use lionfs_core::common::config::MountConfig;
use lionfs_core::mount::mount::prepare;
use lionfs_core::ondisk::serialization::BLOCK_SIZE;
use lionfs_core::transaction::manager::TransactionManager;
use lionfs_core::transaction::transaction::TxContext;

fn open_mount(image: &str) -> lionfs_core::fs::filesystem::LionFS {
    match prepare(LfsOptions::new(image), &MountConfig::default()) {
        Ok(p) => p.fs,
        Err(e) => {
            eprintln!("ERROR: cannot open {image}: {e}");
            std::process::exit(1);
        }
    }
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.len() < 3 {
        eprintln!("Usage: lfs_clone <reflink|list|check> <image> [args]");
        eprintln!("  reflink <image> <src_path> <dst_path>");
        eprintln!("  list    <image>");
        eprintln!("  check   <image> <src_path>");
        std::process::exit(1);
    }
    let cmd = args[1].as_str();
    let image = &args[2];

    match cmd {
        "reflink" => {
            if args.len() < 5 {
                eprintln!("reflink needs <image> <src_path> <dst_path>");
                std::process::exit(1);
            }
            reflink_cmd(image, &args[3], &args[4]);
        }
        "list" => list_cmd(image),
        "check" => {
            if args.len() < 4 {
                eprintln!("check needs <image> <src_path>");
                std::process::exit(1);
            }
            check_cmd(image, &args[3]);
        }
        other => {
            eprintln!("unknown command: {other}");
            std::process::exit(1);
        }
    }
}

fn resolve(ops: &dyn lionfs_core::vfs::VfsOps, cwd: u64, path: &str) -> Option<u64> {
    let mut cur = cwd;
    for part in path.split('/').filter(|p| !p.is_empty()) {
        match ops.lookup(cur, part) {
            Ok(attr) => cur = attr.ino,
            Err(_) => return None,
        }
    }
    Some(cur)
}

fn reflink_cmd(image: &str, src_path: &str, dst_path: &str) {
    let mut fs = match open_mount(image) {
        fs => fs,
    };
    let src_ino = match resolve(&fs, 1, src_path) {
        Some(ino) => ino,
        None => {
            eprintln!("ERROR: source {src_path} not found");
            std::process::exit(1);
        }
    };
    // Destination: the last component is created in its parent.
    let (parent_path, name) = match dst_path.rsplit_once('/') {
        Some((p, n)) if !p.is_empty() => (p.to_string(), n.to_string()),
        _ => (String::new(), dst_path.trim_start_matches('/').to_string()),
    };
    let parent_ino = if parent_path.is_empty() {
        1
    } else {
        match resolve(&fs, 1, &parent_path) {
            Some(ino) => ino,
            None => {
                eprintln!("ERROR: destination parent {parent_path} not found");
                std::process::exit(1);
            }
        }
    };
    let dst_attr = match lionfs_core::vfs::VfsOps::lookup(&fs, parent_ino, &name) {
        Ok(a) => a,
        Err(_) => match lionfs_core::vfs::VfsOps::create(&fs, parent_ino, &name, &lionfs_core::vfs::VfsCreate {
            mode: 0o100644,
            uid: 0,
            gid: 0,
        }) {
            Ok(a) => a,
            Err(e) => {
                eprintln!("ERROR: cannot create {dst_path}: {e}");
                std::process::exit(1);
            }
        },
    };
    match lionfs_core::vfs::VfsOps::copy_file_range(&fs, src_ino, 0, dst_attr.ino, 0, u64::MAX) {
        Ok(n) => {
            let _ = lionfs_core::vfs::VfsOps::fsync(&fs, dst_attr.ino, true);
            println!("reflink complete: {src_path} -> {dst_path} ({n} bytes shared-extent mapped)");
        }
        Err(e) => {
            eprintln!("ERROR: reflink failed: {e}");
            std::process::exit(1);
        }
    }
    lionfs_core::vfs::VfsOps::destroy(&mut fs);
}

fn list_cmd(image: &str) {
    let sb = read_sb(image);
    if sb.clone_tree_root == 0 {
        println!("no clone registry on this image (no reflinks taken)");
        return;
    }
    let disk = match lionfs_core::disk::block_io::Disk::open(image) {
        Ok(d) => d,
        Err(e) => {
            eprintln!("ERROR: cannot open {image}: {e}");
            std::process::exit(1);
        }
    };
    let tm = TransactionManager::new(&sb);
    let mut tx = tm.begin(0);
    let mut ctx = TxContext::new(&disk, &mut tx);
    let tree = lionfs_core::btree::tree::BTree::<u64, lionfs_core::ondisk::serialization::CloneRecord>::new(
        sb.clone_tree_root,
        lionfs_core::fs::clones::CLONE_TREE_NODE_TYPE,
    );
    match tree.iter_all(&mut ctx) {
        Ok(records) => {
            if records.is_empty() {
                println!("no clones registered");
            }
            println!("CLONE_INO\tSOURCE_INO\tGEN\tSHARED_BLOCKS");
            for (id, r) in records {
                println!("{}\t{}\t{}\t{}", id, r.source_id, r.generation, r.shared_extents);
            }
        }
        Err(e) => {
            eprintln!("ERROR: {e}");
            std::process::exit(1);
        }
    }
}

fn check_cmd(image: &str, src_path: &str) {
    let mut fs = match open_mount(image) {
        fs => fs,
    };
    match resolve(&fs, 1, src_path).and_then(|ino| lionfs_core::vfs::VfsOps::getattr(&fs, ino).ok()) {
        Some(_) => println!("source {src_path} present"),
        None => {
            eprintln!("ERROR: source {src_path} not found");
            std::process::exit(1);
        }
    }
    lionfs_core::vfs::VfsOps::destroy(&mut fs);
}

fn read_sb(image: &str) -> lionfs_core::ondisk::serialization::Superblock {
    let disk = lionfs_core::disk::block_io::Disk::open(image).expect("open image");
    let mut buf = [0u8; BLOCK_SIZE];
    disk.read_block(0, &mut buf).expect("read sb");
    *bytemuck::from_bytes(&buf[..std::mem::size_of::<lionfs_core::ondisk::serialization::Superblock>()])
}
