//! Prometheus exposition & latency histograms (RFC-004 §8).
//!
//! The 2.0 telemetry module kept in-process counters; RFC-004 §8 makes
//! them scrapeable. This module is a small, dependency-free metrics
//! registry that renders the Prometheus text exposition format
//! (version 0.0.4) -- the format the entire monitoring universe
//! already speaks, which is the whole point of not inventing one.
//!
//! Pieces:
//!
//! * [`Histogram`] -- log-linear latency buckets from 1 us to ~1 h
//!   (plus +Inf), matching the shape Prometheus's own defaults for IO
//!   latency. Integer bucket counts; rendering emits the cumulative
//!   buckets, sum, and count the scraper expects.
//! * [`Counter`]/[`Gauge`] -- the obvious two.
//! * [`Handle`] -- what the engine keeps: a cheap `Rc` to one series;
//!   `observe`/`inc`/`set` on the hot path is one `RefCell` borrow.
//! * [`Registry`] -- named metric families with HELP/TYPE metadata,
//!   escaped labels, and deterministic output order (a scrape must be
//!   diffable).
//!
//! Per-file latency histograms (RFC-004 §8.2): the registry names are
//! labeled series (`lfs_io_latency_us{op="read",tier="nvme0"}`) fed by
//! the shard dispatcher on completion. The exporter is pull-based over
//! the daemon's health socket -- out-of-band, never on the IO path.

use std::cell::RefCell;
use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::rc::Rc;

/// Histogram bucket upper bounds, in microseconds: log-linear
/// (12 per decade) from 1 us to ~36 minutes, then a 1-hour cap.
pub const LATENCY_BUCKETS_US: [u64; 49] = [
    1, 2, 4, 7, 12, 21, 38, 68, 120, 210, 380, 680,
    1_200, 2_100, 3_800, 6_800, 12_000, 21_000, 38_000, 68_000, 120_000, 210_000, 380_000, 680_000,
    1_200_000, 2_100_000, 3_800_000, 6_800_000, 12_000_000, 21_000_000, 38_000_000, 68_000_000,
    120_000_000, 210_000_000, 380_000_000, 680_000_000, 1_200_000_000, 2_100_000_000,
    3_800_000_000, 6_800_000_000, 12_000_000_000, 21_000_000_000, 38_000_000_000,
    68_000_000_000, 120_000_000_000, 210_000_000_000, 380_000_000_000, 680_000_000_000,
    2_160_000_000_000,
];

/// A cumulative histogram over [`LATENCY_BUCKETS_US`] plus +Inf.
#[derive(Debug, Clone)]
pub struct Histogram {
    /// Cumulative counts: `cumulative[i]` = observations <= bucket i.
    cumulative: [u64; 49],
    /// Sum of observations (the `_sum` series).
    sum_us: u128,
    /// Observation count (the `_count` series; also the +Inf bucket).
    count: u64,
}

impl Default for Histogram {
    fn default() -> Self {
        Self {
            cumulative: [0; 49],
            sum_us: 0,
            count: 0,
        }
    }
}

impl Histogram {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Records one observation in microseconds.
    pub fn observe(&mut self, value_us: u64) {
        // First bucket whose bound >= value; 49 = beyond all = +Inf.
        let idx = LATENCY_BUCKETS_US
            .iter()
            .position(|&b| b >= value_us)
            .unwrap_or(LATENCY_BUCKETS_US.len());
        // Cumulative semantics: every bucket at or above idx includes
        // this observation. i == 49 is the implicit +Inf (== count).
        for (i, c) in self.cumulative.iter_mut().enumerate() {
            if i >= idx {
                *c += 1;
            }
        }
        self.count += 1;
        self.sum_us += u128::from(value_us);
    }

    /// Bucket upper bounds as strings, with "+Inf" last.
    #[must_use]
    pub fn bounds(&self) -> Vec<String> {
        LATENCY_BUCKETS_US
            .iter()
            .map(|b| b.to_string())
            .chain(std::iter::once("+Inf".to_owned()))
            .collect()
    }

    /// Cumulative counts including the +Inf bucket (== count).
    #[must_use]
    pub fn counts(&self) -> Vec<u64> {
        let mut v = self.cumulative.to_vec();
        v.push(self.count);
        v
    }

    /// Approximate quantile (0.0..=1.0) from the cumulative curve,
    /// linearly interpolated inside the containing bucket. `None` when
    /// empty. This is for humans; the scraper computes its own from
    /// the buckets.
    #[must_use]
    pub fn quantile(&self, q: f64) -> Option<u64> {
        if self.count == 0 {
            return None;
        }
        let target = q.clamp(0.0, 1.0) * self.count as f64;
        let mut prev_bound = 0u64;
        for (i, &bound) in LATENCY_BUCKETS_US.iter().enumerate() {
            if self.cumulative[i] as f64 >= target {
                let prev_c = if i == 0 { 0 } else { self.cumulative[i - 1] };
                let span = (self.cumulative[i] - prev_c) as f64;
                let into = if span > 0.0 {
                    ((target - prev_c as f64) / span).clamp(0.0, 1.0)
                } else {
                    0.0
                };
                return Some(prev_bound + (into * (bound - prev_bound) as f64) as u64);
            }
            prev_bound = bound;
        }
        LATENCY_BUCKETS_US.last().copied()
    }

