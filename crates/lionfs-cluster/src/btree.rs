//! Bε-tree: a write-optimized index that amortizes small writes through
//! per-node **message buffers** (spec §5.1).
//!
//! Classic B-tree: every insert touches a root-to-leaf path — O(log_B N)
//! *expensive* page writes. A Bε-tree instead drops a message into the
//! root's buffer and returns; buffers flush lazily down the tree in
//! amortized batches:
//!
//! ```text
//! T_insert = O( log_B N / B^(1-ε) )   amortized        (spec §5.1)
//! T_point  = O( log_B N · B^ε / ε + log B )            (see audit M3:
//!                                                       the B^ε factor is a
//!                                                       buffer-scan cost)
//! ```
//!
//! ε ∈ (0, 1] trades write amplification against read amplification:
//! fanout = Θ(B^ε) children per node, buffer = Θ((1-ε)·B) messages per node.
//!
//! ⚠️ Direction of ε (audit M3b): by the insert bound above, **small ε =
//! write-optimized** (tiny fanout, huge buffers — the Arge buffer tree
//! limit as ε→0); **ε → 1 = read-optimized** (a plain B-tree). The spec's
//! §5.1 prose states the opposite ("write-heavy (large ε)"), which
//! contradicts its own formula; this implementation follows the formula.
//!
//! Semantics: values are versioned by **upsert** and **delete** messages
//! carrying a global sequence number, so a point lookup merges the leaf
//! entry with every pending message along the root-to-leaf path, in
//! sequence order — the tree is always logically consistent even with
//! full buffers.
//!
//! Structural note: while a node's buffer is being flushed, that node is
//! allowed to transiently exceed its child budget (a *split barrier*):
//! flushing a message can split a child leaf, which inserts a new child
//! into the node being flushed; splitting the flushed node mid-loop would
//! leave its remaining buffered messages routing into the wrong half.
//! The barrier defers the flushed node's own split to a post-loop repair
//! pass, which restores the fanout invariant.

use serde::{Deserialize, Serialize};

/// Tree shape knobs (the ε parameter made concrete).
#[derive(Clone, Copy, Debug, Serialize, Deserialize)]
pub struct TreeConfig {
    /// Max children per internal node ≈ B^ε (fanout).
    pub max_children: usize,
    /// Max buffered messages per internal node ≈ (1-ε)·B.
    pub buffer_capacity: usize,
    /// Max sorted entries per leaf.
    pub leaf_max_entries: usize,
}

impl Default for TreeConfig {
    fn default() -> Self {
        // Production-ish defaults; tests use tiny values to force splits.
        Self { max_children: 32, buffer_capacity: 256, leaf_max_entries: 256 }
    }
}

/// Pick a tree shape for an observed read:write ratio (spec §12 auto-tune).
///
/// Write-heavy → small ε (small fanout, large buffers → cheap writes).
/// Read-heavy → large ε (large fanout, shallow tree → cheap reads).
pub fn tree_shape_for_ratio(reads: u64, writes: u64) -> TreeConfig {
    let total = (reads + writes).max(1) as f64;
    let write_share = writes as f64 / total; // ∈ [0, 1]
    let epsilon = 0.9 - 0.65 * write_share; // ε ∈ [0.25, 0.9]
    let b = 256usize;
    let max_children = ((b as f64).powf(epsilon) as usize).clamp(4, 64);
    let buffer_capacity = (((1.0 - epsilon) * b as f64) as usize).clamp(8, 512);
    TreeConfig {
        max_children,
        buffer_capacity,
        leaf_max_entries: buffer_capacity,
    }
}

#[derive(Debug, thiserror::Error)]
pub enum BTreeError {
    #[error("corrupt tree: {0}")]
    Corrupt(String),
}

type NodeId = usize;

/// The deferred mutation carried by a Bε-tree message.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum Op<V> {
    Upsert(V),
    Delete,
}

/// A deferred mutation routed by key (Bε-tree upsert/tombstone message).
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Message<V> {
    pub key: Vec<u8>,
    pub seq: u64,
    pub op: Op<V>,
}

