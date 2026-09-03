# Contributing to LionFS

First off, thank you for considering contributing to LionFS. It's people like you that make LionFS such a great storage system.

## Where do I go from here?

If you've noticed a bug or have a feature request, make sure to check our [Issues](https://github.com/lionxlover/lionfs/issues) page to see if someone else in the community has already created a ticket. If not, go ahead and make one!

## Fork & create a branch

If this is something you think you can fix, then fork LionFS and create a branch with a descriptive name.

## Get the test suite running

The 2.0 tree is cross-platform (Linux, macOS, Windows). Prerequisites:
- Rust 1.75+ (latest stable recommended)
- Linux: nothing for tests; `libfuse3-dev` only for mount experiments
- macOS: [macFUSE](https://osxfuse.github.io/) only for mount experiments
- Windows: only the Rust toolchain (MSVC)

To build and run tests:
```bash
cargo test                      # portable suite (462 tests)
cargo test --features io_uring  # Linux: with the ring backend
cargo build --all-targets
cargo clippy --lib --bins -- -D warnings   # the CI lint gate
```

## Implement your fix or feature

At this point, you're ready to make your changes! Feel free to ask for help; everyone is a beginner at first 😸

## Code Style

- Use `cargo fmt` before committing.
- Ensure `cargo clippy` emits zero warnings (`cargo clippy --all-targets --all-features -- -D warnings`).
- Stick to safe Rust wherever possible. Unsafe code must be extensively commented and isolated to the lowest possible tier of the `disk` or `ondisk` hierarchy.

## Pull Request Process

1. Ensure any install or build dependencies are removed before the end of the layer when doing a build.
2. Update the README.md with details of changes to the interface, this includes new environment variables, exposed ports, useful file locations and container parameters.
3. Increase the version numbers in any examples files and the README.md to the new version that this Pull Request would represent.
4. The PR will be merged once you have the sign-off of at least one core maintainer.

## 2.0-specific rules (the short list)

1. **No platform conditionals outside `src/pal/`** (and the `vfs`
   bridges). See PORTING.md.
2. **No `libc::` in core modules** — constants come from `pal::posix`.
3. **Every unsafe block carries a SAFETY comment** — the PAL's FFI in
   particular.
4. **Numbers need commands.** Any performance claim in docs must come
   with the runnable command that produced it (the RFC-002 honesty
   rule).
5. **Tests travel with the module.** New write-path code ships with
   new kill-point/failure cases, listed in the PR (RFC-002 §9.5).