    /// Arithmetic mean, `None` when empty.
    #[must_use]
    pub fn mean_us(&self) -> Option<u64> {
        if self.count == 0 {
            None
        } else {
            Some((self.sum_us / u128::from(self.count)) as u64)
        }
    }

    #[must_use]
    pub fn count(&self) -> u64 {
        self.count
    }
}

/// A monotonically increasing counter.
#[derive(Debug, Default, Clone)]
pub struct Counter(u64);

impl Counter {
    pub fn inc(&mut self) {
        self.0 = self.0.saturating_add(1);
    }

    pub fn add(&mut self, v: u64) {
        self.0 = self.0.saturating_add(v);
    }

    #[must_use]
    pub fn get(&self) -> u64 {
        self.0
    }
}

/// A point-in-time value.
#[derive(Debug, Default, Clone)]
pub struct Gauge(i64);

impl Gauge {
    pub fn set(&mut self, v: i64) {
        self.0 = v;
    }

    pub fn add(&mut self, v: i64) {
        self.0 = self.0.saturating_add(v);
    }

    #[must_use]
    pub fn get(&self) -> i64 {
        self.0
    }
}

/// What one metric series holds.
#[derive(Debug, Clone)]
pub enum MetricValue {
    Counter(Counter),
    Gauge(Gauge),
    Histogram(Histogram),
}

/// The engine-side handle to one series. `observe`/`inc`/`set` are
/// one `RefCell` borrow; wrong-type calls are no-ops that debug-assert
/// (a counter handle observed as a histogram is a caller bug).
pub struct Handle(RefCell<MetricValue>);

impl std::fmt::Debug for Handle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Handle").field("value", &self.0.borrow()).finish()
    }
}

impl Handle {
    /// Increments the counter (no-op on other types).
    pub fn inc(&self) {
        if let MetricValue::Counter(c) = &mut *self.0.borrow_mut() {
            c.inc();
        } else {
            debug_assert!(false, "inc on a non-counter handle");
        }
    }

    /// Adds to the counter (no-op on other types).
    pub fn add(&self, v: u64) {
        if let MetricValue::Counter(c) = &mut *self.0.borrow_mut() {
            c.add(v);
        } else {
            debug_assert!(false, "add on a non-counter handle");
        }
    }

    /// Sets the gauge (no-op on other types).
    pub fn set(&self, v: i64) {
        if let MetricValue::Gauge(g) = &mut *self.0.borrow_mut() {
            g.set(v);
        } else {
            debug_assert!(false, "set on a non-gauge handle");
        }
    }

    /// Records a histogram observation in microseconds (no-op on
    /// other types).
    pub fn observe(&self, value_us: u64) {
        if let MetricValue::Histogram(h) = &mut *self.0.borrow_mut() {
            h.observe(value_us);
        } else {
            debug_assert!(false, "observe on a non-histogram handle");
        }
    }
}

/// One rendered series: name + labels -> value.
#[derive(Debug, Clone)]
struct Series {
    name: String,
    labels: Vec<(String, String)>,
    value: MetricValue,
}

impl Series {
    /// Appends this series' sample line(s): one line for
    /// counter/gauge; bucket + sum + count lines for histograms.
    fn render(&self, out: &mut String) {
        let label_str = render_labels(&self.labels);
        match &self.value {
            MetricValue::Counter(c) => {
                let _ = writeln!(out, "{}{} {}", self.name, label_str, c.get());
            }
            MetricValue::Gauge(g) => {
                let _ = writeln!(out, "{}{} {}", self.name, label_str, g.get());
            }
            MetricValue::Histogram(h) => {
                for (bound, count) in h.bounds().iter().zip(h.counts().iter()) {
                    let mut labels = self.labels.clone();
                    labels.push(("le".to_owned(), bound.clone()));
                    let _ = writeln!(out, "{}_bucket{} {}", self.name, render_labels(&labels), count);
                }
                let _ = writeln!(out, "{}_sum{} {}", self.name, label_str, h.sum_us);
                let _ = writeln!(out, "{}_count{} {}", self.name, label_str, h.count);
            }
        }
    }
}

