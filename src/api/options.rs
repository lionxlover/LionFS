//! Public, FFI-friendly mount options -- the stable-surface counterpart to
//! `common::config::MountConfig` (which this wraps rather than duplicates)
//! for `api::builder`/the C API to accept without exposing internal types
//! directly across the FFI boundary.

#[derive(Debug, Clone)]
pub struct LfsOptions {
    pub device_path: String,
    pub extra_devices: Vec<String>,
    pub read_only: bool,
    pub default_compression: u8,
    pub default_encryption: u8,
}

impl LfsOptions {
    pub fn new(device_path: impl Into<String>) -> Self {
        Self {
            device_path: device_path.into(),
            extra_devices: Vec::new(),
            read_only: false,
            default_compression: 0,
            default_encryption: 0,
        }
    }

    pub fn with_extra_device(mut self, path: impl Into<String>) -> Self {
        self.extra_devices.push(path.into());
        self
    }

    pub fn read_only(mut self, ro: bool) -> Self {
        self.read_only = ro;
        self
    }

    pub fn with_mount_config_str(mut self, options_str: &str) -> Self {
        let cfg = crate::common::config::MountConfig::from_options_str(options_str);
        self.read_only = cfg.read_only;
        self.default_compression = cfg.default_compression;
        self.default_encryption = cfg.default_encryption;
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builder_methods_chain() {
        let opts = LfsOptions::new("/dev/loop0")
            .with_extra_device("/dev/loop1")
            .read_only(true);
        assert_eq!(opts.device_path, "/dev/loop0");
        assert_eq!(opts.extra_devices, vec!["/dev/loop1".to_string()]);
        assert!(opts.read_only);
    }

    #[test]
    fn parses_mount_option_string() {
        let opts = LfsOptions::new("/dev/loop0").with_mount_config_str("ro,compress=zstd");
        assert!(opts.read_only);
        assert_eq!(
            opts.default_compression,
            crate::common::constants::COMPRESSION_ZSTD
        );
    }
}
