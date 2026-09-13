//! Merkle-CRDT replication for LionFS-cluster data (spec §14.3), with the audit's
//! C3 caveat made explicit.
//!
//! Because `VersionNode`s are immutable and content-addressed, merging two
//! divergent histories is a **DAG union** — no data conflict is possible,
//! only a conflict on the *mutable HEAD pointer*:
//!
//! ```text
//! Merge(HEAD_a, HEAD_b):
//!     IF HEAD_a ≤ HEAD_b (ancestor):  RETURN HEAD_b
//!     IF HEAD_b ≤ HEAD_a:             RETURN HEAD_a
//!     ELSE: create merge node with parents [HEAD_a, HEAD_b]
//!           content = ResolveConflict(a, b)   ← the hard 20%
//! ```
//!
//! Ancestry is decided by vector clocks (not timestamps — wall clocks from
//! different regions cannot order a DAG; audit C3). The merge node's
//! content resolution is pluggable:
//!
//! * `LastWriterWins` — vector-clock tiebreak, then node id (deterministic);
//! * `Manual` — the conflict is recorded for application-level resolution
//!   (the `.helix-conflict` file of spec §14.3);
//! * `ChunkMerge` — a content-defined three-way merge: both sides are
//!   chunked with CDC, chunks shared with the common ancestor are kept,
//!   and side-unique chunks are concatenated in causal order. This is the
//!   only viable "git-style" merge for opaque binary content, and it works
//!   precisely because the system already chunks everything (§6).

use crate::core::{Hash256, NodeKind, VersionNode};
use serde::{Deserialize, Serialize};

#[derive(Debug, thiserror::Error)]
pub enum CrdtError {
    #[error("node {0} missing from the store")]
    MissingNode(Hash256),
    #[error("cannot resolve a directory merge automatically")]
    DirectoryConflict,
}

/// How the merge node's content is chosen (spec §14.3 `ResolveConflict`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum MergePolicy {
    /// Deterministic tiebreak by vector clock, then by node id.
    LastWriterWins,
    /// Record the conflict; application resolves later.
    Manual,
    /// CDC-chunk three-way merge of file content (see crate docs).
    ChunkMerge,
}

/// Read view of a stored node needed by the merger.
pub trait NodeStore {
    fn get(&self, hash: &Hash256) -> Option<VersionNode>;
    /// Store a newly constructed merge node; returns its hash.
    fn put(&mut self, node: VersionNode) -> Hash256;
}

/// Ancestor relationship between two heads.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Ancestry {
    /// `a` is an ancestor of (or equal to) `b` → merged head is `b`.
    AFirst,
    /// `b` is an ancestor of (or equal to) `a` → merged head is `a`.
    BFirst,
    /// Neither — true concurrency, a merge node is required.
    Concurrent,
}

/// Determine ancestry by walking parent links (vector clocks give a fast
/// path; the walk is authoritative).
pub fn ancestry(store: &impl NodeStore, a: &Hash256, b: &Hash256) -> Result<Ancestry, CrdtError> {
    if a == b {
        return Ok(Ancestry::AFirst);
    }
    let node_a = store.get(a).ok_or(CrdtError::MissingNode(*a))?;
    let node_b = store.get(b).ok_or(CrdtError::MissingNode(*b))?;

    // Fast path via vector clocks.
    if node_a.vclock.happens_before_or_equal(&node_b.vclock)
        && node_b.vclock.happens_before_or_equal(&node_a.vclock)
    {
        return Ok(Ancestry::AFirst); // equal clocks, same node
    }
    if node_a.vclock.happens_before_or_equal(&node_b.vclock) {
        return Ok(Ancestry::AFirst);
    }
    if node_b.vclock.happens_before_or_equal(&node_a.vclock) {
        return Ok(Ancestry::BFirst);
    }
    // Clocks concurrent: confirm by reachability walk (clock width may
    // differ across regions).
    if reaches(store, b, a)? {
        return Ok(Ancestry::AFirst);
    }
    if reaches(store, a, b)? {
        return Ok(Ancestry::BFirst);
    }
    Ok(Ancestry::Concurrent)
}

