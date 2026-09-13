//! OS identity and host capability probing.
//!
//! The `Platform` enum is resolved from the compile target (never guessed at
//! runtime), while page size, CPU count, and the capability report are
//! probed once and cached in `OnceLock`s -- `page_size()` is on the hot
//! path of every allocation, so it must be a plain atomic load after the
//! first call.

use std::num::NonZeroUsize;
use std::sync::OnceLock;

/// The operating systems LionFS 2.0 supports in this source tree.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Platform {
    Linux,
    MacOs,
    Windows,
    /// FreeBSD/OpenBSD/NetBSD: unix surface present (libc, FUSE via
    /// fusefs on FreeBSD), not a CI tier.
    Bsd,
    /// Anything else the crate happens to compile on. The PAL degrades
    /// to the generic unix/std paths where possible.
    Other,
}

/// Compile-time platform identity. `current_platform()` is the single
/// source of truth for "which OS am I on" in runtime code; `cfg!` remains
/// the right tool inside cfg-gated blocks.
#[must_use]
pub fn current_platform() -> Platform {
    #[cfg(target_os = "linux")]
    {
        Platform::Linux
    }
    #[cfg(target_os = "macos")]
    {
        Platform::MacOs
    }
    #[cfg(target_os = "windows")]
    {
        Platform::Windows
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos", target_os = "windows")))]
    {
        #[cfg(any(target_os = "freebsd", target_os = "openbsd", target_os = "netbsd"))]
        {
            Platform::Bsd
        }
        #[cfg(not(any(target_os = "freebsd", target_os = "openbsd", target_os = "netbsd")))]
        {
            Platform::Other
        }
    }
}

impl Platform {
    /// Human-readable name used in superblock provenance and tool output.
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            Platform::Linux => "linux",
            Platform::MacOs => "macos",
            Platform::Windows => "windows",
            Platform::Bsd => "bsd",
            Platform::Other => "other",
        }
    }

    /// Whether the FUSE mounting backend is compiled in on this platform
    /// (Linux: kernel FUSE; macOS: macFUSE; FreeBSD: fusefs).
    #[must_use]
    pub fn has_fuse(self) -> bool {
        matches!(self, Platform::Linux | Platform::MacOs | Platform::Bsd)
    }

    /// Whether the Linux io_uring submission plane can even be attempted.
    #[must_use]
    pub fn supports_io_uring(self) -> bool {
        cfg!(any(target_os = "linux", target_os = "android")) && self == Platform::Linux
    }
}

/// Host OS version string for diagnostics (" uname -a"-class information,
/// never used for behavior). Best-effort: "unknown" if nothing can be read.
pub fn os_version_string() -> String {
    static CACHE: OnceLock<String> = OnceLock::new();
    CACHE.get_or_init(probe_os_version).clone()
}

