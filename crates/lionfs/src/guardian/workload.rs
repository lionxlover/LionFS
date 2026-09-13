//! Workload classification (RFC-004 §7.3): online moments over IO
//! streams, mapped to service profiles that drive policy retunes.
//!
//! This is the classifier that answers "what *kind* of workload is
//! hitting this volume" from cheap sufficient statistics -- IO size,
//! read/write mix, sequentiality, sync frequency -- rather than from
//! anything expensive. The classes map directly onto retunable
//! policies:
//!
//! * `Db` -> punch-through escape stays armed, journal on the fastest
//!   tier, disable compression on the WAL region.
//! * `Log` -> sequential-append grouping, LZ4, large group-commit
//!   windows.
//! * `Stream` -> large sequential reads: readahead pinned, zstd-12
//!   cold tiering.
//! * `Meta` -> small-file record-log path (RFC-004 §5) priority.
//! * `Vm` -> 4K random RW: inline-small-file off, dedup on (image
//!   backing files are page-sparse).
//! * `Vhost` -> mixed unpredictable: defaults, no aggressive tuning.
//!
//! The statistics are EWMA over per-window aggregates; the classifier
//! is a threshold cascade over them (interpretable, auditable, and
//! cheap enough to run every window -- which is the point: "small AI"
//! means the model fits in the operator's head).

/// IO stream class.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Hash)]
pub enum StreamClass {
    /// Database-ish: medium random RW + frequent syncs.
    Db,
    /// Append-mostly sequential writes with periodic syncs.
    Log,
    /// Large sequential reads.
    Stream,
    /// Small metadata-dense IO (many tiny creates/stats).
    Meta,
    /// VM/container backing: random 4K RW, page-sparse.
    Vm,
    /// Unclassifiable / mixed: defaults apply.
    Vhost,
}

impl StreamClass {
    /// Stable policy-JSON tag.
    #[must_use]
    pub fn tag(self) -> &'static str {
        match self {
            Self::Db => "db",
            Self::Log => "log",
            Self::Stream => "stream",
            Self::Meta => "meta",
            Self::Vm => "vm",
            Self::Vhost => "vhost",
        }
    }

    #[must_use]
    pub fn from_tag(tag: &str) -> Option<Self> {
        match tag {
            "db" => Some(Self::Db),
            "log" => Some(Self::Log),
            "stream" => Some(Self::Stream),
            "meta" => Some(Self::Meta),
            "vm" => Some(Self::Vm),
            "vhost" => Some(Self::Vhost),
            _ => None,
        }
    }
}

/// One window's aggregate IO statistics, as exported by the shard.
#[derive(Clone, Copy, Debug, Default)]
pub struct WindowStats {
    /// Number of operations.
    pub ops: u64,
    /// Bytes moved.
    pub bytes: u64,
    /// Read operations.
    pub reads: u64,
    /// Sync/fsync operations.
    pub syncs: u64,
    /// Max consecutive-sequential run observed (bytes).
    pub max_seq_run_bytes: u64,
}

/// The classifier state: EWMAs (all 32.32) of the window signals.
pub struct WorkloadClassifier {
    /// Mean IO size, bytes (32.32).
    ewma_size: u64,
    /// Read fraction (32.32).
    ewma_read_frac: u64,
    /// Sync-per-op fraction (32.32).
    ewma_sync_frac: u64,
    /// Sequentiality: max-seq-run / bytes (32.32).
    ewma_seq_frac: u64,
    alpha: u64,
}

impl WorkloadClassifier {
    #[must_use]
    pub fn new() -> Self {
        Self {
            ewma_size: 0,
            ewma_read_frac: 0,
            ewma_sync_frac: 0,
            ewma_seq_frac: 0,
            alpha: (1 << 32) / 4,
        }
    }

