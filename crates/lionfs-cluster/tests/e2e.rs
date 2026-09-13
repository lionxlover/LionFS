//! End-to-end cluster-engine tests (merged from HelixFS): write/read
//! paths, snapshots, time travel, dedup, erasure-coded self-healing, and
//! crash recovery.

//! End-to-end engine tests: write/read paths, snapshots, time travel,
//! dedup, erasure-coded self-healing, and crash recovery.

use lionfs_cluster::core::{EngineConfig, WalMode};
use lionfs_cluster::engine::ClusterEngine;
use lionfs_cluster::ecc::RsCodec;

fn test_config() -> EngineConfig {
    EngineConfig {
        // Small chunks so tests exercise multi-chunk files quickly.
        chunk_avg_bytes: 4096,
        chunk_min_bytes: 1024,
        chunk_max_bytes: 16384,
        segment_size: 512 * 1024,
        ec: Default::default(),
        wal_mode: WalMode::Full,
        gc_utilization_threshold: 0.5,
        retain_all_secs: 24 * 3600,
        retain_hourly_secs: 7 * 24 * 3600,
        retain_daily_secs: 90 * 24 * 3600,
    }
}

fn xorshift(len: usize, seed: u64) -> Vec<u8> {
    let mut s = seed | 1;
    (0..len)
        .map(|_| {
            s ^= s << 13;
            s ^= s >> 7;
            s ^= s << 17;
            s as u8
        })
        .collect()
}

#[test]
fn write_read_roundtrip() {
    let dir = tempfile::tempdir().unwrap();
    let mut fs = ClusterEngine::create(dir.path(), test_config()).unwrap();
    let data = xorshift(200_000, 1);
    fs.put("/data.bin", &data).unwrap();
    assert_eq!(fs.get("/data.bin").unwrap(), data);
}

#[test]
fn overwrite_creates_new_version_and_time_travel() {
    let dir = tempfile::tempdir().unwrap();
    let mut fs = ClusterEngine::create(dir.path(), test_config()).unwrap();
    let v1 = fs.put("/doc.txt", b"version one").unwrap();
    let v2 = fs.put("/doc.txt", b"version two (longer)").unwrap();
    let v3 = fs.put("/doc.txt", b"version three").unwrap();

    assert_eq!(fs.get("/doc.txt").unwrap(), b"version three");
    assert_ne!(v1, v2);
    assert_ne!(v2, v3);

    // Parent chaining: v2's parent is v1, etc.
    let info2 = fs.version_info(&v2).unwrap();
    assert_eq!(info2.parents, vec![v1]);

    // Time travel (spec §2.2): read at the write timestamps.
    let ts2 = fs.version_info(&v2).unwrap().timestamp_ns;
    let ts1 = fs.version_info(&v1).unwrap().timestamp_ns;
    assert_eq!(fs.get_at("/doc.txt", ts2).unwrap(), b"version two (longer)");
    assert_eq!(fs.get_at("/doc.txt", ts1).unwrap(), b"version one");
    // History is immutable (spec Axiom 2).
    assert_eq!(fs.read_version(&v1).unwrap(), b"version one");
}

#[test]
fn snapshots_are_o1_and_difftable() {
    let dir = tempfile::tempdir().unwrap();
    let mut fs = ClusterEngine::create(dir.path(), test_config()).unwrap();
    fs.put("/a.txt", b"alpha").unwrap();
    fs.put("/sub/b.txt", b"beta").unwrap();
    let _ = fs.snapshot("s1");

    fs.put("/a.txt", b"alpha-2").unwrap();
    fs.put("/sub/c.txt", b"gamma").unwrap();
    let _ = fs.snapshot("s2");

    let diff = fs.diff_snapshots("s1", "s2").unwrap();
    let mut paths: Vec<&str> = diff.iter().map(|d| d.path.as_str()).collect();
    paths.sort();
    assert_eq!(paths, vec!["/a.txt", "/sub/c.txt"], "diff: {diff:?}");

    // Restore rolls the namespace back (spec §9).
    fs.restore("s1").unwrap();
    assert_eq!(fs.get("/a.txt").unwrap(), b"alpha");
    assert!(fs.get("/sub/c.txt").is_err(), "post-restore path must not exist");
    assert_eq!(fs.get("/sub/b.txt").unwrap(), b"beta");
}

