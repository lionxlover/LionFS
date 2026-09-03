use std::env;
use std::process;

fn main() {
    let args: Vec<String> = env::args().collect();
    if args.len() < 2 {
        eprintln!("Usage: lfs_scrub <action> [options]");
        eprintln!("Actions: start, status, pause, resume, cancel");
        process::exit(1);
    }

    let action = &args[1];
    let mount_point = if args.len() > 2 {
        &args[2]
    } else {
        "/mnt/lion"
    };

    let control_file = format!("{}/.lfs_scrub", mount_point);

    if action == "status" {
        match std::fs::read_to_string(&control_file) {
            Ok(status) => print!("{}", status),
            Err(e) => eprintln!(
                "Failed to read scrub status (is LionFS mounted at {}?): {}",
                mount_point, e
            ),
        }
        return;
    }

    match action.as_str() {
        "start" | "pause" | "resume" | "stop" => match std::fs::write(&control_file, action) {
            Ok(_) => println!("Command '{}' sent to scrubber.", action),
            Err(e) => eprintln!(
                "Failed to send command (is LionFS mounted at {}?): {}",
                mount_point, e
            ),
        },
        _ => {
            eprintln!("Unknown action: {}", action);
            process::exit(1);
        }
    }
}