/// A child pointer of an internal node: `bound` is the minimum key of the
/// child's subtree (`None` = −∞, the first child).
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Child {
    pub bound: Option<Vec<u8>>,
    pub id: NodeId,
}

/// Tree node: an internal node buffers messages and routes to children;
/// a leaf holds sorted key/value entries.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[non_exhaustive]
pub enum Node<V> {
    Internal {
        parent: Option<NodeId>,
        children: Vec<Child>,
        buffer: Vec<Message<V>>,
    },
    Leaf {
        parent: Option<NodeId>,
        entries: Vec<(Vec<u8>, V)>,
    },
}

/// A Bε-tree keyed by byte strings.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct BeTree<V> {
    nodes: Vec<Node<V>>,
    root: NodeId,
    config: TreeConfig,
    next_seq: u64,
    live_entries: u64, // exact after flush_all()
    pending: u64,      // messages still sitting in buffers
    /// Node whose split is currently deferred (see module docs).
    split_barrier: Option<NodeId>,
}

impl<V: Clone> BeTree<V> {
    pub fn new(config: TreeConfig) -> Self {
        assert!(config.max_children >= 2, "fanout must be >= 2");
        assert!(config.buffer_capacity >= 2, "buffer capacity must be >= 2");
        assert!(config.leaf_max_entries >= 2, "leaf capacity must be >= 2");
        Self {
            nodes: vec![Node::Leaf { parent: None, entries: Vec::new() }],
            root: 0,
            config,
            next_seq: 0,
            live_entries: 0,
            pending: 0,
            split_barrier: None,
        }
    }

    pub fn config(&self) -> TreeConfig {
        self.config
    }

    /// Number of live entries (exact after `flush_all`).
    pub fn live_entries(&self) -> u64 {
        self.live_entries
    }

    /// Messages still buffered (write-backlog depth).
    pub fn pending_messages(&self) -> u64 {
        self.pending
    }

    // -- node plumbing -----------------------------------------------------

    fn node(&self, id: NodeId) -> &Node<V> {
        &self.nodes[id]
    }

    fn node_mut(&mut self, id: NodeId) -> &mut Node<V> {
        &mut self.nodes[id]
    }

    fn is_leaf(&self, id: NodeId) -> bool {
        matches!(self.nodes[id], Node::Leaf { .. })
    }

    fn parent_of(&self, id: NodeId) -> Option<NodeId> {
        match &self.nodes[id] {
            Node::Internal { parent, .. } => *parent,
            Node::Leaf { parent, .. } => *parent,
        }
    }

    /// Last child whose bound ≤ key (children sorted by bound).
    fn route(&self, id: NodeId, key: &[u8]) -> NodeId {
        match self.node(id) {
            Node::Internal { children, .. } => {
                let mut chosen = children[0].id;
                for child in children {
                    match &child.bound {
                        None => chosen = child.id,
                        Some(b) if b.as_slice() <= key => chosen = child.id,
                        _ => break,
                    }
                }
                chosen
            }
            Node::Leaf { .. } => panic!("route() called on leaf"),
        }
    }

    /// Number of children of a node (0 for leaves). Test/telemetry aid.
    #[cfg(test)]
    fn children_count(&self, id: NodeId) -> usize {
        match self.node(id) {
            Node::Internal { children, .. } => children.len(),
            _ => 0,
        }
    }

    // -- public operations --------------------------------------------------

    /// Insert/replace `key → value` (buffered; returns immediately).
    pub fn insert(&mut self, key: &[u8], value: V) {
        let seq = self.bump_seq();
        self.enqueue(Message { key: key.to_vec(), seq, op: Op::Upsert(value) });
    }

    /// Remove `key` (buffered tombstone).
    pub fn delete(&mut self, key: &[u8]) {
        let seq = self.bump_seq();
        self.enqueue(Message { key: key.to_vec(), seq, op: Op::Delete });
    }