    /// Feeds one window: per-op `io_size` representative sample (the
    /// mean of the window), whether that window was read-dominated,
    /// and the largest sequential run seen. The simple call signature
    /// keeps the FUSE bridge side trivial.
    pub fn observe(&mut self, io_size: u64, is_read: bool, max_seq_run: u64) {
        let s = io_size.min(1 << 20); // clamp outliers: 1 MiB is "large" enough
        self.ewma_size = ewma(self.ewma_size, s << 32, self.alpha);
        let rf = if is_read { 1 << 32 } else { 0 };
        self.ewma_read_frac = ewma(self.ewma_read_frac, rf, self.alpha);
        // Sync fraction and sequentiality come from window_aggregate.
        let _ = max_seq_run; // kept for API symmetry; see observe_window
    }

    /// Feeds a full window aggregate (the richer path the engine uses).
    pub fn observe_window(&mut self, w: &WindowStats) {
        if w.ops == 0 {
            return;
        }
        let mean = (w.bytes / w.ops).min(1 << 20);
        self.ewma_size = ewma(self.ewma_size, mean << 32, self.alpha);
        let read_frac = (w.reads << 32) / w.ops;
        self.ewma_read_frac = ewma(self.ewma_read_frac, read_frac, self.alpha);
        let sync_frac = (w.syncs << 32) / w.ops;
        self.ewma_sync_frac = ewma(self.ewma_sync_frac, sync_frac, self.alpha);
        let seq_frac = if w.bytes == 0 {
            0
        } else {
            (w.max_seq_run_bytes << 32) / w.bytes
        };
        self.ewma_seq_frac = ewma(self.ewma_seq_frac, seq_frac, self.alpha);
    }

    /// Current EWMA evidence (diagnostics): (size, read_frac,
    /// sync_frac, seq_frac), all 32.32.
    #[must_use]
    pub fn evidence(&self) -> (u64, u64, u64, u64) {
        (self.ewma_size, self.ewma_read_frac, self.ewma_sync_frac, self.ewma_seq_frac)
    }

    /// The classified profile.
    ///
    /// The cascade (RFC-004 §7.3, Table 4), most-specific-first:
    ///
    /// 1. mean size < 1 KiB and seq < 20% -> `Meta`
    /// 2. size >= 256 KiB and read >= 80% and seq >= 50% -> `Stream`
    /// 3. size >= 256 KiB and read < 20% and seq >= 80% -> `Log`
    /// 4. sync >= 5% and 4 KiB <= size <= 64 KiB -> `Db`
    /// 5. 4 KiB <= size <= 64 KiB and seq < 20% -> `Vm`
    /// 6. otherwise -> `Vhost`
    #[must_use]
    pub fn classify(&self) -> StreamClass {
        let size = self.ewma_size >> 32; // whole bytes
        let read = self.ewma_read_frac;
        let sync = self.ewma_sync_frac;
        let seq = self.ewma_seq_frac;
        // Thresholds in 32.32: 0.8, 0.2, 0.5, 0.05.
        let r_hi = (8 << 32) / 10;
        let r_lo = (2 << 32) / 10;
        let seq_hi = r_hi;
        let seq_mid = 1 << 31;
        let seq_lo = r_lo;
        let sync_hi = (5 << 32) / 100;
        if size < 1024 && seq < seq_lo {
            return StreamClass::Meta;
        }
        if size >= 256 * 1024 && read >= r_hi && seq >= seq_mid {
            return StreamClass::Stream;
        }
        if size >= 256 * 1024 && read < r_lo && seq >= seq_hi {
            return StreamClass::Log;
        }
        if sync >= sync_hi && (4 * 1024..=64 * 1024).contains(&size) {
            return StreamClass::Db;
        }
        if (4 * 1024..=64 * 1024).contains(&size) && seq < seq_lo {
            return StreamClass::Vm;
        }
        StreamClass::Vhost
    }
}

impl Default for WorkloadClassifier {
    fn default() -> Self {
        Self::new()
    }
}

/// EWMA in 32.32.
fn ewma(old: u64, sample: u64, alpha: u64) -> u64 {
    let a = alpha.min(1 << 32);
    let one_minus = (1 << 32) - a;
    let num = u128::from(sample) * u128::from(a) + u128::from(old) * u128::from(one_minus);
    (num >> 32) as u64
}

#[cfg(test)]
mod tests {
    use super::*;

    fn window(ops: u64, bytes: u64, reads: u64, syncs: u64, seq: u64) -> WindowStats {
        WindowStats { ops, bytes, reads, syncs, max_seq_run_bytes: seq }
    }

