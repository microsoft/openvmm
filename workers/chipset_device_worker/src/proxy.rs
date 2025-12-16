// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Client-side proxy for communicating with a remote ChipsetDevice.
//!
//! This module provides a [`ChipsetDeviceProxy`] that implements the
//! [`ChipsetDevice`] trait and forwards all operations over a channel.

use crate::guestmem::GuestMemoryProxy;
use crate::protocol::*;
use anyhow::Context;
use chipset_device::ChipsetDevice;
use chipset_device::io::IoError;
use chipset_device::io::IoResult;
use chipset_device::io::deferred::DeferredRead;
use chipset_device::io::deferred::DeferredWrite;
use chipset_device::io::deferred::defer_read;
use chipset_device::io::deferred::defer_write;
use chipset_device::mmio::MmioIntercept;
use chipset_device::pci::PciConfigSpace;
use chipset_device::pio::PortIoIntercept;
use chipset_device::poll_device::PollDevice;
use chipset_device_resources::ResolveChipsetDeviceHandleParams;
use futures::StreamExt;
use inspect::Inspect;
use inspect::InspectMut;
use mesh::rpc::RpcSend;
use mesh_worker::WorkerHandle;
use pal_async::local::block_on;
use slab::Slab;
use std::task::Poll;
use vmcore::device_state::ChangeDeviceState;
use vmcore::save_restore::ProtobufSaveRestore;
use vmcore::save_restore::RestoreError;
use vmcore::save_restore::SaveError;
use vmcore::save_restore::SavedStateBlob;

/// A proxy that forwards ChipsetDevice operations over a channel.
///
/// This type implements ChipsetDevice and can be used anywhere a device is
/// needed, while the actual device logic runs elsewhere.
#[derive(InspectMut)]
pub(crate) struct ChipsetDeviceProxy {
    #[inspect(skip)]
    req_send: RemoteDevice,
    #[inspect(skip)]
    resp_recv: mesh::Receiver<DeviceResponse>,
    worker: WorkerHandle,

    #[inspect(with = "Slab::len")]
    in_flight_reads: Slab<DeferredRead>,
    #[inspect(with = "Slab::len")]
    in_flight_writes: Slab<DeferredWrite>,

    #[inspect(skip)]
    gm_proxy: GuestMemoryProxy,
    #[inspect(skip)]
    enc_gm_proxy: GuestMemoryProxy,

    mmio: Option<MmioProxy>,
    pio: Option<PioProxy>,
    pci: Option<PciProxy>,
}

enum RemoteDevice {
    Present(mesh::Sender<DeviceRequest>),
    Failed,
}

#[derive(Inspect)]
struct MmioProxy {
    #[inspect(iter_by_index)]
    static_regions: Vec<Box<dyn chipset_device::mmio::ControlMmioIntercept>>,
}

#[derive(Inspect)]
struct PioProxy {
    #[inspect(iter_by_index)]
    static_regions: Vec<Box<dyn chipset_device::pio::ControlPortIoIntercept>>,
}

#[derive(Inspect)]
struct PciProxy {
    #[inspect(with = "Option::is_some")]
    suggested_bdf: Option<(u8, u8, u8)>,
}

impl ChipsetDeviceProxy {
    /// Create a new ChipsetDeviceProxy.
    pub(crate) async fn new(
        req_send: mesh::Sender<DeviceRequest>,
        resp_recv: mesh::Receiver<DeviceResponse>,
        cap_recv: mesh::OneshotReceiver<DeviceInit>,
        worker: WorkerHandle,
        gm_proxy: GuestMemoryProxy,
        enc_gm_proxy: GuestMemoryProxy,
        input: ResolveChipsetDeviceHandleParams<'_>,
    ) -> anyhow::Result<Self> {
        let DeviceInit { mmio, pio, pci } = cap_recv
            .await
            .context("failed receiving device capabilities")?;

        let mmio = mmio.map(|MmioInit { static_regions }| MmioProxy {
            static_regions: static_regions
                .into_iter()
                .map(|r| {
                    let mut region = input.register_mmio.new_io_region(&r.0, r.2 - r.1 + 1);
                    region.map(r.1);
                    region
                })
                .collect(),
        });

        let pio = pio.map(|PioInit { static_regions }| PioProxy {
            static_regions: static_regions
                .into_iter()
                .map(|r| {
                    let mut region = input.register_pio.new_io_region(&r.0, r.2 - r.1 + 1);
                    region.map(r.1);
                    region
                })
                .collect(),
        });

        let pci = pci.map(|PciInit { suggested_bdf }| PciProxy { suggested_bdf });

        Ok(Self {
            req_send: RemoteDevice::Present(req_send),
            resp_recv,
            worker,
            in_flight_reads: Slab::new(),
            in_flight_writes: Slab::new(),
            mmio,
            pio,
            pci,
            gm_proxy,
            enc_gm_proxy,
        })
    }
}