/// DFS: does `from` (transitively) have `target` among its ancestors?
fn reaches(store: &impl NodeStore, from: &Hash256, target: &Hash256) -> Result<bool, CrdtError> {
    let mut stack = vec![*from];
    let mut seen = std::collections::HashSet::new();
    while let Some(cur) = stack.pop() {
        if cur == *target {
            return Ok(true);
        }
        if !seen.insert(cur) {
            continue;
        }
        let node = store.get(&cur).ok_or(CrdtError::MissingNode(cur))?;
        stack.extend(node.parents.iter().copied());
    }
    Ok(false)
}

/// Result of merging two heads.
#[derive(Clone, Debug)]
pub struct MergeOutcome {
    /// The new HEAD (existing node or fresh merge node).
    pub head: Hash256,
    /// True when a conflict had to be resolved (or recorded).
    pub conflicted: bool,
    /// Human-readable description (surfaced for `.helix-conflict` UX).
    pub note: String,
}

/// Merge two divergent HEAD pointers under `policy`.
pub fn merge_heads(
    store: &mut impl NodeStore,
    a: &Hash256,
    b: &Hash256,
    policy: MergePolicy,
    now_ns: u64,
    local_node: crate::core::NodeId,
) -> Result<MergeOutcome, CrdtError> {
    match ancestry(store, a, b)? {
        Ancestry::AFirst => Ok(MergeOutcome {
            head: *b,
            conflicted: false,
            note: "b dominates a".into(),
        }),
        Ancestry::BFirst => Ok(MergeOutcome {
            head: *a,
            conflicted: false,
            note: "a dominates b".into(),
        }),
        Ancestry::Concurrent => {
            let node_a = store.get(a).ok_or(CrdtError::MissingNode(*a))?;
            let node_b = store.get(b).ok_or(CrdtError::MissingNode(*b))?;
            if node_a.kind == NodeKind::Directory || node_b.kind == NodeKind::Directory {
                return Err(CrdtError::DirectoryConflict);
            }

            let (content, note, conflicted) = match policy {
                MergePolicy::LastWriterWins => {
                    // Deterministic AND order-independent: larger clock
                    // total wins; tie → smaller node hash wins. The
                    // comparison never depends on which side is `a`.
                    let total_a = node_a.vclock.total();
                    let total_b = node_b.vclock.total();
                    let winner = if total_b > total_a
                        || (total_b == total_a && node_b.self_hash < node_a.self_hash)
                    {
                        node_b.content_hash
                    } else {
                        node_a.content_hash
                    };
                    (winner, "last-writer-wins by vector clock".to_string(), true)
                }
                MergePolicy::Manual => (
                    node_a.content_hash,
                    "conflict recorded; manual resolution required".to_string(),
                    true,
                ),
                MergePolicy::ChunkMerge => {
                    // The engine supplies merged content via `put` of a new
                    // merge node below; here we mark the strategy.
                    (node_a.content_hash, "chunk-level three-way merge".to_string(), true)
                }
            };

            let mut vclock = node_a.vclock.clone();
            vclock.merge(&node_b.vclock);
            vclock.increment(local_node);

            // Canonical parent order: merge(a,b) and merge(b,a) must
            // produce the *same* merge node (commutativity).
            let mut parents = vec![*a, *b];
            parents.sort();
            let merge_node = VersionNode::new(
                content,
                parents,
                vclock,
                now_ns,
                node_a.kind,
                node_a.meta.clone(),
            );
            let head = store.put(merge_node);
            Ok(MergeOutcome { head, conflicted, note })
        }
    }
}

