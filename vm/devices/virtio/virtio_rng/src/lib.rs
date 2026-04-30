// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Virtio entropy (RNG) device implementation.
//!
//! Implements the virtio-rng device (device ID 4) as specified in the
//! VIRTIO 1.3 specification, §5.9 "Entropy Device". The guest sends
//! writable buffers on a single virtqueue, and the device fills them
//! with random bytes using the host's cryptographic random number
//! generator.

#![expect(missing_docs)]
#![forbid(unsafe_code)]

pub mod resolver;

use anyhow::Context as _;
use futures::StreamExt;
use guestmem::GuestMemory;
use inspect::InspectMut;
use pal_async::wait::PolledWait;
use task_control::AsyncRun;
use task_control::Cancelled;
use task_control::InspectTaskMut;
use task_control::StopTask;
use task_control::TaskControl;
use virtio::DeviceTraits;
use virtio::DeviceTraitsSharedMemory;
use virtio::QueueResources;
use virtio::VirtioDevice;
use virtio::VirtioQueue;
use virtio::VirtioQueueCallbackWork;
use virtio::queue::QueueState;
use virtio::spec::VirtioDeviceFeatures;
use vmcore::vm_task::VmTaskDriver;
use vmcore::vm_task::VmTaskDriverSource;

#[derive(InspectMut)]
pub struct VirtioRngDevice {
    driver: VmTaskDriver,
    #[inspect(mut)]
    worker: TaskControl<RngWorker, RngQueue>,
}

impl VirtioRngDevice {
    pub fn new(driver_source: &VmTaskDriverSource) -> Self {
        Self {
            driver: driver_source.simple(),
            worker: TaskControl::new(RngWorker),
        }
    }
}

impl VirtioDevice for VirtioRngDevice {
    fn traits(&self) -> DeviceTraits {
        DeviceTraits {
            device_id: virtio::spec::VirtioDeviceType::RNG,
            device_features: VirtioDeviceFeatures::new()
                .with_ring_event_idx(true)
                .with_ring_indirect_desc(true)
                .with_ring_packed(true),
            max_queues: 1,
            device_register_length: 0,
            shared_memory: DeviceTraitsSharedMemory::default(),
        }
    }

    async fn read_registers_u32(&mut self, _offset: u16) -> u32 {
        0
    }

    async fn write_registers_u32(&mut self, _offset: u16, _val: u32) {}

    async fn start_queue(
        &mut self,
        idx: u16,
        resources: QueueResources,
        features: &VirtioDeviceFeatures,
        initial_state: Option<QueueState>,
    ) -> anyhow::Result<()> {
        assert_eq!(idx, 0);

        let queue_event = PolledWait::new(&self.driver, resources.event)
            .context("failed to create polled wait")?;
        let queue = VirtioQueue::new(
            *features,
            resources.params,
            resources.guest_memory.clone(),
            resources.notify,
            queue_event,
            initial_state,
        )
        .context("failed to create virtio queue")?;

        self.worker.insert(
            self.driver.clone(),
            "virtio-rng-queue",
            RngQueue {
                queue,
                mem: resources.guest_memory,
            },
        );
        self.worker.start();
        Ok(())
    }

    async fn stop_queue(&mut self, idx: u16) -> Option<QueueState> {
        assert_eq!(idx, 0);
        if !self.worker.has_state() {
            return None;
        }
        self.worker.stop().await;
        let state = self.worker.remove().queue.queue_state();
        Some(state)
    }

    fn supports_save_restore(&self) -> bool {
        true
    }
}

#[derive(InspectMut)]
struct RngWorker;

#[derive(InspectMut)]
struct RngQueue {
    queue: VirtioQueue,
    mem: GuestMemory,
}

impl InspectTaskMut<RngQueue> for RngWorker {
    fn inspect_mut(&mut self, req: inspect::Request<'_>, state: Option<&mut RngQueue>) {
        req.respond().merge(self).merge(state);
    }
}

/// Maximum bytes to serve per request, to prevent a malicious guest from
/// causing unbounded host memory allocation.
const MAX_REQUEST_BYTES: usize = 64 * 1024;

impl AsyncRun<RngQueue> for RngWorker {
    async fn run(
        &mut self,
        stop: &mut StopTask<'_>,
        state: &mut RngQueue,
    ) -> Result<(), Cancelled> {
        loop {
            let work = stop.until_stopped(state.queue.next()).await?;
            let Some(work) = work else { break };
            match work {
                Ok(work) => {
                    let bytes = process_rng_request(&state.mem, &work);
                    state.queue.complete(work, bytes);
                }
                Err(err) => {
                    tracelimit::error_ratelimited!(
                        err = &err as &dyn std::error::Error,
                        "queue error"
                    );
                    break;
                }
            }
        }
        Ok(())
    }
}

