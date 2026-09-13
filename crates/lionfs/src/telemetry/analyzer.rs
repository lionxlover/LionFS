use super::metrics::TelemetryMetrics;
use std::sync::atomic::Ordering;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WorkloadProfile {
    Unknown,
    Desktop,
    Database,
    MediaStreaming,
    Archive,
}

pub struct WorkloadAnalyzer;

impl Default for WorkloadAnalyzer {
    fn default() -> Self {
        Self::new()
    }
}

impl WorkloadAnalyzer {
    pub fn new() -> Self {
        Self {}
    }

    pub fn classify(&self, metrics: &TelemetryMetrics) -> WorkloadProfile {
        let reads = metrics.read_ops.load(Ordering::Relaxed);
        let writes = metrics.write_ops.load(Ordering::Relaxed);
        let total_ops = reads + writes;

        if total_ops == 0 {
            return WorkloadProfile::Unknown;
        }

        let read_ratio = reads as f64 / total_ops as f64;
        let avg_read_bytes = metrics
            .read_bytes
            .load(Ordering::Relaxed)
            .checked_div(reads)
            .unwrap_or(0);
        let avg_write_bytes = metrics
            .write_bytes
            .load(Ordering::Relaxed)
            .checked_div(writes)
            .unwrap_or(0);

        if avg_read_bytes > 1024 * 1024 && read_ratio > 0.8 {
            WorkloadProfile::MediaStreaming
        } else if avg_read_bytes <= 16384 && avg_write_bytes <= 16384 && total_ops > 1000 {
            WorkloadProfile::Database
        } else if read_ratio < 0.2 && avg_write_bytes > 128 * 1024 {
            WorkloadProfile::Archive
        } else {
            WorkloadProfile::Desktop
        }
    }
}
