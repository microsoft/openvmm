// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Definitions for the chipset device worker.
//!
//! This worker enables running any ChipsetDevice implementation in a separate
//! process for isolation purposes.

#![forbid(unsafe_code)]

use chipset_device::io::IoError;
use chipset_device::io::IoResult;
use chipset_device::io::deferred;
use mesh::MeshPayload;
use mesh::rpc::Rpc;
use mesh_worker::WorkerHost;
use vm_resource::Resource;
use vm_resource::ResourceId;
use vm_resource::kind::ChipsetDeviceHandleKind;

/// The proxy for communicating with a chipset device worker.
mod proxy;
/// The resolver for chipset device workers.
pub mod resolver;
/// The worker implementation.
pub mod worker;

/// A handle to a construct a chipset device in a separate worker process.
#[derive(MeshPayload)]
pub struct ChipsetDeviceWorkerHandle {
    /// The device to run in the worker.
    pub device: Resource<ChipsetDeviceHandleKind>,
    /// The worker host to launch the worker in.
    pub worker_host: WorkerHost,
}

impl ResourceId<ChipsetDeviceHandleKind> for ChipsetDeviceWorkerHandle {
    const ID: &'static str = "ChipsetDeviceWorkerHandle";
}

/// Requests sent to the device worker.
#[derive(Debug, MeshPayload)]
enum DeviceRequest {
    /// Inspect the device.
    Inspect(inspect::Deferred),
    /// Perform a MMIO read operation.
    MmioRead(Rpc<ReadRequest<u64>, ReadResult<Vec<u8>>>),
    /// Perform a MMIO write operation.
    MmioWrite(Rpc<WriteRequest<u64, Vec<u8>>, IoResult>),
    /// Perform a PIO read operation.
    PioRead(Rpc<ReadRequest<u16>, ReadResult<Vec<u8>>>),
    /// Perform a PIO write operation.
    PioWrite(Rpc<WriteRequest<u16, Vec<u8>>, IoResult>),
    /// Perform a PCI config space read.
    PciConfigRead(Rpc<ReadRequest<u16>, ReadResult<u32>>),
    /// Perform a PCI config space write.
    PciConfigWrite(Rpc<WriteRequest<u16, u32>, IoResult>),
    /// Poll the device for asynchronous work.
    Poll,
}

/// Requests sent to the device worker for a read.
#[derive(Debug, MeshPayload)]
struct ReadRequest<T> {
    /// Address to read from.
    pub address: T,
    /// Size of the read (1, 2, 4, or 8 bytes).
    pub size: u8,
}

/// Responses from the device worker for a read.
#[derive(Debug, MeshPayload)]
enum ReadResult<T> {
    /// The read operation succeeded.
    Ok(T),
    /// The read operation failed due to an access error.
    Err(IoError),
    /// Defer this request until [`deferred::DeferredRead::complete`] is called.
    Defer(deferred::DeferredToken),
}

/// Requests sent to the device worker for a write.
#[derive(Debug, MeshPayload)]
struct WriteRequest<T, V> {
    /// Address to write to.
    pub address: T,
    /// Data to write.
    pub data: V,
}
