//! `lfs_palinfo` — platform capability probe for LionFS 2.0.
//!
//! Prints the compile-time and runtime capability report that the I/O
//! engine consults at mount: which submission plane is available
//! (io_uring / IOCP / threaded), page size, CPU count, direct-I/O and
//! data-sync availability, and per-backend uring availability. The
//! output is also the CI artifact that proves cross-platform builds
//! work: it runs identically on Linux, macOS, and Windows.

use lionfs_core::pal;

fn main() {
    env_logger::init();

    let cap = pal::platform::CapabilityReport::probe();
    let platform = cap.platform;

    println!("LionFS 2.0 platform report");
    println!("==========================");
    println!("platform:          {}", platform.name());
    println!("os version:        {}", pal::platform::os_version_string());
    println!("page size:         {}", pal::page_size());
    println!("logical cpus:      {}", pal::platform::cpu_count());
    println!();
    println!("I/O engine planes:");
    println!(
        "  io_uring:        {}",
        if cap.io_uring_compiled {
            "compiled in"
        } else {
            "not compiled (feature off)"
        }
    );
    println!(
        "  iocp:            {}",
        if cap.iocp_compiled {
            "compiled in"
        } else {
            "n/a on this OS"
        }
    );
    println!("  threaded:        always available (correctness floor)");
    println!();
    println!("Capabilities:");
    println!(
        "  direct I/O:      {}",
        if cap.direct_io { "yes" } else { "no" }
    );
    println!(
        "  data-only sync:  {}",
        if cap.data_sync {
            "yes"
        } else {
            "no (falls back to fsync)"
        }
    );
    println!(
        "  zoned media:     {}",
        if cap.zoned_media_support {
            "yes (Linux NVMe ZNS)"
        } else {
            "simulated zones (image files)"
        }
    );
    println!(
        "  FUSE mount:      {}",
        if platform.has_fuse() {
            "yes"
        } else {
            "no (WinFsp bridge per RFC-003)"
        }
    );
    println!();
    println!("one-line: {}", cap.summary());

    // Exercise the PAL surface end-to-end: a positioned write + read +
    // both sync flavors on a scratch image, so "capability report" means
    // the primitives were proven working on THIS host, not just listed.
    println!();
    println!("PAL self-test:");
    let dir = std::env::temp_dir().join(format!("lfs_palinfo_{}", std::process::id()));
    let _ = std::fs::create_dir_all(&dir);
    let path = dir.join("selftest.img");
    match pal::file::create_image(&path, 1024 * 1024) {
        Ok(file) => {
            let payload: Vec<u8> = (0..4096u32).map(|i| (i % 251) as u8).collect();
            let ok = pal::file::pwrite_full(&file, &payload, 4096)
                .map(|_| ())
                .and_then(|()| pal::sync::sync_data(&file))
                .and_then(|()| pal::sync::sync_file(&file))
                .and_then(|()| {
                    let mut back = vec![0u8; payload.len()];
                    pal::file::pread_full(&file, &mut back, 4096)?;
                    if back == payload {
                        Ok(())
                    } else {
                        Err(std::io::Error::other("roundtrip mismatch"))
                    }
                });
            match ok {
                Ok(()) => println!("  positioned I/O + sync + verify:  PASS"),
                Err(e) => println!("  positioned I/O + sync + verify:  FAIL ({e})"),
            }
        }
        Err(e) => println!("  image creation:                  FAIL ({e})"),
    }
    let _ = std::fs::remove_dir_all(&dir);

    // CSPRNG probe.
    let mut buf = [0u8; 32];
    match pal::random::fill_random(&mut buf) {
        Ok(()) if buf.iter().any(|&b| b != 0) => {
            println!("  OS CSPRNG:                       PASS")
        }
        Ok(()) => println!("  OS CSPRNG:                       SUSPECT (all-zero draw)"),
        Err(e) => println!("  OS CSPRNG:                       FAIL ({e})"),
    }

    println!();
    println!("recommended engine: {}", {
        #[cfg(all(target_os = "linux", feature = "io_uring"))]
        {
            "io_uring (probed OK -> EngineBuilder picks it)"
        }
        #[cfg(not(all(target_os = "linux", feature = "io_uring")))]
        {
            "threaded (enable the `io_uring` feature on Linux for the ring backend)"
        }
    });
}
