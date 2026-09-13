//! FastCDC chunking benchmarks: cut throughput and the local-shift
//! property cost (the dedup pipeline's front-end budget).

use criterion::{black_box, criterion_group, criterion_main, Criterion, Throughput};
use lionfs_core::pipeline::dedup::chunk_hash;
use lionfs_core::pipeline::fastcdc::fastcdc;

/// Deterministic pseudorandom corpus (the dedup-hostile case).
fn corpus(len: usize, seed: u64) -> Vec<u8> {
    let mut v = Vec::with_capacity(len);
    let mut s = seed;
    for _ in 0..len {
        s = lionfs_core::io_engine::shard::splitmix64(s);
        v.push((s >> 56) as u8);
    }
    v
}

fn bench_cut(c: &mut Criterion) {
    let mut group = c.benchmark_group("fastcdc_cut");
    for len in [1 << 20, 1 << 22] {
        let data = corpus(len, 7);
        group.throughput(Throughput::Bytes(len as u64));
        group.bench_function(format!("random_{}MiB", len >> 20), |b| {
            b.iter(|| {
                let chunks = fastcdc(black_box(&data));
                black_box(chunks.len())
            })
        });
    }
    // Compressible corpus: long runs of repeated bytes (cut points hit
    // the hard-minimum path).
    let rep = vec![0x42u8; 1 << 22];
    group.throughput(Throughput::Bytes(rep.len() as u64));
    group.bench_function("repetitive_4MiB", |b| {
        b.iter(|| {
            let chunks = fastcdc(black_box(&rep));
            black_box(chunks.len())
        })
    });
    group.finish();
}

fn bench_hash(c: &mut Criterion) {
    let mut group = c.benchmark_group("chunk_hash");
    let data = corpus(8 * 1024, 11); // One avg-sized chunk.
    group.throughput(Throughput::Bytes(data.len() as u64));
    group.bench_function("blake3_128_8k", |b| {
        b.iter(|| {
            let h = chunk_hash(black_box(&data));
            black_box(h)
        })
    });
    group.finish();
}

criterion_group!(benches, bench_cut, bench_hash);
criterion_main!(benches);