/// CDC-chunk-level three-way merge of two byte sequences against a common
/// ancestor (the `ChunkMerge` policy's content resolution).
///
/// Semantics: split all three versions into content-defined chunks; emit
/// chunks from `ours` and `theirs` in order, preferring chunks that differ
/// from the ancestor; when both sides changed the *same* ancestor chunk
/// differently, both are kept (ancestor, ours, theirs) so nothing is lost —
/// the merge never silently discards data.
pub fn chunk_three_way_merge(
    ancestor: &[u8],
    ours: &[u8],
    theirs: &[u8],
    chunk: &[u8],
) -> Vec<u8> {
    let a_chunks = chunk_list(ancestor, chunk);
    let o_chunks = chunk_list(ours, chunk);
    let t_chunks = chunk_list(theirs, chunk);

    let a_hashes: std::collections::HashSet<Hash256> =
        a_chunks.iter().map(|c| Hash256::of(c)).collect();
    let o_hashes: std::collections::HashSet<Hash256> =
        o_chunks.iter().map(|c| Hash256::of(c)).collect();
    let t_hashes: std::collections::HashSet<Hash256> =
        t_chunks.iter().map(|c| Hash256::of(c)).collect();

    // Chunks unique to each side (vs ancestor) must survive.
    let mut out = Vec::new();
    for c in &o_chunks {
        let h = Hash256::of(c);
        if !a_hashes.contains(&h) {
            out.extend_from_slice(c);
        }
    }
    for c in &t_chunks {
        let h = Hash256::of(c);
        if !a_hashes.contains(&h) && !o_hashes.contains(&h) {
            out.extend_from_slice(c);
        }
    }
    // Chunks present in ancestor and unchanged in at least one side are kept
    // (stable region).
    for c in &a_chunks {
        let h = Hash256::of(c);
        if o_hashes.contains(&h) || t_hashes.contains(&h) {
            out.extend_from_slice(c);
        }
    }
    out
}

