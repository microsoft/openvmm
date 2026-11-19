// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Performance tests.

// UNSAFETY: testing unsafe interfaces
#![expect(unsafe_code)]
#![expect(missing_docs)]

use guestmem::AlignedHeapMemory;
use guestmem::GuestMemory;
use guestmem::GuestMemoryAccess;
use guestmem::cached::CacheInvalidation;
use guestmem::cached::CachedAccess;
use guestmem::cached::with_access;
use std::hint::black_box;
use std::ptr::NonNull;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::sync::atomic::fence;
use zerocopy::FromBytes;
use zerocopy::Immutable;
use zerocopy::IntoBytes;
use zerocopy::KnownLayout;

criterion::criterion_main!(benches);

criterion::criterion_group!(benches, bench_access, bench_queues);

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

    fn cache_invalidation(&self) -> Option<Arc<CacheInvalidation>> {
        self.memory.cache_invalidation()
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

trait QueueMemory {
    fn read<T: FromBytes + Immutable + KnownLayout>(&self, offset: u64) -> T;
    fn write<T: IntoBytes + Immutable + KnownLayout>(&self, offset: u64, value: &T);
}

impl QueueMemory for GuestMemory {
    #[inline]
    fn read<T: FromBytes + Immutable + KnownLayout>(&self, offset: u64) -> T {
        self.read_plain(offset).unwrap()
    }

    #[inline]
    fn write<T: IntoBytes + Immutable + KnownLayout>(&self, offset: u64, value: &T) {
        self.write_plain(offset, value).unwrap();
    }
}

impl QueueMemory for CachedAccess<'_> {
    #[inline]
    fn read<T: FromBytes + Immutable + KnownLayout>(&self, offset: u64) -> T {
        self.read_plain(offset).unwrap()
    }

    #[inline]
    fn write<T: IntoBytes + Immutable + KnownLayout>(&self, offset: u64, value: &T) {
        self.write_plain(offset, value).unwrap();
    }
}

#[repr(C)]
#[derive(FromBytes, IntoBytes, Immutable, KnownLayout)]
struct Descriptor {
    address: u64,
    len: u32,
    id: u16,
    flags: u16,
}

#[repr(C)]
#[derive(FromBytes, IntoBytes, Immutable, KnownLayout)]
struct UsedElement {
    id: u32,
    len: u32,
}

const QUEUE_SIZE: usize = 256;
const AVAIL: u64 = 4096;
const USED: u64 = 8192;

// These exercise the metadata access patterns and ordering of queue fetch plus
// completion, not a complete virtqueue state machine or payload I/O.
fn split_batch(
    desc: &impl QueueMemory,
    avail: &impl QueueMemory,
    used: &impl QueueMemory,
    batch: usize,
) {
    let available: u16 = avail.read(2);
    fence(Ordering::Acquire);
    for i in 0..batch as u16 {
        let head: u16 = avail.read(4 + 2 * u64::from(i));
        let descriptor: Descriptor = desc.read(16 * u64::from(head));
        black_box(descriptor.address);
        used.write(
            4 + 8 * u64::from(i),
            &UsedElement {
                id: u32::from(head),
                len: descriptor.len,
            },
        );
        fence(Ordering::Release);
        used.write(2, &available.wrapping_add(i + 1));
        fence(Ordering::SeqCst);
        black_box(avail.read::<u16>(4 + 2 * QUEUE_SIZE as u64));
    }
}

fn packed_batch(desc: &impl QueueMemory, events: &impl QueueMemory, batch: usize) {
    for i in 0..batch as u64 {
        let offset = 16 * i;
        let flags: u16 = desc.read(offset + 14);
        fence(Ordering::Acquire);
        let descriptor: Descriptor = desc.read(offset);
        black_box((descriptor.address, descriptor.id));
        desc.write(offset + 8, &descriptor.len);
        fence(Ordering::Release);
        desc.write(offset + 14, &(flags ^ 0x8000));
        fence(Ordering::SeqCst);
        black_box(events.read::<u32>(0));
    }
}

fn queue_memory() -> GuestMemory {
    let memory = GuestMemory::new("queue-bench", PerfGuestMemory::new(4 * 4096));
    for i in 0..QUEUE_SIZE as u16 {
        memory
            .write_plain(
                16 * u64::from(i),
                &Descriptor {
                    address: 0x10000 + 4096 * u64::from(i),
                    len: 4096,
                    id: i,
                    flags: 0x80,
                },
            )
            .unwrap();
        memory
            .write_plain(AVAIL + 4 + 2 * u64::from(i), &i)
            .unwrap();
    }
    memory.write_plain(AVAIL + 2, &(QUEUE_SIZE as u16)).unwrap();
    memory
}

fn bench_queues(c: &mut criterion::Criterion) {
    let memory = queue_memory();
    let desc = memory.subrange(0, 4096, true).unwrap();
    let avail = memory.subrange(AVAIL, 4096, true).unwrap();
    let used = memory.subrange(USED, 4096, true).unwrap();
    let mut desc_cache = memory.cache_range(0, 4096).unwrap();
    let mut avail_cache = memory.cache_range(AVAIL, 4096).unwrap();
    let mut used_cache = memory.cache_range(USED, 4096).unwrap();

    let mut group = c.benchmark_group("queue-split");
    for batch in [1, 8, 32] {
        group.throughput(criterion::Throughput::Elements(batch as u64));
        group.bench_with_input(
            criterion::BenchmarkId::new("guestmem", batch),
            &batch,
            |b, &n| {
                b.iter(|| split_batch(&desc, &avail, &used, black_box(n)));
            },
        );
        group.bench_with_input(
            criterion::BenchmarkId::new("scoped-guestmem", batch),
            &batch,
            |b, &n| {
                b.iter(|| with_access(|_| split_batch(&desc, &avail, &used, black_box(n))));
            },
        );
        group.bench_with_input(
            criterion::BenchmarkId::new("cached", batch),
            &batch,
            |b, &n| {
                b.iter(|| {
                    with_access(|scope| {
                        let desc = desc_cache.access(scope);
                        let avail = avail_cache.access(scope);
                        let used = used_cache.access(scope);
                        split_batch(&desc, &avail, &used, black_box(n));
                    })
                });
            },
        );
    }
    group.finish();

    let mut event_cache = memory.cache_range(AVAIL, 4).unwrap();
    let mut group = c.benchmark_group("queue-packed");
    for batch in [1, 8, 32] {
        group.throughput(criterion::Throughput::Elements(batch as u64));
        group.bench_with_input(
            criterion::BenchmarkId::new("guestmem", batch),
            &batch,
            |b, &n| {
                b.iter(|| packed_batch(&desc, &avail, black_box(n)));
            },
        );
        group.bench_with_input(
            criterion::BenchmarkId::new("scoped-guestmem", batch),
            &batch,
            |b, &n| {
                b.iter(|| with_access(|_| packed_batch(&desc, &avail, black_box(n))));
            },
        );
        group.bench_with_input(
            criterion::BenchmarkId::new("cached", batch),
            &batch,
            |b, &n| {
                b.iter(|| {
                    with_access(|scope| {
                        let desc = desc_cache.access(scope);
                        let events = event_cache.access(scope);
                        packed_batch(&desc, &events, black_box(n));
                    })
                });
            },
        );
    }
    group.finish();
}
