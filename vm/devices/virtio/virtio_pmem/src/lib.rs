// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

#![expect(missing_docs)]
#![forbid(unsafe_code)]

pub mod resolver;

use anyhow::Context;
use async_trait::async_trait;
use guestmem::GuestMemory;
use pal_async::task::Spawn;
use std::fs;
use std::sync::Arc;
use task_control::TaskControl;
use virtio::DeviceTraits;
use virtio::DeviceTraitsSharedMemory;
use virtio::Resources;
use virtio::VirtioDevice;
use virtio::VirtioQueueCallbackWork;
use virtio::VirtioQueueState;
use virtio::VirtioQueueWorker;
use virtio::VirtioQueueWorkerContext;
use virtio::spec::VirtioDeviceFeatures;
use vmcore::vm_task::VmTaskDriver;
use vmcore::vm_task::VmTaskDriverSource;

pub struct Device {
    driver: VmTaskDriver,
    file: Arc<fs::File>,
    mappable: sparse_mmap::Mappable,
    len: u64,
    writable: bool,
    worker: Option<TaskControl<VirtioQueueWorker, VirtioQueueState>>,
    memory: GuestMemory,
    exit_event: event_listener::Event,
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
            file: Arc::new(file),
            mappable,
            len,
            writable,
            worker: None,
            memory,
            exit_event: event_listener::Event::new(),
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

    fn read_registers_u32(&self, _offset: u16) -> u32 {
        // The PmemConfig type is not used--instead, the memory region is
        // reported via the shared memory capability.
        0
    }

    fn write_registers_u32(&mut self, _offset: u16, _val: u32) {}

    fn enable(&mut self, mut resources: Resources) {
        assert!(self.worker.is_none());
        if !resources.queues[0].params.enable {
            return;
        }

        let shared_memory_region = resources.shared_memory_region.clone();
        let _ = shared_memory_region.unwrap().map(
            0,
            &self.mappable,
            0,
            self.len as usize,
            self.writable,
        );

        self.worker = {
            let worker = PmemWorker {
                writable: self.writable,
                file: self.file.clone(),
                mem: self.memory.clone(),
            };

            let worker = VirtioQueueWorker::new(self.driver.clone(), Box::new(worker));
            Some(worker.into_running_task(
                "virtio-pmem-queue".to_string(),
                self.memory.clone(),
                resources.features,
                resources.queues.remove(0),
                self.exit_event.listen(),
            ))
        };
    }

    fn disable(&mut self) {
        self.exit_event.notify(usize::MAX);
        if let Some(mut worker) = self.worker.take() {
            self.driver
                .spawn("shutdown-virtio-pmem-queue".to_owned(), async move {
                    worker.stop().await;
                })
                .detach();
        }
    }
}

struct PmemWorker {
    writable: bool,
    file: Arc<fs::File>,
    mem: GuestMemory,
}

#[async_trait]
impl VirtioQueueWorkerContext for PmemWorker {
    async fn process_work(&mut self, work: anyhow::Result<VirtioQueueCallbackWork>) -> bool {
        if let Err(err) = work {
            tracing::error!(err = err.as_ref() as &dyn std::error::Error, "queue error");
            return false;
        }

        let mut work = work.unwrap();
        let mut req = [0; 4];
        let err = match work.read(&self.mem, &mut req) {
            Ok(_) => match u32::from_le_bytes(req) {
                0 if !self.writable => {
                    // Ignore the request for read-only devices.
                    0
                }
                0 => match self.file.sync_all() {
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
        let _ = work.write(&self.mem, &u32::to_le_bytes(err));
        work.complete(4);
        true
    }
}