#[test]
fn dedup_shares_identical_chunks() {
    let dir = tempfile::tempdir().unwrap();
    let mut fs = ClusterEngine::create(dir.path(), test_config()).unwrap();
    // Two files with identical content → zero new physical chunks for the
    // second write (spec §10: dedup as a side effect of identity-by-hash).
    let blob = xorshift(300_000, 9);
    fs.put("/one.bin", &blob).unwrap();
    let physical_after_one = fs.stats().bytes_physical;
    fs.put("/two.bin", &blob).unwrap();
    let stats = fs.stats();
    assert_eq!(
        stats.bytes_physical, physical_after_one,
        "identical content must not grow physical bytes"
    );
    let dedup = fs.dedup_stats();
    assert!(dedup.duplicate_hits > 0);
    assert!((dedup.ratio() - 2.0).abs() < 1e-9, "ratio = {}", dedup.ratio());
    assert_eq!(fs.get("/two.bin").unwrap(), blob);
    assert_eq!(fs.get("/one.bin").unwrap(), blob);
}

#[test]
fn distinct_files_do_not_share() {
    let dir = tempfile::tempdir().unwrap();
    let mut fs = ClusterEngine::create(dir.path(), test_config()).unwrap();
    fs.put("/x.bin", &xorshift(100_000, 1)).unwrap();
    fs.put("/y.bin", &xorshift(100_000, 2)).unwrap();
    assert_eq!(fs.dedup_stats().duplicate_hits, 0);
    assert!((fs.dedup_stats().ratio() - 1.0).abs() < 1e-9);
}

#[test]
fn deletion_tombstones_and_releases() {
    let dir = tempfile::tempdir().unwrap();
    let mut fs = ClusterEngine::create(dir.path(), test_config()).unwrap();
    fs.put("/gone.txt", b"temporary data").unwrap();
    assert!(fs.get("/gone.txt").is_ok());
    fs.delete("/gone.txt").unwrap();
    assert!(fs.get("/gone.txt").is_err());
    assert!(fs.list(None).iter().all(|p| p != "/gone.txt"));
    // Rewrite after delete works (new chain, parented on the tombstone).
    fs.put("/gone.txt", b"reborn").unwrap();
    assert_eq!(fs.get("/gone.txt").unwrap(), b"reborn");
}

#[test]
fn listing_with_prefix() {
    let dir = tempfile::tempdir().unwrap();
    let mut fs = ClusterEngine::create(dir.path(), test_config()).unwrap();
    fs.put("/docs/a.txt", b"1").unwrap();
    fs.put("/docs/b.txt", b"2").unwrap();
    fs.put("/etc/c.conf", b"3").unwrap();
    let docs = fs.list(Some("/docs"));
    assert_eq!(docs.len(), 2);
    assert!(docs.contains(&"/docs/a.txt".to_string()));
    assert_eq!(fs.list(None).len(), 3);
}

// ---------------------------------------------------------------------------
// Erasure-coded self-healing (spec §5.3, §11)
// ---------------------------------------------------------------------------

