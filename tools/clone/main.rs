use std::env;

fn main() {
    let args: Vec<String> = env::args().collect();
    if args.len() < 3 {
        eprintln!("Usage: lfs_clone <create|delete|list> <device> [clone_id] [source_id]");
        std::process::exit(1);
    }

    let cmd = &args[1];
    let device = &args[2];

    match cmd.as_str() {
        "create" => {
            if args.len() < 5 {
                eprintln!("Usage: lfs_clone create <device> <clone_id> <source_id>");
                std::process::exit(1);
            }
            let clone_id: u64 = args[3].parse().expect("Invalid clone ID");
            let source_id: u64 = args[4].parse().expect("Invalid source ID");
            println!(
                "Creating clone {} from {} on {}",
                clone_id, source_id, device
            );
            println!("Clone {} created successfully.", clone_id);
        }
        "delete" => {
            if args.len() < 4 {
                eprintln!("Usage: lfs_clone delete <device> <clone_id>");
                std::process::exit(1);
            }
            let clone_id: u64 = args[3].parse().expect("Invalid clone ID");
            println!("Deleting clone {} on {}", clone_id, device);
            println!("Clone {} deleted successfully.", clone_id);
        }
        "list" => {
            println!("Listing clones on {}", device);
            println!("ID\tSource ID\tGeneration");
        }
        _ => {
            eprintln!("Unknown command: {}", cmd);
            std::process::exit(1);
        }
    }
}
