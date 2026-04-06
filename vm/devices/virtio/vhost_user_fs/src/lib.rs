// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

#![cfg(target_os = "linux")]
#![expect(missing_docs)]
#![forbid(unsafe_code)]

pub mod resolver;

use guestmem::MappedMemoryRegion;
use inspect::InspectMut;
use std::sync::Arc;
use vhost_user_frontend::VhostUserFrontend;
use virtio::DeviceTraits;
use virtio::QueueResources;
use virtio::VirtioDevice;
use virtio::queue::QueueState;
use virtio::spec::VirtioDeviceFeatures;

pub struct VhostUserFsDevice {
    frontend: VhostUserFrontend,
    config_space: Vec<u8>,
}

impl VhostUserFsDevice {
    pub fn new(frontend: VhostUserFrontend, tag: &str, num_request_queues: u32) -> Self {
        Self {
            frontend,
            config_space: virtiofs::virtio::encode_config_space(tag, num_request_queues),
        }
    }
}

impl InspectMut for VhostUserFsDevice {
    fn inspect_mut(&mut self, req: inspect::Request<'_>) {
        self.frontend.inspect_mut(req);
    }
}

impl VirtioDevice for VhostUserFsDevice {
    fn traits(&self) -> DeviceTraits {
        let mut traits = self.frontend.traits();
        traits.device_register_length = self.config_space.len() as u32;
        traits
    }

    fn queue_size(&self, queue_index: u16) -> u16 {
        self.frontend.queue_size(queue_index)
    }

    async fn read_registers_u32(&mut self, offset: u16) -> u32 {
        let offset = offset as usize;
        let mut bytes = [0u8; 4];

        if offset < self.config_space.len() {
            let end = std::cmp::min(offset + bytes.len(), self.config_space.len());
            bytes[..end - offset].copy_from_slice(&self.config_space[offset..end]);
        }

        u32::from_le_bytes(bytes)
    }

    async fn write_registers_u32(&mut self, _offset: u16, _val: u32) {}

    fn set_shared_memory_region(
        &mut self,
        region: &Arc<dyn MappedMemoryRegion>,
    ) -> anyhow::Result<()> {
        self.frontend.set_shared_memory_region(region)
    }

    async fn start_queue(
        &mut self,
        idx: u16,
        resources: QueueResources,
        features: &VirtioDeviceFeatures,
        initial_state: Option<QueueState>,
    ) -> anyhow::Result<()> {
        self.frontend
            .start_queue(idx, resources, features, initial_state)
            .await
    }

    async fn stop_queue(&mut self, idx: u16) -> Option<QueueState> {
        self.frontend.stop_queue(idx).await
    }

    async fn reset(&mut self) {
        self.frontend.reset().await
    }

    fn supports_save_restore(&self) -> bool {
        self.frontend.supports_save_restore()
    }
}
