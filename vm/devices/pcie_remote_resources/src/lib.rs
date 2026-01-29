// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Resource definitions for the PCIe remote device.

use mesh::MeshPayload;
use vm_resource::ResourceId;
use vm_resource::kind::PciDeviceHandleKind;

/// Default Unix socket path for PCIe remote device communication.
pub const DEFAULT_SOCKET_PATH: &str = "/tmp/qemu-pci-remote-0-ep.sock";

/// Handle for a PCIe remote device.
///
/// This device acts as a generic PCIe proxy, forwarding all PCIe operations
/// (config space, MMIO, DMA, interrupts) to an external device simulator over
/// a Unix domain socket.
#[derive(MeshPayload)]
pub struct PcieRemoteHandle {
    /// Unique instance identifier for this device.
    pub instance_id: guid::Guid,
    /// Path to the Unix domain socket for communication with the simulator.
    /// If `None`, defaults to [`DEFAULT_SOCKET_PATH`].
    pub socket_path: Option<String>,
    /// The upper 16 bits of the PCI location (segment:bus).
    pub hu: u16,
    /// The lower 16 bits of the PCI location (device:function).
    pub controller: u16,
}

impl PcieRemoteHandle {
    /// Get the socket path, using the default if not specified.
    pub fn socket_path(&self) -> &str {
        self.socket_path.as_deref().unwrap_or(DEFAULT_SOCKET_PATH)
    }
}

impl ResourceId<PciDeviceHandleKind> for PcieRemoteHandle {
    const ID: &'static str = "pcie_remote";
}