/// Renders `{k="v",...}` with `\`, `"`, `\n` escaped per the
/// exposition format; empty label sets render as "".
fn render_labels(labels: &[(String, String)]) -> String {
    if labels.is_empty() {
        return String::new();
    }
    let mut out = String::from("{");
    for (i, (k, v)) in labels.iter().enumerate() {
        if i > 0 {
            out.push(',');
        }
        out.push_str(k);
        out.push_str("=\"");
        for ch in v.chars() {
            match ch {
                '\\' => out.push_str("\\\\"),
                '"' => out.push_str("\\\""),
                '\n' => out.push_str("\\n"),
                c => out.push(c),
            }
        }
        out.push('"');
    }
    out.push('}');
    out
}

/// A metric family: shared name, HELP/TYPE, and its series.
#[derive(Debug)]
struct Family {
    name: String,
    help: String,
    kind: &'static str, // "counter" | "gauge" | "histogram"
    series: Vec<Series>,
}

/// The registry: families in name order, series sorted by labels
/// within each family (deterministic scrapes), values snapshotted
/// from live handles at render time.
#[derive(Debug, Default)]
pub struct Registry {
    families: BTreeMap<String, Family>,
    handles: Vec<(String, Vec<(String, String)>, Rc<Handle>)>,
}

impl Registry {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    fn declare_family(&mut self, name: &str, help: &str, kind: &'static str) {
        self.families.entry(name.to_owned()).or_insert_with(|| Family {
            name: name.to_owned(),
            help: help.to_owned(),
            kind,
            series: Vec::new(),
        });
    }

    /// Registers a counter series; returns its live handle.
    pub fn counter(
        &mut self,
        name: &str,
        help: &str,
        labels: Vec<(String, String)>,
    ) -> Rc<Handle> {
        self.declare_family(name, help, "counter");
        let handle = Rc::new(Handle(RefCell::new(MetricValue::Counter(Counter::default()))));
        self.handles.push((name.to_owned(), labels, Rc::clone(&handle)));
        handle
    }

    /// Registers a gauge series; returns its live handle.
    pub fn gauge(&mut self, name: &str, help: &str, labels: Vec<(String, String)>) -> Rc<Handle> {
        self.declare_family(name, help, "gauge");
        let handle = Rc::new(Handle(RefCell::new(MetricValue::Gauge(Gauge::default()))));
        self.handles.push((name.to_owned(), labels, Rc::clone(&handle)));
        handle
    }

    /// Registers a histogram series; returns its live handle.
    pub fn histogram(
        &mut self,
        name: &str,
        help: &str,
        labels: Vec<(String, String)>,
    ) -> Rc<Handle> {
        self.declare_family(name, help, "histogram");
        let handle = Rc::new(Handle(RefCell::new(MetricValue::Histogram(Histogram::default()))));
        self.handles.push((name.to_owned(), labels, Rc::clone(&handle)));
        handle
    }

    /// Number of registered series.
    #[must_use]
    pub fn series_count(&self) -> usize {
        self.handles.len()
    }

    /// Rebuilds the renderable series from the live handles.
    fn refresh(&mut self) {
        for family in self.families.values_mut() {
            family.series.clear();
        }
        for (name, labels, handle) in &self.handles {
            let value = handle.0.borrow().clone();
            if let Some(family) = self.families.get_mut(name) {
                family.series.push(Series {
                    name: name.clone(),
                    labels: labels.clone(),
                    value,
                });
            }
        }
        for family in self.families.values_mut() {
            family.series.sort_by(|a, b| a.labels.cmp(&b.labels));
        }
    }