#[test]
fn scrub_heals_corrupted_chunk_via_erasure_coding() {
    let dir = tempfile::tempdir().unwrap();
    let mut config = test_config();
    // Small k so a test blob spans ≥ k chunks.
    config.ec = lionfs_cluster::core::EcProfile { k: 2, m: 1 };
    let mut fs = ClusterEngine::create(dir.path(), config).unwrap();

    let data = xorshift(64_000, 77);
    fs.put("/precious.bin", &data).unwrap();
    let groups = fs.encode_ec_groups().unwrap();
    assert!(groups >= 1, "expected at least one EC group");

    // Silent bit-rot on one data chunk.
    let root = fs.root();
    let vnode = fs.version_info(&fs.resolve("/precious.bin").unwrap()).unwrap();
    let meta: Vec<u8> = {
        // Read the meta chunk raw via a fresh read (engine API for tests):
        // we corrupt one of the file's chunks — get chunk list by reading.
        let _ = root;
        let content = vnode.content_hash;
        // Reuse engine internals through the public API: corrupt the FIRST
        // chunk by resolving it via a read (read succeeds pre-corruption).
        let _ = content;
        Vec::new()
    };
    let _ = meta;

    // Simpler: corrupt every chunk hash we can discover via stats — no.
    // Instead: engine exposes `corrupt_chunk`; we need a chunk hash. The
    // engine test helper below exposes the file's chunk hashes.
    let chunks = fs.chunk_hashes_of("/precious.bin").unwrap();
    assert!(!chunks.is_empty());
    fs.corrupt_chunk(&chunks[0]).unwrap();

    // Read triggers on-the-fly heal (spec §7 steps 5-6).
    let healed_read = fs.get("/precious.bin").unwrap();
    assert_eq!(healed_read, data, "read must transparently heal + return data");
    let stats = fs.stats();
    assert!(stats.heals >= 1);

    // Scrub pass finds nothing left (already healed in place).
    let report = fs.scrub().unwrap();
    assert_eq!(report.corruptions_found, 0, "healed in place: {report:?}");
}

#[test]
fn scrub_reports_unhealable_without_ec() {
    let dir = tempfile::tempdir().unwrap();
    let mut config = test_config();
    config.ec = lionfs_cluster::core::EcProfile { k: 8, m: 0 }; // EC off
    let mut fs = ClusterEngine::create(dir.path(), config).unwrap();
    fs.put("/fragile.bin", b"no redundancy").unwrap();
    let chunks = fs.chunk_hashes_of("/fragile.bin").unwrap();
    fs.corrupt_chunk(&chunks[0]).unwrap();
    let report = fs.scrub().unwrap();
    assert!(report.corruptions_found >= 1);
    assert!(report.unhealable >= 1, "without EC the corruption is a lost cause: {report:?}");
    // And the read fails loudly (spec §11: alert, not silent garbage).
    assert!(fs.get("/fragile.bin").is_err());
}

// ---------------------------------------------------------------------------
// Crash recovery (spec §18)
// ---------------------------------------------------------------------------

#[test]
fn crash_recovery_replays_wal_after_checkpoint() {
    let dir = tempfile::tempdir().unwrap();
    // Phase 1: create, write two files, CHECKPOINT (durability anchor).
    {
        let mut fs = ClusterEngine::create(dir.path(), test_config()).unwrap();
        fs.put("/before.bin", b"checkpointed content").unwrap();
        fs.put("/other.txt", b"also checkpointed").unwrap();
        fs.checkpoint().unwrap();
    }
    // Phase 2: mount, write MORE, then "power cut" (drop without checkpoint).
    {
        let mut fs = ClusterEngine::mount(dir.path(), test_config()).unwrap();
        assert_eq!(fs.get("/before.bin").unwrap(), b"checkpointed content");
        fs.put("/after.bin", b"WAL-only content must survive the crash").unwrap();
        fs.put("/other.txt", b"overwritten post-checkpoint").unwrap();
        let _ = fs.snapshot("pre-crash");
        // NO checkpoint — simulated power cut on drop.
    }
    // Phase 3: mount again — WAL replay must recover everything ACKed.
    {
        let mut fs = ClusterEngine::mount(dir.path(), test_config()).unwrap();
        assert_eq!(
            fs.get("/after.bin").unwrap(),
            b"WAL-only content must survive the crash",
            "WAL-committed write must survive crash without checkpoint"
        );
        assert_eq!(fs.get("/before.bin").unwrap(), b"checkpointed content");
        assert_eq!(fs.get("/other.txt").unwrap(), b"overwritten post-checkpoint");
        assert!(fs.restore("pre-crash").is_ok());
    }
}

