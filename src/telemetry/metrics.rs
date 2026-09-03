use std::sync::atomic::{AtomicU64, Ordering};

pub struct TelemetryMetrics {
    pub read_ops: AtomicU64,
    pub write_ops: AtomicU64,
    pub read_bytes: AtomicU64,
    pub write_bytes: AtomicU64,
    pub cache_hits: AtomicU64,
    pub cache_misses: AtomicU64,
    // Using simple atomics for moving averages in a lock-free manner
    // Normally, this would be an EWMA (Exponentially Weighted Moving Average)
    pub avg_read_latency_ns: AtomicU64,
    pub avg_write_latency_ns: AtomicU64,
}

impl Default for TelemetryMetrics {
    fn default() -> Self {
        Self::new()
    }
}

impl TelemetryMetrics {
    pub fn new() -> Self {
        Self {
            read_ops: AtomicU64::new(0),
            write_ops: AtomicU64::new(0),
            read_bytes: AtomicU64::new(0),
            write_bytes: AtomicU64::new(0),
            cache_hits: AtomicU64::new(0),
            cache_misses: AtomicU64::new(0),
            avg_read_latency_ns: AtomicU64::new(0),
            avg_write_latency_ns: AtomicU64::new(0),
        }
    }

    pub fn record_read(&self, bytes: u64, latency_ns: u64) {
        self.read_ops.fetch_add(1, Ordering::Relaxed);
        self.read_bytes.fetch_add(bytes, Ordering::Relaxed);
        self.update_ewma(&self.avg_read_latency_ns, latency_ns);
    }

    pub fn record_write(&self, bytes: u64, latency_ns: u64) {
        self.write_ops.fetch_add(1, Ordering::Relaxed);
        self.write_bytes.fetch_add(bytes, Ordering::Relaxed);
        self.update_ewma(&self.avg_write_latency_ns, latency_ns);
    }

    pub fn record_cache_hit(&self) {
        self.cache_hits.fetch_add(1, Ordering::Relaxed);
    }

    pub fn record_cache_miss(&self) {
        self.cache_misses.fetch_add(1, Ordering::Relaxed);
    }

    // A very simple lock-free EWMA approximation
    fn update_ewma(&self, metric: &AtomicU64, new_val: u64) {
        let mut current = metric.load(Ordering::Relaxed);
        loop {
            // alpha = 0.1 (shift by 3 ~ 1/8)
            let updated = if current == 0 {
                new_val
            } else {
                current - (current >> 3) + (new_val >> 3)
            };

            match metric.compare_exchange_weak(
                current,
                updated,
                Ordering::SeqCst,
                Ordering::Relaxed,
            ) {
                Ok(_) => break,
                Err(val) => current = val,
            }
        }
    }
}
