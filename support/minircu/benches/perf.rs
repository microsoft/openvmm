// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Performance tests.

#![expect(missing_docs)]

criterion::criterion_main!(benches);

criterion::criterion_group!(benches, bench_access);

#[inline(never)]
fn result_body() -> Result<[u64; 4], u64> {
    inline_result_body()
}

#[inline(always)]
fn inline_result_body() -> Result<[u64; 4], u64> {
    let value = std::hint::black_box(1u64);
    if value & 1 != 0 {
        Ok([value, value + 1, value + 2, value + 3])
    } else {
        Err(value)
    }
}

#[inline(never)]
fn scalar_body() -> u64 {
    std::hint::black_box(1u64).wrapping_add(1)
}

fn bench_access(c: &mut criterion::Criterion) {
    c.bench_function("rcu-read", |b| {
        b.iter(|| {
            minircu::global().run(|| {});
        });
    });
    c.bench_function("rcu-read-opaque-domain", |b| {
        let domain = std::hint::black_box(minircu::global());
        b.iter(|| domain.run(|| {}));
    });
    c.bench_function("rcu-read-nested", |b| {
        b.iter(|| minircu::global().run(|| minircu::global().run(|| {})));
    });
    c.bench_function("rcu-read-after-quiesce", |b| {
        b.iter(|| {
            minircu::global().quiesce();
            minircu::global().run(|| {});
        });
    });
    c.bench_function("call-result", |b| {
        b.iter(result_body);
    });
    c.bench_function("rcu-read-result", |b| {
        b.iter(|| minircu::global().run(result_body));
    });
    c.bench_function("rcu-read-result-opaque-domain", |b| {
        let domain = std::hint::black_box(minircu::global());
        b.iter(|| domain.run(result_body));
    });
    c.bench_function("call-scalar", |b| {
        b.iter(scalar_body);
    });
    c.bench_function("rcu-read-scalar", |b| {
        b.iter(|| minircu::global().run(scalar_body));
    });
    c.bench_function("call-result-opaque-callback", |b| {
        let callback = std::hint::black_box(result_body as fn() -> Result<[u64; 4], u64>);
        b.iter(callback);
    });
    c.bench_function("rcu-read-result-opaque-callback", |b| {
        let callback = std::hint::black_box(result_body as fn() -> Result<[u64; 4], u64>);
        b.iter(|| minircu::global().run(callback));
    });
    c.bench_function("rcu-read-result-inline", |b| {
        b.iter(|| minircu::global().run(inline_result_body));
    });
    c.bench_function("rcu-read-nested-after-quiesce", |b| {
        b.iter(|| {
            minircu::global().quiesce();
            minircu::global().run(|| minircu::global().run(|| {}));
        });
    });
}
