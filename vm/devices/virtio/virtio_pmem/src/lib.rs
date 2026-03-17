// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

#![expect(missing_docs)]
#![forbid(unsafe_code)]

pub mod resolver;

use anyhow::Context;
use futures::StreamExt;
use guestmem::GuestMemory;
use inspect::InspectMut;
use pal_async::wait::PolledWait;
use std::fs;
use std::task::Poll;
use std::task::ready;
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
pub struct Device {
    driver: VmTaskDriver,
    #[inspect(skip)]
    mappable: sparse_mmap::Mappable,
    len: u64,
    writable: bool,
    #[inspect(mut)]
    worker: TaskControl<PmemWorker, PmemQueue>,
}

impl Device {
    pub fn new(
        driver_source: &VmTaskDriverSource,
        memory: GuestMemory,
        file: fs::File,
        writable: bool,
    ) -> anyhow::Result<Self> {
        let metadata = file.metadata().context("failed to get metadata")?;
        let len = metadata.len();
        let mappable = sparse_mmap::new_mappable_from_file(&file, writable, true)
            .context("failed to create file mapping")?;
        Ok(Self {
            driver: driver_source.simple(),
            worker: TaskControl::new(PmemWorker {
                writable,
                file,
                mem: memory,
            }),
            mappable,
            len,
            writable,
        })
    }
}

#[repr(C)]
struct PmemConfig {
    start: u64,
    size: u64,
}

impl VirtioDevice for Device {
    fn traits(&self) -> DeviceTraits {
        DeviceTraits {
            device_id: 27,
            device_features: VirtioDeviceFeatures::new(),
            max_queues: 1,
            device_register_length: size_of::<PmemConfig>() as u32,
            shared_memory: DeviceTraitsSharedMemory {
                id: 0,
                size: self.len.next_power_of_two().max(0x200000),
            },
        }
    }

    fn read_registers_u32(&mut self, _offset: u16) -> u32 {
        // The PmemConfig type is not used--instead, the memory region is
        // reported via the shared memory capability.
        0
    }

    fn write_registers_u32(&mut self, _offset: u16, _val: u32) {}

    fn set_shared_memory_region(
        &mut self,
        region: &std::sync::Arc<dyn guestmem::MappedMemoryRegion>,
    ) -> anyhow::Result<()> {
        region
            .map(0, &self.mappable, 0, self.len as usize, self.writable)
            .context("failed to map shared memory region")?;

        Ok(())
    }

    fn start_queue(
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
            features.clone(),
            resources.params,
            self.worker.task().mem.clone(),
            resources.notify,
            queue_event,
            initial_state,
        )
        .context("failed to create virtio queue")?;

        self.worker.insert(
            self.driver.clone(),
            "virtio-pmem-queue",
            PmemQueue { queue },
        );
        self.worker.start();
        Ok(())
    }

    fn poll_stop_queue(
        &mut self,
        cx: &mut std::task::Context<'_>,
        idx: u16,
    ) -> Poll<Option<QueueState>> {
        assert_eq!(idx, 0);
        if !self.worker.has_state() {
            return Poll::Ready(None);
        }
        ready!(self.worker.poll_stop(cx));
        let state = self.worker.remove().queue.queue_state();
        Poll::Ready(Some(state))
    }
}

#[derive(InspectMut)]
struct PmemWorker {
    writable: bool,
    file: fs::File,
    mem: GuestMemory,
}

impl InspectTaskMut<PmemQueue> for PmemWorker {
    fn inspect_mut(&mut self, req: inspect::Request<'_>, state: Option<&mut PmemQueue>) {
        req.respond().merge(self).merge(state);
    }
}

#[derive(InspectMut)]
struct PmemQueue {
    queue: VirtioQueue,
}

impl AsyncRun<PmemQueue> for PmemWorker {
    async fn run(
        &mut self,
        stop: &mut StopTask<'_>,
        state: &mut PmemQueue,
    ) -> Result<(), Cancelled> {
        loop {
            let work = stop.until_stopped(state.queue.next()).await?;
            let Some(work) = work else { break };
            match work {
                Ok(work) => {
                    process_pmem_request(self, work);
                }
                Err(err) => {
                    tracing::error!(error = &err as &dyn std::error::Error, "queue error");
                    break;
                }
            }
        }
        Ok(())
    }
}

fn process_pmem_request(worker: &PmemWorker, mut work: VirtioQueueCallbackWork) {
    let mut req = [0; 4];
    let err = match work.read(&worker.mem, &mut req) {
        Ok(_) => match u32::from_le_bytes(req) {
            0 if !worker.writable => {
                // Ignore the request for read-only devices.
                0
            }
            0 => match worker.file.sync_all() {
                Ok(()) => 0,
                Err(err) => {
                    tracing::error!(error = &err as &dyn std::error::Error, "flush error");
                    1
                }
            },
            n => {
                tracing::error!(n, "unsupported request");
                1
            }
        },
        Err(err) => {
            tracing::error!(error = &err as &dyn std::error::Error, "invalid descriptor");
            1
        }
    };
    let _ = work.write(&worker.mem, &u32::to_le_bytes(err));
    work.complete(4);
}
