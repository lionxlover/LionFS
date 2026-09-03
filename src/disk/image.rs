//! Creating and growing sparse disk-image files -- the same operation
//! `Disk::create`/`create_pool` do inline via `File::set_len`, exposed
//! here as standalone helpers for tools that want to prepare image files
//! without going through the full `Disk` construction (e.g. a setup
//! script that pre-creates images before a batch `mkfs-lfs` run, or
//! growing an existing image ahead of a future online-resize feature).

use std::fs::{File, OpenOptions};
use std::io::Result;
use std::path::Path;

/// Creates a new sparse file of exactly `size_bytes`, failing if one
/// already exists at `path` (use `grow` to resize an existing image).
pub fn create_sparse<P: AsRef<Path>>(path: P, size_bytes: u64) -> Result<()> {
    let file = OpenOptions::new().write(true).create_new(true).open(path)?;
    file.set_len(size_bytes)
}

/// Grows (or shrinks) an existing image file to `new_size_bytes`. Growing
/// is always safe (extends with a sparse hole); shrinking below the
/// filesystem's actual data extent would truncate real data, so this is a
/// thin, deliberately unchecked primitive -- callers are responsible for
/// knowing it's safe to call, the same way `truncate(1)` doesn't ask
/// whether you meant it.
pub fn resize<P: AsRef<Path>>(path: P, new_size_bytes: u64) -> Result<()> {
    let file = OpenOptions::new().write(true).open(path)?;
    file.set_len(new_size_bytes)
}

pub fn current_size<P: AsRef<Path>>(path: P) -> Result<u64> {
    Ok(File::open(path)?.metadata()?.len())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_path(tag: &str) -> std::path::PathBuf {
        let dir =
            std::env::temp_dir().join(format!("lionfs_image_test_{tag}_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        dir.join("image.bin")
    }

    #[test]
    fn create_then_grow_then_shrink() {
        let path = temp_path("resize");
        create_sparse(&path, 1024).unwrap();
        assert_eq!(current_size(&path).unwrap(), 1024);

        resize(&path, 4096).unwrap();
        assert_eq!(current_size(&path).unwrap(), 4096);

        resize(&path, 512).unwrap();
        assert_eq!(current_size(&path).unwrap(), 512);

        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn create_sparse_does_not_use_excess_disk_space() {
        // Sparse-ness is filesystem/OS dependent, so this only checks the
        // logical size is right, not actual block allocation on disk.
        let path = temp_path("sparse");
        create_sparse(&path, 10 * 1024 * 1024 * 1024).unwrap(); // 10 GiB, logical
        assert_eq!(current_size(&path).unwrap(), 10 * 1024 * 1024 * 1024);
        let _ = std::fs::remove_file(&path);
    }
}