impl ChipsetDevice for ChipsetDeviceProxy {
    fn supports_mmio(&mut self) -> Option<&mut dyn MmioIntercept> {
        self.mmio.is_some().then_some(self)
    }

    fn supports_pio(&mut self) -> Option<&mut dyn PortIoIntercept> {
        self.pio.is_some().then_some(self)
    }

    fn supports_pci(&mut self) -> Option<&mut dyn PciConfigSpace> {
        self.pci.is_some().then_some(self)
    }

    fn supports_poll_device(&mut self) -> Option<&mut dyn PollDevice> {
        Some(self)
    }
}

impl MmioIntercept for ChipsetDeviceProxy {
    fn mmio_read(&mut self, address: u64, data: &mut [u8]) -> IoResult {
        match &self.req_send {
            RemoteDevice::Present(req_send) => {
                let (read, token) = defer_read();
                let id = self.in_flight_reads.insert(read);
                req_send.send(DeviceRequest::MmioRead(ReadRequest {
                    id,
                    address,
                    size: data.len(),
                }));
                IoResult::Defer(token)
            }
            RemoteDevice::Failed => IoResult::Err(IoError::NoResponse),
        }
    }

    fn mmio_write(&mut self, address: u64, data: &[u8]) -> IoResult {
        match &self.req_send {
            RemoteDevice::Present(req_send) => {
                let (write, token) = defer_write();
                let id = self.in_flight_writes.insert(write);
                req_send.send(DeviceRequest::MmioWrite(WriteRequest {
                    id,
                    address,
                    data: data.to_vec(),
                }));
                IoResult::Defer(token)
            }
            RemoteDevice::Failed => IoResult::Err(IoError::NoResponse),
        }
    }
}

impl PortIoIntercept for ChipsetDeviceProxy {
    fn io_read(&mut self, port: u16, data: &mut [u8]) -> IoResult {
        match &self.req_send {
            RemoteDevice::Present(req_send) => {
                let (read, token) = defer_read();
                let id = self.in_flight_reads.insert(read);
                req_send.send(DeviceRequest::PioRead(ReadRequest {
                    id,
                    address: port,
                    size: data.len(),
                }));
                IoResult::Defer(token)
            }
            RemoteDevice::Failed => IoResult::Err(IoError::NoResponse),
        }
    }

    fn io_write(&mut self, port: u16, data: &[u8]) -> IoResult {
        match &self.req_send {
            RemoteDevice::Present(req_send) => {
                let (write, token) = defer_write();
                let id = self.in_flight_writes.insert(write);
                req_send.send(DeviceRequest::PioWrite(WriteRequest {
                    id,
                    address: port,
                    data: data.to_vec(),
                }));
                IoResult::Defer(token)
            }
            RemoteDevice::Failed => IoResult::Err(IoError::NoResponse),
        }
    }
}

impl PciConfigSpace for ChipsetDeviceProxy {
    fn pci_cfg_read(&mut self, offset: u16, _value: &mut u32) -> IoResult {
        match &self.req_send {
            RemoteDevice::Present(req_send) => {
                let (read, token) = defer_read();
                let id = self.in_flight_reads.insert(read);
                req_send.send(DeviceRequest::PciConfigRead(ReadRequest {
                    id,
                    address: offset,
                    size: 4,
                }));
                IoResult::Defer(token)
            }
            RemoteDevice::Failed => IoResult::Err(IoError::NoResponse),
        }
    }

