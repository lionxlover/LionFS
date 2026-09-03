//! Mount-time configuration, gathering the scattered options
//! (`Superblock::default_compression`/`default_encryption`, cache sizes,
//! RAID chunk size) into one place a CLI tool can populate from flags and
//! hand to `LionFS` construction, instead of setting individual superblock
//! fields by hand at each call site.

#[derive(Debug, Clone, Copy)]
pub struct MountConfig {
    pub read_only: bool,
    pub default_compression: u8,
    pub default_encryption: u8,
    pub inode_cache_capacity: u64,
    pub node_cache_capacity: u64,
    /// zstd level for newly written compression clusters (Phase 4;
    /// default 3, zstd's own sensible default).
    pub zstd_level: i32,
}

impl Default for MountConfig {
    fn default() -> Self {
        Self {
            read_only: false,
            default_compression: 0,
            default_encryption: 0,
            inode_cache_capacity: 10_000,
            node_cache_capacity: 10_000,
            zstd_level: 3,
        }
    }
}

impl MountConfig {
    /// Parses a comma-separated `-o`-style option string, e.g.
    /// `"ro,compress=zstd,encrypt=aes256gcm"`. Unknown options are ignored
    /// rather than rejected, matching how real mount(8) implementations
    /// tolerate options meant for other filesystems in a shared fstab line.
    pub fn from_options_str(s: &str) -> Self {
        let mut cfg = Self::default();
        for opt in s.split(',').map(str::trim).filter(|s| !s.is_empty()) {
            match opt {
                "ro" => cfg.read_only = true,
                "rw" => cfg.read_only = false,
                _ => {
                    if let Some(v) = opt.strip_prefix("compress=") {
                        cfg.default_compression = compression_id(v);
                    } else if let Some(v) = opt.strip_prefix("encrypt=") {
                        cfg.default_encryption = encryption_id(v);
                    } else if let Some(v) = opt.strip_prefix("inode_cache=") {
                        if let Ok(n) = v.parse() {
                            cfg.inode_cache_capacity = n;
                        }
                    } else if let Some(v) = opt.strip_prefix("node_cache=") {
                        if let Ok(n) = v.parse() {
                            cfg.node_cache_capacity = n;
                        }
                    } else if let Some(v) = opt.strip_prefix("zstd_level=") {
                        if let Ok(n) = v.parse::<i32>() {
                            cfg.zstd_level = n.clamp(1, 22);
                        }
                    }
                }
            }
        }
        cfg
    }
}

fn compression_id(name: &str) -> u8 {
    match name.to_lowercase().as_str() {
        "lz4" => crate::common::constants::COMPRESSION_LZ4,
        "zstd" => crate::common::constants::COMPRESSION_ZSTD,
        "deflate" | "zlib" => crate::common::constants::COMPRESSION_DEFLATE,
        _ => crate::common::constants::ALGO_NONE,
    }
}

fn encryption_id(name: &str) -> u8 {
    match name.to_lowercase().as_str() {
        "aes256gcm" | "aes-256-gcm" | "aes" => crate::common::constants::ENCRYPTION_AES_256_GCM,
        "chacha20poly1305" | "chacha20-poly1305" | "chacha" => {
            crate::common::constants::ENCRYPTION_CHACHA20_POLY1305
        }
        _ => crate::common::constants::ALGO_NONE,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_known_options() {
        let cfg = MountConfig::from_options_str("ro,compress=zstd,encrypt=chacha20poly1305");
        assert!(cfg.read_only);
        assert_eq!(
            cfg.default_compression,
            crate::common::constants::COMPRESSION_ZSTD
        );
        assert_eq!(
            cfg.default_encryption,
            crate::common::constants::ENCRYPTION_CHACHA20_POLY1305
        );
    }

    #[test]
    fn unknown_options_are_ignored_not_rejected() {
        let cfg = MountConfig::from_options_str("noatime,compress=zstd,some_other_fs_option=5");
        assert_eq!(
            cfg.default_compression,
            crate::common::constants::COMPRESSION_ZSTD
        );
        assert!(!cfg.read_only);
    }

    #[test]
    fn empty_string_yields_defaults() {
        let cfg = MountConfig::from_options_str("");
        assert_eq!(cfg.default_compression, 0);
        assert_eq!(cfg.default_encryption, 0);
        assert!(!cfg.read_only);
    }
}
