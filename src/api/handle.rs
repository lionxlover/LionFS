//! An opaque handle registry mapping small integer ids to live, mounted
//! `fuser` sessions -- what makes a real `lfs_unmount` possible from the C
//! API (previously `lfs_mount_fuse` had no way to refer back to a mount it
//! started, since it didn't actually start one). Uses
//! `fuser::spawn_mount2`, which mounts in a background thread and returns
//! a `BackgroundSession` that unmounts when dropped -- confidence note:
//! this specific API shape is what `fuser` 0.12 (the version pinned in
//! Cargo.toml) is expected to expose for non-blocking mounts, but it's
//! one of the less-verified corners of this pass without a way to compile
//! against the real crate here.

use fuser::BackgroundSession;
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;

static NEXT_HANDLE: AtomicU64 = AtomicU64::new(1);
static SESSIONS: Mutex<Option<HashMap<u64, BackgroundSession>>> = Mutex::new(None);

fn registry() -> std::sync::MutexGuard<'static, Option<HashMap<u64, BackgroundSession>>> {
    let mut guard = SESSIONS.lock().unwrap();
    if guard.is_none() {
        *guard = Some(HashMap::new());
    }
    guard
}

/// Registers a live session, returning an opaque handle for later
/// `unmount`. `0` is never returned (reserved as an "invalid handle"
/// sentinel for the C API).
pub fn register(session: BackgroundSession) -> u64 {
    let id = NEXT_HANDLE.fetch_add(1, Ordering::SeqCst).max(1);
    registry().as_mut().unwrap().insert(id, session);
    id
}

/// Unmounts and drops the session for `handle`, if it exists. Returns
/// `true` if a session was actually found and removed.
pub fn unmount(handle: u64) -> bool {
    registry().as_mut().unwrap().remove(&handle).is_some()
}

pub fn is_mounted(handle: u64) -> bool {
    registry().as_ref().unwrap().contains_key(&handle)
}

pub fn active_count() -> usize {
    registry().as_ref().unwrap().len()
}
