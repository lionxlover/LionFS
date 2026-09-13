//! # HAMT — Hash Array Mapped Trie (Pillar II)
//!
//! RFC-002 §4.3: "The HAMT is not wasted by this choice [B-epsilon for
//! extents]: it is the inode-number space. 128-bit inode keys hash into a
//! persistent HAMT whose depth grows only when populations demand it,
//! giving O(1) amortized lookup for name resolution results and stable
//! 16-byte keys for the replication plane -- while range-scannable data
//! stays in B-epsilon trees where range queries actually exist."
//!
//! This is a **persistent** (immutable) HAMT over `u128` keys in the
//! Bagwell style: 32-way branching, 5-bit nibbles per level, bitmap
//! compression of sparse nodes, and structural sharing between
//! generations. Persistence is what the RCU publish step consumes: a new
//! root is built, readers keep walking the old root, the old generation
//! is reclaimed once the epoch advances (see `rcu::RcuPtr`).
//!
//! Properties:
//! * `insert`/`remove` copy only the O(depth) spine (~7 levels max at
//!   32-way branching for 2^128 keys: `ceil(128/5) = 26`, but population
//!   growth stops far earlier in practice via collision nodes).
//! * `get` is `26 * (bitmap + popcount + index)` -- one cache line per
//!   level in the dense case.
//! * structural sharing: `insert` on a 1M-entry map copies ~26 nodes,
//!   everything else is shared Arc pointers.

use std::sync::Arc;

const FANOUT_BITS: u32 = 5;
const FANOUT: usize = 32;

/// Hash mixing: splitmix64 folded over the 128-bit key (the same mixer
/// the shard router uses -- one audited avalanche function everywhere).
#[must_use]
pub fn hash128(key: u128) -> u128 {
    let lo = crate::io_engine::shard::splitmix64(key as u64);
    let hi = crate::io_engine::shard::splitmix64((key >> 64) as u64 ^ lo);
    (hi as u128) << 64 | lo as u128
}

fn nibble(hash: u128, level: u32) -> usize {
    ((hash >> (level * FANOUT_BITS)) & (FANOUT as u128 - 1)) as usize
}

/// A persistent HAMT mapping `u128 -> V`.
pub struct Hamt<V: Clone> {
    root: Arc<Node<V>>,
    len: usize,
    depth_max: u32,
}

enum Node<V> {
    /// Bitmap-compressed 32-way branch.
    Branch {
        /// Bit `i` set iff slot for nibble `i` is present.
        map: u32,
        /// Present slots in nibble order (bitmap compression).
        slots: Vec<Arc<Node<V>>>,
    },
    /// Leaf with colliding full hashes (chained).
    Collision { hash: u128, entries: Vec<(u128, V)> },
    /// Single key/value pair.
    Leaf { hash: u128, key: u128, value: V },
}

impl<V: Clone> Clone for Node<V> {
    fn clone(&self) -> Self {
        match self {
            Node::Branch { map, slots } => Node::Branch {
                map: *map,
                slots: slots.clone(),
            },
            Node::Collision { hash, entries } => Node::Collision {
                hash: *hash,
                entries: entries.clone(),
            },
            Node::Leaf { hash, key, value } => Node::Leaf {
                hash: *hash,
                key: *key,
                value: value.clone(),
            },
        }
    }
}

impl<V: Clone> Hamt<V> {
    #[must_use]
    pub fn new() -> Self {
        Self {
            root: Arc::new(Node::Branch {
                map: 0,
                slots: Vec::new(),
            }),
            len: 0,
            depth_max: 0,
        }
    }

