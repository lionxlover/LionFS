use std::sync::{Arc, Weak};

use crate::fs::filesystem::{LionFS, SharedCore};
use crate::fs::page_cache::FLUSH_SOFT_LIMIT_BYTES;

pub struct FlusherWorker {}

impl FlusherWorker {
    pub fn start(core: &Arc<SharedCore>) {
        let mut handle = core.flusher.lock().unwrap();
        if handle.is_some() {
            return;
        }
        let weak = Arc::downgrade(core);
        let spawned = std::thread::Builder::new()
            .name("lfs-flusher".to_string())
            .spawn(move || Self::flusher_main(weak));
        if let Ok(h) = spawned {
            *handle = Some(h);
        }
    }

    pub fn stop(core: &SharedCore) {
        let handle = core.flusher.lock().unwrap().take();
        if let Some(h) = handle {
            core.flusher_shutdown.store(true, std::sync::atomic::Ordering::SeqCst);
            core.page_cache.flusher_wake.notify_all();
            let _ = h.join();
        }
    }

    fn flusher_main(weak: Weak<SharedCore>) {
        loop {
            let core = match weak.upgrade() {
                Some(c) => c,
                None => return, // Mount dropped
            };

            if core.flusher_shutdown.load(std::sync::atomic::Ordering::SeqCst) {
                return;
            }

            let mut did_work = false;

            // Drain dirty inodes in largest-first order until total dirty
            // falls below the soft limit or there are no more victims.
            // Iterating more than one victim per wake-up prevents write
            // stalls when many inodes accumulate between wake-ups.
            while core.page_cache.total_dirty() >= FLUSH_SOFT_LIMIT_BYTES {
                if core.flusher_shutdown.load(std::sync::atomic::Ordering::Relaxed) {
                    return;
                }
                match core.page_cache.largest_dirty() {
                    Some(victim) => {
                        let lfs = LionFS {
                            core: Arc::clone(&core),
                            scrubber: crate::worker::scrubber::ScrubberWorker::new(),
                            image_path: String::new(),
                        };
                        let _ = lfs.flush_ino_marked(victim);
                        did_work = true;
                    }
                    None => break, // No dirty inodes
                }
            }

            // Wait for signal if no work was done
            if !did_work {
                core.page_cache.wait_for_flusher();
            }
        }
    }
}
