use criterion::{criterion_group, criterion_main, Criterion};

fn bench_btree_insert(c: &mut Criterion) {
    c.bench_function("btree_insert_mock", |b| {
        // Mocked because we need a full initialized disk for real btree
        b.iter(|| {
            let _val = (1..100).fold(0, |acc, x| acc + x);
        })
    });
}

criterion_group!(benches, bench_btree_insert);
criterion_main!(benches);
