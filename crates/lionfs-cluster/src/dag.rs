//! The Merkle-DAG namespace (spec §2.1, §5.2, §9).
//!
//! * Every object is a [`VersionNode`] identified by the hash of its
//!   content (Axiom 1).
//! * Directories are themselves VersionNodes whose `content_hash` covers a
//!   serialized sorted child map `{name → child HEAD}` — Merkle trees of
//!   children, exactly like Git tree objects, mutable only at the HEAD
//!   pointer (Axiom 2).
//! * A **snapshot is free** (spec §9): one hash recorded in the table,
//!   O(1), no data copied. Restore is an O(1) pointer swap.
//! * **Time travel** (spec §2.2): `resolve(path, t)` walks the root's
//!   version chain back to the newest root version with `ts ≤ t`, then
//!   descends. ⚠️ Audit C3: this is only well-defined on *linear* history —
//!   under multi-master CRDT merges, timestamps cannot order a DAG, and
//!   this implementation **errors out** on merge nodes instead of guessing
//!   (production needs hybrid logical clocks + snapshot cuts).
//! * **Diff** (spec §9): Merkle-pruned — identical subtrees are skipped by
//!   hash equality, so cost is O(changed nodes).

use crate::core::{Hash256, NodeKind, VersionNode};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

#[derive(Debug, thiserror::Error)]
pub enum DagError {
    #[error("node {0} not found")]
    MissingNode(Hash256),
    #[error("path {0} not found")]
    PathNotFound(String),
    #[error("not a directory: {0}")]
    NotADirectory(String),
    #[error("time travel across merge nodes is undefined (audit C3): {0}")]
    TimeTravelAcrossMerge(String),
    #[error("store error: {0}")]
    Store(#[from] crate::store::StoreError),
    #[error("serialization: {0}")]
    Codec(#[from] bincode::Error),
}

pub type DagResult<T> = Result<T, DagError>;

/// A directory's child map: sorted name → child HEAD.
pub type ChildMap = BTreeMap<String, Hash256>;

// ---------------------------------------------------------------------------
// Repository (content-addressed node storage)
// ---------------------------------------------------------------------------

/// Content-addressed VersionNode storage backed by the extent store.
/// (Prototype: an in-memory map with extent write-through; the checkpoint
/// serializes the map — production shards this across metadata nodes.)
#[derive(Default)]
pub struct NodeRepository {
    nodes: std::collections::HashMap<Hash256, VersionNode>,
}

impl NodeRepository {
    pub fn new() -> Self {
        Self::default()
    }

    /// Store a node; verifies self-consistency first (Axiom 3).
    pub fn put(&mut self, node: VersionNode) -> DagResult<Hash256> {
        if !node.verify() {
            return Err(DagError::MissingNode(node.self_hash)); // corrupted input
        }
        let h = node.self_hash;
        self.nodes.insert(h, node);
        Ok(h)
    }

    pub fn get(&self, hash: &Hash256) -> DagResult<&VersionNode> {
        self.nodes.get(hash).ok_or(DagError::MissingNode(*hash))
    }

    pub fn contains(&self, hash: &Hash256) -> bool {
        self.nodes.contains_key(hash)
    }

    pub fn len(&self) -> usize {
        self.nodes.len()
    }

    pub fn is_empty(&self) -> bool {
        self.nodes.is_empty()
    }

    /// Serialize for checkpointing.
    pub fn to_bytes(&self) -> DagResult<Vec<u8>> {
        let flat: Vec<(&Hash256, &VersionNode)> =
            self.nodes.iter().collect();
        Ok(bincode::serialize(&flat)?)
    }

    pub fn from_bytes(bytes: &[u8]) -> DagResult<Self> {
        let flat: Vec<(Hash256, VersionNode)> = bincode::deserialize(bytes)?;
        let mut nodes = std::collections::HashMap::with_capacity(flat.len());
        for (h, node) in flat {
            // Integrity: every node must verify on load (Axiom 3).
            if node.self_hash != h || !node.verify() {
                return Err(DagError::MissingNode(h));
            }
            nodes.insert(h, node);
        }
        Ok(Self { nodes })
    }
}

// ---------------------------------------------------------------------------
// Directory content
// ---------------------------------------------------------------------------

/// Serialize a child map and hash it (the directory's `content_hash`).
pub fn dir_content_hash(children: &ChildMap) -> Hash256 {
    Hash256::of(&bincode::serialize(children).expect("serialize child map"))
}

/// Borrowed serialization shape of [`Namespace::to_bytes`] (kept as named
/// aliases so the two halves of the checkpoint format cannot drift apart).
type NamespaceSerState<'a> = (
    Vec<u8>,
    Vec<(&'a Hash256, &'a ChildMap)>,
    Hash256,
    &'a Vec<(u64, Hash256)>,
    &'a std::collections::BTreeMap<String, Hash256>,
    u64,
    crate::core::NodeId,
);

/// Owned counterpart of [`NamespaceSerState`] — the shape
/// [`Namespace::from_bytes`] deserializes into.
type NamespaceDeState = (
    Vec<u8>,
    Vec<(Hash256, ChildMap)>,
    Hash256,
    Vec<(u64, Hash256)>,
    std::collections::BTreeMap<String, Hash256>,
    u64,
    crate::core::NodeId,
);

/// The namespace: node repository + directory maps + root chain.
pub struct Namespace {
    pub repo: NodeRepository,
    /// Directory child maps keyed by the directory's content_hash.
    dirs: std::collections::HashMap<Hash256, ChildMap>,
    /// Current root VersionNode hash.
    root_head: Hash256,
    /// Root version chain materialized newest-first: (timestamp, root hash).
    root_chain: Vec<(u64, Hash256)>,
    /// Snapshot table (spec §9): name → root hash. O(1) create/restore.
    snapshots: std::collections::BTreeMap<String, Hash256>,
    /// Monotone clock for this writer.
    clock: u64,
    /// Local node id (vector clock dimension).
    node_id: crate::core::NodeId,
    /// Nodes created since the last drain (engine WAL bookkeeping).
    created: Vec<(VersionNode, Option<ChildMap>)>,
}

impl Namespace {
    /// Create a fresh namespace with an empty root directory.
    pub fn new(node_id: crate::core::NodeId, start_ts_ns: u64) -> DagResult<Self> {
        let mut repo = NodeRepository::new();
        let empty: ChildMap = BTreeMap::new();
        let content = dir_content_hash(&empty);
        let mut vclock = crate::core::VectorClock::new();
        vclock.increment(node_id);
        let root = VersionNode::new(
            content,
            Vec::new(),
            vclock,
            start_ts_ns,
            NodeKind::Directory,
            Default::default(),
        );
        let root_hash = repo.put(root)?;
        let mut dirs = std::collections::HashMap::new();
        dirs.insert(content, empty);
        Ok(Self {
            repo,
            dirs,
            root_head: root_hash,
            root_chain: vec![(start_ts_ns, root_hash)],
            snapshots: BTreeMap::new(),
            clock: start_ts_ns,
            node_id,
            created: Vec::new(),
        })
    }

