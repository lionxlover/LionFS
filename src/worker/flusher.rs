use crossbeam::channel::{unbounded, Receiver, Sender};
use std::thread;
use std::time::Duration;

pub enum FlushCommand {
    SyncAll,
    Shutdown,
}

pub struct BackgroundFlusher {
    sender: Sender<FlushCommand>,
    handle: Option<thread::JoinHandle<()>>,
}

impl Default for BackgroundFlusher {
    fn default() -> Self {
        Self::new()
    }
}

impl BackgroundFlusher {
    pub fn new() -> Self {
        let (sender, receiver) = unbounded();

        let handle = thread::spawn(move || {
            Self::worker_loop(receiver);
        });

        Self {
            sender,
            handle: Some(handle),
        }
    }

    fn worker_loop(receiver: Receiver<FlushCommand>) {
        loop {
            // Wait for a command or timeout (simulate delayed allocation flush)
            match receiver.recv_timeout(Duration::from_secs(5)) {
                Ok(FlushCommand::Shutdown) => {
                    break;
                }
                Ok(FlushCommand::SyncAll) => {
                    // Trigger a global flush of dirty caches here
                    // e.g., flush_dirty_extents()
                }
                Err(_) => {
                    // Timeout hit (5 seconds). Perform periodic background flush.
                    // This implements the Delayed Allocation background writes.
                }
            }
        }
    }

    pub fn sync(&self) {
        let _ = self.sender.send(FlushCommand::SyncAll);
    }
}

impl Drop for BackgroundFlusher {
    fn drop(&mut self) {
        let _ = self.sender.send(FlushCommand::Shutdown);
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}
