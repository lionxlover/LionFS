#[cfg(test)]
mod tests {
    use crate::optimizer::adaptive_cache::AdaptiveCacheManager;
    use crate::optimizer::policy::{OptimizationProfile, PolicyEngine};
    use crate::optimizer::predictor::PredictiveReadEngine;
    use crate::telemetry::metrics::TelemetryMetrics;

    #[test]
    fn test_predictive_read_engine() {
        let engine = PredictiveReadEngine::new();

        // Sequence 1 -> 2
        engine.record_sequence(1, 2);
        engine.record_sequence(1, 2);
        engine.record_sequence(1, 2);

        assert_eq!(engine.predict_next(1), Some(2));
        assert_eq!(engine.predict_next(2), None);
    }

    #[test]
    fn test_policy_engine() {
        let mut policy = PolicyEngine::default();
        assert_eq!(policy.current_profile, OptimizationProfile::Balanced);

        policy.set_profile(OptimizationProfile::PerformanceFirst);
        assert_eq!(
            policy.current_profile,
            OptimizationProfile::PerformanceFirst
        );
    }

    #[test]
    fn test_adaptive_cache() {
        let mut cache = AdaptiveCacheManager::new();
        let metrics = TelemetryMetrics::new();

        // Simulate low hit ratio (0 hits, 100 misses)
        for _ in 0..100 {
            metrics.record_cache_miss();
        }

        cache.tune_cache_sizes(&metrics);
        assert!(cache.max_inodes > 10_000);
        assert!(cache.max_extents > 20_000);
    }
}
