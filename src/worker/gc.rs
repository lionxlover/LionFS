use crate::fs::filesystem::LionFS;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

pub struct GcWorker {
    running: Arc<Mutex<bool>>,
    handle: Option<thread::JoinHandle<()>>,
}

impl Default for GcWorker {
    fn default() -> Self {
        Self::new()
    }
}

impl GcWorker {
    pub fn new() -> Self {
        Self {
            running: Arc::new(Mutex::new(false)),
            handle: None,
        }
    }

    pub fn start(&mut self, _fs: Arc<Mutex<LionFS>>) {
        let running = self.running.clone();
        *running.lock().unwrap() = true;

        self.handle = Some(thread::spawn(move || {
            while *running.lock().unwrap() {
                // Garbage Collection Logic
                // 1. Find deleted snapshots/clones
                // 2. Decrement refcounts for their exclusive blocks
                // 3. Reclaim blocks with refcount = 0
                // Placeholder for Phase 6

                thread::sleep(Duration::from_secs(60));
            }
        }));
    }

    pub fn stop(&mut self) {
        if let Ok(mut running) = self.running.lock() {
            *running = false;
        }
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}