    fn bump_seq(&mut self) -> u64 {
        self.next_seq += 1;
        self.next_seq
    }

    fn enqueue(&mut self, msg: Message<V>) {
        if self.is_leaf(self.root) {
            // Tiny tree: apply directly to the leaf root.
            let root = self.root;
            self.apply_to_leaf(root, msg);
            self.maybe_split_leaf(root);
            return;
        }
        self.pending += 1;
        let root = self.root;
        let full = match self.node(root) {
            Node::Internal { buffer, .. } => buffer.len() >= self.config.buffer_capacity,
            _ => false,
        };
        if full {
            self.flush_node(root); // also repairs/splits the root at its end
        }
        if let Node::Internal { buffer, .. } = self.node_mut(self.root) {
            buffer.push(msg);
        }
    }

    /// Point lookup: leaf entry merged with all pending messages on the
    /// root-to-leaf path, in sequence order.
    pub fn get(&self, key: &[u8]) -> Option<V> {
        let mut pending: Vec<&Message<V>> = Vec::new();
        let mut cur = self.root;
        loop {
            match self.node(cur) {
                Node::Internal { children, buffer, .. } => {
                    for msg in buffer {
                        if msg.key.as_slice() == key {
                            pending.push(msg);
                        }
                    }
                    cur = {
                        let mut chosen = children[0].id;
                        for child in children {
                            match &child.bound {
                                None => chosen = child.id,
                                Some(b) if b.as_slice() <= key => chosen = child.id,
                                _ => break,
                            }
                        }
                        chosen
                    };
                }
                Node::Leaf { entries, .. } => {
                    let mut value = entries
                        .binary_search_by(|(k, _)| k.as_slice().cmp(key))
                        .ok()
                        .map(|i| entries[i].1.clone());
                    pending.sort_by_key(|m| m.seq);
                    for msg in pending {
                        match &msg.op {
                            Op::Upsert(v) => value = Some(v.clone()),
                            Op::Delete => value = None,
                        }
                    }
                    return value;
                }
            }
        }
    }

    /// Range scan `[lo, hi]` (inclusive; `None` = unbounded on that side).
    /// Buffers along the visited paths are merged by sequence number.
    pub fn scan(&self, lo: Option<&[u8]>, hi: Option<&[u8]>) -> Vec<(Vec<u8>, V)> {
        let mut msgs: Vec<Message<V>> = Vec::new();
        let mut leaves: Vec<NodeId> = Vec::new();
        self.collect_range(self.root, lo, hi, &mut msgs, &mut leaves);

        let mut result: std::collections::BTreeMap<Vec<u8>, Option<V>> =
            std::collections::BTreeMap::new();
        for leaf_id in leaves {
            if let Node::Leaf { entries, .. } = self.node(leaf_id) {
                for (k, v) in entries {
                    if in_range(k, lo, hi) {
                        result.insert(k.clone(), Some(v.clone()));
                    }
                }
            }
        }
        msgs.sort_by_key(|m| m.seq);
        for msg in msgs {
            if in_range(&msg.key, lo, hi) {
                match msg.op {
                    Op::Upsert(v) => {
                        result.insert(msg.key.clone(), Some(v));
                    }
                    Op::Delete => {
                        result.insert(msg.key.clone(), None);
                    }
                }
            }
        }
        result
            .into_iter()
            .filter_map(|(k, v)| v.map(|v| (k, v)))
            .collect()
    }

    fn collect_range(
        &self,
        id: NodeId,
        lo: Option<&[u8]>,
        hi: Option<&[u8]>,
        msgs: &mut Vec<Message<V>>,
        leaves: &mut Vec<NodeId>,
    ) {
        match self.node(id) {
            Node::Internal { children, buffer, .. } => {
                for msg in buffer {
                    if in_range(&msg.key, lo, hi) {
                        msgs.push(msg.clone());
                    }
                }
                // Visit every child (bounds prune subtrees fully above `hi`).
                for child in children {
                    let below_hi = child
                        .bound
                        .as_ref()
                        .is_none_or(|b| hi.is_none_or(|h| b.as_slice() <= h));
                    if !below_hi {
                        continue;
                    }
                    self.collect_range(child.id, lo, hi, msgs, leaves);
                }
            }
            Node::Leaf { .. } => leaves.push(id),
        }
    }