    fn pci_cfg_write(&mut self, offset: u16, value: u32) -> IoResult {
        match &self.req_send {
            RemoteDevice::Present(req_send) => {
                let (write, token) = defer_write();
                let id = self.in_flight_writes.insert(write);
                req_send.send(DeviceRequest::PciConfigWrite(WriteRequest {
                    id,
                    address: offset,
                    data: value,
                }));
                IoResult::Defer(token)
            }
            RemoteDevice::Failed => IoResult::Err(IoError::NoResponse),
        }
    }

    fn suggested_bdf(&mut self) -> Option<(u8, u8, u8)> {
        self.pci.as_ref().unwrap().suggested_bdf
    }
}

impl PollDevice for ChipsetDeviceProxy {
    fn poll_device(&mut self, cx: &mut std::task::Context<'_>) {
        self.gm_proxy.poll(cx);
        self.enc_gm_proxy.poll(cx);

        while let Poll::Ready(resp) = self.resp_recv.poll_next_unpin(cx) {
            match resp {
                Some(DeviceResponse::Read { id, result }) => {
                    let deferred_read = self.in_flight_reads.remove(id);
                    match result {
                        Ok(data) => deferred_read.complete(&data),
                        Err(e) => deferred_read.complete_error(e),
                    }
                }
                Some(DeviceResponse::Write { id, result }) => {
                    let deferred_write = self.in_flight_writes.remove(id);
                    match result {
                        Ok(()) => deferred_write.complete(),
                        Err(e) => deferred_write.complete_error(e),
                    }
                }
                None => {
                    // The remote device has closed the channel, fail all in-flight
                    // requests and prevent any new ones.
                    for deferred_read in self.in_flight_reads.drain() {
                        deferred_read.complete_error(IoError::NoResponse);
                    }
                    for deferred_write in self.in_flight_writes.drain() {
                        deferred_write.complete_error(IoError::NoResponse);
                    }
                    self.req_send = RemoteDevice::Failed;
                }
            }
        }
    }
}

// TODO: Figure out what to do on errors for all the below.
impl ChangeDeviceState for ChipsetDeviceProxy {
    fn start(&mut self) {
        match &self.req_send {
            RemoteDevice::Present(req_send) => req_send.send(DeviceRequest::Start),
            RemoteDevice::Failed => todo!(),
        }
    }

    async fn stop(&mut self) {
        match &self.req_send {
            RemoteDevice::Present(req_send) => {
                req_send.call(DeviceRequest::Stop, ()).await.unwrap()
            }
            RemoteDevice::Failed => todo!(),
        }
    }

    async fn reset(&mut self) {
        self.in_flight_reads.clear();
        self.in_flight_writes.clear();
        match &self.req_send {
            RemoteDevice::Present(req_send) => {
                req_send.call(DeviceRequest::Reset, ()).await.unwrap()
            }
            RemoteDevice::Failed => todo!(),
        }
    }
}

// TODO: Remove block_on calls for the below
impl ProtobufSaveRestore for ChipsetDeviceProxy {
    fn save(&mut self) -> Result<SavedStateBlob, SaveError> {
        // TODO: Do we need to include any state from ourselves?
        match &self.req_send {
            RemoteDevice::Present(req_send) => block_on(req_send.call(DeviceRequest::Save, ()))
                .map_err(|e| SaveError::Other(anyhow::anyhow!(e)))?
                .map_err(|e| SaveError::Other(anyhow::anyhow!(e))),
            RemoteDevice::Failed => Err(SaveError::Other(anyhow::anyhow!(
                "remote device not available"
            ))),
        }
    }

    fn restore(&mut self, state: SavedStateBlob) -> Result<(), RestoreError> {
        match &self.req_send {
            RemoteDevice::Present(req_send) => {
                block_on(req_send.call(DeviceRequest::Restore, state))
                    .map_err(|e| RestoreError::Other(anyhow::anyhow!(e)))?
                    .map_err(|e| RestoreError::Other(anyhow::anyhow!(e)))
            }
            RemoteDevice::Failed => Err(RestoreError::Other(anyhow::anyhow!(
                "remote device not available"
            ))),
        }
    }
}