    fn feed(c: &mut WorkloadClassifier, w: WindowStats, rounds: usize) {
        for _ in 0..rounds {
            c.observe_window(&w);
        }
    }

    #[test]
    fn meta_class_for_tiny_random_io() {
        let mut c = WorkloadClassifier::new();
        // 512 B ops, no reads, no syncs, no sequentiality.
        feed(&mut c, window(10_000, 5_120_000, 0, 100, 512), 20);
        assert_eq!(c.classify(), StreamClass::Meta);
    }

    #[test]
    fn stream_class_for_large_sequential_reads() {
        let mut c = WorkloadClassifier::new();
        // 1 MiB ops, 95% reads, 90% sequential.
        feed(&mut c, window(1_000, 1 << 30, 950, 0, (90 * (1 << 30)) / 100), 20);
        assert_eq!(c.classify(), StreamClass::Stream);
    }

    #[test]
    fn log_class_for_large_sequential_appends() {
        let mut c = WorkloadClassifier::new();
        // 512 KiB appends, writes, 95% sequential, occasional sync.
        feed(&mut c, window(2_000, 1 << 30, 10, 2, (95 * (1 << 30)) / 100), 20);
        assert_eq!(c.classify(), StreamClass::Log);
    }

    #[test]
    fn db_class_for_medium_random_rw_with_syncs() {
        let mut c = WorkloadClassifier::new();
        // 8 KiB ops, mixed, 10% syncs, 10% sequential.
        feed(&mut c, window(10_000, 80 << 20, 5_000, 1_000, 8 << 20), 20);
        assert_eq!(c.classify(), StreamClass::Db);
    }

    #[test]
    fn vm_class_for_4k_random_rw() {
        let mut c = WorkloadClassifier::new();
        // 4 KiB ops, mixed R/W, no syncs, no sequentiality.
        feed(&mut c, window(10_000, 40 << 20, 5_000, 0, 4 << 10), 20);
        assert_eq!(c.classify(), StreamClass::Vm);
    }

    #[test]
    fn vhost_is_the_honest_default() {
        let mut c = WorkloadClassifier::new();
        // 128 KiB mixed reads: no specific rule matches (size between
        // the large and small bands).
        feed(&mut c, window(1_000, 128 << 20, 400, 0, (40 * (128 << 20)) / 100), 20);
        assert_eq!(c.classify(), StreamClass::Vhost);
    }

    #[test]
    fn simple_observe_path_agrees_on_extremes() {
        let mut c = WorkloadClassifier::new();
        for _ in 0..20 {
            c.observe(1 << 20, true, 1 << 20);
        }
        // The simple path has no sync/seq evidence: a large read is at
        // least not Meta/Vm; exact class may be Stream or Vhost.
        let size = c.evidence().0 >> 32;
        assert!(size >= 1 << 19, "size {size}");
    }

    #[test]
    fn empty_windows_are_ignored() {
        let mut c = WorkloadClassifier::new();
        c.observe_window(&WindowStats::default());
        assert_eq!(c.evidence(), (0, 0, 0, 0));
    }

    #[test]
    fn tags_roundtrip() {
        for class in [
            StreamClass::Db,
            StreamClass::Log,
            StreamClass::Stream,
            StreamClass::Meta,
            StreamClass::Vm,
            StreamClass::Vhost,
        ] {
            assert_eq!(StreamClass::from_tag(class.tag()), Some(class));
        }
        assert!(StreamClass::from_tag("nope").is_none());
    }

    #[test]
    fn classifier_adapts_when_workload_shifts() {
        let mut c = WorkloadClassifier::new();
        feed(&mut c, window(1_000, 1 << 30, 950, 0, (90 * (1 << 30)) / 100), 30); // Stream
        assert_eq!(c.classify(), StreamClass::Stream);
        // Workload shifts to 4K random RW (VM booted on the volume).
        feed(&mut c, window(10_000, 40 << 20, 5_000, 0, 4 << 10), 60);
        assert_eq!(c.classify(), StreamClass::Vm);
    }
}
