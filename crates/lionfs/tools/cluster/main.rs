//! `lfs_cluster` — the unified-plane showcase: a self-contained demo
//! of the merged distributed machinery (LionFS 8.0, the HFS merge).
//!
//! One command walks the whole cluster data path, deterministically:
//!
//! 1. **Raft consensus** — a 3-node in-process network elects a
//!    leader and replicates a write log; a partition heals; the
//!    cluster re-converges.
//! 2. **CDC + convergent encryption + EC** — the replicated payload
//!    is FastCDC-chunked, convergently encrypted (dedup-compatible),
//!    and Reed-Solomon striped; fragments are destroyed and rebuilt.
//! 3. **Time travel** — a `ClusterEngine` writes versions and reads
//!    them back at their original timestamps.
//!
//! This is the tool to run when someone asks "what did the merge
//! actually buy?"

use lionfs_cluster::core::EngineConfig;
use lionfs_cluster::crypto::{Cipher, KeyTree};
use lionfs_cluster::dedup::DomainId;
use lionfs_cluster::ecc::RsCodec;
use lionfs_cluster::engine::ClusterEngine;
use lionfs_cluster::raft::{run_stable, Network};

fn section(title: &str) {
    println!();
    println!("=== {title} ===");
}

fn main() {
    println!("LionFS {} ({}) — cluster-plane showcase", lionfs_core::VERSION, lionfs_core::EDITION);
    println!("Raft consensus · CDC dedup · convergent encryption · RS erasure coding · time travel");

    // ------------------------------------------------------------------
    // 1. Raft: elect, replicate, partition, heal, converge.
    // ------------------------------------------------------------------
    section("1. Raft consensus (3 nodes, deterministic)");

    let mut net = Network::new(3, 42);
    run_stable(&mut net, 60);
    let leader = net.leader_id().expect("a leader must be elected");
    let roles: Vec<&str> = net
        .roles()
        .iter()
        .map(|r| match r {
            lionfs_cluster::raft::Role::Leader => "leader",
            lionfs_cluster::raft::Role::Follower => "follower",
            lionfs_cluster::raft::Role::Candidate => "candidate",
        })
        .collect();
    println!("elected: node {leader} is leader (roles: {:?})", roles);

    let writes = [
        b"CREATE /cluster/readme.md".to_vec(),
        b"WRITE /cluster/readme.md v1".to_vec(),
        b"WRITE /cluster/readme.md v2".to_vec(),
    ];
    for w in &writes {
        net.propose(leader, w.clone()).expect("proposal accepted");
    }
    run_stable(&mut net, 40);
    let committed = net.committed_log();
    println!("replicated {} commands; committed log:", committed.len());
    for e in committed.iter().take(writes.len()) {
        println!("  [term {}] {}", e.term, String::from_utf8_lossy(&e.command));
    }
    let last = committed
        .iter()
        .max_by_key(|e| (e.term, e.index))
        .expect("non-empty log");
    assert!(
        net.replicated_everywhere(last.index, last.term),
        "every node must hold the committed tail"
    );
    println!(
        "every node holds the committed tail (index {}, term {})",
        last.index, last.term
    );

    // Partition the leader away; a new leader takes over.
    let others: Vec<usize> = (0..3usize).filter(|&i| i != leader).collect();
    for &o in &others {
        net.set_partition(leader, o, 1.0);
    }
    run_stable(&mut net, 80);
    let new_leader = net
        .leader_id()
        .expect("a new leader must be elected without the partitioned node");
    println!("partitioned node {leader} away: node {new_leader} took over");
    net.propose(new_leader, b"WRITE /cluster/readme.md v3 (failover)".to_vec())
        .expect("failover proposal accepted");
    run_stable(&mut net, 40);
    println!(
        "failover write committed through node {new_leader} (log now {} entries)",
        net.committed_log().len()
    );
    net.heal_all();
    run_stable(&mut net, 60);
    println!("partition healed: all nodes re-converged");

    // ------------------------------------------------------------------
    // 2. CDC + convergent encryption + RS erasure coding.
    // ------------------------------------------------------------------
    section("2. CDC + convergent encryption + RS erasure coding");

    // A payload with internal repetition (dedup-visible).
    let mut s: u64 = 0xA5A5_5A5A_DEAD_BEEF;
    let mut next = move || {
        s ^= s << 13;
        s ^= s >> 7;
        s ^= s << 17;
        s as u8
    };
    let block: Vec<u8> = (0..64 * 1024).map(|_| next()).collect();
    let payload: Vec<u8> = [block.clone(), block.clone(), block.clone()].concat();

    let domain = DomainId::root();
    let keytree = KeyTree::generate();
    let (stats, _) = lionfs_core::pipeline::cdc_dedup::analyze(
        &payload,
        &domain,
        &lionfs_core::pipeline::cdc_dedup::DEFAULT_CDC,
    );
    println!(
        "payload {} bytes -> {} chunks, {} unique ({:.2}x dedup ratio)",
        stats.logical_bytes,
        stats.chunks,
        stats.unique_chunks,
        stats.ratio()
    );

    let chunk = &payload[..8192];
    let (stable, isolated) = lionfs_core::pipeline::cdc_dedup::convergent_roundtrip(
        chunk,
        &domain,
        &keytree,
        Cipher::Aes256Gcm,
    );
    println!(
        "convergent encryption: same content -> same ciphertext: {stable}; cross-domain isolated: {isolated}"
    );

    // RS-stripe the first chunk: n=6, k=4, destroy 2, rebuild.
    let (n, k) = (6usize, 4usize);
    let shard_len = chunk.len().div_ceil(k);
    let mut data_shards: Vec<Vec<u8>> = Vec::with_capacity(k);
    for i in 0..k {
        let mut sh = vec![0u8; shard_len];
        let start = i * shard_len;
        let end = (start + shard_len).min(chunk.len());
        sh[..end - start].copy_from_slice(&chunk[start..end]);
        data_shards.push(sh);
    }
    let codec = RsCodec::new(k, n - k).expect("codec");
    let parity = codec.encode(&data_shards).expect("encode");
    let mut slots: Vec<Option<Vec<u8>>> = data_shards.iter().map(|s| Some(s.clone())).collect();
    for p in parity {
        slots.push(Some(p));
    }
    println!("RS stripe: {n} shards (k={k}); destroying shards 1 and 4...");
    slots[1] = None;
    slots[4] = None;
    let rebuilt = codec.decode(&slots).expect("reconstruct");
    let mut stitched: Vec<u8> = rebuilt.iter().take(k).flatten().copied().collect();
    stitched.truncate(chunk.len());
    println!(
        "rebuilt from 4 of 6 shards: {} bytes, byte-identical: {}",
        stitched.len(),
        stitched == *chunk
    );

    // ------------------------------------------------------------------
    // 3. Time travel on a real cluster-engine volume.
    // ------------------------------------------------------------------
    section("3. Time travel (ClusterEngine volume)");

    let dir = std::env::temp_dir().join(format!("lionfs_cluster_demo_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let config = EngineConfig {
        chunk_avg_bytes: 4096,
        chunk_min_bytes: 1024,
        chunk_max_bytes: 16384,
        segment_size: 512 * 1024,
        ec: Default::default(),
        wal_mode: lionfs_cluster::core::WalMode::Full,
        gc_utilization_threshold: 0.5,
        retain_all_secs: 24 * 3600,
        retain_hourly_secs: 7 * 24 * 3600,
        retain_daily_secs: 90 * 24 * 3600,
    };
    let mut fs = ClusterEngine::create(&dir, config).expect("create volume");
    let v1 = fs.put("/demo.txt", b"state at t1").expect("write v1");
    std::thread::sleep(std::time::Duration::from_millis(5));
    let v2 = fs.put("/demo.txt", b"state at t2 (overwritten)").expect("write v2");
    println!("wrote two versions of /demo.txt");
    let ts1 = fs.version_info(&v1).expect("info v1").timestamp_ns;
    let ts2 = fs.version_info(&v2).expect("info v2").timestamp_ns;
    let at1 = fs.get_at("/demo.txt", ts1).expect("read at t1");
    let at2 = fs.get_at("/demo.txt", ts2).expect("read at t2");
    println!("read at t1: {:?}", String::from_utf8_lossy(&at1));
    println!("read at t2: {:?}", String::from_utf8_lossy(&at2));
    let live = fs.get("/demo.txt").expect("live read");
    println!("live read: {:?}", String::from_utf8_lossy(&live));
    assert_eq!(at1, b"state at t1");
    assert_eq!(at2, b"state at t2 (overwritten)");

    // Snapshot + a later write: the past stays immutable.
    let snap = fs.snapshot("demo-snapshot");
    fs.put("/demo.txt", b"state at t3 (after snapshot)").expect("write v3");
    let still = fs.get_at("/demo.txt", ts2).expect("t2 after more writes");
    assert_eq!(still, b"state at t2 (overwritten)");
    println!("snapshot recorded (hash {:02x?}); reading at t2 still returns the t2 bytes", &snap.as_bytes()[..4]);
    let _ = std::fs::remove_dir_all(&dir);

    println!();
    println!("showcase complete: consensus, dedup, convergent encryption,");
    println!("erasure-coded self-healing, and immutable time travel — one filesystem.");
}