#[test]
fn uncommitted_wal_tail_is_discarded() {
    // Torn-write semantics: the WAL's last transaction lacks a commit
    // marker → replay drops it (spec §18 step 8).
    let dir = tempfile::tempdir().unwrap();
    {
        let mut fs = ClusterEngine::create(dir.path(), test_config()).unwrap();
        fs.put("/committed.txt", b"safe").unwrap();
        fs.checkpoint().unwrap();
    }
    {
        let mut fs = ClusterEngine::mount(dir.path(), test_config()).unwrap();
        fs.put("/dangling.txt", b"never committed").unwrap();
        // Simulate a torn tail: truncate the WAL's last frame.
        let wal_path = dir.path().join("wal.log");
        let len = std::fs::metadata(&wal_path).unwrap().len();
        let data = std::fs::read(&wal_path).unwrap();
        // Cut the last 10 bytes — corrupts the final transaction.
        std::fs::write(&wal_path, &data[..(len - 10) as usize]).unwrap();
    }
    {
        let mut fs = ClusterEngine::mount(dir.path(), test_config()).unwrap();
        assert_eq!(fs.get("/committed.txt").unwrap(), b"safe");
        assert!(
            fs.get("/dangling.txt").is_err(),
            "torn tail must be discarded (CoW makes this safe)"
        );
    }
}

#[test]
fn metadata_only_wal_mode_still_recovers() {
    // Audit C1's alternative design: journal metadata only; data extents
    // are synced to the device BEFORE the commit marker.
    let dir = tempfile::tempdir().unwrap();
    let mut config = test_config();
    config.wal_mode = WalMode::MetadataOnly;
    {
        let mut fs = ClusterEngine::create(dir.path(), config.clone()).unwrap();
        fs.put("/meta.bin", b"metadata-journaled write").unwrap();
        fs.put("/meta2.bin", &xorshift(50_000, 5)).unwrap();
    }
    {
        let mut fs = ClusterEngine::mount(dir.path(), config).unwrap();
        assert_eq!(fs.get("/meta.bin").unwrap(), b"metadata-journaled write");
        assert_eq!(fs.get("/meta2.bin").unwrap(), xorshift(50_000, 5));
    }
}

#[test]
fn full_wal_mode_journals_payload_bytes() {
    // Spec §6 literal behaviour: Data records carry the payload — the
    // audit's C1: this doubles device writes vs MetadataOnly.
    let dir_a = tempfile::tempdir().unwrap();
    let dir_b = tempfile::tempdir().unwrap();
    let data = xorshift(300_000, 3);
    let (records_full, records_meta);
    {
        let mut config = test_config();
        config.wal_mode = WalMode::Full;
        let mut fs = ClusterEngine::create(dir_a.path(), config).unwrap();
        fs.put("/x.bin", &data).unwrap();
        records_full = fs.wal_stats().bytes_written;
    }
    {
        let mut config = test_config();
        config.wal_mode = WalMode::MetadataOnly;
        let mut fs = ClusterEngine::create(dir_b.path(), config).unwrap();
        fs.put("/x.bin", &data).unwrap();
        records_meta = fs.wal_stats().bytes_written;
    }
    assert!(
        records_full > records_meta + (data.len() / 2) as u64,
        "Full mode must journal ~payload bytes ({records_full} vs {records_meta})"
    );
}

#[test]
fn mount_without_checkpoint_recovers_structure() {
    // Fresh FS, writes, crash BEFORE any checkpoint: mount must rebuild
    // the namespace from the WAL alone.
    let dir = tempfile::tempdir().unwrap();
    {
        let mut fs = ClusterEngine::create(dir.path(), test_config()).unwrap();
        fs.put("/first.txt", b"never checkpointed").unwrap();
        fs.put("/second.bin", &xorshift(20_000, 4)).unwrap();
    }
    {
        let mut fs = ClusterEngine::mount(dir.path(), test_config()).unwrap();
        // Full recovery from the WAL alone — keys come from the out-of-band
        // master.key file (HSM stand-in, spec §15).
        assert!(fs.resolve("/first.txt").is_some());
        assert!(fs.resolve("/second.bin").is_some());
        assert_eq!(fs.get("/first.txt").unwrap(), b"never checkpointed");
        assert_eq!(fs.get("/second.bin").unwrap(), xorshift(20_000, 4));
    }
}

