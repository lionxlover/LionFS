use crate::fs::filesystem::LionFS;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

pub struct RebalancerWorker {
    running: Arc<Mutex<bool>>,
    handle: Option<thread::JoinHandle<()>>,
}

impl Default for RebalancerWorker {
    fn default() -> Self {
        Self::new()
    }
}

impl RebalancerWorker {
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
                // Online Rebalancer Logic
                // 1. Find fragmented extents
                // 2. Relocate to contiguous blocks
                // 3. Update BTree
                // Placeholder for Phase 6

                thread::sleep(Duration::from_secs(120));
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
