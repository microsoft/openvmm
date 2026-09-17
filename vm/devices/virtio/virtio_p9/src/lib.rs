// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

#![expect(missing_docs)]
#![forbid(unsafe_code)]
#![cfg(any(windows, target_os = "linux"))]

#[cfg(test)]
mod integration_tests;
pub mod resolver;

use anyhow::Context as _;
use futures::StreamExt;
use guestmem::GuestMemory;
use inspect::InspectMut;
use pal_async::wait::PolledWait;
use plan9::Plan9FileSystem;
use task_control::AsyncRun;
use task_control::Cancelled;
use task_control::InspectTaskMut;
use task_control::StopTask;
use task_control::TaskControl;
use virtio::DeviceTraits;
use virtio::QueueResources;
use virtio::VirtioDevice;
use virtio::VirtioQueue;
use virtio::VirtioQueueCallbackWork;
use virtio::queue::QueueState;
use virtio::spec::VirtioDeviceFeatures;
use vmcore::vm_task::VmTaskDriver;
use vmcore::vm_task::VmTaskDriverSource;

const VIRTIO_9P_F_MOUNT_TAG: u32 = 1;

#[derive(InspectMut)]
pub struct VirtioPlan9Device {
    tag: Vec<u8>,
    driver: VmTaskDriver,
    #[inspect(mut)]
    worker: TaskControl<Plan9Worker, Plan9Queue>,
}

impl VirtioPlan9Device {
    pub fn new(
        driver_source: &VmTaskDriverSource,
        tag: &str,
        fs: Plan9FileSystem,
    ) -> VirtioPlan9Device {
        // The tag uses the same format as 9p protocol strings (2 byte length followed by string).
        let length = tag.len() + size_of::<u16>();

        // Round the length up to a multiple of 4 to make the read function simpler.
        let length = (length + 3) & !3;
        let mut tag_buffer = vec![0u8; length];

        // Write a string preceded by a two byte length.
        {
            use std::io::Write;
            let mut cursor = std::io::Cursor::new(&mut tag_buffer);
            cursor.write_all(&(tag.len() as u16).to_le_bytes()).unwrap();
            cursor.write_all(tag.as_bytes()).unwrap();
        }

        VirtioPlan9Device {
            tag: tag_buffer,
            driver: driver_source.simple(),
            worker: TaskControl::new(Plan9Worker { fs }),
        }
    }
}

impl VirtioDevice for VirtioPlan9Device {
    fn traits(&self) -> DeviceTraits {
        DeviceTraits {
            device_id: virtio::spec::VirtioDeviceType::P9,
            device_features: VirtioDeviceFeatures::new()
                .with_device_specific_low(VIRTIO_9P_F_MOUNT_TAG)
                .with_ring_event_idx(true)
                .with_ring_indirect_desc(true)
                .with_ring_packed(true),
            max_queues: 1,
            device_register_length: self.tag.len() as u32,
            ..Default::default()
        }
    }

    async fn read_registers_u32(&mut self, offset: u16) -> u32 {
        assert!(self.tag.len().is_multiple_of(4));
        assert!(offset.is_multiple_of(4));

        let offset = offset as usize;
        if offset < self.tag.len() {
            u32::from_le_bytes(
                self.tag[offset..offset + 4]
                    .try_into()
                    .expect("Incorrect length"),
            )
        } else {
            0
        }
    }

    async fn write_registers_u32(&mut self, offset: u16, val: u32) {
        tracing::warn!(offset, val, "[VIRTIO 9P] Unknown write",);
    }

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
            "virtio-9p-queue",
            Plan9Queue {
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

    async fn reset(&mut self) {
        self.worker.task().fs.reset();
    }
}

#[derive(InspectMut)]
struct Plan9Worker {
    #[inspect(skip)]
    fs: Plan9FileSystem,
}

#[derive(InspectMut)]
struct Plan9Queue {
    queue: VirtioQueue,
    mem: GuestMemory,
}

impl InspectTaskMut<Plan9Queue> for Plan9Worker {
    fn inspect_mut(&mut self, req: inspect::Request<'_>, state: Option<&mut Plan9Queue>) {
        req.respond().merge(self).merge(state);
    }
}

impl AsyncRun<Plan9Queue> for Plan9Worker {
    async fn run(
        &mut self,
        stop: &mut StopTask<'_>,
        state: &mut Plan9Queue,
    ) -> Result<(), Cancelled> {
        loop {
            let work = stop.until_stopped(state.queue.next()).await?;
            let Some(work) = work else { break };
            match work {
                Ok(work) => {
                    let bytes = process_9p_request(&state.mem, &self.fs, &work);
                    state.queue.complete(work, bytes);
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

fn process_9p_request(
    mem: &GuestMemory,
    fs: &Plan9FileSystem,
    work: &VirtioQueueCallbackWork,
) -> u32 {
    // Make a copy of the incoming message.
    let mut message = vec![0; work.get_payload_length(false) as usize];
    if let Err(e) = work.read(mem, &mut message) {
        tracing::error!(
            error = &e as &dyn std::error::Error,
            "[VIRTIO 9P] Failed to read guest memory"
        );
        return 0;
    }

    // Allocate a temporary buffer for the response.
    let mut response = vec![9; work.get_payload_length(true) as usize];
    let Ok(size) = fs.process_message(&message, &mut response) else {
        return 0;
    };

    // Write out the response.
    if let Err(e) = work.write(mem, &response[0..size]) {
        tracing::error!(
            error = &e as &dyn std::error::Error,
            "[VIRTIO 9P] Failed to write guest memory"
        );
        return 0;
    }

    size as u32
}