    pub fn root(&self) -> Hash256 {
        self.root_head
    }

    fn tick(&mut self) -> u64 {
        self.clock += 1;
        self.clock
    }

    /// Child map of a directory node.
    pub fn children_of(&self, dir_hash: &Hash256) -> DagResult<ChildMap> {
        let node = self.repo.get(dir_hash)?;
        if node.kind != NodeKind::Directory {
            return Err(DagError::NotADirectory(dir_hash.short()));
        }
        self.dirs
            .get(&node.content_hash)
            .cloned()
            .ok_or(DagError::MissingNode(node.content_hash))
    }

    /// Create a new directory VersionNode with the given children.
    fn new_dir_node_at(&mut self, children: ChildMap, parent: Hash256, ts: u64) -> DagResult<Hash256> {
        let content = dir_content_hash(&children);
        let vclock = {
            let parent_clock = self.repo.get(&parent).map(|p| p.vclock.clone()).unwrap_or_default();
            let mut v = parent_clock;
            v.increment(self.node_id);
            v
        };
        let node = VersionNode::new(content, vec![parent], vclock, ts, NodeKind::Directory, Default::default());
        self.dirs.insert(content, children.clone());
        let hash = self.repo.put(node.clone())?;
        self.created.push((node, Some(children)));
        Ok(hash)
    }

