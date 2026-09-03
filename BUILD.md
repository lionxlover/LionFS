# Building LionFS

LionFS 2.0 builds from one code base on Linux, macOS, and Windows.

## Prerequisites

- **Rust** 1.75+ (`rustup` recommended). The toolchain is pinned as a
  floor in `Cargo.toml` (`rust-version`).
- **Linux**: nothing else for the default build. FUSE *mounting*
  needs `libfuse` for the kernel side (distro package `fuse3` or
  `libfuse-dev`); `cargo test` does not need it.
- **macOS**: nothing for the default build; FUSE *mounting* needs
  [macFUSE](https://osxfuse.github.io/).
- **Windows**: only the Rust toolchain (MSVC target). The PAL uses raw
  FFI — zero external crates.

## Commands

```bash
cargo build --release                        # portable, every OS
cargo build --release --features io_uring   # Linux fast path
cargo test                                  # 462 tests, every OS
cargo test --features io_uring              # same suite + ring tests
cargo clippy --lib --bins -- -D warnings    # lint gate (CI parity)
cargo fmt                                   # format
cargo bench                                 # criterion benches
```

## Feature flags

| Feature | Default | Effect |
|---|---|---|
| `io_uring` | off | Compiles the Linux io_uring engine backend. Without it (or on non-Linux, or where the kernel refuses the ring) the engine uses the threaded backend — correct, slower, logged. |

## Targets of interest

| Binary | Purpose |
|---|---|
| `mkfs_lfs` | format an image/device (optionally a RAID pool) |
| `mount_lfs` | mount via FUSE (unix) |
| `lfs_palinfo` | platform capability report + PAL self-test |
| `lfs_engine` | I/O engine benchmark (backend-aware) |
| `lfs_zns` | ZNS zone-append simulation + media policy matrix |
| `lfs_ioperf` | the 1.x in-process core benchmark harness |
| (30+ more) | scrub, verify, repair, pool, raid, telemetry, … see `tools/` |

## Cross-compiling

The PAL keeps the core platform-neutral, so cross builds are standard
cargo:

```bash
rustup target add x86_64-pc-windows-gnu
cargo build --target x86_64-pc-windows-gnu
```

## Docker

```bash
docker build -t lionfs-dev .
docker run --rm -v "$PWD":/src lionfs-dev cargo test
```