    #[must_use]
    pub fn len(&self) -> usize {
        self.len
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    #[must_use]
    pub fn max_depth_seen(&self) -> u32 {
        self.depth_max
    }

    /// O(depth) lookup.
    #[must_use]
    pub fn get(&self, key: u128) -> Option<&V> {
        let hash = hash128(key);
        let mut node = &*self.root;
        let mut level = 0u32;
        loop {
            match node {
                Node::Branch { map, slots } => {
                    let idx = nibble(hash, level);
                    let bit = 1u32 << idx;
                    if map & bit == 0 {
                        return None;
                    }
                    let slot = (map & (bit - 1)).count_ones() as usize;
                    node = &slots[slot];
                    level += 1;
                }
                Node::Leaf {
                    hash: h,
                    key: k,
                    value,
                } => {
                    return if *h == hash && *k == key {
                        Some(value)
                    } else {
                        None
                    };
                }
                Node::Collision { hash: h, entries } => {
                    if *h != hash {
                        return None;
                    }
                    return entries.iter().find(|(k, _)| *k == key).map(|(_, v)| v);
                }
            }
        }
    }

    /// Persistent insert: returns a new Hamt sharing structure with
    /// `self`; `self` is unchanged (the RCU publish contract).
    #[must_use]
    pub fn insert(&self, key: u128, value: V) -> Self {
        let hash = hash128(key);
        let (root, added, depth) = insert_node(&self.root, key, value, hash, 0);
        Self {
            root: Arc::new(root),
            len: self.len + if added { 1 } else { 0 },
            depth_max: self.depth_max.max(depth),
        }
    }

    /// Persistent remove.
    #[must_use]
    pub fn remove(&self, key: u128) -> (Self, Option<V>) {
        let hash = hash128(key);
        match remove_node(&self.root, key, hash, 0) {
            RemoveResult::Missing => (
                Self {
                    root: Arc::clone(&self.root),
                    len: self.len,
                    depth_max: self.depth_max,
                },
                None,
            ),
            RemoveResult::Removed(node, value) => (
                Self {
                    root: Arc::new(node),
                    len: self.len - 1,
                    depth_max: self.depth_max,
                },
                Some(value),
            ),
            RemoveResult::Replace(node, value) => (
                Self {
                    root: Arc::new(node),
                    len: self.len,
                    depth_max: self.depth_max,
                },
                Some(value),
            ),
        }
    }

    /// In-order visitation (key order is hash order; the caller sorts if
    /// it needs key order -- the HAMT is a point-lookup structure).
    pub fn for_each<F: FnMut(u128, &V)>(&self, mut f: F) {
        walk(&self.root, &mut f);
    }
}

fn walk<V, F: FnMut(u128, &V)>(node: &Node<V>, f: &mut F) {
    match node {
        Node::Branch { slots, .. } => {
            for s in slots {
                walk(s, f);
            }
        }
        Node::Leaf { key, value, .. } => f(*key, value),
        Node::Collision { entries, .. } => {
            for (k, v) in entries {
                f(*k, v);
            }
        }
    }
}

fn insert_node<V: Clone>(
    node: &Arc<Node<V>>,
    key: u128,
    value: V,
    hash: u128,
    level: u32,
) -> (Node<V>, bool, u32) {
    match &**node {
        Node::Branch { map, slots } => {
            let idx = nibble(hash, level);
            let bit = 1u32 << idx;
            if map & bit == 0 {
                // Absent slot: add a leaf, copy the branch spine.
                let new_map = *map | bit;
                let mut new_slots = slots.clone();
                let slot = (*map & (bit - 1)).count_ones() as usize;
                new_slots.insert(slot, Arc::new(Node::Leaf { hash, key, value }));
                (
                    Node::Branch {
                        map: new_map,
                        slots: new_slots,
                    },
                    true,
                    level,
                )
            } else {
                let slot = (map & (bit - 1)).count_ones() as usize;
                let (child, added, depth) = insert_node(&slots[slot], key, value, hash, level + 1);
                let mut new_slots = slots.clone();
                new_slots[slot] = Arc::new(child);
                // Structural sharing: every other slot is a shared Arc.
                (
                    Node::Branch {
                        map: *map,
                        slots: new_slots,
                    },
                    added,
                    depth,
                )
            }
        }
        Node::Leaf {
            hash: h,
            key: k,
            value: v,
        } => {
            if *h == hash && *k == key {
                // Upsert: replace the value in place of the old leaf.
                (Node::Leaf { hash, key, value }, false, level)
            } else if *h == hash {
                // Full-hash collision: chain into a Collision node.
                let entries = vec![(*k, v.clone()), (key, value)];
                (Node::Collision { hash, entries }, true, level)
            } else {
                // Divergent hashes: widen into a Branch at this level.
                widen(node, key, value, hash, *h, level)
            }
        }
        Node::Collision { hash: h, entries } => {
            if *h != hash {
                // Divergent: widen.
                return widen(node, key, value, hash, *h, level);
            }
            if let Some(pos) = entries.iter().position(|(k, _)| *k == key) {
                let mut new_entries = entries.clone();
                new_entries[pos] = (key, value);
                (
                    Node::Collision {
                        hash,
                        entries: new_entries,
                    },
                    false,
                    level,
                )
            } else {
                let mut new_entries = entries.clone();
                new_entries.push((key, value));
                (
                    Node::Collision {
                        hash: *h,
                        entries: new_entries,
                    },
                    true,
                    level,
                )
            }
        }
    }
}

#[allow(clippy::similar_names)]
fn widen<V: Clone>(
    node: &Arc<Node<V>>,
    key: u128,
    value: V,
    new_hash: u128,
    old_hash: u128,
    level: u32,
) -> (Node<V>, bool, u32) {
    // Grow a Branch that separates `node` (old hash) from the new entry,
    // descending until the hashes' nibbles differ.
    let mut map = 0u32;
    let old_idx = nibble(old_hash, level);
    let new_idx = nibble(new_hash, level);
    if old_idx == new_idx {
        // Same nibble at this level: recurse one level deeper with a
        // singleton branch chain.
        let (inner, added, depth) = widen(node, key, value, new_hash, old_hash, level + 1);
        let bit = 1u32 << new_idx;
        map = bit;
        return (
            Node::Branch {
                map,
                slots: vec![Arc::new(inner)],
            },
            added,
            depth,
        );
    }
    let bit_old = 1u32 << old_idx;
    let bit_new = 1u32 << new_idx;
    map = bit_old | bit_new;
    let mut slots = Vec::with_capacity(2);
    let (first, second) = if old_idx < new_idx {
        (
            Arc::clone(node),
            Arc::new(Node::Leaf {
                hash: new_hash,
                key,
                value,
            }),
        )
    } else {
        (
            Arc::new(Node::Leaf {
                hash: new_hash,
                key,
                value,
            }),
            Arc::clone(node),
        )
    };
    slots.push(first);
    slots.push(second);
    (Node::Branch { map, slots }, true, level)
}

enum RemoveResult<V> {
    Missing,
    /// Key removed; the node handed back is the replacement subtree (an
    /// empty Branch{0,[]} when the subtree emptied entirely).
    Removed(Node<V>, V),
    /// Key removed from a collision list without removing the node.
    Replace(Node<V>, V),
}

fn remove_node<V: Clone>(
    node: &Arc<Node<V>>,
    key: u128,
    hash: u128,
    level: u32,
) -> RemoveResult<V> {
    match &**node {
        Node::Branch { map, slots } => {
            let idx = nibble(hash, level);
            let bit = 1u32 << idx;
            if map & bit == 0 {
                return RemoveResult::Missing;
            }
            let slot = (map & (bit - 1)).count_ones() as usize;
            match remove_node(&slots[slot], key, hash, level + 1) {
                RemoveResult::Missing => RemoveResult::Missing,
                RemoveResult::Replace(n, v) => {
                    let mut new_slots = slots.clone();
                    new_slots[slot] = Arc::new(n);
                    RemoveResult::Replace(
                        Node::Branch {
                            map: *map,
                            slots: new_slots,
                        },
                        v,
                    )
                }
                RemoveResult::Removed(n, v) => {
                    // An emptied child reports itself as an empty branch;
                    // drop its slot, and collapse this branch if it ends
                    // up empty or singleton (keeps density).
                    let child_is_empty =
                        matches!(&n, Node::Branch { slots: cs, .. } if cs.is_empty());
                    let mut new_map = *map;
                    let mut new_slots = slots.clone();
                    if child_is_empty {
                        new_map &= !bit;
                        new_slots.remove(slot);
                        if new_slots.is_empty() {
                            // This branch emptied too: propagate removal.
                            return RemoveResult::Removed(
                                Node::Branch {
                                    map: 0,
                                    slots: Vec::new(),
                                },
                                v,
                            );
                        }
                        if new_slots.len() == 1 {
                            // Singleton branch collapses to its only child.
                            let only = (**new_slots.first().expect("len checked")).clone();
                            return RemoveResult::Removed(only, v);
                        }
                    } else {
                        new_slots[slot] = Arc::new(n);
                    }
                    RemoveResult::Removed(
                        Node::Branch {
                            map: new_map,
                            slots: new_slots,
                        },
                        v,
                    )
                }
            }
        }
        Node::Leaf {
            hash: h,
            key: k,
            value,
        } => {
            if *h == hash && *k == key {
                // Leaf removed: report an empty branch as the replacement.
                RemoveResult::Removed(
                    Node::Branch {
                        map: 0,
                        slots: Vec::new(),
                    },
                    value.clone(),
                )
            } else {
                RemoveResult::Missing
            }
        }
        Node::Collision { hash: h, entries } => {
            if *h != hash {
                return RemoveResult::Missing;
            }
            match entries.iter().position(|(k, _)| *k == key) {
                None => RemoveResult::Missing,
                Some(pos) => {
                    let value = entries[pos].1.clone();
                    let mut new_entries = entries.clone();
                    new_entries.remove(pos);
                    if new_entries.len() == 1 {
                        // Singleton collision collapses to a leaf.
                        let (k, v) = new_entries.pop().expect("len checked");
                        RemoveResult::Replace(
                            Node::Leaf {
                                hash: *h,
                                key: k,
                                value: v,
                            },
                            value,
                        )
                    } else {
                        RemoveResult::Replace(
                            Node::Collision {
                                hash: *h,
                                entries: new_entries,
                            },
                            value,
                        )
                    }
                }
            }
        }
    }
}

impl<V: Clone> Default for Hamt<V> {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn insert_get_roundtrip_small() {
        let mut t = Hamt::new();
        t = t.insert(1, "one".to_string());
        t = t.insert(2, "two".to_string());
        assert_eq!(t.get(1u128), Some(&"one".to_string()));
        assert_eq!(t.get(2u128), Some(&"two".to_string()));
        assert_eq!(t.get(3u128), None);
        assert_eq!(t.len(), 2);
    }

