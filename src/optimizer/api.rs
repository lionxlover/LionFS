use crate::optimizer::recommender::RecommendationEngine;
use crate::telemetry::analyzer::{WorkloadAnalyzer, WorkloadProfile};
use crate::telemetry::metrics::TelemetryMetrics;
use std::sync::atomic::Ordering;

pub struct DashboardApi;

impl DashboardApi {
    pub fn get_telemetry_json(metrics: &TelemetryMetrics) -> String {
        let reads = metrics.read_ops.load(Ordering::Relaxed);
        let writes = metrics.write_ops.load(Ordering::Relaxed);
        let hits = metrics.cache_hits.load(Ordering::Relaxed);
        let misses = metrics.cache_misses.load(Ordering::Relaxed);

        let hit_ratio = if hits + misses > 0 {
            hits as f64 / (hits + misses) as f64
        } else {
            0.0
        };

        format!(
            "{{\"read_ops\": {}, \"write_ops\": {}, \"cache_hit_ratio\": {:.4}}}",
            reads, writes, hit_ratio
        )
    }

    pub fn get_recommendations_json(metrics: &TelemetryMetrics) -> String {
        let recommender = RecommendationEngine::new();
        let recs = recommender.generate_recommendations(metrics);

        let items: Vec<String> = recs.iter().map(|s| format!("\"{}\"", s)).collect();
        format!("[{}]", items.join(", "))
    }

    pub fn get_workload_profile_json(metrics: &TelemetryMetrics) -> String {
        let analyzer = WorkloadAnalyzer::new();
        let profile = analyzer.classify(metrics);

        let profile_str = match profile {
            WorkloadProfile::Unknown => "Unknown",
            WorkloadProfile::Desktop => "Desktop",
            WorkloadProfile::Database => "Database",
            WorkloadProfile::MediaStreaming => "MediaStreaming",
            WorkloadProfile::Archive => "Archive",
        };

        format!("{{\"profile\": \"{}\"}}", profile_str)
    }
}
