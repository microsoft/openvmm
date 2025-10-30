// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Benchmarks for the VMBus ring buffer.

#![expect(missing_docs)]

use criterion::BenchmarkId;
use criterion::Criterion;
use criterion::Throughput;
use criterion::criterion_group;
use criterion::criterion_main;
use safeatomic::AsAtomicBytes;
use safeatomic::AtomicSliceOps;
use std::hint::black_box;
use std::sync::atomic::AtomicU8;
use std::sync::atomic::AtomicU32;
use vmbus_ring::CONTROL_WORD_COUNT;
use vmbus_ring::PAGE_SIZE;
use vmbus_ring::PagedMemory;
use vmbus_ring::PagedRingMem;
use vmbus_ring::RingMem;

criterion_main!(benches);

criterion_group!(benches, paged_ring_mem,);

#[derive(Debug)]
struct PageRefs<'a>(&'a [&'a [AtomicU8; PAGE_SIZE]]);

impl PagedMemory for PageRefs<'_> {
    fn control(&self) -> &[AtomicU8; PAGE_SIZE] {
        self.0[0]
    }

    fn data_page_count(&self) -> usize {
        (self.0.len() - 1) / 2
    }

    fn data(&self, page: usize) -> &[AtomicU8; PAGE_SIZE] {
        self.0[page + 1]
    }
}

#[derive(Debug)]
struct SlowRingMem<T>(T);

impl<T: PagedMemory> RingMem for SlowRingMem<T> {
    fn len(&self) -> usize {
        self.0.data_page_count() * PAGE_SIZE
    }

    fn read_at(&self, addr: usize, data: &mut [u8]) {
        for (i, b) in data.iter_mut().enumerate() {
            let addr = (addr + i) % self.len();
            *b = self.0.data(addr / PAGE_SIZE)[addr % PAGE_SIZE]
                .load(std::sync::atomic::Ordering::Relaxed);
        }
    }

    fn write_at(&self, addr: usize, data: &[u8]) {
        for (i, b) in data.iter().enumerate() {
            let addr = (addr + i) % self.len();
            self.0.data(addr / PAGE_SIZE)[addr % PAGE_SIZE]
                .store(*b, std::sync::atomic::Ordering::Relaxed);
        }
    }

    fn control(&self) -> &[AtomicU32; CONTROL_WORD_COUNT] {
        self.0.control().as_atomic_slice().unwrap()[..CONTROL_WORD_COUNT]
            .try_into()
            .unwrap()
    }
}

fn paged_ring_mem(c: &mut Criterion) {
    let mut pages = vec![[0u8; PAGE_SIZE]; 12];
    let pages: Vec<_> = pages
        .iter_mut()
        .map(|p| <&[AtomicU8; PAGE_SIZE]>::try_from(p.as_atomic_bytes()).unwrap())
        .collect();
    let pages: Vec<_> = pages.iter().chain(pages.iter().skip(1)).copied().collect();
    let mem = PagedRingMem::new(PageRefs(pages.as_ref()));
    let slow_mem = SlowRingMem(PageRefs(pages.as_ref()));

    let mut data = [0; 8192];

    let mut group = c.benchmark_group("read");
    for size in &[16usize, 256, 8192] {
        group
            .throughput(Throughput::Bytes(*size as u64))
            .bench_with_input(
                BenchmarkId::new("PagedRingMem::read_at", size),
                size,
                |b, &i| {
                    b.iter(|| {
                        mem.read_at(black_box(4088), black_box(&mut data[..i]));
                    });
                },
            )
            .bench_with_input(
                BenchmarkId::new("PagedRingMem::read_aligned", size),
                size,
                |b, &i| {
                    b.iter(|| {
                        mem.read_aligned(black_box(4088), black_box(&mut data[..i]));
                    });
                },
            )
            .bench_with_input(
                BenchmarkId::new("SlowRingMem::read_at", size),
                size,
                |b, &i| {
                    b.iter(|| {
                        slow_mem.read_at(black_box(4088), black_box(&mut data[..i]));
                    });
                },
            );
    }
    group.finish();

    let mut group = c.benchmark_group("write");
    for size in &[16usize, 40, 256, 8192] {
        group
            .throughput(Throughput::Bytes(*size as u64))
            .bench_with_input(
                BenchmarkId::new("PagedRingMem::write_at", size),
                size,
                |b, &i| {
                    b.iter(|| {
                        mem.write_at(black_box(4088), black_box(&data[..i]));
                    });
                },
            )
            .bench_with_input(
                BenchmarkId::new("PagedRingMem::write_aligned", size),
                size,
                |b, &i| {
                    b.iter(|| {
                        mem.write_aligned(black_box(4088), black_box(&data[..i]));
                    });
                },
            )
            .bench_with_input(
                BenchmarkId::new("SlowRingMem::write_at", size),
                size,
                |b, &i| {
                    b.iter(|| {
                        slow_mem.write_at(black_box(4088), black_box(&data[..i]));
                    });
                },
            );
    }
    group.finish();
}
