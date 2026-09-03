use crate::ondisk::serialization::{BlockGroupDescriptor, Superblock};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;
// In a full implementation, this thread would iterate the ChecksumTree
// and verify each block, reporting to a shared state or log file.

pub struct ScrubberWorker {
    pub active: Arc<Mutex<bool>>,
    pub paused: Arc<Mutex<bool>>,
    pub current_block: Arc<Mutex<u64>>,
    pub total_blocks: Arc<Mutex<u64>>,
    pub handle: Option<thread::JoinHandle<()>>,
}

impl Default for ScrubberWorker {
    fn default() -> Self {
        Self::new()
    }
}

impl ScrubberWorker {
    pub fn new() -> Self {
        Self {
            active: Arc::new(Mutex::new(false)),
            paused: Arc::new(Mutex::new(false)),
            current_block: Arc::new(Mutex::new(0)),
            total_blocks: Arc::new(Mutex::new(0)),
            handle: None,
        }
    }

    pub fn start(&mut self, sb: Superblock, _bg: BlockGroupDescriptor, image_path: String) {
        let active_clone = Arc::clone(&self.active);
        let paused_clone = Arc::clone(&self.paused);
        let current_block_clone = Arc::clone(&self.current_block);

        *self.total_blocks.lock().unwrap() = sb.total_blocks;
        *active_clone.lock().unwrap() = true;

        let handle = thread::spawn(move || {
            let _disk = match crate::disk::block_io::Disk::open(&image_path) {
                Ok(d) => d,
                Err(e) => {
                    eprintln!("[Scrub Worker] Failed to open disk for scrubbing: {}", e);
                    return;
                }
            };

            let total_blocks = sb.total_blocks;

            while *active_clone.lock().unwrap() {
                if *paused_clone.lock().unwrap() {
                    thread::sleep(Duration::from_secs(1));
                    continue;
                }

                // Sleep to yield to foreground I/O
                thread::sleep(Duration::from_secs(2));

                let mut current = current_block_clone.lock().unwrap();

                // Pretend to verify the block (Check ChecksumTree / Check BadBlocksTree)
                // In a real implementation we would:
                // 1. Check if the block is allocated in the FreeSpaceTree.
                // 2. If it is, lookup its Checksum in the ChecksumTree.
                // 3. Read the block from physical disk.
                // 4. Compute and compare checksums.
                // 5. If corrupt, add to BadBlocksTree.

                *current += 1;
                if *current >= total_blocks {
                    *current = 0; // wrap around and restart scrub
                }
            }
        });

        self.handle = Some(handle);
    }

    pub fn stop(&mut self) {
        if let Ok(mut lock) = self.active.lock() {
            *lock = false;
        }
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }

    pub fn get_status(&self) -> String {
        let active = *self.active.lock().unwrap();
        let paused = *self.paused.lock().unwrap();
        let current = *self.current_block.lock().unwrap();
        let total = *self.total_blocks.lock().unwrap();

        let progress = if total > 0 {
            (current as f64 / total as f64) * 100.0
        } else {
            0.0
        };

        let mut status = String::new();
        if !active {
            status.push_str("Status: STOPPED\n");
        } else if paused {
            status.push_str("Status: PAUSED\n");
        } else {
            status.push_str("Status: RUNNING\n");
        }

        status.push_str(&format!(
            "Progress: {:.2}% ({}/{})\n",
            progress, current, total
        ));
        status
    }

    pub fn handle_command(&self, cmd: &str) {
        match cmd {
            "pause" => *self.paused.lock().unwrap() = true,
            "resume" => *self.paused.lock().unwrap() = false,
            "stop" => *self.active.lock().unwrap() = false,
            _ => eprintln!("Scrubber received unknown command: {}", cmd),
        }
    }
}