fn process_rng_request(mem: &GuestMemory, work: &VirtioQueueCallbackWork) -> u32 {
    let writable_len = std::cmp::min(work.get_payload_length(true) as usize, MAX_REQUEST_BYTES);
    if writable_len == 0 {
        return 0;
    }

    let mut buf = vec![0u8; writable_len];
    getrandom::fill(&mut buf).expect("host entropy source failure");
    match work.write(mem, &buf) {
        Ok(()) => writable_len as u32,
        Err(err) => {
            tracelimit::error_ratelimited!(
                err = &err as &dyn std::error::Error,
                "failed to write random bytes to guest memory"
            );
            0
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use core::mem::offset_of;
    use pal_async::DefaultDriver;
    use pal_async::async_test;
    use pal_async::wait::PolledWait;
    use pal_event::Event;
    use test_with_tracing::test;
    use virtio::QueueResources;
    use virtio::queue::QueueParams;
    use virtio::spec::VirtioDeviceFeatures;
    use virtio::spec::queue::AVAIL_ELEMENT_SIZE;
    use virtio::spec::queue::AVAIL_OFFSET_FLAGS;
    use virtio::spec::queue::AVAIL_OFFSET_IDX;
    use virtio::spec::queue::AVAIL_OFFSET_RING;
    use virtio::spec::queue::DescriptorFlags;
    use virtio::spec::queue::SplitDescriptor;
    use virtio::spec::queue::USED_ELEMENT_SIZE;
    use virtio::spec::queue::USED_OFFSET_FLAGS;
    use virtio::spec::queue::USED_OFFSET_IDX;
    use virtio::spec::queue::USED_OFFSET_RING;
    use virtio::spec::queue::UsedElement;
    use vmcore::interrupt::Interrupt;
    use vmcore::vm_task::SingleDriverBackend;

    const QUEUE_SIZE: u16 = 16;
    const DESC_ADDR: u64 = 0x0000;
    const AVAIL_ADDR: u64 = 0x1000;
    const USED_ADDR: u64 = 0x2000;
    const DATA_BASE: u64 = 0x10000;
    const TOTAL_MEM_SIZE: usize = 0x40000;

    fn write_descriptor(
        mem: &GuestMemory,
        index: u16,
        addr: u64,
        len: u32,
        flags: DescriptorFlags,
        next: u16,
    ) {
        let base = DESC_ADDR + size_of::<SplitDescriptor>() as u64 * index as u64;
        mem.write_at(
            base + offset_of!(SplitDescriptor, address) as u64,
            &addr.to_le_bytes(),
        )
        .unwrap();
        mem.write_at(
            base + offset_of!(SplitDescriptor, length) as u64,
            &len.to_le_bytes(),
        )
        .unwrap();
        mem.write_at(
            base + offset_of!(SplitDescriptor, flags_raw) as u64,
            &u16::from(flags).to_le_bytes(),
        )
        .unwrap();
        mem.write_at(
            base + offset_of!(SplitDescriptor, next) as u64,
            &next.to_le_bytes(),
        )
        .unwrap();
    }

    fn init_rings(mem: &GuestMemory) {
        mem.write_at(AVAIL_ADDR + AVAIL_OFFSET_FLAGS, &0u16.to_le_bytes())
            .unwrap();
        mem.write_at(AVAIL_ADDR + AVAIL_OFFSET_IDX, &0u16.to_le_bytes())
            .unwrap();
        mem.write_at(USED_ADDR + USED_OFFSET_FLAGS, &0u16.to_le_bytes())
            .unwrap();
        mem.write_at(USED_ADDR + USED_OFFSET_IDX, &0u16.to_le_bytes())
            .unwrap();
    }

    fn make_available(mem: &GuestMemory, desc_index: u16, avail_idx: &mut u16) {
        let ring_offset =
            AVAIL_ADDR + AVAIL_OFFSET_RING + AVAIL_ELEMENT_SIZE * (*avail_idx % QUEUE_SIZE) as u64;
        mem.write_at(ring_offset, &desc_index.to_le_bytes())
            .unwrap();
        *avail_idx = avail_idx.wrapping_add(1);
        mem.write_at(AVAIL_ADDR + AVAIL_OFFSET_IDX, &avail_idx.to_le_bytes())
            .unwrap();
    }

    fn read_used_idx(mem: &GuestMemory) -> u16 {
        let mut buf = [0u8; 2];
        mem.read_at(USED_ADDR + USED_OFFSET_IDX, &mut buf).unwrap();
        u16::from_le_bytes(buf)
    }

    fn read_used_entry(mem: &GuestMemory, index: u16) -> (u32, u32) {
        let offset = USED_ADDR + USED_OFFSET_RING + USED_ELEMENT_SIZE * (index % QUEUE_SIZE) as u64;
        let mut id_buf = [0u8; 4];
        let mut len_buf = [0u8; 4];
        mem.read_at(offset + offset_of!(UsedElement, id) as u64, &mut id_buf)
            .unwrap();
        mem.read_at(offset + offset_of!(UsedElement, len) as u64, &mut len_buf)
            .unwrap();
        (u32::from_le_bytes(id_buf), u32::from_le_bytes(len_buf))
    }

    struct TestHarness {
        device: VirtioRngDevice,
        mem: GuestMemory,
        driver: DefaultDriver,
        queue_event: Event,
        interrupt_event: Event,
        avail_idx: u16,
        used_idx: u16,
    }

    impl TestHarness {
        fn new(driver: &DefaultDriver) -> Self {
            let mem = GuestMemory::allocate(TOTAL_MEM_SIZE);
            init_rings(&mem);

            let driver_source = VmTaskDriverSource::new(SingleDriverBackend::new(driver.clone()));
            let device = VirtioRngDevice::new(&driver_source);
            let queue_event = Event::new();
            let interrupt_event = Event::new();

            Self {
                device,
                mem,
                driver: driver.clone(),
                queue_event,
                interrupt_event,
                avail_idx: 0,
                used_idx: 0,
            }
        }

        async fn enable(&mut self) {
            let interrupt = Interrupt::from_event(self.interrupt_event.clone());

            self.device
                .start_queue(
                    0,
                    QueueResources {
                        params: QueueParams {
                            size: QUEUE_SIZE,
                            enable: true,
                            desc_addr: DESC_ADDR,
                            avail_addr: AVAIL_ADDR,
                            used_addr: USED_ADDR,
                        },
                        notify: interrupt,
                        event: self.queue_event.clone(),
                        guest_memory: self.mem.clone(),
                    },
                    &VirtioDeviceFeatures::new(),
                    None,
                )
                .await
                .unwrap();
        }

        /// Submit a single writable buffer and wait for completion.
        /// Returns (descriptor_id, bytes_written).
        async fn submit_and_wait(&mut self, data_gpa: u64, len: u32) -> (u16, u32) {
            let desc_idx = 0u16;
            let flags = DescriptorFlags::new().with_write(true);
            write_descriptor(&self.mem, desc_idx, data_gpa, len, flags, 0);
            make_available(&self.mem, desc_idx, &mut self.avail_idx);
            self.queue_event.signal();

            let mut wait = PolledWait::new(&self.driver, self.interrupt_event.clone()).unwrap();
            mesh::CancelContext::new()
                .with_timeout(std::time::Duration::from_secs(5))
                .until_cancelled(async {
                    loop {
                        let current = read_used_idx(&self.mem);
                        if current != self.used_idx {
                            let (id, written) = read_used_entry(&self.mem, self.used_idx);
                            self.used_idx = self.used_idx.wrapping_add(1);
                            return (id as u16, written);
                        }
                        wait.wait().await.unwrap();
                    }
                })
                .await
                .expect("timed out waiting for used ring entry")
        }
    }

    #[async_test]
    async fn rng_fills_buffer_with_random_bytes(driver: DefaultDriver) {
        let mut harness = TestHarness::new(&driver);
        harness.enable().await;

        let buf_len = 64u32;
        let data_gpa = DATA_BASE;

        // Zero the target region first.
        let zeroes = vec![0u8; buf_len as usize];
        harness.mem.write_at(data_gpa, &zeroes).unwrap();

        let (_id, written) = harness.submit_and_wait(data_gpa, buf_len).await;

        assert_eq!(written, buf_len, "device should fill the entire buffer");

        // Read back and verify not all zeros (random data).
        let mut result = vec![0u8; buf_len as usize];
        harness.mem.read_at(data_gpa, &mut result).unwrap();
        assert_ne!(result, zeroes, "random data should not be all zeros");
    }

    #[async_test]
    async fn rng_handles_zero_length_buffer(driver: DefaultDriver) {
        let mut harness = TestHarness::new(&driver);
        harness.enable().await;

        let (_id, written) = harness.submit_and_wait(DATA_BASE, 0).await;
        assert_eq!(
            written, 0,
            "zero-length request should complete with 0 bytes"
        );
    }

    #[async_test]
    async fn rng_reports_correct_traits(driver: DefaultDriver) {
        let harness = TestHarness::new(&driver);
        let traits = harness.device.traits();
        assert_eq!(traits.device_id, virtio::spec::VirtioDeviceType::RNG);
        assert_eq!(traits.max_queues, 1);
        assert_eq!(traits.device_register_length, 0);
    }
}