    /// Flush every buffer to the leaves (checkpoint operation).
    ///
    /// Splits performed during draining create new reachable nodes (right
    /// siblings and new roots) *after* any single downward walk has
    /// captured its child lists — so a naive root-to-leaf walk can miss
    /// buffers that migrated into fresh siblings. This implementation
    /// therefore works in rounds: each round re-collects the *current*
    /// reachable internal-node set and flushes each node once. Messages
    /// move strictly downward, so the rounds converge (2-3 in practice).
    pub fn flush_all(&mut self) {
        for _round in 0..128 {
            let internals = self.collect_internal_ids();
            for id in internals {
                if !self.is_leaf(id) {
                    self.flush_node(id);
                }
            }
            if self.pending == 0 {
                return;
            }
        }
        debug_assert_eq!(self.pending, 0, "flush_all did not converge");
    }

    /// BFS over the *current* reachable set, collecting internal node ids.
    fn collect_internal_ids(&self) -> Vec<NodeId> {
        let mut out = Vec::new();
        let mut stack = vec![self.root];
        while let Some(id) = stack.pop() {
            if self.is_leaf(id) {
                continue;
            }
            out.push(id);
            if let Node::Internal { children, .. } = self.node(id) {
                for c in children {
                    stack.push(c.id);
                }
            }
        }
        out
    }

    /// Height of the tree (root = 1).
    pub fn height(&self) -> usize {
        let mut h = 1;
        let mut cur = self.root;
        while let Node::Internal { children, .. } = self.node(cur) {
            cur = children[0].id;
            h += 1;
        }
        h
    }

    /// Test-only: root node id.
    pub fn root_id_for_debug(&self) -> usize { self.root }

    /// Test-only: children ids of a node.
    pub fn children_for_debug(&self, id: usize) -> Option<Vec<usize>> {
        match self.node(id) {
            Node::Internal { children, .. } => Some(children.iter().map(|c| c.id).collect()),
            _ => None,
        }
    }

    /// Iterator over all nodes (test/telemetry use).
    pub fn nodes_iter(&self) -> impl Iterator<Item = &Node<V>> {
        self.nodes.iter()
    }

    /// Structural stats for tuning/telemetry.
    pub fn stats(&self) -> TreeStats {
        let mut stats = TreeStats::default();
        for node in &self.nodes {
            match node {
                Node::Internal { buffer, children, .. } => {
                    stats.internal_nodes += 1;
                    stats.buffered_messages += buffer.len();
                    stats.children_sum += children.len();
                }
                Node::Leaf { entries, .. } => {
                    stats.leaves += 1;
                    stats.leaf_entries += entries.len();
                }
            }
        }
        stats
    }

    // -- flushing ------------------------------------------------------------

    /// Move every buffered message of `id` exactly one level down.
    ///
    /// `id` itself is allowed to transiently exceed its child budget while
    /// its buffer drains (split barrier); the post-loop `repair_chain`
    /// restores the invariant. Recursive flushes of children set their own
    /// barrier, so cascades of *their* splits may add children to `id` but
    /// never split `id` mid-drain.
    fn flush_node(&mut self, id: NodeId) {
        if self.is_leaf(id) {
            return;
        }
        let msgs: Vec<Message<V>> = match self.node_mut(id) {
            Node::Internal { buffer, .. } => std::mem::take(buffer),
            _ => unreachable!(),
        };
        self.pending -= msgs.len() as u64;

        let prev_barrier = self.split_barrier.replace(id);
        for msg in msgs {
            let child = self.route(id, &msg.key);
            if self.is_leaf(child) {
                self.apply_to_leaf(child, msg);
                self.maybe_split_leaf(child);
            } else {
                let full = match self.node(child) {
                    Node::Internal { buffer, .. } =>
                        buffer.len() >= self.config.buffer_capacity,
                    _ => false,
                };
                if full {
                    // Drains the child's buffer (and repairs/splits the
                    // child at its end — which may add children to `id`,
                    // deferred by the barrier).
                    self.flush_node(child);
                }
                // Re-route: the child may have split during its flush, and
                // `id`'s children may have gained new entries.
                let target = self.route(id, &msg.key);
                if let Node::Internal { buffer, .. } = self.node_mut(target) {
                    buffer.push(msg);
                    self.pending += 1;
                }
            }
        }
        self.split_barrier = prev_barrier;
        // Restore the fanout invariant for `id` (and any right siblings
        // created by repeated splits).
        self.repair_chain(id);
    }

