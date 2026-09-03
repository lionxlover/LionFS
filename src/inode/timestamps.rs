//! Timestamp helpers, consolidating the `SystemTime <-> i64 seconds`
//! conversions that were previously repeated inline at each call site in
//! `fs::filesystem` (`create`, `mkdir`, `setattr`, ...).

use std::time::{SystemTime, UNIX_EPOCH};

pub fn now_secs() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

pub fn system_time_to_secs(t: SystemTime) -> i64 {
    t.duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

pub fn secs_to_system_time(secs: i64) -> SystemTime {
    if secs >= 0 {
        UNIX_EPOCH + std::time::Duration::from_secs(secs as u64)
    } else {
        UNIX_EPOCH - std::time::Duration::from_secs((-secs) as u64)
    }
}

/// Resolves a `setattr` time argument (either an explicit time or
/// "now") to seconds since the epoch. Takes the neutral
/// [`crate::fs::metadata::TimeOrNow`] (the 1.x version took fuser's
/// type, which welded this helper to Linux FUSE).
pub fn resolve_time_or_now(t: crate::fs::metadata::TimeOrNow, now: i64) -> i64 {
    match t {
        crate::fs::metadata::TimeOrNow::At(secs) => secs,
        crate::fs::metadata::TimeOrNow::Now => now,
    }
}

#[derive(Debug, Clone, Copy)]
pub struct Timestamps {
    pub atime: i64,
    pub mtime: i64,
    pub ctime: i64,
}

impl Timestamps {
    pub fn now() -> Self {
        let n = now_secs();
        Self {
            atime: n,
            mtime: n,
            ctime: n,
        }
    }

    /// A `Timestamps` for a metadata-only change: bumps ctime (and, per
    /// POSIX, ctime alone -- not atime/mtime, which only change when
    /// content or its "last used" time actually changes) to `now`.
    pub fn touched_metadata(mut self) -> Self {
        self.ctime = now_secs();
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fs::metadata::TimeOrNow;

    #[test]
    fn round_trips_through_system_time() {
        let secs = 1_700_000_000i64;
        let st = secs_to_system_time(secs);
        assert_eq!(system_time_to_secs(st), secs);
    }

    #[test]
    fn resolve_now_uses_provided_now() {
        assert_eq!(resolve_time_or_now(TimeOrNow::Now, 12345), 12345);
    }

    #[test]
    fn resolve_specific_time_ignores_now() {
        let st = secs_to_system_time(500);
        assert_eq!(resolve_time_or_now(TimeOrNow::At(500), 12345), 500);
    }

    #[test]
    fn touched_metadata_only_bumps_ctime() {
        let ts = Timestamps {
            atime: 1,
            mtime: 2,
            ctime: 3,
        };
        let touched = ts.touched_metadata();
        assert_eq!(touched.atime, 1);
        assert_eq!(touched.mtime, 2);
        assert!(touched.ctime >= 3);
    }
}