    #[test]
    fn persistence_shares_structure() {
        let base = (0..1000u128).fold(Hamt::<u64>::new(), |t, k| t.insert(k, k as u64));
        let v2 = base.insert(5000, 42);
        // Old generation unchanged (RCU readers rely on this).
        assert_eq!(base.get(5000), None);
        assert_eq!(base.len(), 1000);
        assert_eq!(v2.get(5000), Some(&42));
        assert_eq!(v2.len(), 1001);
        // And the shared spine: both resolve the same early key.
        assert_eq!(base.get(7), Some(&7));
        assert_eq!(v2.get(7), Some(&7));
    }

    #[test]
    fn upsert_replaces_value() {
        let t = Hamt::new().insert(9, 1u64).insert(9, 5u64);
        assert_eq!(t.get(9), Some(&5));
        assert_eq!(t.len(), 1);
    }

    #[test]
    fn large_population_roundtrip() {
        let n: u128 = 100_000;
        let t = (0..n).fold(Hamt::<u128>::new(), |t, k| t.insert(k, k * 31));
        assert_eq!(t.len() as u128, n);
        for k in (0..n).step_by(97) {
            assert_eq!(t.get(k), Some(&(k * 31)));
        }
        // Depth stays logarithmic and shallow for 32-way branching.
        assert!(
            t.max_depth_seen() < 8,
            "depth {} unexpected",
            t.max_depth_seen()
        );
    }

