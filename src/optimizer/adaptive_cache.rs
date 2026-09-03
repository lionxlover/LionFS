use crate::telemetry::metrics::TelemetryMetrics;

pub struct AdaptiveCacheManager {
    pub max_inodes: usize,
    pub max_extents: usize,
}

impl Default for AdaptiveCacheManager {
    fn default() -> Self {
        Self::new()
    }
}

impl AdaptiveCacheManager {
    pub fn new() -> Self {
        Self {
            max_inodes: 10_000,
            max_extents: 20_000,
        }
    }

    pub fn tune_cache_sizes(&mut self, metrics: &TelemetryMetrics) {
        let hits = metrics
            .cache_hits
            .load(std::sync::atomic::Ordering::Relaxed);
        let misses = metrics
            .cache_misses
            .load(std::sync::atomic::Ordering::Relaxed);

        let total = hits + misses;
        if total > 0 {
            let hit_ratio = hits as f64 / total as f64;
            if hit_ratio < 0.8 {
                // If hit ratio is low, gently expand cache
                self.max_inodes += 1000;
                self.max_extents += 2000;
            } else if hit_ratio > 0.98 {
                // If hit ratio is extremely high, we can try saving memory
                self.max_inodes = self.max_inodes.saturating_sub(500).max(1000);
                self.max_extents = self.max_extents.saturating_sub(1000).max(2000);
            }
        }
    }
}
