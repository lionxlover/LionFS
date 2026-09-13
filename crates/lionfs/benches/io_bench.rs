use criterion::{criterion_group, criterion_main, Criterion};

fn bench_parallel_io(c: &mut Criterion) {
    c.bench_function("parallel_io_dispatch", |b| {
        // Mocking parallel dispatch overhead
        b.iter(|| {
            let _val = (1..200).fold(0, |acc, x| acc + x);
        })
    });
}

criterion_group!(benches, bench_parallel_io);
criterion_main!(benches);
