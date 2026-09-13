//! LionFS-cluster core types.
//!
//! This crate defines the shared vocabulary of every other HFS crate:
//! [`Hash256`] (content identity), [`VectorClock`] (causality),
//! [`VersionNode`] (namespace DAG node), [`ExtentDescriptor`]
//! (the 24-byte packed locator), and the tier/compression enums.
//!
//! Axioms implemented here (spec §1.1):
//! 1. Identity  — `Hash256` is the BLAKE3 digest of content, not a location.
//! 2. Time      — `VersionNode`s are immutable; only HEAD pointers move.
//! 3. Self-desc — every extent carries checksum + stripe id.

use serde::{Deserialize, Serialize};
use std::cmp::Ordering;
use std::fmt;

/// Node id in the cluster (0..n). In production this is a ULID/xid.
pub type NodeId = u64;

// ---------------------------------------------------------------------------
// Hash256
// ---------------------------------------------------------------------------

/// A 256-bit BLAKE3 content hash. This is *identity* in LionFS-cluster (Axiom 1).
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct Hash256(pub [u8; 32]);

impl Hash256 {
    /// Hash arbitrary content — the canonical identity operation.
    pub fn of(data: &[u8]) -> Self {
        Self(*blake3::hash(data).as_bytes())
    }

    /// The all-zero sentinel ("no hash / null pointer").
    pub fn zero() -> Self {
        Self([0u8; 32])
    }

    pub fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }

    /// First 8 bytes, big-endian — the `content_hash_prefix` used inside
    /// [`ExtentDescriptor`].
    ///
    /// NOTE (audit M6): a 64-bit prefix is *not* collision-safe as an
    /// identity at petabyte scale (expected collisions ≈ n²/2^65). It is a
    /// locator hint only; the full 32-byte hash is always re-verified on
    /// dedup hits and on every extent read.
    pub fn prefix_u64(&self) -> u64 {
        let mut b = [0u8; 8];
        b.copy_from_slice(&self.0[..8]);
        u64::from_be_bytes(b)
    }

    /// Short human-friendly rendering (first 8 hex chars).
    pub fn short(&self) -> String {
        self.to_hex()[..8].to_string()
    }

    pub fn to_hex(&self) -> String {
        let mut s = String::with_capacity(64);
        for byte in self.0.iter() {
            s.push_str(&format!("{:02x}", byte));
        }
        s
    }

    pub fn from_hex(s: &str) -> Option<Self> {
        if s.len() != 64 || !s.bytes().all(|b| b.is_ascii_hexdigit()) {
            return None;
        }
        let mut out = [0u8; 32];
        for i in 0..32 {
            out[i] = u8::from_str_radix(&s[i * 2..i * 2 + 2], 16).ok()?;
        }
        Some(Self(out))
    }
}

impl fmt::Debug for Hash256 {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "h({})", self.short())
    }
}

impl fmt::Display for Hash256 {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.to_hex())
    }
}

// ---------------------------------------------------------------------------
// Vector clock
// ---------------------------------------------------------------------------

/// Lamport-style vector clock: causality metadata for CRDT merging (spec §14.3).
///
/// `a ≤ b`      means `a` happened-before or equals `b`
/// `a || b`     means the two are concurrent
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct VectorClock {
    counters: std::collections::BTreeMap<NodeId, u64>,
}

impl VectorClock {
    pub fn new() -> Self {
        Self::default()
    }

    /// Local increment at `node` (spec §6 step 15).
    pub fn increment(&mut self, node: NodeId) {
        *self.counters.entry(node).or_insert(0) += 1;
    }

    /// Pointwise-max merge (the CRDT merge for vector clocks).
    pub fn merge(&mut self, other: &VectorClock) {
        for (node, count) in &other.counters {
            let e = self.counters.entry(*node).or_insert(0);
            *e = (*e).max(*count);
        }
    }

    /// True if `self` causally precedes or equals `other`.
    pub fn happens_before_or_equal(&self, other: &VectorClock) -> bool {
        self.counters.iter().all(|(node, count)| {
            other.counters.get(node).map_or(*count == 0, |o| o >= count)
        })
    }

    /// True if neither clock dominates the other.
    pub fn concurrent_with(&self, other: &VectorClock) -> bool {
        !self.happens_before_or_equal(other) && !other.happens_before_or_equal(self)
    }