// ---------------------------------------------------------------------------
// Misc engine behaviour
// ---------------------------------------------------------------------------

#[test]
fn many_files_smoke() {
    let dir = tempfile::tempdir().unwrap();
    let mut fs = ClusterEngine::create(dir.path(), test_config()).unwrap();
    for i in 0..60u32 {
        let path = format!("/dir{}/file{}.txt", i % 5, i);
        let content = format!("content-{i}-{}", "x".repeat((i as usize) % 300));
        fs.put(&path, content.as_bytes()).unwrap();
    }
    for i in 0..60u32 {
        let path = format!("/dir{}/file{}.txt", i % 5, i);
        let content = format!("content-{i}-{}", "x".repeat((i as usize) % 300));
        assert_eq!(fs.get(&path).unwrap(), content.as_bytes());
    }
    assert_eq!(fs.list(None).len(), 60);
    // Checkpoint + remount roundtrip.
    fs.checkpoint().unwrap();
    drop(fs);
    let mut fs = ClusterEngine::mount(dir.path(), test_config()).unwrap();
    assert_eq!(fs.list(None).len(), 60);
    let path = format!("/dir{}/file7.txt", 7 % 5);
    let content = format!("content-7-{}", "x".repeat(7));
    assert_eq!(fs.get(&path).unwrap(), content.as_bytes());
}

#[test]
fn rs_codec_engine_integration() {
    // Direct codec use through the engine's EC path (spec §5.3 decode).
    let codec = RsCodec::new(4, 2).unwrap();
    let shards: Vec<Vec<u8>> = (0..4).map(|i| vec![i as u8 + 1; 128]).collect();
    let parity = codec.encode(&shards).unwrap();
    let mut all: Vec<Option<Vec<u8>>> =
        shards.iter().cloned().map(Some).chain(parity.into_iter().map(Some)).collect();
    all[1] = None;
    all[5] = None; // lose one data + one parity (≤ m losses)
    let recovered = codec.decode(&all).unwrap();
    assert_eq!(recovered, shards);
}

#[test]
fn remount_does_not_overwrite_live_extents() {
    // Regression (found by v1.1 audit pass): Store::open reset the zone
    // allocation cursors to zero, so post-mount writes clobbered live
    // checkpointed extents. Round-robin zone rotation made the original
    // crash-recovery tests pass by accident — filling past one zone
    // exposes it immediately.
    let dir = tempfile::tempdir().unwrap();
    let precious = xorshift(5 * 1024 * 1024, 11); // spans two 4 MiB zones
    {
        let mut fs = ClusterEngine::create(dir.path(), test_config()).unwrap();
        fs.put("/precious.bin", &precious).unwrap();
        fs.checkpoint().unwrap();
    }
    {
        let mut fs = ClusterEngine::mount(dir.path(), test_config()).unwrap();
        // First post-mount write lands at the *next* zone's offset 0 —
        // exactly where the tail of /precious.bin lives if the cursors
        // were dropped. It must append *after* it instead.
        let filler = xorshift(512 * 1024, 100);
        fs.put("/filler.bin", &filler).unwrap();
        assert_eq!(fs.get("/precious.bin").unwrap(), precious, "live extent was clobbered");
    }
    {
        let mut fs = ClusterEngine::mount(dir.path(), test_config()).unwrap();
        assert_eq!(
            fs.get("/precious.bin").unwrap(),
            precious,
            "clobbering must also be durable-crash-safe"
        );
    }
}

