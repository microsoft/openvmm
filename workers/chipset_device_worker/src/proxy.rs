// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Client-side proxy for communicating with a ChipsetDevice worker.
//!
//! This module provides a [`ChipsetDeviceProxy`] that implements the
//! [`ChipsetDevice`] trait and forwards all operations over a channel.

use crate::DeviceRequest;
use crate::ReadRequest;
use crate::ReadResult;
use crate::WriteRequest;
use chipset_device::ChipsetDevice;
use chipset_device::io::IoResult;
use chipset_device::mmio::MmioIntercept;
use chipset_device::pci::PciConfigSpace;
use chipset_device::pio::PortIoIntercept;
use chipset_device::poll_device::PollDevice;
use inspect::InspectMut;
use mesh::rpc::Rpc;
use mesh::rpc::RpcError;
use mesh::rpc::RpcSend;
use mesh_worker::WorkerHandle;
use pal_async::local::block_on;
use std::task::Context;
use vmcore::device_state::ChangeDeviceState;
use vmcore::save_restore::NoSavedState;
use vmcore::save_restore::SaveRestore;

/// A proxy that forwards ChipsetDevice operations over a channel.
///
/// This type implements ChipsetDevice and can be used anywhere a device is
/// needed, while the actual device logic runs elsewhere.
#[derive(InspectMut)]
pub(crate) struct ChipsetDeviceProxy {
    #[inspect(send = "DeviceRequest::Inspect")]
    send: mesh::Sender<DeviceRequest>,
    worker: WorkerHandle,
    supports_mmio: bool,
    supports_pio: bool,
    supports_pci: bool,
    supports_poll: bool,
}

impl ChipsetDeviceProxy {
    /// Create a new ChipsetDeviceProxy.
    pub(crate) fn new(send: mesh::Sender<DeviceRequest>, worker: WorkerHandle) -> Self {
        Self {
            send,
            worker,
            // TODO: Query the actual device capabilities from the worker.
            supports_mmio: true,
            supports_pio: true,
            supports_pci: true,
            supports_poll: true,
        }
    }

    fn do_read<A, V: Send + 'static>(
        &mut self,
        req_ctor: fn(Rpc<ReadRequest<A>, ReadResult<V>>) -> DeviceRequest,
        req: ReadRequest<A>,
        copier: impl FnOnce(V),
    ) -> IoResult {
        let pending = self.send.call(req_ctor, req);
        let result = block_on(pending);
        match result {
            Ok(ReadResult::Ok(value)) => {
                copier(value);
                IoResult::Ok
            }
            Ok(ReadResult::Err(e)) => IoResult::Err(e),
            Ok(ReadResult::Defer(token)) => IoResult::Defer(token),
            Err(RpcError::Channel(e)) => panic!("proxied device channel error: {:?}", e),
        }
    }

    fn do_write<A, V: Send + 'static>(
        &mut self,
        req_ctor: fn(Rpc<WriteRequest<A, V>, IoResult>) -> DeviceRequest,
        req: WriteRequest<A, V>,
    ) -> IoResult {
        let pending = self.send.call(req_ctor, req);
        let result = block_on(pending);
        match result {
            Ok(r) => r,
            Err(RpcError::Channel(e)) => panic!("proxied device channel error: {:?}", e),
        }
    }
}

impl ChipsetDevice for ChipsetDeviceProxy {
    fn supports_mmio(&mut self) -> Option<&mut dyn MmioIntercept> {
        self.supports_mmio.then_some(self)
    }

    fn supports_pio(&mut self) -> Option<&mut dyn PortIoIntercept> {
        self.supports_pio.then_some(self)
    }

    fn supports_pci(&mut self) -> Option<&mut dyn PciConfigSpace> {
        self.supports_pci.then_some(self)
    }

    fn supports_poll_device(&mut self) -> Option<&mut dyn PollDevice> {
        self.supports_poll.then_some(self)
    }
}

impl MmioIntercept for ChipsetDeviceProxy {
    fn mmio_read(&mut self, address: u64, data: &mut [u8]) -> IoResult {
        self.do_read(
            DeviceRequest::MmioRead,
            ReadRequest {
                address,
                size: data.len() as u8,
            },
            |value| data.copy_from_slice(&value),
        )
    }

    fn mmio_write(&mut self, address: u64, data: &[u8]) -> IoResult {
        self.do_write(
            DeviceRequest::MmioWrite,
            WriteRequest {
                address,
                data: data.to_vec(),
            },
        )
    }
}

impl PortIoIntercept for ChipsetDeviceProxy {
    fn io_read(&mut self, port: u16, data: &mut [u8]) -> IoResult {
        self.do_read(
            DeviceRequest::PioRead,
            ReadRequest {
                address: port,
                size: data.len() as u8,
            },
            |value| data.copy_from_slice(&value),
        )
    }

    fn io_write(&mut self, port: u16, data: &[u8]) -> IoResult {
        self.do_write(
            DeviceRequest::PioWrite,
            WriteRequest {
                address: port,
                data: data.to_vec(),
            },
        )
    }
}

impl PciConfigSpace for ChipsetDeviceProxy {
    fn pci_cfg_read(&mut self, offset: u16, value: &mut u32) -> IoResult {
        self.do_read(
            DeviceRequest::PciConfigRead,
            ReadRequest {
                address: offset,
                size: 4,
            },
            |v| *value = v,
        )
    }

    fn pci_cfg_write(&mut self, offset: u16, value: u32) -> IoResult {
        self.do_write(
            DeviceRequest::PciConfigWrite,
            WriteRequest {
                address: offset,
                data: value,
            },
        )
    }
}

impl PollDevice for ChipsetDeviceProxy {
    fn poll_device(&mut self, _cx: &mut Context<'_>) {
        todo!()
    }
}

impl ChangeDeviceState for ChipsetDeviceProxy {
    fn start(&mut self) {
        todo!()
    }

    async fn stop(&mut self) {
        todo!()
    }

    async fn reset(&mut self) {
        todo!()
    }
}

impl SaveRestore for ChipsetDeviceProxy {
    type SavedState = NoSavedState;

    fn save(&mut self) -> Result<Self::SavedState, vmcore::save_restore::SaveError> {
        todo!()
    }

    fn restore(
        &mut self,
        state: Self::SavedState,
    ) -> Result<(), vmcore::save_restore::RestoreError> {
        todo!()
    }
}
