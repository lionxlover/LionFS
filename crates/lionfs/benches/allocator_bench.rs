use criterion::{criterion_group, criterion_main, Criterion};

fn bench_allocator(c: &mut Criterion) {
    c.bench_function("allocator_cache_hit", |b| {
        // Mocking cache logic speed
        b.iter(|| {
            let _val = (1..50).fold(0, |acc, x| acc + x);
        })
    });
}

criterion_group!(benches, bench_allocator);
criterion_main!(benches);