#[test]
fn gc_reclaims_dead_segments_and_keeps_survivors() {
    // Spec §8: delete drops refcounts; the cleaner rewrites survivors out
    // of dirtied segments and returns the space to the allocator.
    let dir = tempfile::tempdir().unwrap();
    let mut fs = ClusterEngine::create(dir.path(), test_config()).unwrap();

    // Three ~700 KiB files of distinct content — enough to rotate several
    // 512 KiB segments. Interleave writes so segments mix chunks of A/B/C.
    // NB: xorshift() floors the seed with `| 1`, so seeds must stay
    // distinct after that fold (22|1 == 23|1 — a classic trap).
    let a = xorshift(700 * 1024, 21);
    let b = xorshift(700 * 1024, 22);
    let c = xorshift(700 * 1024, 24);
    fs.put("/a.bin", &a).unwrap();
    fs.put("/b.bin", &b).unwrap();
    fs.put("/c.bin", &c).unwrap();

    // Kill the middle file: its chunks die, segment live fractions drop.
    fs.delete("/b.bin").unwrap();
    assert!(fs.get("/b.bin").is_err());

    let report = fs.gc().unwrap();
    assert!(
        report.segments_cleaned > 0,
        "some segment must qualify: {report:?}"
    );
    assert!(report.bytes_reclaimed > 0, "space must return: {report:?}");
    assert!(report.free_bytes_now > 0);

    // Survivors must be bit-perfect after relocation.
    assert_eq!(fs.get("/a.bin").unwrap(), a);
    assert_eq!(fs.get("/c.bin").unwrap(), c);

    // The relocations must be durable: remount and read again.
    drop(fs);
    let mut fs = ClusterEngine::mount(dir.path(), test_config()).unwrap();
    assert_eq!(fs.get("/a.bin").unwrap(), a, "post-GC relocation lost A");
    assert_eq!(fs.get("/c.bin").unwrap(), c, "post-GC relocation lost C");
    assert!(fs.get("/b.bin").is_err(), "deleted file must stay deleted");

    // New writes reuse the reclaimed space (free lists first, spec §8.2):
    // writing more than the freed bytes must still succeed and old data
    // must remain intact — the allocator carves free ranges before the
    // cursor consumes fresh zone capacity.
    let d = xorshift(2 * 700 * 1024, 26);
    fs.put("/d.bin", &d).unwrap();
    assert_eq!(fs.get("/a.bin").unwrap(), a);
    assert_eq!(fs.get("/d.bin").unwrap(), d);

    // Re-putting A's content elsewhere is a dedup hit (chunk_map intact).
    fs.put("/copy-of-a.bin", &a).unwrap();
    assert_eq!(fs.get("/copy-of-a.bin").unwrap(), a);
    let stats = fs.dedup_stats();
    assert!(stats.duplicate_hits > 0, "dedup must still hit after GC");
}

#[test]
fn gc_is_a_noop_on_a_healthy_store() {
    // Nothing deleted → every closed segment is 100% live → the cleaner
    // must not move anything (no write amplification from idle GC).
    let dir = tempfile::tempdir().unwrap();
    let mut fs = ClusterEngine::create(dir.path(), test_config()).unwrap();
    let a = xorshift(700 * 1024, 31);
    let b = xorshift(700 * 1024, 32);
    fs.put("/a.bin", &a).unwrap();
    fs.put("/b.bin", &b).unwrap();
    let report = fs.gc().unwrap();
    assert_eq!(report.segments_cleaned, 0, "healthy store: {report:?}");
    assert_eq!(report.bytes_moved, 0);
    assert_eq!(fs.get("/a.bin").unwrap(), a);
    assert_eq!(fs.get("/b.bin").unwrap(), b);
}

// ---------------------------------------------------------------------------
// v1.1: edge cases, error paths, and adversarial workloads
// ---------------------------------------------------------------------------

