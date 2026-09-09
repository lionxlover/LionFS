//! `lfs_replicate` -- snapshot replication (3.6: REAL; the 3.5 binary
//! printed a banner and exited).
//!
//!   send <image> <snapshot_id> <stream_file>
//!       Serialize a snapshot's FROZEN view into a portable, checksummed
//!       stream file (every block verified against the snapshot's own
//!       checksum tree while it is read).
//!
//!   recv <image> <stream_file> [--no-snapshot]
//!       Replay a stream into the target image through the ordinary
//!       POSIX write path; every file's SHA-256 and the manifest are
//!       verified; the received state is then frozen as a snapshot
//!       with the SOURCE's id (unless --no-snapshot).
//!
//!   list <image>
//!       List the snapshots available to send.
//!
//! The stream is self-describing ("LFSS" v1) and chunked -- see
//! `src/fs/replication.rs` for the wire format and the honest limits
//! (no incremental deltas yet, no symlink records, encrypted inodes
//! refused).

use std::io::Write;

use lionfs_core::disk::block_io::Disk;
use lionfs_core::fs::replication::{recv_stream, SendStream};
use lionfs_core::ondisk::serialization::{Superblock, BLOCK_SIZE, LIONFS_MAGIC};
use lionfs_core::transaction::manager::TransactionManager;
use lionfs_core::transaction::transaction::TxContext;

fn read_sb(disk: &Disk) -> Superblock {
    let mut buf = [0u8; BLOCK_SIZE];
    disk.read_block(0, &mut buf).expect("Failed to read superblock");
    let sb: Superblock = *bytemuck::from_bytes(&buf[..std::mem::size_of::<Superblock>()]);
    if sb.magic != LIONFS_MAGIC {
        eprintln!("ERROR: not a LionFS image (bad magic)");
        std::process::exit(1);
    }
    sb
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.len() < 3 {
        eprintln!("Usage: lfs_replicate <send|recv|list> <image> [args]");
        eprintln!("  send <image> <snapshot_id> <stream_file>");
        eprintln!("  recv <image> <stream_file> [--no-snapshot]");
        eprintln!("  list <image>");
        std::process::exit(1);
    }
    let cmd = args[1].as_str();
    let image = &args[2];

    match cmd {
        "send" => {
            if args.len() < 5 {
                eprintln!("send needs <image> <snapshot_id> <stream_file>");
                std::process::exit(1);
            }
            let snap_id: u64 = args[3].parse().expect("snapshot id must be numeric");
            let disk = Disk::open(image).expect("open image");
            let sb = read_sb(&disk);
            let count = {
                let file = std::fs::File::create(&args[4]).expect("create stream file");
                let mut writer = std::io::BufWriter::new(file);
                let mut stream =
                    SendStream::open(&disk, &sb, snap_id, &mut writer).expect("open send stream");
                let n = stream.send_all().expect("send failed");
                writer.flush().expect("flush stream");
                n
            };
            println!("sent {count} file(s) from snapshot {snap_id} to {}", args[4]);
            let _ = disk.sync();
        }
        "recv" => {
            if args.len() < 4 {
                eprintln!("recv needs <image> <stream_file>");
                std::process::exit(1);
            }
            let no_snapshot = args.iter().any(|a| a == "--no-snapshot");
            let mut fs = match lionfs_core::mount::mount::prepare(
                lionfs_core::api::options::LfsOptions::new(image),
                &lionfs_core::common::config::MountConfig::default(),
            ) {
                Ok(p) => p.fs,
                Err(e) => {
                    eprintln!("ERROR: cannot open {image}: {e}");
                    std::process::exit(1);
                }
            };
            let sb = {
                let mut buf = [0u8; BLOCK_SIZE];
                let _ = fs.core.disk.read_block(0, &mut buf);
                *bytemuck::from_bytes(&buf[..std::mem::size_of::<Superblock>()])
            };
            let file = std::fs::File::open(&args[3]).expect("open stream file");
            let mut reader = std::io::BufReader::new(file);
            // The receiver needs a mutable Disk handle for the final
            // snapshot record; borrow the core's Arc disk is shared, so
            // open a second read-only handle is WRONG for writing...
            // instead the snapshot record goes through a fresh handle
            // on the same file AFTER unmount (single-writer discipline).
            let summary =
                recv_stream(&mut reader, &fs, &sb, &fs.core.disk, !no_snapshot)
                    .expect("recv failed");
            lionfs_core::vfs::VfsOps::destroy(&mut fs);
            println!(
                "recv complete: {} file(s), {} dir(s), {} bytes; snapshot recorded: {:?}",
                summary.files, summary.dirs, summary.bytes, summary.snapshot_recorded
            );
        }
        "list" => {
            let disk = Disk::open(image).expect("open image");
            let sb = read_sb(&disk);
            if sb.snapshot_tree_root == 0 {
                println!("no snapshot registry on this image");
                return;
            }
            let tm = TransactionManager::new(&sb);
            let mut tx = tm.begin(0);
            let snap = lionfs_core::fs::snapshots::SnapshotManager::new(sb.snapshot_tree_root);
            let mut ctx = TxContext::new(&disk, &mut tx);
            match snap.list_snapshots(&mut ctx) {
                Ok(records) => {
                    if records.is_empty() {
                        println!("no snapshots");
                    }
                    println!("ID\tCREATED\tBARRIER");
                    for r in records {
                        println!("{}\t{}\t{}", r.id, r.creation_time, r.generation);
                    }
                }
                Err(e) => {
                    eprintln!("ERROR: {e}");
                    std::process::exit(1);
                }
            }
        }
        other => {
            eprintln!("unknown command: {other}");
            std::process::exit(1);
        }
    }
}
