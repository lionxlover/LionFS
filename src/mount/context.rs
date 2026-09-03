//! Runtime state for an active mount -- when it started, which paths are
//! involved -- for introspection (`tools::inspect`/`tools::health`
//! reporting "how long has this been mounted") separate from `LionFS`
//! itself, which doesn't currently track its own mount time.

use std::time::{Instant, SystemTime};

#[derive(Debug, Clone)]
pub struct MountContext {
    pub device_path: String,
    pub extra_devices: Vec<String>,
    pub mount_point: String,
    pub mounted_at: SystemTime,
    started: Instant,
}

impl MountContext {
    pub fn new(
        device_path: impl Into<String>,
        extra_devices: Vec<String>,
        mount_point: impl Into<String>,
    ) -> Self {
        Self {
            device_path: device_path.into(),
            extra_devices,
            mount_point: mount_point.into(),
            mounted_at: SystemTime::now(),
            started: Instant::now(),
        }
    }

    pub fn uptime(&self) -> std::time::Duration {
        self.started.elapsed()
    }

    pub fn device_count(&self) -> usize {
        1 + self.extra_devices.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tracks_device_count_including_primary() {
        let ctx = MountContext::new(
            "/dev/loop0",
            vec!["/dev/loop1".to_string(), "/dev/loop2".to_string()],
            "/mnt/lionfs",
        );
        assert_eq!(ctx.device_count(), 3);
    }

    #[test]
    fn uptime_is_nonnegative_and_increases() {
        let ctx = MountContext::new("/dev/loop0", vec![], "/mnt/lionfs");
        let first = ctx.uptime();
        std::thread::sleep(std::time::Duration::from_millis(5));
        let second = ctx.uptime();
        assert!(second >= first);
    }
}