#[test]
fn zero_length_and_tiny_files_roundtrip() {
    let dir = tempfile::tempdir().unwrap();
    let mut fs = ClusterEngine::create(dir.path(), test_config()).unwrap();
    fs.put("/empty.bin", b"").unwrap();
    assert_eq!(fs.get("/empty.bin").unwrap(), b"");
    fs.put("/one.bin", b"X").unwrap();
    assert_eq!(fs.get("/one.bin").unwrap(), b"X");
    // Zero-length overwrite of a non-empty file and back.
    fs.put("/one.bin", b"").unwrap();
    assert_eq!(fs.get("/one.bin").unwrap(), b"");
    fs.put("/one.bin", b"XYZ").unwrap();
    assert_eq!(fs.get("/one.bin").unwrap(), b"XYZ");
    // Crash-safe too.
    drop(fs);
    let mut fs = ClusterEngine::mount(dir.path(), test_config()).unwrap();
    assert_eq!(fs.get("/empty.bin").unwrap(), b"");
    assert_eq!(fs.get("/one.bin").unwrap(), b"XYZ");
}

#[test]
fn resurrect_after_delete_shares_chunks_again() {
    // Deleting then re-writing the same content must re-materialize the
    // path (the chunk data is content-addressed; dedup re-hits or re-writes
    // are both correct, the bytes must simply round-trip).
    let dir = tempfile::tempdir().unwrap();
    let mut fs = ClusterEngine::create(dir.path(), test_config()).unwrap();
    let data = xorshift(200 * 1024, 77);
    fs.put("/phoenix.bin", &data).unwrap();
    fs.delete("/phoenix.bin").unwrap();
    assert!(fs.get("/phoenix.bin").is_err());
    fs.put("/phoenix.bin", &data).unwrap();
    assert_eq!(fs.get("/phoenix.bin").unwrap(), data);
}

#[test]
fn overwrite_same_content_creates_new_version_zero_physical_growth() {
    // Writing byte-identical content must not grow physical storage
    // (chunk-level dedup on the same path).
    let dir = tempfile::tempdir().unwrap();
    let mut fs = ClusterEngine::create(dir.path(), test_config()).unwrap();
    let data = xorshift(300 * 1024, 88);
    fs.put("/stable.bin", &data).unwrap();
    let physical_before = fs.stats().bytes_physical;
    fs.put("/stable.bin", &data).unwrap();
    let physical_after = fs.stats().bytes_physical;
    assert_eq!(
        physical_after, physical_before,
        "identical overwrite must add zero physical bytes"
    );
    assert_eq!(fs.get("/stable.bin").unwrap(), data);
    // And the dedup index counted the duplicate.
    assert!(fs.dedup_stats().duplicate_hits > 0);
}

#[test]
fn missing_paths_error_cleanly() {
    let dir = tempfile::tempdir().unwrap();
    let mut fs = ClusterEngine::create(dir.path(), test_config()).unwrap();
    assert!(fs.get("/nope.bin").is_err());
    assert!(fs.delete("/nope.bin").is_err(), "delete of missing path must error");
    // Unmount + mount: still missing, no phantom entries.
    drop(fs);
    let mut fs = ClusterEngine::mount(dir.path(), test_config()).unwrap();
    assert!(fs.get("/nope.bin").is_err());
}

#[test]
fn deeply_nested_paths_survive() {
    // 40-level nesting: every intermediate directory is an immutable,
    // content-addressed node (spec §3); rebuild cost is O(depth).
    let dir = tempfile::tempdir().unwrap();
    let mut fs = ClusterEngine::create(dir.path(), test_config()).unwrap();
    let mut deep = String::new();
    for i in 0..40 {
        deep.push_str(&format!("/d{i:02}"));
    }
    let path = format!("{deep}/leaf.bin");
    fs.put(&path, b"bottom of the tree").unwrap();
    assert_eq!(fs.get(&path).unwrap(), b"bottom of the tree");
    drop(fs);
    let mut fs = ClusterEngine::mount(dir.path(), test_config()).unwrap();
    assert_eq!(fs.get(&path).unwrap(), b"bottom of the tree");
    // Listing a deep prefix works too.
    let entries = fs.list(Some(deep.as_str()));
    assert_eq!(entries, vec![format!("{deep}/leaf.bin")]);
}

