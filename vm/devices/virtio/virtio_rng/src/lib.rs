// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Virtio entropy (RNG) device implementation.
//!
//! Implements the virtio-rng device (device ID 4) which provides hardware
//! random number generation to the guest. The guest sends writable buffers
//! on a single virtqueue, and the device fills them with random bytes using
//! the host's cryptographic random number generator.

#![expect(missing_docs)]
#![forbid(unsafe_code)]

pub mod resolver;

use async_trait::async_trait;
use guestmem::GuestMemory;
use inspect::InspectMut;
use std::task::Poll;
use std::task::ready;
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

const VIRTIO_RNG_DEVICE_ID: u16 = 4;

#[derive(InspectMut)]
pub struct VirtioRngDevice {
    driver: VmTaskDriver,
    #[inspect(skip)]
    worker: Option<TaskControl<VirtioQueueWorker, VirtioQueueState>>,
    memory: GuestMemory,
    #[inspect(skip)]
    exit_event: event_listener::Event,
}

impl VirtioRngDevice {
    pub fn new(driver_source: &VmTaskDriverSource, memory: GuestMemory) -> Self {
        Self {
            driver: driver_source.simple(),
            worker: None,
            memory,
            exit_event: event_listener::Event::new(),
        }
    }
}

impl VirtioDevice for VirtioRngDevice {
    fn traits(&self) -> DeviceTraits {
        DeviceTraits {
            device_id: VIRTIO_RNG_DEVICE_ID,
            device_features: VirtioDeviceFeatures::new(),
            max_queues: 1,
            device_register_length: 0,
            shared_memory: DeviceTraitsSharedMemory::default(),
        }
    }

    fn read_registers_u32(&self, _offset: u16) -> u32 {
        0
    }

    fn write_registers_u32(&mut self, _offset: u16, _val: u32) {}

    fn enable(&mut self, mut resources: Resources) -> anyhow::Result<()> {
        assert!(self.worker.is_none());
        if !resources.queues[0].params.enable {
            return Ok(());
        }

        let worker_context = RngWorker {
            mem: self.memory.clone(),
        };

        let worker = VirtioQueueWorker::new(self.driver.clone(), Box::new(worker_context));
        self.worker = Some(worker.into_running_task(
            "virtio-rng-queue".to_string(),
            self.memory.clone(),
            resources.features,
            resources.queues.remove(0),
            self.exit_event.listen(),
        ));
        Ok(())
    }

    fn poll_disable(&mut self, cx: &mut std::task::Context<'_>) -> Poll<()> {
        self.exit_event.notify(usize::MAX);
        if let Some(worker) = &mut self.worker {
            ready!(worker.poll_stop(cx));
        }
        self.worker = None;
        Poll::Ready(())
    }
}

struct RngWorker {
    mem: GuestMemory,
}

/// Maximum bytes to serve per request, to prevent a malicious guest from
/// causing unbounded host memory allocation.
const MAX_REQUEST_BYTES: usize = 64 * 1024;

#[async_trait]
impl VirtioQueueWorkerContext for RngWorker {
    async fn process_work(&mut self, work: anyhow::Result<VirtioQueueCallbackWork>) -> bool {
        let mut work = match work {
            Ok(work) => work,
            Err(err) => {
                tracing::error!(err = err.as_ref() as &dyn std::error::Error, "queue error");
                return false;
            }
        };

        let writable_len = std::cmp::min(work.get_payload_length(true) as usize, MAX_REQUEST_BYTES);
        if writable_len == 0 {
            work.complete(0);
            return true;
        }

        let mut buf = vec![0u8; writable_len];
        match getrandom::fill(&mut buf) {
            Ok(()) => match work.write(&self.mem, &buf) {
                Ok(()) => {
                    work.complete(writable_len as u32);
                }
                Err(err) => {
                    tracing::error!(
                        err = &err as &dyn std::error::Error,
                        "failed to write random bytes to guest memory"
                    );
                    work.complete(0);
                }
            },
            Err(err) => {
                tracing::error!(%err, "failed to generate random bytes");
                work.complete(0);
            }
        }

        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_device_id() {
        assert_eq!(VIRTIO_RNG_DEVICE_ID, 4);
    }
}
