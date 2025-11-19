// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Performance tests.

#![expect(missing_docs)]

criterion::criterion_main!(benches);

criterion::criterion_group!(benches, bench_access);

fn bench_access(c: &mut criterion::Criterion) {
    c.bench_function("rcu-read", |b| {
        b.iter(|| {
            minircu::global().run(|| {});
        });
    });
}