    fn apply_to_leaf(&mut self, leaf_id: NodeId, msg: Message<V>) {
        if let Node::Leaf { entries, .. } = self.node_mut(leaf_id) {
            match entries.binary_search_by(|(k, _)| k.as_slice().cmp(&msg.key)) {
                Ok(pos) => match msg.op {
                    Op::Upsert(v) => entries[pos].1 = v,
                    Op::Delete => {
                        entries.remove(pos);
                        self.live_entries -= 1;
                    }
                },
                Err(pos) => {
                    if let Op::Upsert(v) = msg.op {
                        entries.insert(pos, (msg.key.clone(), v));
                        self.live_entries += 1;
                    }
                    // Delete of a missing key: no-op.
                }
            }
        }
    }

    // -- splitting -----------------------------------------------------------

    fn maybe_split_leaf(&mut self, leaf_id: NodeId) {
        let needs = match self.node(leaf_id) {
            Node::Leaf { entries, .. } => entries.len() > self.config.leaf_max_entries,
            _ => false,
        };
        if !needs {
            return;
        }
        let mid = self.config.leaf_max_entries / 2 + 1;
        let right_entries = match self.node_mut(leaf_id) {
            Node::Leaf { entries, .. } => entries.split_off(mid),
            _ => unreachable!(),
        };
        let right_bound = right_entries
            .first()
            .map(|(k, _)| k.clone())
            .expect("split leaf with entries");
        let right_id = self.nodes.len();
        let parent = self.parent_of(leaf_id);
        self.nodes.push(Node::Leaf { parent, entries: right_entries });

        match parent {
            None => {
                // Leaf is the root: grow a new root above it.
                let old_root = self.root;
                self.nodes.push(Node::Internal {
                    parent: None,
                    children: vec![
                        Child { bound: None, id: old_root },
                        Child { bound: Some(right_bound), id: right_id },
                    ],
                    buffer: Vec::new(),
                });
                let new_root = self.nodes.len() - 1;
                self.set_parent(old_root, Some(new_root));
                self.set_parent(right_id, Some(new_root));
                self.root = new_root;
            }
            Some(p) => {
                self.insert_child(p, right_bound, right_id);
                let _ = self.maybe_split_from(p); // barrier-aware
            }
        }
    }

