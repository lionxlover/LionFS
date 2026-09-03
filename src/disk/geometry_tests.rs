//! Phase 2 regression tests: geometry validation on open, recommended
//! chunk sizing, and the parity-write alignment counters.

use crate::disk::block_io::Disk;
use crate::pool::raid::{RaidEngine, RaidProfile};

#[test]
fn open_probes_and_reports_geometry() {
    let path = format!("/tmp/lfs_geom_open_{}.img", std::process::id());
    let _ = std::fs::remove_file(&path);
    let disk = Disk::create(&path, 1024 * 1024).unwrap();
    // Regular files report 512-byte sectors and their length.
    let geo = disk.geometry(0).unwrap();
    assert_eq!(geo.logical_sector_size, 512);
    assert_eq!(geo.size_bytes, 1024 * 1024);
    // 512 divides 4096, so sectors_per_block succeeds.
    assert_eq!(
        crate::disk::sectors::sectors_per_block(geo.logical_sector_size),
        Some(8)
    );
    let _ = std::fs::remove_file(&path);
}

#[test]
fn recommended_chunk_is_128k_and_sector_aligned() {
    // Default at 4 KiB blocks: 32 blocks = 128 KiB.
    assert_eq!(RaidEngine::recommended_chunk_size_blocks(0), 32);
    assert_eq!(RaidEngine::recommended_chunk_size_blocks(512), 32);
    // 4 Kn devices: 128 KiB is a whole number of 4 KiB sectors too.
    assert_eq!(RaidEngine::recommended_chunk_size_blocks(4096), 32);
    // A hypothetical 96 KiB-multiple sector size shrinks the chunk but
    // stays a multiple of blocks and sector-aligned in bytes.
    let weird = RaidEngine::recommended_chunk_size_blocks(12 * 1024);
    let chunk_bytes = weird as u64 * 4096;
    assert_eq!(
        chunk_bytes % (12 * 1024),
        0,
        "chunk must stay sector-aligned"
    );
    assert!(weird > 0);
}

#[test]
fn parity_counters_measure_partial_chunk_writes() {
    crate::debug::stats::reset_parity_counters();
    // A 3-device RAID5 pool on scratch images.
    let pid = std::process::id();
    let paths: Vec<String> = (0..3)
        .map(|i| format!("/tmp/lfs_parity_{}_{}.img", pid, i))
        .collect();
    for p in &paths {
        let _ = std::fs::remove_file(p);
    }
    let disk = Disk::create_pool(&paths, 1024 * 1024 * 8, RaidProfile::Raid5, 8).unwrap();

    // Single-block live write: always partial-chunk (1 < 8 blocks),
    // but Phase 3's incremental path serves it -- no full-row reads.
    let data = vec![0xABu8; 4096];
    disk.write_block(100, &data).unwrap();

    let total = crate::debug::stats::PARITY_WRITES_TOTAL.load(std::sync::atomic::Ordering::Relaxed);
    let partial =
        crate::debug::stats::PARITY_WRITES_PARTIAL_CHUNK.load(std::sync::atomic::Ordering::Relaxed);
    let incremental =
        crate::debug::stats::PARITY_INCREMENTAL_UPDATES.load(std::sync::atomic::Ordering::Relaxed);
    let row_reads =
        crate::debug::stats::PARITY_ROW_READS.load(std::sync::atomic::Ordering::Relaxed);
    assert_eq!(total, 1, "one parity write issued");
    assert_eq!(partial, 1, "single-block write covers a partial chunk");
    assert_eq!(
        incremental, 1,
        "live partial-chunk write takes the incremental RMW path"
    );
    assert_eq!(row_reads, 0, "incremental path reads no full stripe row");

    // Read it back to prove the write landed (and parity is consistent).
    let mut buf = vec![0u8; 4096];
    disk.read_block(100, &mut buf).unwrap();
    assert_eq!(buf, data);

    // Overwrite the SAME block with different data: the incremental
    // delta path must keep parity correct across rewrites (this is the
    // equivalence the 200-round pool::raid tests prove mathematically;
    // this proves the Disk wiring does too, end to end).
    let data2 = vec![0xCDu8; 4096];
    disk.write_block(100, &data2).unwrap();
    let mut buf2 = vec![0u8; 4096];
    disk.read_block(100, &mut buf2).unwrap();
    assert_eq!(buf2, data2);

    // Recovery-path write: forces the full-row recompute (idempotent
    // under journal replay).
    crate::debug::stats::reset_parity_counters();
    let data3 = vec![0x5Au8; 4096];
    disk.write_block_recovery(100, &data3).unwrap();
    let row_reads =
        crate::debug::stats::PARITY_ROW_READS.load(std::sync::atomic::Ordering::Relaxed);
    let incremental =
        crate::debug::stats::PARITY_INCREMENTAL_UPDATES.load(std::sync::atomic::Ordering::Relaxed);
    assert_eq!(row_reads, 1, "recovery write uses the full-row recompute");
    assert_eq!(
        incremental, 0,
        "recovery write must NOT use the incremental path"
    );
    let mut buf3 = vec![0u8; 4096];
    disk.read_block(100, &mut buf3).unwrap();
    assert_eq!(buf3, data3);

    // And a parity-consistency spot check after the mixed paths: take
    // the pool's P device content for that row and verify it equals a
    // full recompute of the row (i.e. mixed incremental/full writes
    // leave consistent parity). Layout: 3-dev RAID5, chunk 8; block 100
    // -> stripe/row layout from the engine.
    let layout = disk.raid_engine.layout(100);
    let data_dev = layout.data_devs[0];
    let mut on_disk = vec![0u8; 4096];
    let mut others: Vec<Vec<u8>> = Vec::new();
    for &(dev, _col) in &layout.other_data {
        let mut b = vec![0u8; 4096];
        disk.read_block_direct(dev, layout.phys_block, &mut b)
            .unwrap();
        others.push(b);
    }
    disk.read_block_direct(data_dev, layout.phys_block, &mut on_disk)
        .unwrap();
    let mut expect_p = on_disk.clone();
    for o in &others {
        for (i, v) in o.iter().enumerate() {
            expect_p[i] ^= v;
        }
    }
    let mut actual_p = vec![0u8; 4096];
    disk.read_block_direct(layout.parity_devs[0], layout.phys_block, &mut actual_p)
        .unwrap();
    assert_eq!(
        actual_p, expect_p,
        "parity must equal a full recompute after mixed write paths"
    );

    crate::debug::stats::reset_parity_counters();
    for p in &paths {
        let _ = std::fs::remove_file(p);
    }
}
