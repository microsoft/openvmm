// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Performance tests.

// UNSAFETY: testing unsafe interfaces
#![expect(unsafe_code)]
#![expect(missing_docs)]

use std::hint::black_box;
use trycopy::initialize_try_copy;

criterion::criterion_main!(benches);

criterion::criterion_group!(benches, bench_access);

fn bench_access(c: &mut criterion::Criterion) {
    initialize_try_copy();
    c.bench_function("try-read-8", |b| {
        // SAFETY: passing a valid src.
        b.iter(|| unsafe {
            let n = 0u8;
            trycopy::try_read_volatile(&n).unwrap();
        });
    })
    .bench_function("read-8", |b| {
        // SAFETY: passing a valid src.
        b.iter(|| unsafe {
            let n = 0u8;
            std::ptr::read_volatile(black_box(&n));
        })
    })
    .bench_function("try-copy-1", try_copy_n::<1>)
    .bench_function("try-copy-4", try_copy_n::<4>)
    .bench_function("try-copy-8", try_copy_n::<8>)
    .bench_function("try-copy-32", try_copy_n::<32>)
    .bench_function("try-copy-256", try_copy_n::<256>)
    .bench_function("try-copy-4096", try_copy_n::<4096>)
    .bench_function("try-set-1", try_set_n::<1>)
    .bench_function("try-set-32", try_set_n::<32>)
    .bench_function("try-set-256", try_set_n::<256>)
    .bench_function("try-set-4096", try_set_n::<4096>);
}

fn try_copy_n<const N: usize>(b: &mut criterion::Bencher<'_>) {
    let src = [0u8; N];
    let mut dest = [0u8; N];
    // SAFETY: passing valid src and dest.
    b.iter(|| unsafe {
        trycopy::try_copy(black_box(src.as_ptr()), black_box(dest.as_mut_ptr()), N).unwrap();
    })
}

fn try_set_n<const N: usize>(b: &mut criterion::Bencher<'_>) {
    let mut dest = [0u8; N];
    // SAFETY: passing valid dest.
    b.iter(|| unsafe {
        trycopy::try_write_bytes(black_box(dest.as_mut_ptr()), 0u8, N).unwrap();
    })
}