    /// Split an over-full internal node once. Returns the new right sibling
    /// if a split occurred. Respects the split barrier (deferred splits).
    fn maybe_split_from(&mut self, id: NodeId) -> Option<NodeId> {
        if self.split_barrier == Some(id) {
            return None; // deferred — owning flush_node will repair
        }
        let needs = match self.node(id) {
            Node::Internal { children, .. } => children.len() > self.config.max_children,
            _ => false,
        };
        if !needs {
            return None;
        }
        let mid = self.config.max_children / 2 + 1;
        let (right_children, right_msgs, left_msgs, bound) = match self.node_mut(id) {
            Node::Internal { children, buffer, .. } => {
                let right = children.split_off(mid);
                // The right half's low bound is its first child's bound
                // (always `Some` — never the original first child).
                let split_key = right[0].bound.clone();
                let mut left_msgs = Vec::new();
                let mut right_msgs = Vec::new();
                for msg in std::mem::take(buffer) {
                    let goes_right = split_key
                        .as_ref()
                        .is_none_or(|b| msg.key.as_slice() >= b.as_slice());
                    if goes_right {
                        right_msgs.push(msg);
                    } else {
                        left_msgs.push(msg);
                    }
                }

                (right, right_msgs, left_msgs, split_key)
            }
            _ => unreachable!(),
        };
        if let Node::Internal { buffer, .. } = self.node_mut(id) {
            *buffer = left_msgs;
        }
        let parent = self.parent_of(id);
        let right_id = self.nodes.len();
        let right_child_ids: Vec<NodeId> = right_children.iter().map(|c| c.id).collect();
        self.nodes.push(Node::Internal {
            parent,
            children: right_children,
            buffer: right_msgs,
        });
        for cid in right_child_ids {
            self.set_parent(cid, Some(right_id));
        }

        match parent {
            None => {
                // Splitting the root: grow a new root with two children.
                let old_root = self.root;
                let right_bound = bound;
                self.nodes.push(Node::Internal {
                    parent: None,
                    children: vec![
                        Child { bound: None, id: old_root },
                        Child { bound: right_bound, id: right_id },
                    ],
                    buffer: Vec::new(),
                });
                let new_root = self.nodes.len() - 1;
                self.set_parent(old_root, Some(new_root));
                self.set_parent(right_id, Some(new_root));
                self.root = new_root;
            }
            Some(p) => {
                self.insert_child(p, bound.unwrap_or_default(), right_id);
                let _ = self.maybe_split_from(p); // +1 cascade upward
            }
        }
        Some(right_id)
    }

    /// Split `id` and then its successively-created right siblings until the
    /// child budget holds. Used by `flush_node` after its barrier lifts.
    fn repair_chain(&mut self, mut id: NodeId) {
        while let Some(right) = self.maybe_split_from(id) {
            id = right;
        }
    }

    fn set_parent(&mut self, id: NodeId, parent: Option<NodeId>) {
        match self.node_mut(id) {
            Node::Internal { parent: p, .. } => *p = parent,
            Node::Leaf { parent: p, .. } => *p = parent,
        }
    }

    fn insert_child(&mut self, parent_id: NodeId, bound: Vec<u8>, child_id: NodeId) {
        if let Node::Internal { children, .. } = self.node_mut(parent_id) {
            let pos = children
                .binary_search_by(|c| match &c.bound {
                    None => std::cmp::Ordering::Less,
                    Some(b) => b.as_slice().cmp(&bound),
                })
                .unwrap_or_else(|p| p);
            children.insert(pos, Child { bound: Some(bound), id: child_id });
        }
        self.set_parent(child_id, Some(parent_id));
    }
}

fn in_range(key: &[u8], lo: Option<&[u8]>, hi: Option<&[u8]>) -> bool {
    lo.is_none_or(|l| key >= l) && hi.is_none_or(|h| key <= h)
}

/// Structural telemetry.
#[derive(Clone, Copy, Debug, Default, Serialize, Deserialize)]
pub struct TreeStats {
    pub internal_nodes: usize,
    pub leaves: usize,
    pub buffered_messages: usize,
    pub children_sum: usize,
    pub leaf_entries: usize,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tiny_config() -> TreeConfig {
        TreeConfig { max_children: 3, buffer_capacity: 4, leaf_max_entries: 4 }
    }

    #[test]
    fn basic_insert_get_delete() {
        let mut t = BeTree::<u64>::new(tiny_config());
        t.insert(b"alpha", 1);
        t.insert(b"beta", 2);
        t.insert(b"gamma", 3);
        assert_eq!(t.get(b"alpha"), Some(1));
        assert_eq!(t.get(b"beta"), Some(2));
        assert_eq!(t.get(b"gamma"), Some(3));
        assert_eq!(t.get(b"delta"), None);

        // Upsert overwrites.
        t.insert(b"beta", 22);
        assert_eq!(t.get(b"beta"), Some(22));

        // Delete tombstones.
        t.delete(b"beta");
        assert_eq!(t.get(b"beta"), None);
        t.flush_all();
        assert_eq!(t.get(b"beta"), None);
        assert_eq!(t.live_entries(), 2);
    }