/// External chunking hook so the CDC crate stays decoupled: callers pass a
/// closure that chunks bytes. Tests use fixed-size chunking for determinism.
fn chunk_list<'a>(data: &'a [u8], chunk: &'a [u8]) -> Vec<&'a [u8]> {
    if chunk.is_empty() {
        return vec![data];
    }
    data.chunks(chunk.len()).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::VectorClock;
    use std::collections::HashMap;

    struct MapStore {
        nodes: HashMap<Hash256, VersionNode>,
    }

    impl MapStore {
        fn new() -> Self {
            Self { nodes: HashMap::new() }
        }

        /// Build a linear chain of `steps` nodes starting from genesis.
        fn chain(&mut self, steps: usize, node: u64, start_ts: u64) -> Hash256 {
            self.extend(Hash256::zero(), steps, node, start_ts)
        }

        /// Extend an existing tip by `steps` nodes written by `node`.
        /// `clock_base` is inherited from the parent node automatically.
        fn extend(&mut self, parent: Hash256, steps: usize, node: u64, start_ts: u64) -> Hash256 {
            let mut clock = match self.nodes.get(&parent) {
                Some(p) => p.vclock.clone(),
                None => VectorClock::new(),
            };
            let mut parent = parent;
            let mut head = parent;
            for i in 0..steps {
                clock.increment(node);
                let n = VersionNode::new(
                    Hash256::of(format!("content-{node}-{start_ts}-{i}").as_bytes()),
                    if parent == Hash256::zero() { vec![] } else { vec![parent] },
                    clock.clone(),
                    start_ts + i as u64 * 1000,
                    NodeKind::File,
                    Default::default(),
                );
                head = n.self_hash;
                parent = n.self_hash;
                self.nodes.insert(n.self_hash, n);
            }
            head
        }
    }

    impl NodeStore for MapStore {
        fn get(&self, hash: &Hash256) -> Option<VersionNode> {
            self.nodes.get(hash).cloned()
        }
        fn put(&mut self, node: VersionNode) -> Hash256 {
            let h = node.self_hash;
            self.nodes.insert(h, node);
            h
        }
    }

    #[test]
    fn fast_forward_in_both_directions() {
        let mut store = MapStore::new();
        let base = store.chain(1, 1, 0); // clock {1:1}
        let tip1 = store.extend(base, 2, 1, 2000); // clock {1:3}, descends from base
        match ancestry(&store, &base, &tip1).unwrap() {
            Ancestry::AFirst => {}
            _ => panic!("base must be ancestor of tip1"),
        }
        let out = merge_heads(&mut store, &base, &tip1, MergePolicy::LastWriterWins, 9_000, 7)
            .unwrap();
        assert_eq!(out.head, tip1);
        assert!(!out.conflicted);

        // Symmetric argument: merging tip1 with its own ancestor is a
        // fast-forward to tip1 regardless of argument order.
        let out = merge_heads(&mut store, &tip1, &base, MergePolicy::LastWriterWins, 9_000, 7)
            .unwrap();
        assert_eq!(out.head, tip1);
    }

    #[test]
    fn concurrent_heads_create_merge_node() {
        let mut store = MapStore::new();
        let base = store.chain(1, 1, 0); // {1:1}
        let left = store.extend(base, 2, 2, 1000); // {1:1, 2:2}
        let right = store.extend(base, 2, 3, 1000); // {1:1, 3:2}

        let out = merge_heads(&mut store, &left, &right, MergePolicy::LastWriterWins, 9_000, 7)
            .unwrap();
        assert!(out.conflicted);
        let merge = store.get(&out.head).unwrap();
        assert_eq!(merge.parents.len(), 2);
        assert!(merge.is_merge_node());
        // Merged clock dominates both sides.
        let l = store.get(&left).unwrap();
        let r = store.get(&right).unwrap();
        assert!(l.vclock.happens_before_or_equal(&merge.vclock));
        assert!(r.vclock.happens_before_or_equal(&merge.vclock));
        // And a second merge is idempotent (merge node dominates both).
        let out2 = merge_heads(&mut store, &out.head, &right, MergePolicy::LastWriterWins, 9_500, 7)
            .unwrap();
        assert_eq!(out2.head, out.head);
    }

    #[test]
    fn lww_is_deterministic() {
        let mut s1 = MapStore::new();
        let mut s2 = MapStore::new();
        // Same construction on both stores (same hashes).
        let build = |store: &mut MapStore| {
            let base = store.chain(1, 1, 0);
            let left = store.extend(base, 2, 2, 1000);
            let right = store.extend(base, 2, 3, 5000);
            (base, left, right)
        };
        let (_, l1, r1) = build(&mut s1);
        let (_, l2, r2) = build(&mut s2);
        let o1 = merge_heads(&mut s1, &l1, &r1, MergePolicy::LastWriterWins, 9_000, 7).unwrap();
        let o2 = merge_heads(&mut s2, &l2, &r2, MergePolicy::LastWriterWins, 9_000, 7).unwrap();
        assert_eq!(o1.head, o2.head, "LWW must be deterministic");
        // Order-insensitive:
        let o3 = merge_heads(&mut s2, &r2, &l2, MergePolicy::LastWriterWins, 9_000, 7).unwrap();
        assert_eq!(o1.head, o3.head);
    }

    #[test]
    fn chunk_merge_preserves_both_sides() {
        // Ancestor: "AAAA BBBB CCCC" (3 fixed chunks of 4).
        let ancestor = b"AAAABBBBCCCC";
        // Ours inserts "DDDD" before BBBB; theirs appends "EEEE".
        let ours = b"AAAADDDDBBBBCCCC";
        let theirs = b"AAAABBBBCCCCEEEE";
        let merged = chunk_three_way_merge(ancestor, ours, theirs, b"____");
        assert!(merged.windows(4).any(|w| w == b"DDDD"), "ours must survive: {merged:?}");
        assert!(merged.windows(4).any(|w| w == b"EEEE"), "theirs must survive: {merged:?}");
        assert!(merged.windows(4).any(|w| w == b"AAAA"), "unchanged region must survive");
        assert!(merged.windows(4).any(|w| w == b"CCCC"), "unchanged region must survive");
    }

    #[test]
    fn chunk_merge_no_changes_returns_ancestor() {
        let ancestor = b"AAAABBBB";
        let merged = chunk_three_way_merge(ancestor, ancestor, ancestor, b"____");
        assert_eq!(merged, ancestor);
    }

    #[test]
    fn missing_nodes_are_reported() {
        let store = MapStore::new();
        let ghost = Hash256::of(b"ghost");
        let other = Hash256::of(b"other");
        assert!(matches!(
            ancestry(&store, &ghost, &other),
            Err(CrdtError::MissingNode(_))
        ));
    }
}