fn probe_os_version() -> String {
    #[cfg(target_os = "linux")]
    {
        // /proc/sys/kernel/osrelease is a stable, tiny, always-present file.
        std::fs::read_to_string("/proc/sys/kernel/osrelease")
            .map(|s| s.trim().to_string())
            .unwrap_or_else(|_| "unknown".to_string())
    }
    #[cfg(target_os = "macos")]
    {
        std::process::Command::new("uname")
            .arg("-r")
            .output()
            .ok()
            .and_then(|o| {
                String::from_utf8(o.stdout)
                    .ok()
                    .map(|s| s.trim().to_string())
            })
            .unwrap_or_else(|| "unknown".to_string())
    }
    #[cfg(target_os = "windows")]
    {
        "windows".to_string()
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos", target_os = "windows")))]
    {
        std::process::Command::new("uname")
            .arg("-r")
            .output()
            .ok()
            .and_then(|o| {
                String::from_utf8(o.stdout)
                    .ok()
                    .map(|s| s.trim().to_string())
            })
            .unwrap_or_else(|| "unknown".to_string())
    }
}

/// Virtual memory page size in bytes, probed once.
#[must_use]
pub fn page_size() -> usize {
    static CACHE: OnceLock<usize> = OnceLock::new();
    *CACHE.get_or_init(|| {
        #[cfg(unix)]
        {
            // SAFETY: sysconf is a pure query with no failure mode on a
            // valid _SC_PAGESIZE argument (it cannot return an error per
            // POSIX). A zero result, which the standard does not allow for
            // _SC_PAGESIZE, falls back to 4096 defensively.
            let v = unsafe { libc::sysconf(libc::_SC_PAGESIZE) };
            if v > 0 {
                v as usize
            } else {
                4096
            }
        }
        #[cfg(windows)]
        {
            // SAFETY: GetSystemInfo writes into a plain POD struct that we
            // fully own; the function has no failure mode.
            #[repr(C)]
            struct SystemInfo {
                reserved1: [u32; 2],
                page_size: u32,
                min_app_addr: *mut u8,
                max_app_addr: *mut u8,
                active_processor_mask: usize,
                number_of_processors: u32,
                reserved2: [u32; 8],
            }
            extern "system" {
                fn GetSystemInfo(info: *mut SystemInfo);
            }
            let mut info = SystemInfo {
                reserved1: [0; 2],
                page_size: 0,
                min_app_addr: std::ptr::null_mut(),
                max_app_addr: std::ptr::null_mut(),
                active_processor_mask: 0,
                number_of_processors: 0,
                reserved2: [0; 8],
            };
            unsafe { GetSystemInfo(&mut info) };
            if info.page_size > 0 {
                info.page_size as usize
            } else {
                4096
            }
        }
        #[cfg(not(any(unix, windows)))]
        {
            4096
        }
    })
}

/// Number of logical CPUs visible to the process. Used to size the engine's
/// per-core shard count (LFS-RFC-002 §3.3: shards are a power-of-two
/// `NUM_SHARDS = next_pow2(cpu_count)` rounded to at least 1).
#[must_use]
pub fn cpu_count() -> NonZeroUsize {
    static CACHE: OnceLock<NonZeroUsize> = OnceLock::new();
    *CACHE.get_or_init(|| {
        let n = std::thread::available_parallelism()
            .map(|n| n.get())
            .unwrap_or(1);
        // available_parallelism is already >= 1; guard anyway.
        NonZeroUsize::new(n.max(1)).expect("max(1) is nonzero")
    })
}

/// Runtime capability report for the I/O engine's backend selection
/// (LFS-RFC-002 Table 7: submission plane selection). All fields are
/// cheap, cacheable probes; the engine consults this once at mount.
#[derive(Debug, Clone)]
pub struct CapabilityReport {
    pub platform: Platform,
    /// io_uring compiled in (feature `io_uring`) AND kernel >= 5.1.
    pub io_uring_compiled: bool,
    /// Windows IOCP backend compiled in (target = windows).
    pub iocp_compiled: bool,
    /// Whether O_DIRECT-style unbuffered device I/O is available.
    pub direct_io: bool,
    /// Whether the platform can flush without a full sync_all (fdatasync
    /// on Linux, F_FULLFSYNC on macOS, FlushFileBuffers on Windows).
    pub data_sync: bool,
    /// Whether ZNS/SMR media policy machinery is meaningful on this OS
    /// (it still *simulates* zone placement for image files everywhere).
    pub zoned_media_support: bool,
}

impl CapabilityReport {
    /// Probe the current host. Cheap; results are cached per-process.
    pub fn probe() -> Self {
        let platform = current_platform();
        Self {
            platform,
            io_uring_compiled: cfg!(feature = "io_uring") && platform.supports_io_uring(),
            iocp_compiled: cfg!(target_os = "windows"),
            direct_io: cfg!(unix),
            data_sync: true,
            zoned_media_support: cfg!(target_os = "linux"),
        }
    }

    /// One-line human summary for `lfs_palinfo` and mount logs.
    #[must_use]
    pub fn summary(&self) -> String {
        format!(
            "platform={} cpus={} page={} io_uring={} iocp={} direct_io={} data_sync={} zoned={}",
            self.platform.name(),
            cpu_count(),
            page_size(),
            self.io_uring_compiled,
            self.iocp_compiled,
            self.direct_io,
            self.data_sync,
            self.zoned_media_support
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn capability_probe_consistent_with_platform() {
        let cap = CapabilityReport::probe();
        assert_eq!(cap.platform, current_platform());
        if cap.platform == Platform::Windows {
            assert!(cap.iocp_compiled);
            assert!(!cap.io_uring_compiled);
        }
        if cap.platform == Platform::Linux {
            // Whether io_uring is compiled in depends on the feature flag;
            // the probe must simply agree with it.
            assert_eq!(cap.io_uring_compiled, cfg!(feature = "io_uring"));
        }
    }

    #[test]
    fn cpu_count_is_at_least_one() {
        assert!(cpu_count().get() >= 1);
    }

    #[test]
    fn platform_names_are_stable() {
        assert_eq!(Platform::Linux.name(), "linux");
        assert_eq!(Platform::MacOs.name(), "macos");
        assert_eq!(Platform::Windows.name(), "windows");
    }
}