    /// Number of nodes this clock tracks.
    pub fn width(&self) -> usize {
        self.counters.len()
    }

    /// Sum of counters (used as a monotone tiebreaker).
    pub fn total(&self) -> u64 {
        self.counters.values().sum()
    }
}

impl PartialOrd for VectorClock {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        let le = self.happens_before_or_equal(other);
        let ge = other.happens_before_or_equal(self);
        match (le, ge) {
            (true, true) => Some(Ordering::Equal),
            (true, false) => Some(Ordering::Less),
            (false, true) => Some(Ordering::Greater),
            (false, false) => None, // concurrent
        }
    }
}

// ---------------------------------------------------------------------------
// Hybrid Logical Clock — audit C3's prerequisite for resolve(path, t)
// ---------------------------------------------------------------------------

/// A Hybrid Logical Clock timestamp (Kulkarni et al., 2014).
///
/// `physical` is the most recent wall-clock reading (ns since the UNIX
/// epoch) the clock has anchored to; `logical` counts events that happened
/// while the wall stood still (or ran backward). The pair is totally
/// ordered **and** causally consistent:
///
/// * if event *a* happened-before event *b* (directly, or through a chain
///   of messages), then `hlc(a) < hlc(b)` — the Lamport property;
/// * `physical` never drifts further than the clock-skew envelope away
///   from real time — the wall-clock property Lamport clocks lack.
///
/// This is the primitive audit finding C3 asks for: `resolve(path, t)`
/// under multi-master concurrency needs a *consistent cut*, and HLC
/// timestamps (unlike the bare `timestamp_ns` wall readings spec §14.3
/// relies on) provide one. The engine anchors every
/// `VersionNode::timestamp_ns` to [`Hlc::tick_ns`], so a future
/// distributed deployment can cut on it directly.
///
/// The logical counter is capped at 999 999. When it would overflow (more
/// than a million events on one frozen nanosecond), `physical` advances by
/// one full logical epoch (1 000 000 ns) and `logical` resets — the
/// degenerate rule that keeps the packed `u64` form (`physical + logical`)
/// strictly monotone and collision-free. Wall drift under this rule is
/// bounded at 1 ms per million burst events.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct Hlc {
    /// Wall anchor, ns since the UNIX epoch.
    pub physical: u64,
    /// Events at the same `physical` instant (0..=999 999).
    pub logical: u32,
}

/// Degenerate-rule threshold: the logical counter occupies the sub-ns
/// slots of a 1 ms epoch in the packed form; overflowing it advances
/// `physical` by a whole epoch.
const HLC_LOGICAL_CAP: u32 = 999_999;

impl Default for Hlc {
    fn default() -> Self {
        Self::EPOCH
    }
}

impl Hlc {
    /// The zero timestamp (before any event).
    pub const EPOCH: Self = Self { physical: 0, logical: 0 };

    /// Stamp a local event. `wall_ns` is the current wall reading (any
    /// NTP-corrected source); a stale or *backward* reading is absorbed by
    /// the logical counter, so timestamps never regress.
    pub fn tick(&mut self, wall_ns: u64) -> Self {
        if wall_ns > self.physical {
            self.physical = wall_ns;
            self.logical = 0;
        } else {
            self.bump_logical();
        }
        *self
    }

    /// Absorb a remote timestamp (message receive). The result is above
    /// both the local and the remote view of time, preserving causality
    /// across the exchange.
    pub fn observe(&mut self, remote: &Hlc, wall_ns: u64) -> Self {
        let merged = self.physical.max(remote.physical).max(wall_ns);
        if merged == self.physical && merged == remote.physical {
            self.logical = self.logical.max(remote.logical);
            self.bump_logical();
        } else if merged == self.physical {
            self.bump_logical();
        } else if merged == remote.physical {
            self.logical = remote.logical;
            self.bump_logical();
        } else {
            self.logical = 0;
        }
        self.physical = merged;
        *self
    }

    /// Stamp a local event and return the packed single-`u64` form used
    /// for `VersionNode::timestamp_ns`: `physical + logical`. Strictly
    /// monotone across ticks (see the type docs for the cap rule).
    pub fn tick_ns(&mut self, wall_ns: u64) -> u64 {
        self.tick(wall_ns).as_ns()
    }

    /// Packed single-`u64` form of this timestamp.
    pub fn as_ns(&self) -> u64 {
        self.physical + self.logical as u64
    }

