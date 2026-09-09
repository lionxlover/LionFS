fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.len() > 1 && (args[1] == "--version" || args[1] == "-v") {
        println!("LionFS {} ({})", lionfs_core::VERSION, lionfs_core::EDITION);
        return;
    }

    println!("🦁 LionFS {} ({})", lionfs_core::VERSION, lionfs_core::EDITION);
    println!("Universal Line-Rate Storage Engine based on Decoupled Structural State Machines (DSSM)");
    println!("\nActive Architectural Pillars:");
    for pillar in lionfs_core::theory::PILLARS {
        println!("  • {}", pillar);
    }
    println!("\nFor tools and utilities, use:");
    println!("  mkfs.lionfs   - Format storage devices/images");
    println!("  mount.lionfs  - Mount LionFS filesystems via FUSE");
    println!("  lfs-admin     - Administrative monitoring and control");
}