    #[test]
    fn buffered_writes_are_immediately_visible() {
        // The core Bε-tree property: a message in the root buffer (never
        // flushed) must still be returned by get(). First grow the tree so
        // the root is internal and messages land in its buffer.
        let mut t = BeTree::<u64>::new(TreeConfig {
            max_children: 4,
            buffer_capacity: 1000, // never flushes in this test
            leaf_max_entries: 8,
        });
        for i in 0..9u64 {
            t.insert(format!("warmup{}", i).as_bytes(), i);
        }
        assert!(t.height() >= 2, "root must be internal for this test");
        t.insert(b"k1", 10);
        t.insert(b"k2", 20);
        t.insert(b"k1", 11);
        t.delete(b"k2");
        assert_eq!(t.get(b"k1"), Some(11));
        assert_eq!(t.get(b"k2"), None);
        assert_eq!(t.get(b"warmup3"), Some(3));
        assert_eq!(t.pending_messages(), 4);
    }

    #[test]
    fn stress_random_ops_match_hashmap() {
        // Model-based test: Bε-tree must agree with a BTreeMap under a
        // random upsert/delete workload, both before and after flushes.
        let mut t = BeTree::<u64>::new(tiny_config());
        let mut model: std::collections::BTreeMap<Vec<u8>, u64> = Default::default();
        let mut s: u64 = 0x9E3779B97F4A7C15;
        let mut next = move || {
            s ^= s << 13;
            s ^= s >> 7;
            s ^= s << 17;
            s
        };

        for i in 0..20_000u64 {
            let key = format!("key{:04}", next() % 600).into_bytes();
            let choice = next() % 10;
            if choice < 7 {
                let val = next() % 1_000_000;
                t.insert(&key, val);
                model.insert(key, val);
            } else {
                t.delete(&key);
                model.remove(&key);
            }
            if i % 977 == 0 {
                for _ in 0..50 {
                    let probe = format!("key{:04}", next() % 600).into_bytes();
                    assert_eq!(
                        t.get(&probe),
                        model.get(&probe).copied(),
                        "probe during buffered phase"
                    );
                }
            }
        }

        t.flush_all();
        for (k, v) in &model {
            assert_eq!(t.get(k), Some(*v), "key {k:?}");
        }
        assert_eq!(t.live_entries(), model.len() as u64);

        let scanned = t.scan(None, None);
        let expected: Vec<(Vec<u8>, u64)> = model.into_iter().collect();
        assert_eq!(scanned, expected);
    }

    #[test]
    fn debug_sequential_full_check() {
        let mut t = BeTree::<u64>::new(tiny_config());
        for i in 0..200u64 {
            t.insert(format!("k{:03}", i).as_bytes(), i);
            for j in 0..=i {
                assert_eq!(
                    t.get(format!("k{:03}", j).as_bytes()),
                    Some(j),
                    "lost k{:03} right after inserting k{:03}",
                    j,
                    i
                );
            }
        }
    }

    #[test]
    fn scan_range_respects_bounds() {
        let mut t = BeTree::<u64>::new(tiny_config());
        for i in 0..200u64 {
            t.insert(format!("k{:04}", i).as_bytes(), i);
        }
        t.flush_all();
        let scanned = t.scan(Some(b"k0050"), Some(b"k0059"));
        assert_eq!(scanned.len(), 10);
        assert_eq!(scanned[0].0, b"k0050".to_vec());
        assert_eq!(scanned[9].0, b"k0059".to_vec());
    }

    #[test]
    fn tree_grows_in_height_under_load() {
        let mut t = BeTree::<u64>::new(tiny_config());
        assert_eq!(t.height(), 1);
        for i in 0..5_000u64 {
            t.insert(format!("key{:05}", i).as_bytes(), i);
        }
        t.flush_all();
        assert!(t.height() >= 3, "height = {}", t.height());
        let stats = t.stats();
        assert!(stats.leaf_entries >= 5_000);
        assert!(stats.internal_nodes >= 2);
    }