    /// Resolve `path` (e.g. `/a/b/file.txt`) under root `root`.
    pub fn resolve(&self, path: &str) -> DagResult<Hash256> {
        self.resolve_under(self.root_head, path)
    }

    fn resolve_under(&self, root: Hash256, path: &str) -> DagResult<Hash256> {
        let parts: Vec<&str> = path.trim_matches('/').split('/').filter(|s| !s.is_empty()).collect();
        let mut cur = root;
        for (i, part) in parts.iter().enumerate() {
            let children = self.children_of(&cur)?;
            let child = children.get(*part).ok_or_else(|| {
                let prefix = if i == 0 {
                    String::new()
                } else {
                    format!("/{}", parts[..i].join("/"))
                };
                DagError::PathNotFound(format!("{prefix}/{part}"))
            })?;
            cur = *child;
        }
        Ok(cur)
    }

    /// Update `path` (all parents must exist) to point at `new_head`,
    /// rebuilding every directory VersionNode up to the root — §6 steps
    /// 12-18 of the write path at namespace level. Returns the new root.
    pub fn set_path(&mut self, path: &str, new_head: Hash256) -> DagResult<Hash256> {
        let ts = self.tick();
        self.set_path_inner(path, new_head, false, ts)
    }

    /// Like [`Self::set_path`] but creates missing intermediate directories
    /// (mkdir -p semantics) — used for first writes to a path.
    pub fn set_path_create(&mut self, path: &str, new_head: Hash256) -> DagResult<Hash256> {
        let ts = self.tick();
        self.set_path_inner(path, new_head, true, ts)
    }

    /// `set_path_create` with an explicit wall-clock timestamp (engine
    /// entry point — all vnodes in one write share the write's timestamp).
    pub fn set_path_at(&mut self, path: &str, new_head: Hash256, ts_ns: u64) -> DagResult<Hash256> {
        self.set_path_inner(path, new_head, true, ts_ns)
    }

    fn set_path_inner(&mut self, path: &str, new_head: Hash256, create: bool, ts: u64) -> DagResult<Hash256> {
        let parts: Vec<String> = path
            .trim_matches('/')
            .split('/')
            .filter(|s| !s.is_empty())
            .map(String::from)
            .collect();
        if parts.is_empty() {
            return Err(DagError::NotADirectory("cannot set the root as a file".into()));
        }
        let root = self.root_head;
        let new_root = self.set_path_recursive(&root, &parts, new_head, create, ts)?;
        self.root_head = new_root;
        self.root_chain.push((ts, new_root));
        Ok(new_root)
    }

    /// Bottom-up rebuild: returns the new head of `dir_hash`'s subtree with
    /// `parts` → `file_head` installed (creating intermediate dirs if
    /// `create`).
    fn set_path_recursive(
        &mut self,
        dir_hash: &Hash256,
        parts: &[String],
        file_head: Hash256,
        create: bool,
        ts: u64,
    ) -> DagResult<Hash256> {
        let mut children = self.children_of(dir_hash)?;
        let child_head = match children.get(&parts[0]) {
            Some(h) => {
                let node = self.repo.get(h)?;
                if node.kind != NodeKind::Directory {
                    if parts.len() == 1 {
                        *h // overwrite file below
                    } else {
                        return Err(DagError::NotADirectory(parts[0].clone()));
                    }
                } else if parts.len() == 1 {
                    *h // existing file entry — will be replaced
                } else {
                    self.set_path_recursive(h, &parts[1..], file_head, create, ts)?
                }
            }
            None => {
                if parts.len() == 1 {
                    file_head
                } else if create {
                    // mkdir -p: empty directory, then recurse into it.
                    let empty: ChildMap = BTreeMap::new();
                    let new_dir = self.new_dir_node_at(empty, *dir_hash, ts)?;
                    self.set_path_recursive(&new_dir, &parts[1..], file_head, create, ts)?
                } else {
                    return Err(DagError::PathNotFound(parts[0].clone()));
                }
            }
        };
        // The entry value: file head at the leaf, subtree head otherwise.
        let entry_value = if parts.len() == 1 { file_head } else { child_head };
        children.insert(parts[0].clone(), entry_value);
        self.new_dir_node_at(children, *dir_hash, ts)
    }