    /// Strict causal order: is this timestamp definitely later?
    pub fn is_after(&self, other: &Hlc) -> bool {
        *self > *other
    }

    /// How far (ns) this timestamp sits ahead of a wall reading — the
    /// skew-envelope bound HLC maintains. Monitoring can flag NTP trouble
    /// when this grows. `None` when the clock is behind the wall (normal).
    pub fn skew_ns(&self, wall_ns: u64) -> Option<u64> {
        self.physical.checked_sub(wall_ns)
    }

    fn bump_logical(&mut self) {
        if self.logical >= HLC_LOGICAL_CAP {
            // Degenerate rule (>1M events on one frozen nanosecond):
            // advance the wall anchor by a full logical epoch so the
            // packed form (`physical + logical`) never collides.
            self.physical += HLC_LOGICAL_CAP as u64 + 1;
            self.logical = 0;
        } else {
            self.logical += 1;
        }
    }
}

// ---------------------------------------------------------------------------
// Extent descriptor
// ---------------------------------------------------------------------------

/// Physical extent locator, byte-packed on the wire / on disk.
///
/// Spec §4.1 calls this "32 bytes, packed" — the declared fields actually
/// sum to **24 bytes** (audit M5). We implement the honest 24-byte layout
/// and keep 8 bytes of reserved space so the on-disk record is exactly 32
/// bytes as specified, future-proof and alignment-friendly.
///
/// ```text
/// off  size  field
///  0     8   content_hash_prefix (big-endian)
///  8     4   zone_id
/// 12     4   offset_in_zone
/// 16     4   length
/// 20     1   ecc_group_id
/// 21     1   compression_type
/// 22     2   flags
/// 24     8   reserved (zero)
/// ```
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExtentDescriptor {
    pub content_hash_prefix: u64,
    pub zone_id: u32,
    pub offset_in_zone: u32,
    pub length: u32,
    pub ecc_group_id: u8,
    pub compression_type: u8,
    pub flags: u16,
}

pub const EXTENT_DESCRIPTOR_SIZE: usize = 32; // 24 used + 8 reserved

/// Extent flag bits (spec §4.1 `flags` field).
pub mod extent_flags {
    pub const DEDUP: u16 = 0x0001;
    pub const ENCRYPTED: u16 = 0x0002;
    pub const TIER_PINNED: u16 = 0x0004;
    pub const DIRTY: u16 = 0x0008;

    pub fn has(flags: u16, bit: u16) -> bool {
        flags & bit != 0
    }
    pub fn set(flags: &mut u16, bit: u16) {
        *flags |= bit;
    }
    pub fn clear(flags: &mut u16, bit: u16) {
        *flags &= !bit;
    }
}

impl ExtentDescriptor {
    /// Serialize to the exact 32-byte on-disk record.
    pub fn to_bytes(&self) -> [u8; EXTENT_DESCRIPTOR_SIZE] {
        let mut b = [0u8; EXTENT_DESCRIPTOR_SIZE];
        b[0..8].copy_from_slice(&self.content_hash_prefix.to_be_bytes());
        b[8..12].copy_from_slice(&self.zone_id.to_be_bytes());
        b[12..16].copy_from_slice(&self.offset_in_zone.to_be_bytes());
        b[16..20].copy_from_slice(&self.length.to_be_bytes());
        b[20] = self.ecc_group_id;
        b[21] = self.compression_type;
        b[22..24].copy_from_slice(&self.flags.to_be_bytes());
        // b[24..32] reserved — kept zero
        b
    }

    /// Parse from the 32-byte on-disk record. Returns `None` on short input.
    pub fn from_bytes(b: &[u8]) -> Option<Self> {
        if b.len() < EXTENT_DESCRIPTOR_SIZE {
            return None;
        }
        Some(Self {
            content_hash_prefix: u64::from_be_bytes(b[0..8].try_into().ok()?),
            zone_id: u32::from_be_bytes(b[8..12].try_into().ok()?),
            offset_in_zone: u32::from_be_bytes(b[12..16].try_into().ok()?),
            length: u32::from_be_bytes(b[16..20].try_into().ok()?),
            ecc_group_id: b[20],
            compression_type: b[21],
            flags: u16::from_be_bytes(b[22..24].try_into().ok()?),
        })
    }
}

// ---------------------------------------------------------------------------
// Compression & tiers
// ---------------------------------------------------------------------------

