// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Performance tests.

// UNSAFETY: testing unsafe interfaces
#![expect(unsafe_code)]

use sparse_mmap::initialize_try_copy;
use std::hint::black_box;

criterion::criterion_main!(benches);

criterion::criterion_group!(benches, bench_access);

fn bench_access(c: &mut criterion::Criterion) {
    initialize_try_copy();
    c.bench_function("try-read-8", |b| {
        // SAFETY: passing a valid src.
        b.iter(|| unsafe {
            let n = 0u8;
            sparse_mmap::try_read_volatile(&n).unwrap();
        });
    })
    .bench_function("read-8", |b| {
        // SAFETY: passing a valid src.
        b.iter(|| unsafe {
            let n = 0u8;
            std::ptr::read_volatile(black_box(&n));
        })
    });
}
