// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

#![forbid(unsafe_code)]

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
    /// Host Unit - Refers to the PCIe Express Endpoint logic in the
    /// simulator.  If the chip implement multiple PCIe Express IP blocks,
    /// this parameter distinguishes them.
    pub hu: u16,
    /// If the chip implement multiple PCIe Express links within
    /// a single host unit (as with bifurcation), this parameter
    /// distinguishes them.
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