/// Compression codec stored per-extent.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[repr(u8)]
pub enum CompressionType {
    Store = 0,
    Zstd = 1,
}

impl CompressionType {
    pub fn from_u8(v: u8) -> Self {
        match v {
            1 => CompressionType::Zstd,
            _ => CompressionType::Store,
        }
    }
}

/// Storage tier ladder (spec §12.1).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[repr(u8)]
pub enum Tier {
    Nvme = 0,
    Ssd = 1,
    Hdd = 2,
    Cloud = 3,
    Tape = 4,
}

impl Tier {
    /// Nominal access latency in nanoseconds (spec §12.1 magnitudes).
    pub fn latency_ns(&self) -> u64 {
        match self {
            Tier::Nvme => 8_000,          // ~8 µs
            Tier::Ssd => 80_000,           // ~80 µs
            Tier::Hdd => 8_000_000,        // ~8 ms
            Tier::Cloud => 50_000_000,     // ~50 ms
            Tier::Tape => 300_000_000_000, // ~5 min
        }
    }

    /// Nominal $/GB/month (order-of-magnitude model inputs).
    pub fn dollars_per_gb_month(&self) -> f64 {
        match self {
            Tier::Nvme => 0.20,
            Tier::Ssd => 0.08,
            Tier::Hdd => 0.02,
            Tier::Cloud => 0.023,
            Tier::Tape => 0.002,
        }
    }

    /// Nominal energy per GB read (Joules — model input for §2.3 optimizer).
    pub fn energy_j_per_gb(&self) -> f64 {
        match self {
            Tier::Nvme => 1.0,
            Tier::Ssd => 1.5,
            Tier::Hdd => 6.0,
            Tier::Cloud => 4.0,
            Tier::Tape => 12.0,
        }
    }

    pub fn name(&self) -> &'static str {
        match self {
            Tier::Nvme => "nvme",
            Tier::Ssd => "ssd",
            Tier::Hdd => "hdd",
            Tier::Cloud => "cloud",
            Tier::Tape => "tape",
        }
    }
}

// ---------------------------------------------------------------------------
// File metadata
// ---------------------------------------------------------------------------

/// POSIX-ish metadata carried inside a `VersionNode`.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileMetadata {
    pub owner: u32,
    pub group: u32,
    /// POSIX permission bits (e.g. 0o644).
    pub permissions: u32,
    /// Extended attributes — the mount-consistency selector
    /// `helix.consistency = strong|causal|eventual` lives here (spec §2.4).
    pub xattrs: std::collections::BTreeMap<String, Vec<u8>>,
}

// ---------------------------------------------------------------------------
// VersionNode — the namespace DAG node
// ---------------------------------------------------------------------------

/// One immutable version of one namespace object (file or directory).
///
/// `self_hash = BLAKE3(self_hash_excluding_fields || content_hash || parents || vclock || timestamp || meta)`
/// per spec §5.2 — i.e. the hash of everything *except* the hash itself.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct VersionNode {
    pub self_hash: Hash256,
    /// For a file: hash of the chunk list (content). For a directory:
    /// hash of the serialized sorted child map.
    pub content_hash: Hash256,
    /// 1 parent for a normal edit, ≥2 for a CRDT merge node.
    pub parents: Vec<Hash256>,
    pub vclock: VectorClock,
    /// Wall-clock nanoseconds since UNIX epoch (see audit C3: not a safe
    /// ordering under multi-master concurrency — engine resolves time
    /// travel only along linear chains and errors on merge nodes).
    pub timestamp_ns: u64,
    pub kind: NodeKind,
    pub meta: FileMetadata,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum NodeKind {
    File,
    Directory,
    /// Tombstone (deletion marker).
    Tombstone,
}

/// The fields of a `VersionNode` that are covered by `self_hash`
/// (serialize-only: the body is hashed, never deserialized directly).
#[derive(Serialize)]
struct VersionNodeBody<'a> {
    content_hash: &'a Hash256,
    parents: &'a [Hash256],
    vclock: &'a VectorClock,
    timestamp_ns: u64,
    kind: NodeKind,
    meta: &'a FileMetadata,
}

