// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Actual queue fetch/completion cost, with guest publication outside timing.

#![forbid(unsafe_code)]
#![expect(missing_docs)]

use criterion::BenchmarkId;
use criterion::Criterion;
use criterion::Throughput;
use guestmem::GuestMemory;
use pal_async::DefaultPool;
use pal_async::wait::PolledWait;
use pal_event::Event;
use std::hint::black_box;
use std::sync::atomic::Ordering;
use std::sync::atomic::fence;
use std::time::Duration;
use std::time::Instant;
use virtio::VirtioQueue;
use virtio::queue::QueueParams;
use virtio::spec::VirtioDeviceFeatures;
use virtio::spec::queue::DescriptorFlags;
use virtio::spec::queue::PackedDescriptor;
use virtio::spec::queue::SplitDescriptor;
use virtio::spec::u16_le;
use vmcore::interrupt::Interrupt;

const SIZE: u16 = 256;
const AVAIL: u64 = 4096;
const USED: u64 = 8192;

struct QueueBench {
    queue: VirtioQueue,
    memory: GuestMemory,
    packed: bool,
    descriptor: u64,
    available: u16,
    _pool: DefaultPool,
}

impl QueueBench {
    fn new(packed: bool) -> Self {
        let pool = DefaultPool::new();
        let memory = GuestMemory::allocate(4 * 4096);
        let queue = VirtioQueue::new(
            VirtioDeviceFeatures::new().with_ring_packed(packed),
            QueueParams {
                size: SIZE,
                enable: true,
                desc_addr: 0,
                avail_addr: AVAIL,
                used_addr: USED,
            },
            memory.clone(),
            Interrupt::from_fn(|| {}),
            PolledWait::new(&pool.driver(), Event::new()).unwrap(),
            None,
        )
        .unwrap();
        Self {
            queue,
            memory,
            packed,
            descriptor: 0,
            available: 0,
            _pool: pool,
        }
    }

    fn publish(&mut self, chains: u16, descriptors: u16) {
        for chain in 0..chains {
            let first = self.descriptor;
            for entry in 0..descriptors {
                let raw = first + u64::from(entry);
                let index = (raw % u64::from(SIZE)) as u16;
                let flags = DescriptorFlags::new()
                    .with_next(entry + 1 != descriptors)
                    .with_write(true);
                if self.packed {
                    let wrap = (raw / u64::from(SIZE)) & 1 == 0;
                    self.memory
                        .write_plain(
                            u64::from(index) * 16,
                            &PackedDescriptor {
                                address: 0x10000u64.into(),
                                length: 512.into(),
                                buffer_id: ((first % u64::from(SIZE)) as u16).into(),
                                flags_raw: flags
                                    .with_available(wrap)
                                    .with_used(!wrap)
                                    .into_bits()
                                    .into(),
                            },
                        )
                        .unwrap();
                } else {
                    self.memory
                        .write_plain(
                            u64::from(index) * 16,
                            &SplitDescriptor {
                                address: 0x10000u64.into(),
                                length: 512.into(),
                                flags_raw: flags.into_bits().into(),
                                next: ((index + 1) % SIZE).into(),
                            },
                        )
                        .unwrap();
                }
            }
            if !self.packed {
                let slot = self.available.wrapping_add(chain) % SIZE;
                self.memory
                    .write_plain(
                        AVAIL + 4 + 2 * u64::from(slot),
                        &u16_le::new((first % u64::from(SIZE)) as u16),
                    )
                    .unwrap();
            }
            self.descriptor += u64::from(descriptors);
        }
        self.available = self.available.wrapping_add(chains);
        fence(Ordering::Release);
        if !self.packed {
            self.memory
                .write_plain(AVAIL + 2, &u16_le::new(self.available))
                .unwrap();
        }
    }

    fn measure(&mut self, iterations: u64, chains: u16, descriptors: u16) -> Duration {
        let mut elapsed = Duration::ZERO;
        for _ in 0..iterations {
            self.publish(chains, descriptors);
            let start = Instant::now();
            for _ in 0..chains {
                let work = self.queue.try_next().unwrap().unwrap();
                black_box(&work.payload);
                self.queue.complete(work, 512 * u32::from(descriptors));
            }
            elapsed += start.elapsed();
            let expected = if self.packed {
                let index = (self.descriptor % u64::from(SIZE)) as u16;
                let wrap = (self.descriptor / u64::from(SIZE)) & 1 == 0;
                index | (u16::from(wrap) << 15)
            } else {
                self.available
            };
            let state = self.queue.queue_state();
            assert_eq!(state.avail_index, expected);
            assert_eq!(state.used_index, expected);
        }
        elapsed
    }
}

fn queues(c: &mut Criterion) {
    for (layout, packed) in [("split", false), ("packed", true)] {
        let mut group = c.benchmark_group(format!("virtqueue-{layout}"));
        for descriptors in [1, 3] {
            for batch in [1, 16] {
                let mut queue = QueueBench::new(packed);
                group.throughput(Throughput::Elements(u64::from(batch)));
                group.bench_function(
                    BenchmarkId::new(format!("chain-{descriptors}"), batch),
                    |b| b.iter_custom(|iterations| queue.measure(iterations, batch, descriptors)),
                );
            }
        }
        group.finish();
    }
}

criterion::criterion_group!(benches, queues);
criterion::criterion_main!(benches);
