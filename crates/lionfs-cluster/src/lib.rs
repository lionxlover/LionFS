//! # lionfs-cluster — the distributed data plane of LionFS 8.0
//!
//! This crate is the merge of the former HelixFS (HFS) project into
//! LionFS. It provides everything the local engine (the `lionfs` crate)
//! does not: cluster consensus, replication, and cross-node data
//! management.
//!
//! * [`core`]      — shared types: 256-bit content hashes, HLC clocks,
//!                   node IDs, version nodes, tier enums.
//! * [`cdc`]       — content-defined chunking (GF(2) rolling hash, boundary
//!                   conditions) feeding the dedup index.
//! * [`ecc`]       — systematic Reed-Solomon erasure coding over GF(256)
//!                   (M = V·V_top^-1), shard encode/reconstruct/verify.
//! * [`btree`]     — write-optimized B-epsilon tree with message upserts
//!                   and convergent `flush_all`.
//! * [`wal`]       — write-ahead log with idempotent checkpoint recovery.
//! * [`crypto`]    — domain-scoped convergent encryption (dedup-compatible)
//!                   plus per-file keys, AEAD ciphers, key tree.
//! * [`reliability`] — MTTDL / write-amplification / Zipf cache math with
//!                   corrected formulas (WA = 1/(1-u), MTTDL constants).
//! * [`tier`]      — hotness-aware tier placement scorer.
//! * [`dedup`]     — cluster-wide chunk dedup index (domain-scoped keys).
//! * [`crdt`]      — conflict-free replicated namespace (version clocks,
//!                   add-wins / tombstone merge semantics).
//! * [`raft`]      — Raft consensus (leader election, log replication,
//!                   in-memory transport for deterministic tests).
//! * [`store`]     — content-addressed chunk store on block devices
//!                   (in-mem + file devices, extents, compression).
//! * [`dag`]       — version DAG namespace + time-travel `resolve(path, t)`.
//! * [`engine`]    — the [`ClusterEngine`] orchestrator wiring all of the
//!                   above into a mountable cluster filesystem.
//!
//! Every module keeps its original tests; the crate is green as a unit.

pub mod core;
pub mod cdc;
pub mod ecc;
pub mod btree;
pub mod wal;
pub mod crypto;
pub mod reliability;
pub mod tier;
pub mod dedup;
pub mod crdt;
pub mod raft;
pub mod store;
pub mod dag;
pub mod engine;

pub use crate::core::EngineConfig;
pub use engine::{ClusterEngine, EngineError as ClusterError};

/// LionFS-cluster release version (tracks the workspace version).
pub const VERSION: &str = "8.0.0";
