use std::collections::VecDeque;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::RwLock;

/// Per-CPU free-space cache to avoid global allocator lock contention
pub struct PerCpuAllocatorCache {
    caches: Vec<RwLock<VecDeque<u64>>>,
    cpu_count: usize,
    hits: AtomicU64,
    misses: AtomicU64,
}

impl Default for PerCpuAllocatorCache {
    fn default() -> Self {
        Self::new()
    }
}

impl PerCpuAllocatorCache {
    pub fn new() -> Self {
        // Since std doesn't natively expose cpu ids easily, we use thread_id hashing or round-robin as a proxy.
        // For standard optimal setup, num_cpus is used. We'll default to 16 buckets.
        let cpu_count = 16;
        let mut caches = Vec::with_capacity(cpu_count);
        for _ in 0..cpu_count {
            caches.push(RwLock::new(VecDeque::new()));
        }

        Self {
            caches,
            cpu_count,
            hits: AtomicU64::new(0),
            misses: AtomicU64::new(0),
        }
    }

    fn get_cpu_bucket(&self) -> usize {
        // The previous implementation hashed the *length* of
        // `format!("{:?}", thread_id)`, which is 11 characters for every
        // single-digit ThreadId ("ThreadId(1)".."ThreadId(9)") -- meaning
        // the first several threads created by the process all mapped to
        // the exact same bucket, defeating the point of per-CPU sharding
        // for exactly the thread counts most likely in testing or light
        // use. `ThreadId` has no stable public integer accessor, so
        // instead we assign each thread a bucket index once, in creation
        // order, via a thread-local cache backed by a global counter --
        // real round-robin distribution using only safe, stable Rust.
        thread_local! {
            static BUCKET: std::cell::Cell<Option<usize>> = std::cell::Cell::new(None);
        }
        static NEXT_BUCKET: AtomicU64 = AtomicU64::new(0);

        BUCKET.with(|b| {
            if let Some(idx) = b.get() {
                idx
            } else {
                let idx = (NEXT_BUCKET.fetch_add(1, Ordering::Relaxed) as usize) % self.cpu_count;
                b.set(Some(idx));
                idx
            }
        })
    }

    pub fn allocate(&self) -> Option<u64> {
        let bucket_idx = self.get_cpu_bucket();
        if let Ok(mut cache) = self.caches[bucket_idx].try_write() {
            if let Some(block) = cache.pop_front() {
                self.hits.fetch_add(1, Ordering::Relaxed);
                return Some(block);
            }
        }
        self.misses.fetch_add(1, Ordering::Relaxed);
        None
    }

    pub fn free(&self, block: u64) {
        let bucket_idx = self.get_cpu_bucket();
        if let Ok(mut cache) = self.caches[bucket_idx].try_write() {
            cache.push_back(block);
        }
    }
}
