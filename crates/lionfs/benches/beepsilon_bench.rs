//! B-epsilon tree benchmarks: the write-optimization the RFC's P3 exit
//! criteria measure (leaf appends vs. the 1.x B-tree's node rewrites).

use criterion::{black_box, criterion_group, criterion_main, Criterion, Throughput};
use lionfs_core::addressing::{Extent16, ExtentFlags};
use lionfs_core::beepsilon::{coalesce_run, BEpsilonConfig, BEpsilonTree};

fn make_extent(logical: u64) -> Extent16 {
    Extent16::encode(logical, 1_000_000 + logical, 1, ExtentFlags::empty())
        .expect("benchmark extents are in range")
}

fn bench_inserts(c: &mut Criterion) {
    let mut group = c.benchmark_group("beepsilon_insert");
    for n in [1_000u64, 10_000, 100_000] {
        group.throughput(Throughput::Elements(n));
        group.bench_function(format!("seq_{n}"), |b| {
            b.iter(|| {
                let mut t = BEpsilonTree::new(BEpsilonConfig::default());
                for i in 0..n {
                    t.insert(i, make_extent(i), 16);
                }
                black_box(&t);
            })
        });
        // Random-order inserts (the fragmentation stress pattern).
        group.bench_function(format!("shuffled_{n}"), |b| {
            b.iter(|| {
                let mut keys: Vec<u64> = (0..n).collect();
                // xorshift shuffle, deterministic.
                let mut s = 0x9E3779B97F4A7C15u64;
                for i in (1..keys.len()).rev() {
                    s ^= s << 13;
                    s ^= s >> 7;
                    s ^= s << 17;
                    let j = (s % ((i + 1) as u64)) as usize;
                    keys.swap(i, j);
                }
                let mut t = BEpsilonTree::new(BEpsilonConfig::default());
                for k in keys {
                    t.insert(k, make_extent(k), 16);
                }
                black_box(&t);
            })
        });
    }
    group.finish();
}

fn bench_lookups(c: &mut Criterion) {
    let mut group = c.benchmark_group("beepsilon_lookup");
    for n in [10_000u64, 100_000] {
        group.throughput(Throughput::Elements(n));
        let mut t = BEpsilonTree::new(BEpsilonConfig::default());
        for i in 0..n {
            t.insert(i, make_extent(i), 16);
        }
        group.bench_function(format!("hit_{n}"), |b| {
            b.iter(|| {
                let mut found = 0u64;
                for i in 0..n {
                    if t.get(black_box(&i)).is_some() {
                        found += 1;
                    }
                }
                black_box(found);
            })
        });
    }
    group.finish();
}

fn bench_coalesce(c: &mut Criterion) {
    let mut group = c.benchmark_group("beepsilon_coalesce");
    for n in [1_000usize, 10_000] {
        group.throughput(Throughput::Elements(n as u64));
        group.bench_function(format!("adjacent_run_{n}"), |b| {
            b.iter(|| {
                // n sequential 1-block extents.
                let run: Vec<(u64, Extent16)> =
                    (0..n as u64).map(|i| (i, make_extent(i))).collect();
                let out = coalesce_run(run, |a, b| {
                    if a.1.coalescable_with(b.1) {
                        Extent16::encode(
                            a.1.logical_start(),
                            a.1.physical_start(),
                            a.1.length_blocks() + b.1.length_blocks(),
                            ExtentFlags::empty(),
                        )
                    } else {
                        None
                    }
                });
                black_box(out.len())
            })
        });
    }
    group.finish();
}

criterion_group!(benches, bench_inserts, bench_lookups, bench_coalesce);
criterion_main!(benches);