    #[test]
    fn fanout_invariant_holds_after_storm() {
        // After every operation the fanout budget must hold for all nodes
        // (the split barrier is an internal, transient state only).
        let mut t = BeTree::<u64>::new(tiny_config());
        let mut s: u64 = 7;
        let mut next = move || {
            s ^= s << 13;
            s ^= s >> 7;
            s ^= s << 17;
            s
        };
        for _ in 0..10_000 {
            let key = format!("k{:05}", next() % 1500).into_bytes();
            if next() % 4 == 0 {
                t.delete(&key);
            } else {
                t.insert(&key, next());
            }
            check_invariants(&t);
        }
    }

    #[test]
    fn children_count_agrees_with_public_iterator() {
        // children_count() must agree with the public node iterator for
        // every node — guards the telemetry path against representation
        // drift when the split logic changes.
        let mut t = BeTree::<u64>::new(tiny_config());
        for i in 0..500u64 {
            t.insert(format!("k{:04}", i).as_bytes(), i);
        }
        t.flush_all();
        let counts: Vec<usize> = t
            .nodes_iter()
            .map(|n| match n {
                Node::Internal { children, .. } => children.len(),
                Node::Leaf { .. } => 0,
            })
            .collect();
        for (id, expected) in counts.iter().enumerate() {
            assert_eq!(t.children_count(id), *expected, "node {id}");
        }
        // Once grown, the root must be internal with a real fanout.
        // (The root is *not* node 0 — splits push new roots at the end.)
        assert!(
            t.children_count(t.root) >= 2,
            "root fanout = {}",
            t.children_count(t.root)
        );
    }

    fn check_invariants(t: &BeTree<u64>) {
        let cfg = t.config();
        for node in t.nodes_iter() {
            match node {
                Node::Internal { children, buffer, .. } => {
                    assert!(
                        children.len() <= cfg.max_children,
                        "internal node has {} children (max {})",
                        children.len(),
                        cfg.max_children
                    );
                    assert!(buffer.len() <= cfg.buffer_capacity);
                    // Children sorted by bound.
                    let bounds: Vec<&Option<Vec<u8>>> = children.iter().map(|c| &c.bound).collect();
                    for w in bounds.windows(2) {
                        let a = w[0].clone().unwrap_or_default();
                        let b = w[1].clone().unwrap_or_default();
                        assert!(w[0].is_none() || a < b, "children not sorted");
                    }
                }
                Node::Leaf { entries, .. } => {
                    assert!(
                        entries.len() <= cfg.leaf_max_entries,
                        "leaf has {} entries (max {})",
                        entries.len(),
                        cfg.leaf_max_entries
                    );
                    for w in entries.windows(2) {
                        assert!(w[0].0 < w[1].0, "leaf entries not sorted");
                    }
                }
            }
        }
    }

    #[test]
    fn serialization_roundtrip() {
        let mut t = BeTree::<Vec<u8>>::new(tiny_config());
        for i in 0..500u64 {
            t.insert(format!("k{:03}", i).as_bytes(), vec![i as u8]);
        }
        let bytes = bincode::serialize(&t).unwrap();
        let t2: BeTree<Vec<u8>> = bincode::deserialize(&bytes).unwrap();
        for i in 0..500u64 {
            assert_eq!(t2.get(format!("k{:03}", i).as_bytes()), Some(vec![i as u8]));
        }
    }

    #[test]
    fn shape_tuning_from_rw_ratio() {
        let read_heavy = tree_shape_for_ratio(1000, 10);
        let write_heavy = tree_shape_for_ratio(10, 1000);
        // Read-heavy → larger fanout (more children), smaller buffers.
        // (Direction per the insert bound: small ε = write-optimized.)
        assert!(read_heavy.max_children > write_heavy.max_children);
        assert!(read_heavy.buffer_capacity < write_heavy.buffer_capacity);
    }
}






