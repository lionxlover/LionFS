//! `lfs_raid` — Reed-Solomon erasure-coded volume tool (LionFS 8.0:
//! the HFS-merge capability behind a real tool — this was a banner
//! stub through 7.1).
//!
//! Encodes a file into `n` fragments of which any `k` rebuild it
//! (GF(256) systematic Reed-Solomon via the pool's own codec, with
//! per-fragment CRC framing), reconstructs from surviving fragments,
//! and verifies fragment integrity.
//!
//! Usage:
//!   lfs_raid encode <file> [--n N] [--k K] [--out DIR] [--prefix NAME]
//!   lfs_raid reconstruct <frag1> <frag2> ... -o <output>
//!   lfs_raid verify <frag1> [frag2 ...]

use lionfs_core::pool::ec_volume::{
    encode_to_dir, read_fragment, reconstruct, verify_fragments, EcParams,
};
use std::path::PathBuf;

fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.len() < 2 {
        eprintln!("LionFS erasure coding {} ({})", lionfs_core::VERSION, lionfs_core::EDITION);
        eprintln!("Usage:");
        eprintln!("  lfs_raid encode <file> [--n N] [--k K] [--out DIR] [--prefix NAME]");
        eprintln!("  lfs_raid reconstruct <frag...> -o <output>     (any k fragments rebuild)");
        eprintln!("  lfs_raid verify <frag...>                       (CRC-verify fragments)");
        eprintln!();
        eprintln!("  Default profile: n=6 k=4 (tolerates any 2 fragment losses).");
        std::process::exit(1);
    }

    match args[1].as_str() {
        "encode" => {
            if args.len() < 3 {
                eprintln!("ERROR: encode needs <file>");
                std::process::exit(1);
            }
            let file = &args[2];
            let mut n = 6usize;
            let mut k = 4usize;
            let mut out_dir: Option<PathBuf> = None;
            let mut prefix: Option<String> = None;
            let mut i = 3;
            while i < args.len() {
                match args[i].as_str() {
                    "--n" => {
                        i += 1;
                        n = args.get(i).and_then(|s| s.parse().ok()).unwrap_or(0);
                    }
                    "--k" => {
                        i += 1;
                        k = args.get(i).and_then(|s| s.parse().ok()).unwrap_or(0);
                    }
                    "--out" => {
                        i += 1;
                        out_dir = args.get(i).map(PathBuf::from);
                    }
                    "--prefix" => {
                        i += 1;
                        prefix = args.get(i).cloned();
                    }
                    other => {
                        eprintln!("ERROR: unknown option {other:?}");
                        std::process::exit(1);
                    }
                }
                i += 1;
            }
            let params = match EcParams::new(n, k) {
                Ok(p) => p,
                Err(e) => {
                    eprintln!("ERROR: {e}");
                    std::process::exit(1);
                }
            };
            let data = match std::fs::read(file) {
                Ok(d) => d,
                Err(e) => {
                    eprintln!("ERROR: cannot read {file}: {e}");
                    std::process::exit(1);
                }
            };
            let out = out_dir.unwrap_or_else(|| {
                let p = PathBuf::from(file);
                p.with_extension("")
                    .to_path_buf()
            });
            let name = prefix.unwrap_or_else(|| {
                std::path::Path::new(file)
                    .file_name()
                    .map(|s| s.to_string_lossy().into_owned())
                    .unwrap_or_else(|| "blob".into())
            });
            let frags = match encode_to_dir(&data, &params, &out, &name) {
                Ok(f) => f,
                Err(e) => {
                    eprintln!("ERROR: {e}");
                    std::process::exit(1);
                }
            };
            println!(
                "encoded {} bytes into {} fragments (n={} k={}, tolerates {} losses):",
                data.len(),
                frags.len(),
                params.n,
                params.k,
                params.tolerates()
            );
            for f in &frags {
                println!("  {}", f.display());
            }
        }
        "reconstruct" => {
            let mut frags: Vec<PathBuf> = Vec::new();
            let mut output: Option<PathBuf> = None;
            let mut i = 2;
            while i < args.len() {
                match args[i].as_str() {
                    "-o" | "--output" => {
                        i += 1;
                        output = args.get(i).map(PathBuf::from);
                    }
                    other => frags.push(PathBuf::from(other)),
                }
                i += 1;
            }
            let output = match output {
                Some(o) => o,
                None => {
                    eprintln!("ERROR: reconstruct needs -o <output>");
                    std::process::exit(1);
                }
            };
            if frags.is_empty() {
                eprintln!("ERROR: reconstruct needs at least one fragment");
                std::process::exit(1);
            }
            let fragments: Vec<_> = frags
                .iter()
                .map(|p| match read_fragment(p) {
                    Ok(f) => f,
                    Err(e) => {
                        eprintln!("ERROR: {e}");
                        std::process::exit(1);
                    }
                })
                .collect();
            match reconstruct(&fragments) {
                Ok(data) => {
                    if let Err(e) = std::fs::write(&output, &data) {
                        eprintln!("ERROR: cannot write {}: {e}", output.display());
                        std::process::exit(1);
                    }
                    println!(
                        "reconstructed {} bytes from {} fragments into {}",
                        data.len(),
                        fragments.len(),
                        output.display()
                    );
                }
                Err(e) => {
                    eprintln!("ERROR: {e}");
                    std::process::exit(1);
                }
            }
        }
        "verify" => {
            let frags: Vec<PathBuf> = args[2..].iter().map(PathBuf::from).collect();
            if frags.is_empty() {
                eprintln!("ERROR: verify needs at least one fragment");
                std::process::exit(1);
            }
            let report = verify_fragments(&frags);
            println!(
                "{} fragments checked: {} healthy, {} corrupt",
                report.checked,
                report.healthy,
                report.corrupt.len()
            );
            for c in &report.corrupt {
                println!("  CORRUPT: {c}");
            }
            if !report.corrupt.is_empty() {
                std::process::exit(2);
            }
        }
        other => {
            eprintln!("unknown command: {other}");
            std::process::exit(1);
        }
    }
}
