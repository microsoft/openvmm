// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Control-console resource.
//!
//! A [`VirtioControlConsoleHandle`] requests a virtio console whose guest
//! session is served by the VMM-resident control-session broker, configured by
//! [`VirtioControlConsoleBrokerConfig`], in front of a reconnectable host
//! endpoint.

use super::attachment::VirtioConsoleAttachment;
use mesh::MeshPayload;
use vm_resource::Resource;
use vm_resource::ResourceId;
use vm_resource::kind::SerialBackendHandle;
use vm_resource::kind::VirtioDeviceHandle;

#[derive(Clone, MeshPayload)]
pub struct VirtioControlConsoleBrokerConfig {
    pub instance_id: [u8; 16],
    pub capability: [u8; 32],
    pub expected_peer_identity: serial_core::LocalPeerIdentity,
    pub auth_timeout_ms: u64,
}

#[derive(MeshPayload)]
pub struct VirtioControlConsoleHandle {
    /// Reconnectable host endpoint used by the VMM-resident broker.
    pub backend: Resource<SerialBackendHandle>,
    pub broker_config: VirtioControlConsoleBrokerConfig,
    pub attachment: Option<VirtioConsoleAttachment>,
}

impl ResourceId<VirtioDeviceHandle> for VirtioControlConsoleHandle {
    const ID: &'static str = "virtio-control-console";
}

#[cfg(test)]
mod tests {
    use super::super::VirtioConsoleHandle;
    use super::*;

    #[test]
    fn console_resource_ids_are_distinct() {
        assert_eq!(
            <VirtioConsoleHandle as ResourceId<VirtioDeviceHandle>>::ID,
            "virtio-console"
        );
        assert_eq!(
            <VirtioControlConsoleHandle as ResourceId<VirtioDeviceHandle>>::ID,
            "virtio-control-console"
        );
    }
}
