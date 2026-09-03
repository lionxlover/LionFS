# Kernel Integration & FFI API Strategy

LionFS operates beautifully as a Userspace (FUSE) driver leveraging the `fuser` crate. However, its core is designed specifically for direct compilation into the Linux kernel using the modern `rust-for-linux` framework.

## 1. Native VFS Portability
The module `src/kernel/mod.rs` maps exactly to the C structures expected by the Linux VFS:
- `InodeOperations` maps to `struct inode_operations`
- `FileOperations` maps to `struct file_operations`
- `SuperOperations` maps to `struct super_operations`
- `AddressSpaceOperations` maps to `struct address_space_operations`

These structs are marked with `#[repr(C)]` and utilize strict manual memory padding. This guarantees **zero-copy transitions** between Rust and C data structures, allowing LionFS to be dropped directly into the Linux source tree as a native driver in the future without serialization overhead.

## 2. Lock-Free Safe ABI
Kernel concurrency requires extreme care. The LionFS Phase 11 optimizations transitioned the core engine to lock-free atomics and heavily utilizes RCU-style (Read-Copy-Update) semantics within its B+Trees. 

These properties map 1:1 with kernel-level RCU barriers and spinlocks, making LionFS inherently safe for highly contended SMP (Symmetric Multi-Processing) environments.

## 3. C-Compatible API (liblionfs)
For systems that prefer not to run FUSE but wish to access LionFS volumes, the `src/api/mod.rs` module exposes a highly stable, ABI-safe `#[no_mangle] extern "C"` interface.

```c
// Example Header Integration
struct LfsApiStatus {
    bool success;
    int32_t error_code;
};

extern const char* lfs_version();
extern LfsApiStatus lfs_mount_fuse(const char* device, const char* mount_point);
```

This ensures that storage administration GUIs, backup suites, and virtualization platforms can dynamically link to `liblionfs.so` directly, driving the filesystem at maximum performance directly from C, C++, Go, or Python.
