// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Performance tests.

// UNSAFETY: testing unsafe code.
#![expect(unsafe_code)]
#![expect(missing_docs)]

use criterion::BenchmarkId;

criterion::criterion_main!(benches);

criterion::criterion_group!(benches, bench_memcpy);

fn bench_memcpy(c: &mut criterion::Criterion) {
    unsafe extern "C" {
        fn memcpy(dest: *mut u8, src: *const u8, len: usize) -> *mut u8;
    }
    do_bench_memcpy(c.benchmark_group("fast_memcpy"), fast_memcpy::memcpy);
    do_bench_memcpy(c.benchmark_group("system_memcpy"), memcpy);
}

fn do_bench_memcpy(
    mut group: criterion::BenchmarkGroup<'_, criterion::measurement::WallTime>,
    memcpy_fn: unsafe extern "C" fn(*mut u8, *const u8, usize) -> *mut u8,
) {
    for &len in &[
        1usize, 2, 3, 4, 7, 8, 12, 24, 32, 48, 64, 256, 1024, 2048, 4096, 8000,
    ] {
        group
            .bench_function(BenchmarkId::new("len", len), |b| {
                let src = vec![0u8; len];
                let mut dest = vec![0u8; len];
                // SAFETY: operating correctly on src/dest.
                b.iter(|| unsafe {
                    memcpy_fn(
                        core::hint::black_box(dest.as_mut_ptr().cast()),
                        core::hint::black_box(src.as_ptr().cast()),
                        core::hint::black_box(len),
                    )
                });
            })
            .bench_function(BenchmarkId::new("aligned_len", len), |b| {
                const N: usize = 64;
                #[repr(align(64))]
                #[derive(Clone, Copy)]
                struct Aligned {
                    _data: [u8; N],
                }
                let count = len.next_multiple_of(N) / N;
                let elt = Aligned { _data: [0; N] };
                let src = vec![elt; count];
                let mut dest = vec![elt; count];
                // SAFETY: operating correctly on src/dest.
                b.iter(|| unsafe {
                    memcpy_fn(
                        core::hint::black_box(dest.as_mut_ptr().cast()),
                        core::hint::black_box(src.as_ptr().cast()),
                        core::hint::black_box(len),
                    )
                });
            });
    }
}