    // -- snapshots (spec §9) -------------------------------------------------

    /// O(1) snapshot: record the current root hash.
    pub fn snapshot(&mut self, name: &str) -> Hash256 {
        let root = self.root_head;
        self.snapshots.insert(name.to_string(), root);
        root
    }

    /// O(1) restore: swap the HEAD pointer to a snapshot root.
    /// (Later writes fork from the restored point.)
    pub fn restore(&mut self, name: &str) -> DagResult<Hash256> {
        let root = self
            .snapshots
            .get(name)
            .ok_or_else(|| DagError::PathNotFound(format!("snapshot:{name}")))?;
        let root = *root;
        self.root_head = root;
        let ts = self.repo.get(&root)?.timestamp_ns;
        self.root_chain.push((ts, root));
        Ok(root)
    }

    pub fn snapshot_roots(&self) -> Vec<(String, Hash256)> {
        self.snapshots.iter().map(|(k, v)| (k.clone(), *v)).collect()
    }

    pub fn has_snapshot(&self, name: &str) -> bool {
        self.snapshots.contains_key(name)
    }

    // -- time travel (spec §2.2 / audit C3) ----------------------------------

    /// Resolve `path` as of time `t` (ns). Walks the root chain back to
    /// the newest root version with `ts ≤ t`, then resolves the path under
    /// that root.
    ///
    /// Errors on merge nodes in the walked chain — timestamps cannot order
    /// concurrent versions (audit C3); production must use HLCs + cuts.
    pub fn resolve_at(&self, path: &str, t: u64) -> DagResult<Hash256> {
        // Binary search the materialized chain (newest-first).
        let chain = &self.root_chain;
        if chain.is_empty() {
            return Err(DagError::MissingNode(self.root_head));
        }
        // The chain is ordered oldest→newest; scan newest-first and pick
        // the newest root version at or before `t`.
        let mut chosen = chain[0].1;
        for (ts, hash) in chain.iter().rev() {
            if *ts <= t {
                chosen = *hash;
                break;
            }
        }
        // Guard: the chosen root and its ancestry until `t` must be linear.
        let mut cur = chosen;
        loop {
            let node = self.repo.get(&cur)?;
            if node.timestamp_ns > t {
                // Walk parents until within t.
                match node.parents.first() {
                    Some(p) => cur = *p,
                    None => return Err(DagError::TimeTravelAcrossMerge("history predates t".into())),
                }
                continue;
            }
            if node.parents.len() > 1 {
                return Err(DagError::TimeTravelAcrossMerge(format!(
                    "root {} is a merge node — timestamp order is undefined (audit C3)",
                    node.self_hash.short()
                )));
            }
            break;
        }
        self.resolve_under(cur, path)
    }

    /// Root chain (newest first) for time-travel tooling.
    pub fn root_chain(&self) -> &[(u64, Hash256)] {
        &self.root_chain
    }

    // -- Merkle diff (spec §9) -------------------------------------------------

    /// Diff two roots, pruned by hash equality: O(changed nodes).
    pub fn diff(&self, a: Hash256, b: Hash256) -> DagResult<Vec<DiffEntry>> {
        let mut out = Vec::new();
        self.diff_walk(a, b, String::new(), &mut out)?;
        Ok(out)
    }

