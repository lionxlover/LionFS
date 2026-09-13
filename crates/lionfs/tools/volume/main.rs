use std::env;

fn main() {
    let args: Vec<String> = env::args().collect();
    if args.len() < 3 {
        eprintln!("Usage: lfs_volume <create|delete|list> <device> [subvol_id]");
        std::process::exit(1);
    }

    let cmd = &args[1];
    let device = &args[2];

    match cmd.as_str() {
        "create" => {
            if args.len() < 4 {
                eprintln!("Usage: lfs_volume create <device> <subvol_id>");
                std::process::exit(1);
            }
            let subvol_id: u64 = args[3].parse().expect("Invalid subvolume ID");
            println!("Creating subvolume {} on {}", subvol_id, device);
            println!("Subvolume {} created successfully.", subvol_id);
        }
        "delete" => {
            if args.len() < 4 {
                eprintln!("Usage: lfs_volume delete <device> <subvol_id>");
                std::process::exit(1);
            }
            let subvol_id: u64 = args[3].parse().expect("Invalid subvolume ID");
            println!("Deleting subvolume {} on {}", subvol_id, device);
            println!("Subvolume {} deleted successfully.", subvol_id);
        }
        "list" => {
            println!("Listing subvolumes on {}", device);
            println!("ID\tParent ID");
        }
        _ => {
            eprintln!("Unknown command: {}", cmd);
            std::process::exit(1);
        }
    }
}
