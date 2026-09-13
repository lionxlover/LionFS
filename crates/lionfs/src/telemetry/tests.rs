#[cfg(test)]
mod tests {
    use crate::telemetry::analyzer::{WorkloadAnalyzer, WorkloadProfile};
    use crate::telemetry::metrics::TelemetryMetrics;
    use std::sync::atomic::Ordering;

    #[test]
    fn test_telemetry_metrics() {
        let metrics = TelemetryMetrics::new();
        metrics.record_read(4096, 100);
        metrics.record_read(4096, 200);
        metrics.record_write(8192, 150);
        metrics.record_cache_hit();
        metrics.record_cache_miss();

        assert_eq!(metrics.read_ops.load(Ordering::Relaxed), 2);
        assert_eq!(metrics.write_ops.load(Ordering::Relaxed), 1);
        assert_eq!(metrics.read_bytes.load(Ordering::Relaxed), 8192);
        assert_eq!(metrics.cache_hits.load(Ordering::Relaxed), 1);
        assert_eq!(metrics.cache_misses.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn test_workload_analyzer() {
        let metrics = TelemetryMetrics::new();
        let analyzer = WorkloadAnalyzer::new();

        assert_eq!(analyzer.classify(&metrics), WorkloadProfile::Unknown);

        for _ in 0..1000 {
            metrics.record_read(1024 * 1024 + 1, 100);
        }

        assert_eq!(analyzer.classify(&metrics), WorkloadProfile::MediaStreaming);
    }
}