    fn diff_walk(&self, a: Hash256, b: Hash256, prefix: String, out: &mut Vec<DiffEntry>) -> DagResult<()> {
        if a == b {
            return Ok(()); // identical subtree — Merkle prune
        }
        let na = self.repo.get(&a)?;
        let nb = self.repo.get(&b)?;
        match (na.kind, nb.kind) {
            (NodeKind::Directory, NodeKind::Directory) => {
                if na.content_hash == nb.content_hash {
                    return Ok(()); // identical children — prune
                }
                let ma = self.children_of(&a)?;
                let mb = self.children_of(&b)?;
                let mut names: std::collections::BTreeSet<&String> = ma.keys().collect();
                names.extend(mb.keys().collect::<std::collections::BTreeSet<&String>>());
                for name in names {
                    let path = if prefix.is_empty() {
                        format!("/{name}")
                    } else {
                        format!("{prefix}/{name}")
                    };
                    match (ma.get(name), mb.get(name)) {
                        (Some(ha), Some(hb)) => self.diff_walk(*ha, *hb, path, out)?,
                        (Some(_), None) => out.push(DiffEntry { path, change: Change::Removed }),
                        (None, Some(_)) => out.push(DiffEntry { path, change: Change::Added }),
                        (None, None) => unreachable!(),
                    }
                }
            }
            _ => {
                if na.content_hash != nb.content_hash || na.kind != nb.kind {
                    out.push(DiffEntry { path: if prefix.is_empty() { "/".into() } else { prefix }, change: Change::Modified });
                }
            }
        }
        Ok(())
    }

    // -- engine support ---------------------------------------------------------

    /// Snapshot of serializable state (engine checkpoints).
    pub fn clone_state(&self) -> NamespaceState {
        NamespaceState {
            repo: self.repo.to_bytes().unwrap_or_default(),
            dirs: self.dirs.iter().map(|(k, v)| (*k, v.clone())).collect(),
            root_head: self.root_head,
            root_chain: self.root_chain.clone(),
            snapshots: self.snapshots.clone(),
            clock: self.clock,
            node_id: self.node_id,
        }
    }

    /// Restore from serialized state.
    pub fn from_state(state: NamespaceState) -> DagResult<Self> {
        Ok(Self {
            repo: NodeRepository::from_bytes(&state.repo)?,
            dirs: state.dirs.into_iter().collect(),
            root_head: state.root_head,
            root_chain: state.root_chain,
            snapshots: state.snapshots,
            clock: state.clock,
            node_id: state.node_id,
            created: Vec::new(),
        })
    }

    /// Nodes created since the last drain — the engine WALs these
    /// (VNode + DirMap records) so replay can rebuild the namespace.
    pub fn drain_created(&mut self) -> Vec<(VersionNode, Option<ChildMap>)> {
        std::mem::take(&mut self.created)
    }

    /// WAL replay hook: register a directory child map.
    pub fn register_dir(&mut self, content: Hash256, children: ChildMap) {
        self.dirs.insert(content, children);
    }

    /// WAL replay hook: set the recovered root (append to history chain).
    pub fn set_root_from_recovery(&mut self, root: Hash256, ts: u64) {
        if self.root_head != root {
            self.root_head = root;
            self.root_chain.push((ts, root));
        }
    }

    /// WAL replay hook: register a snapshot entry.
    pub fn snapshot_from_recovery(&mut self, name: String, root: Hash256) {
        self.snapshots.insert(name, root);
    }

    /// Public read-only child map access.
    pub fn children_public(&self, dir: &Hash256) -> DagResult<ChildMap> {
        self.children_of(dir)
    }

    /// Snapshot root by name.
    pub fn snapshot_root(&self, name: &str) -> Option<Hash256> {
        self.snapshots.get(name).copied()
    }

    // -- persistence -----------------------------------------------------------

    /// Serialize the whole namespace (checkpoint).
    pub fn to_bytes(&self) -> DagResult<Vec<u8>> {
        let dirs: Vec<(&Hash256, &ChildMap)> = self.dirs.iter().collect();
        let state: NamespaceSerState = (
            self.repo.to_bytes()?,
            dirs,
            self.root_head,
            &self.root_chain,
            &self.snapshots,
            self.clock,
            self.node_id,
        );
        Ok(bincode::serialize(&state)?)
    }

