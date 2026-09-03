# LionFS 2.0 task runner (https://github.com/casey/just)
# Fallback: every command is plain cargo, see BUILD.md.

default:
    @just --list

# Portable build (every OS)
build:
    cargo build

# Linux fast-path build
build-uring:
    cargo build --features io_uring

# Release build with the ring
release:
    cargo build --release --features io_uring

# Full test suite (portable)
test:
    cargo test

# Full test suite with the io_uring backend
test-uring:
    cargo test --features io_uring

# Lint gate (CI parity)
lint:
    cargo fmt -- --check
    cargo clippy --lib --bins -- -D warnings
    cargo clippy --lib --features io_uring -- -D warnings

# Format the tree
fmt:
    cargo fmt

# Criterion benchmarks
bench:
    cargo bench

# Platform capability report + PAL self-test
palinfo:
    cargo run --bin lfs_palinfo

# Engine micro-benchmark: just engine-bench 4096 64 3
engine-bench block="4096" qd="64" rounds="3":
    cargo run --features io_uring --bin lfs_engine -- {{block}} {{qd}} {{rounds}}

# ZNS placement simulation + policy matrix
zns:
    cargo run --bin lfs_zns -- sim
    cargo run --bin lfs_zns -- report

# Clean everything
clean:
    cargo clean