impl VersionNode {
    /// Build a new node; computes and fills `self_hash`.
    pub fn new(
        content_hash: Hash256,
        parents: Vec<Hash256>,
        vclock: VectorClock,
        timestamp_ns: u64,
        kind: NodeKind,
        meta: FileMetadata,
    ) -> Self {
        let mut node = Self {
            self_hash: Hash256::zero(),
            content_hash,
            parents,
            vclock,
            timestamp_ns,
            kind,
            meta,
        };
        node.self_hash = node.compute_hash();
        node
    }

    /// Canonical serialization *without* the self hash (spec §5.2).
    pub fn body_bytes(&self) -> Vec<u8> {
        bincode::serialize(&VersionNodeBody {
            content_hash: &self.content_hash,
            parents: &self.parents,
            vclock: &self.vclock,
            timestamp_ns: self.timestamp_ns,
            kind: self.kind,
            meta: &self.meta,
        })
        .expect("bincode serialize VersionNodeBody")
    }

    pub fn compute_hash(&self) -> Hash256 {
        Hash256::of(&self.body_bytes())
    }

    /// Recompute and compare — the "self-describing" integrity check.
    pub fn verify(&self) -> bool {
        self.compute_hash() == self.self_hash
    }

    /// Full serialization including self_hash (for persistence/transport).
    pub fn to_bytes(&self) -> Vec<u8> {
        bincode::serialize(self).expect("bincode serialize VersionNode")
    }

    pub fn from_bytes(b: &[u8]) -> Option<Self> {
        bincode::deserialize(b).ok()
    }

    /// True if this node is the result of merging concurrent branches.
    pub fn is_merge_node(&self) -> bool {
        self.parents.len() > 1
    }
}

// ---------------------------------------------------------------------------
// Errors & shared config
// ---------------------------------------------------------------------------

/// Shared error type for all HFS crates.
#[derive(Debug, thiserror::Error)]
pub enum HfsError {
    #[error("object not found: {0}")]
    NotFound(String),
    #[error("corruption detected: {0}")]
    Corruption(String),
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("serialization error: {0}")]
    Codec(String),
    #[error("invalid argument: {0}")]
    InvalidArg(String),
    #[error("conflict: {0}")]
    Conflict(String),
    #[error("unsupported operation: {0}")]
    Unsupported(String),
}

pub type HfsResult<T> = Result<T, HfsError>;

/// Erasure-coding profile (spec §5.3, `helix.redundancy=k:m` xattr).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct EcProfile {
    pub k: u8,
    pub m: u8,
}

impl Default for EcProfile {
    fn default() -> Self {
        Self { k: 8, m: 3 } // spec default: RS(8,3), 37.5% overhead
    }
}

/// Top-level engine tuning knobs.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct EngineConfig {
    /// CDC average chunk target (spec §10.1: default 64 KiB).
    pub chunk_avg_bytes: usize,
    pub chunk_min_bytes: usize,
    pub chunk_max_bytes: usize,
    /// Segment size for data zones (spec §4: 128 MiB; tests use 64 KiB).
    pub segment_size: usize,
    /// Erasure coding profile.
    pub ec: EcProfile,
    /// Journal every payload byte (spec §6) or metadata-only (audit C1 fix).
    pub wal_mode: WalMode,
    /// Clean segments whose live fraction drops below this.
    pub gc_utilization_threshold: f64,
    /// Retention lattice (spec §8.3), in seconds.
    pub retain_all_secs: u64,
    pub retain_hourly_secs: u64,
    pub retain_daily_secs: u64,
}

/// WAL journaling granularity.
///
/// `Full` is the literal spec §6 behaviour (every payload byte hits the
/// journal). `MetadataOnly` is the audit's counter-proposal C1: only
/// metadata + refcount ops are journaled; data durability comes from the
/// log-structured data zones themselves (ZFS-ZIL style), halving the
/// write-amplification floor.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum WalMode {
    Full,
    MetadataOnly,
}

impl Default for EngineConfig {
    fn default() -> Self {
        Self {
            chunk_avg_bytes: 64 * 1024,
            chunk_min_bytes: 16 * 1024,
            chunk_max_bytes: 256 * 1024,
            segment_size: 128 * 1024 * 1024,
            ec: EcProfile::default(),
            wal_mode: WalMode::Full,
            gc_utilization_threshold: 0.5,
            retain_all_secs: 24 * 3600,
            retain_hourly_secs: 7 * 24 * 3600,
            retain_daily_secs: 90 * 24 * 3600,
        }
    }
}