    /// Renders the full exposition document: HELP/TYPE headers then
    /// series, deterministic in family-name then label order.
    #[must_use]
    pub fn render(&mut self) -> String {
        self.refresh();
        let mut out = String::new();
        for family in self.families.values() {
            let _ = writeln!(out, "# HELP {} {}", family.name, family.help);
            let _ = writeln!(out, "# TYPE {} {}", family.name, family.kind);
            for s in &family.series {
                s.render(&mut out);
            }
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn histogram_buckets_are_cumulative() {
        let mut h = Histogram::new();
        h.observe(1); // bucket 0 (bound 1)
        h.observe(100); // bucket 8 (bound 120)
        h.observe(5_000); // bucket 15 (bound 6800)
        assert_eq!(h.count(), 3);
        // Cumulative monotonicity and +Inf == count.
        let counts = h.counts();
        assert_eq!(counts.len(), 50);
        for i in 1..counts.len() {
            assert!(counts[i] >= counts[i - 1], "bucket {i}");
        }
        assert_eq!(*counts.last().expect("inf"), 3);
        // Below the first bucket's bound is still bucket 0.
        let mut h2 = Histogram::new();
        h2.observe(0);
        assert_eq!(h2.counts()[0], 1);
    }

    #[test]
    fn huge_observations_land_in_inf() {
        let mut h = Histogram::new();
        h.observe(u64::MAX); // beyond every bound
        let counts = h.counts();
        // Only +Inf includes it.
        assert_eq!(counts[48], 0);
        assert_eq!(counts[49], 1);
    }

    #[test]
    fn histogram_quantiles_interpolate() {
        let mut h = Histogram::new();
        for v in 1..=100u64 {
            h.observe(v);
        }
        let p50 = h.quantile(0.5).expect("non-empty");
        assert!((30..=90).contains(&p50), "p50 {p50}");
        assert!(h.quantile(0.0).expect("non-empty") <= 1);
        assert!(h.quantile(1.0).expect("non-empty") >= 90);
        // Clamp behavior.
        assert_eq!(h.quantile(-1.0), h.quantile(0.0));
        assert_eq!(h.quantile(2.0), h.quantile(1.0));
        // Empty histogram has no quantiles.
        assert!(Histogram::new().quantile(0.5).is_none());
        assert!(Histogram::new().mean_us().is_none());
    }

    #[test]
    fn histogram_mean() {
        let mut h = Histogram::new();
        h.observe(10);
        h.observe(20);
        assert_eq!(h.mean_us(), Some(15));
    }

    #[test]
    fn registry_renders_counter_and_gauge() {
        let mut r = Registry::new();
        let c = r.counter(
            "lfs_io_errors_total",
            "I/O errors by kind",
            vec![("kind".to_owned(), "checksum".to_owned())],
        );
        c.add(5);
        let g = r.gauge("lfs_free_bytes", "free pool bytes", vec![]);
        g.set(1 << 30);
        let doc = r.render();
        assert!(doc.contains("# HELP lfs_io_errors_total I/O errors by kind"));
        assert!(doc.contains("# TYPE lfs_io_errors_total counter"));
        assert!(doc.contains("lfs_io_errors_total{kind=\"checksum\"} 5"));
        assert!(doc.contains("lfs_free_bytes 1073741824"));
        assert!(doc.contains("# TYPE lfs_free_bytes gauge"));
    }

    #[test]
    fn registry_renders_histogram() {
        let mut r = Registry::new();
        let h = r.histogram(
            "lfs_io_latency_us",
            "operation latency",
            vec![("op".to_owned(), "read".to_owned())],
        );
        h.observe(5);
        h.observe(500);
        let doc = r.render();
        assert!(doc.contains("# TYPE lfs_io_latency_us histogram"));
        assert!(doc.contains("lfs_io_latency_us_bucket{op=\"read\",le=\"1\"} 0"));
        assert!(doc.contains("lfs_io_latency_us_bucket{op=\"read\",le=\"+Inf\"} 2"));
        assert!(doc.contains("lfs_io_latency_us_sum{op=\"read\"} 505"));
        assert!(doc.contains("lfs_io_latency_us_count{op=\"read\"} 2"));
    }

    #[test]
    fn labels_are_escaped() {
        let labels = vec![("path".to_owned(), "weird\"path\\\n".to_owned())];
        assert_eq!(render_labels(&labels), "{path=\"weird\\\"path\\\\\\n\"}");
        assert_eq!(render_labels(&[]), "");
    }

    #[test]
    fn scrapes_are_deterministic() {
        let mut r1 = Registry::new();
        let mut r2 = Registry::new();
        // Register in different orders; render must be identical.
        let c1 = r1.counter("lfs_ops_total", "ops", vec![("op".to_owned(), "read".to_owned())]);
        let g1 = r1.gauge("lfs_alpha", "alpha", vec![]);
        c1.inc();
        g1.set(7);
        let g2 = r2.gauge("lfs_alpha", "alpha", vec![]);
        let c2 = r2.counter("lfs_ops_total", "ops", vec![("op".to_owned(), "read".to_owned())]);
        c2.inc();
        g2.set(7);
        assert_eq!(r1.render(), r2.render());
    }

    #[test]
    fn series_sort_by_labels_within_family() {
        let mut r = Registry::new();
        let z = r.counter("lfs_ops_total", "ops", vec![("op".to_owned(), "zeta".to_owned())]);
        let a = r.counter("lfs_ops_total", "ops", vec![("op".to_owned(), "alpha".to_owned())]);
        z.inc();
        a.inc();
        let doc = r.render();
        let alpha = doc.find("op=\"alpha\"").expect("alpha present");
        let zeta = doc.find("op=\"zeta\"").expect("zeta present");
        assert!(alpha < zeta);
        assert_eq!(r.series_count(), 2);
    }

    #[test]
    fn counter_and_gauge_saturate() {
        let mut c = Counter::default();
        c.add(u64::MAX);
        c.add(1);
        assert_eq!(c.get(), u64::MAX);
        let mut g = Gauge::default();
        g.set(i64::MAX);
        g.add(1);
        assert_eq!(g.get(), i64::MAX);
    }
}
