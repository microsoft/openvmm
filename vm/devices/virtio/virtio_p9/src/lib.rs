// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

#![expect(missing_docs)]
#![forbid(unsafe_code)]
#![cfg(any(windows, target_os = "linux"))]

pub mod resolver;

use async_trait::async_trait;
use guestmem::GuestMemory;
use plan9::Plan9FileSystem;
use std::sync::Arc;
use virtio::DeviceTraits;
use virtio::LegacyVirtioDevice;
use virtio::VirtioQueueCallbackWork;
use virtio::VirtioQueueWorkerContext;
use virtio::VirtioState;

const VIRTIO_DEVICE_TYPE_9P_TRANSPORT: u16 = 9;

const VIRTIO_9P_F_MOUNT_TAG: u64 = 1;

pub struct VirtioPlan9Device {
    fs: Arc<Plan9FileSystem>,
    tag: Vec<u8>,
    memory: GuestMemory,
}

impl VirtioPlan9Device {
    pub fn new(tag: &str, fs: Plan9FileSystem, memory: GuestMemory) -> VirtioPlan9Device {
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
            fs: Arc::new(fs),
            tag: tag_buffer,
            memory,
        }
    }
}

impl LegacyVirtioDevice for VirtioPlan9Device {
    fn traits(&self) -> DeviceTraits {
        DeviceTraits {
            device_id: VIRTIO_DEVICE_TYPE_9P_TRANSPORT,
            device_features: VIRTIO_9P_F_MOUNT_TAG,
            max_queues: 1,
            device_register_length: self.tag.len() as u32,
            ..Default::default()
        }
    }

    fn read_registers_u32(&self, offset: u16) -> u32 {
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

    fn write_registers_u32(&mut self, offset: u16, val: u32) {
        tracing::warn!(offset, val, "[VIRTIO 9P] Unknown write",);
    }

    fn get_work_callback(&mut self, index: u16) -> Box<dyn VirtioQueueWorkerContext + Send> {
        assert!(index == 0);
        Box::new(VirtioPlan9Worker {
            mem: self.memory.clone(),
            fs: self.fs.clone(),
        })
    }

    fn state_change(&mut self, _: &VirtioState) {}
}

struct VirtioPlan9Worker {
    mem: GuestMemory,
    fs: Arc<Plan9FileSystem>,
}

#[async_trait]
impl VirtioQueueWorkerContext for VirtioPlan9Worker {
    async fn process_work(&mut self, work: anyhow::Result<VirtioQueueCallbackWork>) -> bool {
        if let Err(err) = work {
            tracing::error!(err = err.as_ref() as &dyn std::error::Error, "queue error");
            return false;
        }
        let mut work = work.unwrap();
        // Make a copy of the incoming message.
        let mut message = vec![0; work.get_payload_length(false) as usize];
        if let Err(e) = work.read(&self.mem, &mut message) {
            tracing::error!(
                error = &e as &dyn std::error::Error,
                "[VIRTIO 9P] Failed to read guest memory"
            );
            return false;
        }

        // Allocate a temporary buffer for the response.
        let mut response = vec![9; work.get_payload_length(true) as usize];
        if let Ok(size) = self.fs.process_message(&message, &mut response) {
            // Write out the response.
            if let Err(e) = work.write(&self.mem, &response[0..size]) {
                tracing::error!(
                    error = &e as &dyn std::error::Error,
                    "[VIRTIO 9P] Failed to write guest memory"
                );
                return false;
            }

            work.complete(size as u32);
        }
        true
    }
}
