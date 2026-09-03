//! Device geometry probing, 2.0: a thin re-export of the platform
//! abstraction layer's prober. The 1.x version carried Linux ioctl
//! constants directly; the PAL (`pal::geometry`) now implements the
//! per-platform probing (Linux BLK* ioctls, macOS DKIOC*, Windows
//! IOCTL_DISK_*, stat fallback for image files), so this module keeps
//! the 1.x call sites (`crate::disk::geometry::probe`) working while
//! the implementation gained three platforms.
//!
//! `alignment_class` is the 2.0 addition: the RFC-002 §6.2
//! page-cluster class derived from probed geometry, feeding the
//! universal alignment engine.

pub use crate::pal::geometry::{probe, DeviceGeometry};

/// The alignment class implied by probed geometry (RFC-002 §6.2:
/// 4K/16K/64K page-cluster classes from the geometry triple).
pub use crate::media::alignment::AlignmentClass;

/// Convenience: probe + resolve the alignment class in one call.
pub fn probe_with_alignment(
    file: &std::fs::File,
) -> std::io::Result<(DeviceGeometry, AlignmentClass)> {
    let geo = probe(file)?;
    Ok((geo, AlignmentClass::from_geometry(&geo)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn regular_file_reports_its_length_as_size() {
        let dir = std::env::temp_dir().join(format!("lionfs_geom2_test_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("image.bin");
        let f = crate::pal::file::create_image(&path, 123_456).unwrap();

        let geom = probe(&f).unwrap();
        assert_eq!(geom.size_bytes, 123_456);
        assert_eq!(geom.logical_sector_size, 512);

        let (_geo, class) = probe_with_alignment(&f).unwrap();
        assert_eq!(class, AlignmentClass::K4);

        drop(f);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