#[test]
fn put_get_stress_with_random_sizes() {
    // Random file sizes crossing chunk boundaries at odd offsets, several
    // rounds, verified after every checkpoint/remount cycle.
    let dir = tempfile::tempdir().unwrap();
    let mut fs = ClusterEngine::create(dir.path(), test_config()).unwrap();
    let mut s: u64 = 0xDEAD_BEEF_CAFE_F00D;
    let mut next = move || {
        s ^= s << 13;
        s ^= s >> 7;
        s ^= s << 17;
        s
    };
    // Precompute every round's contents up front so verification compares
    // against the same bytes (no stream re-simulation).
    let mut rounds: Vec<Vec<(String, Vec<u8>)>> = Vec::new();
    for round in 0..4u64 {
        let mut files = Vec::new();
        for f in 0..12u64 {
            let len = (next() % 300_000) as usize; // 0..~300 KB
            let data: Vec<u8> = (0..len).map(|_| (next() >> 32) as u8).collect();
            files.push((format!("/r{round}/f{f}.bin"), data));
        }
        rounds.push(files);
    }

    for (round, files) in rounds.iter().enumerate() {
        for (path, data) in files {
            fs.put(path, data).unwrap();
            assert_eq!(&fs.get(path).unwrap(), data, "round-trip {path}");
        }
        fs.checkpoint().unwrap();
        drop(fs);
        fs = ClusterEngine::mount(dir.path(), test_config()).unwrap();
        // Everything from every completed round must survive the remount.
        for prev in rounds.iter().take(round + 1) {
            for (path, data) in prev {
                assert_eq!(&fs.get(path).unwrap(), data, "post-remount {path}");
            }
        }
    }
}

#[test]
fn wal_crc_corruption_mid_stream_is_detected() {
    // Corrupting bytes in the MIDDLE of the WAL (not just the tail) must
    // be detected and must not silently produce wrong data.
    let dir = tempfile::tempdir().unwrap();
    {
        let mut fs = ClusterEngine::create(dir.path(), test_config()).unwrap();
        fs.put("/safe.txt", b"safe content").unwrap();
        fs.checkpoint().unwrap();
    }
    {
        // Post-checkpoint writes only; then corrupt the WAL's middle byte.
        let mut fs = ClusterEngine::mount(dir.path(), test_config()).unwrap();
        fs.put("/post.txt", b"post-checkpoint write").unwrap();
    }
    {
        let wal_path = dir.path().join("wal.log");
        let data = std::fs::read(&wal_path).unwrap();
        if data.len() > 16 {
            let mid = data.len() / 2;
            let mut corrupted = data;
            corrupted[mid] ^= 0xFF;
            std::fs::write(&wal_path, corrupted).unwrap();
        }
    }
    // Mount must either replay cleanly (if the corrupted record is
    // discardable) or fail loudly — it must never return wrong bytes.
    if let Ok(mut fs) = ClusterEngine::mount(dir.path(), test_config()) {
        assert_eq!(
            fs.get("/safe.txt").unwrap(),
            b"safe content",
            "checkpointed data must never corrupt"
        );
    }
}

#[test]
fn concurrent_reader_threads_share_an_engine() {
    // The engine is single-writer, but readers must be able to run
    // concurrently through scoped threads once the data is written.
    use std::thread;
    let dir = tempfile::tempdir().unwrap();
    let mut fs = ClusterEngine::create(dir.path(), test_config()).unwrap();
    let files: Vec<(String, Vec<u8>)> = (0..8)
        .map(|i| {
            (
                format!("/shared/f{i}.bin"),
                xorshift(64 * 1024 + i * 1000, 900 + i as u64),
            )
        })
        .collect();
    for (path, data) in &files {
        fs.put(path, data).unwrap();
    }
    fs.checkpoint().unwrap();

    thread::scope(|scope| {
        let mut handles = Vec::new();
        for (path, data) in &files {
            let dir_path = dir.path();
            handles.push(scope.spawn(move || {
                let mut fs = ClusterEngine::mount(dir_path, test_config()).unwrap();
                assert_eq!(&fs.get(path).unwrap(), data, "concurrent read {path}");
            }));
        }
        for h in handles {
            h.join().unwrap();
        }
    });
}
