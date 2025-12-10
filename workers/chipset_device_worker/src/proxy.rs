// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Client-side proxy for communicating with a remote ChipsetDevice.
//!
//! This module provides a [`ChipsetDeviceProxy`] that implements the
//! [`ChipsetDevice`] trait and forwards all operations over a channel.

use crate::DeviceInit;
use crate::DeviceRequest;
use crate::DeviceResponse;
use crate::MmioInit;
use crate::PciInit;
use crate::PioInit;
use crate::ReadRequest;
use crate::WriteRequest;
use crate::guestmem::GuestMemoryProxy;
use anyhow::Context;
use chipset_device::ChipsetDevice;
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
    req_send: mesh::Sender<DeviceRequest>,
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
            req_send,
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
        let (read, token) = defer_read();
        let id = self.in_flight_reads.insert(read);
        self.req_send.send(DeviceRequest::MmioRead(ReadRequest {
            id,
            address,
            size: data.len(),
        }));
        IoResult::Defer(token)
    }

    fn mmio_write(&mut self, address: u64, data: &[u8]) -> IoResult {
        let (write, token) = defer_write();
        let id = self.in_flight_writes.insert(write);
        self.req_send.send(DeviceRequest::MmioWrite(WriteRequest {
            id,
            address,
            data: data.to_vec(),
        }));
        IoResult::Defer(token)
    }
}

impl PortIoIntercept for ChipsetDeviceProxy {
    fn io_read(&mut self, port: u16, data: &mut [u8]) -> IoResult {
        let (read, token) = defer_read();
        let id = self.in_flight_reads.insert(read);
        self.req_send.send(DeviceRequest::PioRead(ReadRequest {
            id,
            address: port,
            size: data.len(),
        }));
        IoResult::Defer(token)
    }

    fn io_write(&mut self, port: u16, data: &[u8]) -> IoResult {
        let (write, token) = defer_write();
        let id = self.in_flight_writes.insert(write);
        self.req_send.send(DeviceRequest::PioWrite(WriteRequest {
            id,
            address: port,
            data: data.to_vec(),
        }));
        IoResult::Defer(token)
    }
}

impl PciConfigSpace for ChipsetDeviceProxy {
    fn pci_cfg_read(&mut self, offset: u16, _value: &mut u32) -> IoResult {
        let (read, token) = defer_read();
        let id = self.in_flight_reads.insert(read);
        self.req_send
            .send(DeviceRequest::PciConfigRead(ReadRequest {
                id,
                address: offset,
                size: 4,
            }));
        IoResult::Defer(token)
    }

    fn pci_cfg_write(&mut self, offset: u16, value: u32) -> IoResult {
        let (write, token) = defer_write();
        let id = self.in_flight_writes.insert(write);
        self.req_send
            .send(DeviceRequest::PciConfigWrite(WriteRequest {
                id,
                address: offset,
                data: value,
            }));
        IoResult::Defer(token)
    }

    fn suggested_bdf(&mut self) -> Option<(u8, u8, u8)> {
        self.pci.as_ref().unwrap().suggested_bdf
    }
}

impl PollDevice for ChipsetDeviceProxy {
    fn poll_device(&mut self, cx: &mut std::task::Context<'_>) {
        self.gm_proxy.poll(cx);
        self.enc_gm_proxy.poll(cx);

        while let Poll::Ready(resp) = self.resp_recv.poll_recv(cx) {
            // TODO: What should we do on error here?
            match resp.unwrap() {
                DeviceResponse::Read { id, result } => {
                    let deferred_read = self.in_flight_reads.remove(id);
                    match result {
                        Ok(data) => deferred_read.complete(&data),
                        Err(e) => deferred_read.complete_error(e),
                    }
                }
                DeviceResponse::Write { id, result } => {
                    let deferred_write = self.in_flight_writes.remove(id);
                    match result {
                        Ok(()) => deferred_write.complete(),
                        Err(e) => deferred_write.complete_error(e),
                    }
                }
            }
        }
    }
}

impl ChangeDeviceState for ChipsetDeviceProxy {
    fn start(&mut self) {
        self.req_send.send(DeviceRequest::Start)
    }

    async fn stop(&mut self) {
        // TODO: Handle errors?
        self.req_send.call(DeviceRequest::Stop, ()).await.unwrap()
    }

    async fn reset(&mut self) {
        self.in_flight_reads.clear();
        self.in_flight_writes.clear();
        // TODO: Handle errors?
        self.req_send.call(DeviceRequest::Reset, ()).await.unwrap()
    }
}

impl ProtobufSaveRestore for ChipsetDeviceProxy {
    fn save(&mut self) -> Result<SavedStateBlob, SaveError> {
        block_on(async {
            self.req_send
                .call(DeviceRequest::Save, ())
                .await
                // TODO: Handle errors?
                .unwrap()
                .map_err(|e| SaveError::Other(anyhow::anyhow!(e)))
        })
    }

    fn restore(&mut self, state: SavedStateBlob) -> Result<(), RestoreError> {
        block_on(async {
            self.req_send
                .call(DeviceRequest::Restore, state)
                .await
                // TODO: Handle errors?
                .unwrap()
                .map_err(|e| RestoreError::Other(anyhow::anyhow!(e)))
        })
    }
}
