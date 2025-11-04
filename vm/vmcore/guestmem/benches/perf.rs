// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Performance tests.

// UNSAFETY: testing unsafe interfaces
#![expect(unsafe_code)]
#![expect(missing_docs)]

use guestmem::AlignedHeapMemory;
use guestmem::GuestMemory;
use guestmem::GuestMemoryAccess;
use std::hint::black_box;
use std::ptr::NonNull;

criterion::criterion_main!(benches);

criterion::criterion_group!(benches, bench_access);

struct PerfGuestMemory {
    memory: AlignedHeapMemory,
    #[cfg(feature = "bitmap")]
    bitmap: Vec<u8>,
}

impl PerfGuestMemory {
    fn new(size: usize) -> Self {
        PerfGuestMemory {
            memory: AlignedHeapMemory::new(size),
            #[cfg(feature = "bitmap")]
            bitmap: vec![!0u8; size.div_ceil(guestmem::PAGE_SIZE).div_ceil(8)],
        }
    }
}

// SAFETY: implements the trait with a valid mapping.
unsafe impl GuestMemoryAccess for PerfGuestMemory {
    fn mapping(&self) -> Option<NonNull<u8>> {
        self.memory.mapping()
    }

    fn max_address(&self) -> u64 {
        self.memory.max_address()
    }

    #[cfg(feature = "bitmap")]
    fn access_bitmap(&self) -> Option<guestmem::BitmapInfo> {
        if std::env::var("GUESTMEM_NO_BITMAP").is_ok_and(|v| !v.is_empty()) {
            return None;
        }
        Some(guestmem::BitmapInfo {
            read_bitmap: NonNull::new(self.bitmap.as_ptr().cast_mut()).unwrap(),
            write_bitmap: NonNull::new(self.bitmap.as_ptr().cast_mut()).unwrap(),
            bit_offset: 0,
        })
    }
}

fn bench_access(c: &mut criterion::Criterion) {
    let backing = PerfGuestMemory::new(16 * 1024 * 1024);
    let mem = GuestMemory::new("perf", backing);
    c.bench_function("read-plain-1", |b| {
        // SAFETY: passing a valid src.
        b.iter(|| {
            let v = mem.read_plain::<u8>(0).unwrap();
            black_box(v);
        });
    })
    .bench_function("read-plain-8", |b| {
        // SAFETY: passing a valid src.
        b.iter(|| {
            let v = mem.read_plain::<u64>(0).unwrap();
            black_box(v);
        });
    })
    .bench_function("read-at-1", |b| read_at_n::<1>(b, &mem))
    .bench_function("read-at-4", |b| read_at_n::<4>(b, &mem))
    .bench_function("read-at-8", |b| read_at_n::<8>(b, &mem))
    .bench_function("read-at-32", |b| read_at_n::<32>(b, &mem))
    .bench_function("read-at-256", |b| read_at_n::<256>(b, &mem))
    .bench_function("read-at-4096", |b| read_at_n::<4096>(b, &mem))
    .bench_function("write-at-1", |b| write_at_n::<1>(b, &mem))
    .bench_function("write-at-4", |b| write_at_n::<4>(b, &mem))
    .bench_function("write-at-8", |b| write_at_n::<8>(b, &mem))
    .bench_function("write-at-32", |b| write_at_n::<32>(b, &mem))
    .bench_function("write-at-256", |b| write_at_n::<256>(b, &mem))
    .bench_function("write-at-4096", |b| write_at_n::<4096>(b, &mem))
    .bench_function("fill-at-1", |b| fill_at_n::<1>(b, &mem))
    .bench_function("fill-at-32", |b| fill_at_n::<32>(b, &mem))
    .bench_function("fill-at-256", |b| fill_at_n::<256>(b, &mem))
    .bench_function("fill-at-4096", |b| fill_at_n::<4096>(b, &mem));
}

fn read_at_n<const N: usize>(b: &mut criterion::Bencher<'_>, mem: &GuestMemory) {
    let mut dest = [0u8; N];
    b.iter(|| {
        mem.read_at(0, &mut dest).unwrap();
        black_box(&dest);
    })
}

fn write_at_n<const N: usize>(b: &mut criterion::Bencher<'_>, mem: &GuestMemory) {
    let src = [0u8; N];
    b.iter(|| {
        mem.write_at(0, &src).unwrap();
    })
}

fn fill_at_n<const N: usize>(b: &mut criterion::Bencher<'_>, mem: &GuestMemory) {
    b.iter(|| {
        mem.fill_at(0, 0u8, N).unwrap();
    })
}
