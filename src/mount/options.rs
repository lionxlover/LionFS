//! Translating `common::config::MountConfig` into the `Vec<fuser::MountOption>`
//! fuser actually wants -- previously built inline, identically, in both
//! `userspace::cli::mount` and `api::mod` (and about to be needed a third
//! time by `mount::mount`), so it's extracted here and those call sites
//! now share it.

use crate::common::config::MountConfig;
// The option builder is unix-only: it speaks fuser's mount option type.
#[cfg(unix)]
use fuser::MountOption;

#[cfg(unix)]
pub fn build_mount_options(config: &MountConfig) -> Vec<MountOption> {
    let mut options = vec![
        MountOption::FSName("lionfs".to_string()),
        MountOption::DefaultPermissions,
    ];
    options.push(if config.read_only {
        MountOption::RO
    } else {
        MountOption::RW
    });
    options
}

#[cfg(test)]
mod tests {
    use super::*;

    fn has_ro(opts: &[MountOption]) -> bool {
        opts.iter().any(|o| matches!(o, MountOption::RO))
    }
    fn has_rw(opts: &[MountOption]) -> bool {
        opts.iter().any(|o| matches!(o, MountOption::RW))
    }

    #[test]
    fn read_write_by_default() {
        let opts = build_mount_options(&MountConfig::default());
        assert!(has_rw(&opts));
        assert!(!has_ro(&opts));
    }

    #[test]
    fn read_only_when_configured() {
        let mut cfg = MountConfig::default();
        cfg.read_only = true;
        let opts = build_mount_options(&cfg);
        assert!(has_ro(&opts));
        assert!(!has_rw(&opts));
    }
}
