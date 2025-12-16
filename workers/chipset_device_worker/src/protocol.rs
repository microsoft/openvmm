// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! The internal protocol for communications between the proxy and the device wrapper.

use chipset_device::io::IoError;
use mesh::MeshPayload;
use mesh::error::RemoteError;
use mesh::rpc::Rpc;
use vmcore::save_restore::SavedStateBlob;

/// Capabilities of the remote chipset device requested at initialization.
#[derive(MeshPayload)]
pub(crate) struct DeviceInit {
    /// MMIO configuration, if MMIO is supported.
    pub mmio: Option<MmioInit>,
    /// PIO configuration, if PIO is supported.
    pub pio: Option<PioInit>,
    /// PCI configuration, if PCI is supported.
    pub pci: Option<PciInit>,
}

/// MMIO capabilities of the remote chipset device requested at initialization.
#[derive(MeshPayload)]
pub(crate) struct MmioInit {
    /// The static MMIO regions requested by the device.
    pub static_regions: Vec<(String, u64, u64)>,
}

/// PIO capabilities of the remote chipset device requested at initialization.
#[derive(MeshPayload)]
pub(crate) struct PioInit {
    /// The static PIO ports requested by the device.
    pub static_regions: Vec<(String, u16, u16)>,
}

/// PCI capabilities of the remote chipset device requested at initialization.
#[derive(MeshPayload)]
pub(crate) struct PciInit {
    /// The suggested BDF (Bus, Device, Function) for the device.
    pub suggested_bdf: Option<(u8, u8, u8)>,
}

/// Requests sent to the remote device.
#[derive(MeshPayload)]
pub(crate) enum DeviceRequest {
    /// Perform a MMIO read operation.
    MmioRead(ReadRequest<u64>),
    /// Perform a MMIO write operation.
    MmioWrite(WriteRequest<u64, Vec<u8>>),
    /// Perform a PIO read operation.
    PioRead(ReadRequest<u16>),
    /// Perform a PIO write operation.
    PioWrite(WriteRequest<u16, Vec<u8>>),
    /// Perform a PCI config space read.
    PciConfigRead(ReadRequest<u16>),
    /// Perform a PCI config space write.
    PciConfigWrite(WriteRequest<u16, u32>),
    /// Start the device
    Start,
    /// Stop the device
    Stop(Rpc<(), ()>),
    /// Reset the device
    Reset(Rpc<(), ()>),
    /// Save the device state
    Save(Rpc<(), Result<SavedStateBlob, RemoteError>>),
    /// Restore the device state
    Restore(Rpc<SavedStateBlob, Result<(), RemoteError>>),
}

/// Responses sent from the remote device.
#[derive(MeshPayload)]
pub(crate) enum DeviceResponse {
    /// Response to a read operation.
    Read {
        /// ID number for the request
        id: usize,
        /// Data read from the device or an error.
        result: Result<Vec<u8>, IoError>,
    },
    /// Response to a write operation.
    Write {
        /// ID number for the request
        id: usize,
        /// Result of the write operation.
        result: Result<(), IoError>,
    },
}

/// Requests sent to the remote device for a read.
#[derive(MeshPayload)]
pub(crate) struct ReadRequest<T> {
    /// ID number for the request
    pub id: usize,
    /// Address to read from.
    pub address: T,
    /// Size of the read (1, 2, 4, or 8 bytes).
    pub size: usize,
}

/// Requests sent to the remote device for a write.
#[derive(MeshPayload)]
pub(crate) struct WriteRequest<T, V> {
    /// ID number for the request
    pub id: usize,
    /// Address to write to.
    pub address: T,
    /// Data to write.
    pub data: V,
}
