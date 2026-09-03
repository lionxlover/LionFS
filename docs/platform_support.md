# Platform Support Matrix (LionFS 2.0)

LionFS 2.0 is one code base targeting three operating systems. The
[`pal`](../src/pal/) module is the only place platform differences are
visible; everything above it is platform-neutral by construction. This
document is the support contract: what works, what degrades, and how.

| Capability | Linux | macOS | Windows |
|---|---|---|---|
| **Core engine** (mkfs, mount, read/write, RAID, compression, checksums, transactions) | full | full | full |
| **Mount backend** | kernel FUSE (via fuser 0.12) | macFUSE (via fuser 0.12) | WinFsp bridge (RFC-003 §5; design complete, binding pending) |
| **I/O engine fast path** | io_uring (feature `io_uring`) | threaded backend | threaded backend (IOCP design in RFC-003 §4) |
| **I/O engine floor** | threaded | threaded | threaded |
| **Positioned I/O** | `pread`/`pwrite` | `pread`/`pwrite` | `seek_read`/`seek_write` |
| **Data sync** | `fdatasync(2)` | `fcntl(F_FULLFSYNC)` (+ `fsync` fallback) | `FlushFileBuffers` |
| **Geometry probing** | `BLKGETSIZE64`/`BLKSSZGET`/`BLKPBSZGET`/`BLKOPTGET` | `DKIOCGETBLOCKCOUNT`/`DKIOCGETBLOCKSIZE`/`DKIOCGETPHYSICALBLOCKSIZE` | `IOCTL_DISK_GET_LENGTH_INFO` (raw FFI) |
| **CSPRNG** | `getrandom(2)` syscall, `/dev/urandom` fallback | `getentropy(2)` (256B chunks) | `ProcessPrng`, `RtlGenRandom` fallback |
| **Waker primitive** | `eventfd` | self-pipe | condvar + generation counter |
| **ZNS zone policies** | real device report (RECONCILE) | simulated (image files) | simulated (image files) |
| **CXL/CLWB hint** | x86-64 `clwb`+`sfence` via CPUID probe | no-op (returns false) | no-op (returns false) |
| **Windows external crates needed** | — | — | **zero** (raw `extern "system"` FFI) |

## Build

```bash
# Linux (full)
cargo build --release
cargo test

# Linux with the io_uring fast path
cargo build --release --features io_uring
cargo test --features io_uring

# macOS
cargo build --release && cargo test
# (mounting requires macFUSE installed: https://osxfuse.github.io)

# Windows (MSVC toolchain)
cargo build --release
cargo test
```

`lfs_palinfo` (built on every platform) prints the live capability
report and self-tests the PAL primitives:

```bash
./target/release/lfs_palinfo
```

## Porting rules

1. **No new `#[cfg(target_os)]` outside `src/pal/`** (exception: the
   FUSE bridge, which is `#[cfg(unix)]` at the `vfs` boundary). If a
   feature needs an OS branch, the branch belongs in the PAL behind a
   portable function.
2. **No `libc::` in core modules.** Errno and mode constants come from
   `pal::posix` (fixed ABI values; see that module's docs for why they
   are constants rather than libc references).
3. **No `std::os::unix`/`std::os::windows` imports outside `src/pal/`
   and the `vfs` bridges.**
4. **The threaded engine is the correctness floor.** Any fast path must
   degrade to it — at build time (feature off), at ring setup (kernel
   refuses), or at runtime (op unsupported) — with a logged line, never
   a failed mount.
5. **Windows keeps zero external crates.** New Windows needs go through
   raw `extern "system"` FFI in `src/pal/`, documented with SAFETY
   comments, not through `windows-sys`.

## Known degradations (stated, not hidden)

- **macOS `fsync` is not a durability barrier** on APFS; the PAL
  therefore uses `F_FULLFSYNC` for `sync_data` and pays its cost. This
  is load-bearing for the crash-consistency model (RFC-002 §5.1).
- **Windows has no `fdatasync`**; `sync_data` maps to
  `FlushFileBuffers` (full flush semantics). Correct, marginally more
  expensive than Linux's data-only variant.
- **Windows positioned I/O mutates the file cursor** under the hood
  (`seek_read`/`seek_write`); the engine never issues concurrent
  positioned I/O on one handle (per-shard ownership), so this is correct
  by construction — the PAL documents the rule.
- **The WinFsp bridge is not yet linked**: mounting on Windows requires
  the RFC-003 §5 deliverable. The engine, mkfs, and every tool run;
  only the mount syscall path is pending.