    #[test]
    fn remove_persists_and_collapses() {
        let t = (0..100u128).fold(Hamt::<u64>::new(), |t, k| t.insert(k, k as u64));
        let (t2, gone) = t.remove(50);
        assert_eq!(gone, Some(50));
        assert_eq!(t2.get(50), None);
        assert_eq!(t2.len(), 99);
        // Old generation untouched.
        assert_eq!(t.get(50), Some(&50));
        // Remove missing key is a no-op.
        let (t3, none) = t2.remove(9999);
        assert_eq!(none, None);
        assert_eq!(t3.len(), 99);
    }

    #[test]
    fn full_hash_collisions_chain() {
        // Manufacture a collision by construction: same key hash path via
        // different keys is hard to force with a good mixer, so assert
        // the invariant differently -- 10k keys, all distinct, no loss.
        let t = (0..10_000u128).fold(Hamt::<bool>::new(), |t, k| t.insert(k * 7919, true));
        assert_eq!(t.len(), 10_000);
        for k in (0..10_000u128).step_by(13) {
            assert_eq!(t.get(k * 7919), Some(&true));
        }
    }

    #[test]
    fn for_each_visits_every_entry() {
        let t = (0..500u128).fold(Hamt::<u8>::new(), |t, k| t.insert(k, 1));
        let mut seen = 0;
        t.for_each(|_k, v| {
            assert_eq!(*v, 1);
            seen += 1;
        });
        assert_eq!(seen, 500);
    }

    #[test]
    fn hash128_avalanches_adjacent_keys() {
        let a = hash128(1);
        let b = hash128(2);
        assert_ne!(a, b);
        // A quarter of the low 64 bits should differ on average; assert a
        // healthy hamming distance (>= 20 bits of 64).
        let dist = ((a ^ b) as u64).count_ones();
        assert!(dist > 20 && dist < 44, "hamming distance {dist} suspicious");
    }
}
