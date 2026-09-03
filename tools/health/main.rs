use std::env;

fn main() {
    let args: Vec<String> = env::args().collect();
    let mount_point = if args.len() > 1 {
        &args[1]
    } else {
        "/mnt/lion"
    };
    let health_file = format!("{}/.lfs_health", mount_point);

    match std::fs::read_to_string(&health_file) {
        Ok(report) => print!("{}", report),
        Err(e) => {
            eprintln!(
                "Failed to read health report from {} (is LionFS mounted?): {}",
                health_file, e
            );
            std::process::exit(1);
        }
    }
}
