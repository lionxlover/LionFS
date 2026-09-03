//! A real, correct hash function for directory entry names, computed and
//! ready to use, but not yet wired into `directory::entries::DirManager`
//! as an actual index -- doing that (à la ext4's htree) needs an on-disk
//! index structure mapping hash -> entry-record offset, which is a bigger
//! structural change to the directory format than adding the hash
//! function itself. What's here is the correct, tested building block;
//! `DirManager` still does a linear scan today.

/// FNV-1a, 64-bit. Chosen for being simple, fast, and dependency-free
/// (LionFS has no `hashbrown`/`fnv`/`ahash` crate dependency to reuse) --
/// not cryptographically secure, which is fine for a directory index
/// (the threat model there is "spread names out evenly," not "resist a
/// deliberate collision attack").
pub fn hash_name(name: &str) -> u64 {
    const FNV_OFFSET_BASIS: u64 = 0xcbf29ce484222325;
    const FNV_PRIME: u64 = 0x100000001b3;

    let mut hash = FNV_OFFSET_BASIS;
    for byte in name.as_bytes() {
        hash ^= *byte as u64;
        hash = hash.wrapping_mul(FNV_PRIME);
    }
    hash
}

/// Maps a name's hash into one of `bucket_count` buckets -- what a future
/// hashed directory index would use to decide which index block to
/// consult first.
pub fn bucket_for(name: &str, bucket_count: u32) -> u32 {
    if bucket_count == 0 {
        return 0;
    }
    (hash_name(name) % bucket_count as u64) as u32
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn same_name_hashes_identically() {
        assert_eq!(hash_name("readme.txt"), hash_name("readme.txt"));
    }

    #[test]
    fn different_names_hash_differently() {
        assert_ne!(hash_name("a"), hash_name("b"));
    }

    #[test]
    fn distributes_reasonably_across_buckets() {
        // Not a rigorous distribution test, just a sanity check that a
        // modest set of similar names doesn't all collide into one bucket.
        let names: Vec<String> = (0..64).map(|i| format!("file_{i}.txt")).collect();
        let buckets: std::collections::HashSet<u32> =
            names.iter().map(|n| bucket_for(n, 16)).collect();
        assert!(
            buckets.len() > 1,
            "expected names to spread across multiple buckets"
        );
    }

    #[test]
    fn zero_buckets_does_not_panic() {
        assert_eq!(bucket_for("anything", 0), 0);
    }
}
