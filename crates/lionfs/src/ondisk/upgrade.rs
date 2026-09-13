//! Compatibility and Upgrade Framework
//! Tracks feature flags for RO_COMPAT (read-only compatible) and INCOMPAT (incompatible).

pub const LFS_FEATURE_COMPAT_DIR_INDEX: u64 = 1 << 0;
pub const LFS_FEATURE_RO_COMPAT_LARGE_FILE: u64 = 1 << 0;
pub const LFS_FEATURE_RO_COMPAT_EXTRA_ISIZE: u64 = 1 << 1;
pub const LFS_FEATURE_INCOMPAT_COMPRESSION: u64 = 1 << 0;
pub const LFS_FEATURE_INCOMPAT_ENCRYPTION: u64 = 1 << 1;
pub const LFS_FEATURE_INCOMPAT_RAID: u64 = 1 << 2;

pub struct FeatureNegotiator {
    pub compat_flags: u64,
    pub ro_compat_flags: u64,
    pub incompat_flags: u64,
}

impl FeatureNegotiator {
    pub fn new(compat: u64, ro_compat: u64, incompat: u64) -> Self {
        Self {
            compat_flags: compat,
            ro_compat_flags: ro_compat,
            incompat_flags: incompat,
        }
    }

    pub fn supports_write(&self, supported_incompat: u64, supported_ro_compat: u64) -> bool {
        // Can only mount read-write if we support all incompat and ro_compat features
        (self.incompat_flags & !supported_incompat == 0)
            && (self.ro_compat_flags & !supported_ro_compat == 0)
    }

    pub fn supports_read(&self, supported_incompat: u64) -> bool {
        // Can mount read-only if we support all incompat features
        self.incompat_flags & !supported_incompat == 0
    }
}