    pub fn from_bytes(bytes: &[u8]) -> DagResult<Self> {
        let (repo_bytes, dirs_flat, root_head, root_chain, snapshots, clock, node_id): NamespaceDeState =
            bincode::deserialize(bytes)?;
        Ok(Self {
            repo: NodeRepository::from_bytes(&repo_bytes)?,
            dirs: dirs_flat.into_iter().collect(),
            root_head,
            root_chain,
            snapshots,
            clock,
            node_id,
            created: Vec::new(),
        })
    }
}

/// Serializable namespace state (engine checkpoint payload).
#[derive(Serialize, Deserialize)]
pub struct NamespaceState {
    repo: Vec<u8>,
    dirs: Vec<(Hash256, ChildMap)>,
    root_head: Hash256,
    root_chain: Vec<(u64, Hash256)>,
    snapshots: std::collections::BTreeMap<String, Hash256>,
    clock: u64,
    node_id: crate::core::NodeId,
}

/// One difference between two roots.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct DiffEntry {
    pub path: String,
    pub change: Change,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Change {
    Added,
    Removed,
    Modified,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ns() -> Namespace {
        Namespace::new(1, 1_000_000).unwrap()
    }

    fn file_node(ns: &mut Namespace, content: &[u8], ts_hint: u64) -> Hash256 {
        let mut v = crate::core::VectorClock::new();
        v.increment(ns_node_id());
        let node = VersionNode::new(
            Hash256::of(content),
            Vec::new(),
            v,
            ts_hint,
            NodeKind::File,
            Default::default(),
        );
        ns.repo.put(node).unwrap()
    }

    fn ns_node_id() -> crate::core::NodeId {
        1
    }

    #[test]
    fn set_and_resolve_paths() {
        let mut n = ns();
        let f = file_node(&mut n, b"v1", 1_100_000);
        n.set_path_create("/docs/hello.txt", f).unwrap();
        assert_eq!(n.resolve("/docs/hello.txt").unwrap(), f);

        let f2 = file_node(&mut n, b"v2", 1_200_000);
        n.set_path_create("/docs/hello.txt", f2).unwrap();
        assert_eq!(n.resolve("/docs/hello.txt").unwrap(), f2);
        // Old version still resolvable by hash (immutable history).
        assert!(n.repo.contains(&f));
    }

    #[test]
    fn missing_paths_error_cleanly() {
        let mut n = ns();
        let f = file_node(&mut n, b"x", 1_100_000);
        n.set_path_create("/a/b.txt", f).unwrap();
        assert!(matches!(n.resolve("/a/nope.txt"), Err(DagError::PathNotFound(_))));
    }

    #[test]
    fn snapshot_is_o1_and_restorable() {
        let mut n = ns();
        let f1 = file_node(&mut n, b"one", 1_100_000);
        n.set_path_create("/data.txt", f1).unwrap();
        let snap = n.snapshot("before");

        let f2 = file_node(&mut n, b"two", 1_200_000);
        n.set_path_create("/data.txt", f2).unwrap();
        let f3 = file_node(&mut n, b"three", 1_300_000);
        n.set_path_create("/extra.txt", f3).unwrap();
        assert_eq!(n.resolve("/data.txt").unwrap(), f2);

        // Restore → back to the snapshot world.
        let restored = n.restore("before").unwrap();
        assert_eq!(restored, snap);
        assert_eq!(n.resolve("/data.txt").unwrap(), f1);
        assert!(matches!(n.resolve("/extra.txt"), Err(DagError::PathNotFound(_))));

        // Writes fork from the restored point.
        let f4 = file_node(&mut n, b"four", 1_400_000);
        n.set_path_create("/post-restore.txt", f4).unwrap();
        assert_eq!(n.resolve("/post-restore.txt").unwrap(), f4);
        assert_eq!(n.resolve("/data.txt").unwrap(), f1);
    }

    #[test]
    fn time_travel_resolves_past_versions() {
        let mut n = Namespace::new(1, 1_000_000).unwrap();
        // Three versions of /log.txt at t=1.1M, 1.2M, 1.3M.
        let v1 = file_node(&mut n, b"log-v1", 1_100_000);
        n.set_path_at("/log.txt", v1, 1_100_000).unwrap();
        let v2 = file_node(&mut n, b"log-v2", 1_200_000);
        n.set_path_at("/log.txt", v2, 1_200_000).unwrap();
        let v3 = file_node(&mut n, b"log-v3", 1_300_000);
        n.set_path_at("/log.txt", v3, 1_300_000).unwrap();

        assert_eq!(n.resolve("/log.txt").unwrap(), v3);
        assert_eq!(n.resolve_at("/log.txt", u64::MAX).unwrap(), v3);
        assert_eq!(n.resolve_at("/log.txt", 1_300_000).unwrap(), v3);
        assert_eq!(n.resolve_at("/log.txt", 1_250_000).unwrap(), v2);
        assert_eq!(n.resolve_at("/log.txt", 1_150_000).unwrap(), v1);
        // Before any version: path didn't exist.
        assert!(matches!(n.resolve_at("/log.txt", 1_000_500), Err(DagError::PathNotFound(_))));
    }

    #[test]
    fn merkle_diff_reports_only_changes() {
        let mut n = ns();
        let a1 = file_node(&mut n, b"a1", 1_100_000);
        let b1 = file_node(&mut n, b"b1", 1_100_001);
        n.set_path_create("/x/a.txt", a1).unwrap();
        n.set_path_create("/x/b.txt", b1).unwrap();
        let snap = n.snapshot("s1");

        let a2 = file_node(&mut n, b"a2", 1_200_000);
        let c = file_node(&mut n, b"c", 1_200_001);
        n.set_path_create("/x/a.txt", a2).unwrap();
        n.set_path_create("/x/c.txt", c).unwrap();

        let before = n.snapshot_roots().iter().find(|(k, _)| k == "s1").unwrap().1;
        let _ = snap;
        let diff = n.diff(before, n.root()).unwrap();
        // Expect: /x/a.txt modified, /x/c.txt added; /x/b.txt NOT reported.
        assert_eq!(diff.len(), 2, "diff entries: {diff:?}");
        assert!(diff.iter().any(|d| d.path == "/x/a.txt" && d.change == Change::Modified));
        assert!(diff.iter().any(|d| d.path == "/x/c.txt" && d.change == Change::Added));
        assert!(!diff.iter().any(|d| d.path == "/x/b.txt"), "unchanged subtree must be pruned");
    }

    #[test]
    fn identical_roots_diff_empty() {
        let mut n = ns();
        let f = file_node(&mut n, b"stable", 1_100_000);
        n.set_path_create("/f.txt", f).unwrap();
        let root = n.root();
        assert!(n.diff(root, root).unwrap().is_empty());
    }

    #[test]
    fn repository_integrity_on_load() {
        let mut n = ns();
        let f = file_node(&mut n, b"data", 1_100_000);
        n.set_path_create("/d.bin", f).unwrap();
        let bytes = n.to_bytes().unwrap();
        let back = Namespace::from_bytes(&bytes).unwrap();
        assert_eq!(back.resolve("/d.bin").unwrap(), f);
        // Tamper → load fails.
        let mut tampered = bytes.clone();
        tampered[0] ^= 0xFF;
        assert!(Namespace::from_bytes(&tampered).is_err());
    }

    #[test]
    fn merge_nodes_block_time_travel_explicitly() {
        // Audit C3 made executable: a merge node in the root chain must
        // refuse timestamp resolution rather than guess.
        let mut n = Namespace::new(1, 1_000_000).unwrap();
        let f = file_node(&mut n, b"x", 1_100_000);
        n.set_path_create("/f.txt", f).unwrap();
        // Hand-craft a merge node as a new root.
        let left = n.root();
        let right = {
            let mut v = crate::core::VectorClock::new();
            v.increment(2);
            n.repo
                .put(VersionNode::new(
                    Hash256::of(b"other"),
                    Vec::new(),
                    v,
                    1_150_000,
                    NodeKind::Directory,
                    Default::default(),
                ))
                .unwrap()
        };
        let merge = {
            let mut v = crate::core::VectorClock::new();
            v.increment(1);
            v.increment(2);
            n.repo
                .put(VersionNode::new(
                    Hash256::of(b"merged"),
                    vec![left, right],
                    v,
                    1_200_000,
                    NodeKind::Directory,
                    Default::default(),
                ))
                .unwrap()
        };
        n.root_head = merge;
        n.root_chain.push((1_200_000, merge));
        let result = n.resolve_at("/f.txt", 1_250_000);
        assert!(
            matches!(result, Err(DagError::TimeTravelAcrossMerge(_))),
            "merge node must refuse timestamp resolution, got {result:?}"
        );
    }
}
