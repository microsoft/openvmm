// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! A worker for running ChipsetDevice implementations in a separate process.
//!
//! This worker provides process isolation for any device implementing the
//! ChipsetDevice trait. It handles serialization and deserialization of
//! device operations across process boundaries.

#![forbid(unsafe_code)]

mod configure;

use crate::DeviceInit;
use crate::DeviceRequest;
use crate::DeviceResponse;
use crate::MmioInit;
use crate::PciInit;
use crate::PioInit;
use crate::ReadRequest;
use crate::WriteRequest;
use crate::guestmem::GuestMemoryRemoteBuilder;
use anyhow::Context;
use chipset_device::ChipsetDevice;
use chipset_device::io::IoResult;
use chipset_device::io::deferred::DeferredToken;
use chipset_device_resources::ErasedChipsetDevice;
use chipset_device_resources::ResolveChipsetDeviceHandleParams;
use mesh::MeshPayload;
use mesh::error::RemoteError;
use mesh_worker::Worker;
use mesh_worker::WorkerId;
use mesh_worker::WorkerRpc;
use pal_async::DefaultPool;
use std::task::Poll;
use vm_resource::Resource;
use vm_resource::ResourceResolver;
use vm_resource::kind::ChipsetDeviceHandleKind;
use vmcore::device_state::ChangeDeviceState;
use vmcore::save_restore::ProtobufSaveRestore;

/// Worker ID for ChipsetDevice workers.
pub const REMOTE_CHIPSET_DEVICE_WORKER_ID: WorkerId<RemoteChipsetDeviceWorkerParameters> =
    WorkerId::new("ChipsetDeviceWorker");

/// Parameters for launching a remote chipset device worker.
#[derive(MeshPayload)]
pub struct RemoteChipsetDeviceWorkerParameters {
    pub(crate) device: Resource<ChipsetDeviceHandleKind>,
    pub(crate) dyn_resolvers: RemoteDynamicResolvers,
    pub(crate) inputs: RemoteChipsetDeviceHandleParams,

    pub(crate) req_recv: mesh::Receiver<DeviceRequest>,
    pub(crate) resp_send: mesh::Sender<DeviceResponse>,
    pub(crate) cap_send: mesh::OneshotSender<DeviceInit>,
}

// FUTURE: Create a way to store a Vec of all registered dynamic resolvers
// and transfer them, instead of maintaining a list of just a few.
// TODO: This should be passed in from the hosting VMM, since something like the
// GET doesn't make sense for openvmm.
#[derive(MeshPayload)]
pub(crate) struct RemoteDynamicResolvers {
    #[cfg(target_os = "linux")]
    pub get: Option<guest_emulation_transport::GuestEmulationTransportClient>,
    pub vmgs: Option<vmgs_broker::VmgsClient>,
}

#[derive(MeshPayload)]
pub(crate) struct RemoteChipsetDeviceHandleParams {
    pub device_name: String,
    pub is_restoring: bool,
    pub vmtime: vmcore::vmtime::VmTimeSourceBuilder,
    pub guest_memory: GuestMemoryRemoteBuilder,
    pub encrypted_guest_memory: GuestMemoryRemoteBuilder,
}

/// The chipset device worker.
///
/// This worker wraps any device implementing ChipsetDevice and handles
/// device operations sent via mesh channels.
pub struct RemoteChipsetDeviceWorker {
    device: ErasedChipsetDevice,
    pool: Option<DefaultPool>,
    req_recv: mesh::Receiver<DeviceRequest>,
    resp_send: mesh::Sender<DeviceResponse>,
    deferred_reads: Vec<DeferredRead>,
    deferred_writes: Vec<DeferredWrite>,
}

struct DeferredRead {
    id: usize,
    token: DeferredToken,
    size: usize,
}

struct DeferredWrite {
    id: usize,
    token: DeferredToken,
}

impl Worker for RemoteChipsetDeviceWorker {
    type Parameters = RemoteChipsetDeviceWorkerParameters;
    type State = ();
    const ID: WorkerId<Self::Parameters> = REMOTE_CHIPSET_DEVICE_WORKER_ID;

    fn new(params: Self::Parameters) -> anyhow::Result<Self> {
        let mut pool = DefaultPool::new();

        let RemoteChipsetDeviceWorkerParameters {
            device,
            dyn_resolvers,
            inputs,

            req_recv,
            resp_send,
            cap_send,
        } = params;

        let mut resolver = ResourceResolver::new();
        #[cfg(target_os = "linux")]
        if let Some(get) = dyn_resolvers.get {
            resolver.add_resolver(get);
        }
        if let Some(vmgs) = dyn_resolvers.vmgs {
            resolver.add_resolver(vmgs);
        }

        let driver = pool.driver();
        let mut device = pool
            .run_until(async move {
                resolver
                    .resolve(
                        device,
                        ResolveChipsetDeviceHandleParams {
                            device_name: &inputs.device_name,
                            guest_memory: &inputs.guest_memory.build("remote_gm"),
                            encrypted_guest_memory: &inputs
                                .encrypted_guest_memory
                                .build("remote_enc_gm"),
                            vmtime: &inputs
                                .vmtime
                                .build(&driver)
                                .await
                                .context("failed to build vmtime source")?,
                            is_restoring: inputs.is_restoring,
                            task_driver_source: &vmcore::vm_task::VmTaskDriverSource::new(
                                vmcore::vm_task::thread::ThreadDriverBackend::new(driver),
                            ),
                            // TODO: Actually wire these up
                            configure: &mut configure::RemoteConfigureChipsetDevice {},
                            register_mmio: &mut configure::RemoteRegisterMmio {},
                            register_pio: &mut configure::RemoteRegisterPio {},
                        },
                    )
                    .await
                    .context("failed to resolve device")
            })?
            .0;

        if device.supports_acknowledge_pic_interrupt().is_some()
            || device.supports_handle_eoi().is_some()
            || device.supports_line_interrupt_target().is_some()
        {
            anyhow::bail!("remote device requires unimplemented functionality");
        }

        cap_send.send(DeviceInit {
            mmio: device.supports_mmio().map(|m| MmioInit {
                static_regions: m
                    .get_static_regions()
                    .iter()
                    .map(|(n, r)| ((*n).into(), *r.start(), *r.end()))
                    .collect(),
            }),
            pio: device.supports_pio().map(|p| PioInit {
                static_regions: p
                    .get_static_regions()
                    .iter()
                    .map(|(n, r)| ((*n).into(), *r.start(), *r.end()))
                    .collect(),
            }),
            pci: device.supports_pci().map(|p| PciInit {
                suggested_bdf: p.suggested_bdf(),
            }),
        });

        Ok(Self {
            device,
            pool: Some(pool),
            req_recv,
            resp_send,
            deferred_reads: Vec::new(),
            deferred_writes: Vec::new(),
        })
    }

    fn restart(_state: Self::State) -> anyhow::Result<Self> {
        todo!()
    }

    fn run(mut self, mut rpc_recv: mesh::Receiver<WorkerRpc<Self::State>>) -> anyhow::Result<()> {
        self.pool.take().unwrap().run_until(async move {
            loop {
                enum WorkerEvent {
                    Rpc(WorkerRpc<<RemoteChipsetDeviceWorker as Worker>::State>),
                    DeviceRequest(DeviceRequest),
                }

                let event = std::future::poll_fn(|cx| {
                    if let Some(poll_device) = self.device.supports_poll_device() {
                        poll_device.poll_device(cx);
                    }

                    self.deferred_reads
                        .extract_if(.., |read| {
                            let mut data = vec![0; read.size];
                            match read.token.poll_read(cx, &mut data) {
                                Poll::Ready(r) => {
                                    self.resp_send.send(DeviceResponse::Read {
                                        id: read.id,
                                        result: r.map(|_| data),
                                    });
                                    true
                                }
                                Poll::Pending => false,
                            }
                        })
                        .for_each(|_| ());

                    self.deferred_writes
                        .extract_if(.., |write| match write.token.poll_write(cx) {
                            Poll::Ready(r) => {
                                self.resp_send.send(DeviceResponse::Write {
                                    id: write.id,
                                    result: r,
                                });
                                true
                            }
                            Poll::Pending => false,
                        })
                        .for_each(|_| ());

                    if let Poll::Ready(r) = rpc_recv.poll_recv(cx) {
                        return Poll::Ready(r.map(WorkerEvent::Rpc));
                    }
                    if let Poll::Ready(r) = self.req_recv.poll_recv(cx) {
                        return Poll::Ready(r.map(WorkerEvent::DeviceRequest));
                    }
                    Poll::Pending
                })
                .await?;

                match event {
                    WorkerEvent::Rpc(rpc) => match rpc {
                        WorkerRpc::Inspect(deferred) => {
                            deferred.inspect(&mut self.device);
                        }
                        WorkerRpc::Stop => {
                            return Ok(());
                        }
                        WorkerRpc::Restart(rpc) => {
                            rpc.complete(Err(RemoteError::new(anyhow::anyhow!("not supported"))));
                        }
                    },
                    WorkerEvent::DeviceRequest(req) => match req {
                        DeviceRequest::Start => self.device.start(),
                        DeviceRequest::Stop(rpc) => {
                            rpc.handle(async |()| self.device.stop().await).await
                        }
                        DeviceRequest::Reset(rpc) => {
                            self.deferred_reads.clear();
                            self.deferred_writes.clear();
                            rpc.handle(async |()| self.device.reset().await).await
                        }
                        DeviceRequest::MmioRead(ReadRequest { id, address, size }) => {
                            let mut data = vec![0; size];
                            let result = self
                                .device
                                .supports_mmio()
                                .unwrap()
                                .mmio_read(address, &mut data);
                            self.handle_read_result(id, result, data);
                        }
                        DeviceRequest::MmioWrite(WriteRequest { id, address, data }) => {
                            let result = self
                                .device
                                .supports_mmio()
                                .unwrap()
                                .mmio_write(address, &data);
                            self.handle_write_result(id, result);
                        }
                        DeviceRequest::PioRead(ReadRequest { id, address, size }) => {
                            let mut data = vec![0; size];
                            let result = self
                                .device
                                .supports_pio()
                                .unwrap()
                                .io_read(address, &mut data);
                            self.handle_read_result(id, result, data);
                        }
                        DeviceRequest::PioWrite(WriteRequest { id, address, data }) => {
                            let result =
                                self.device.supports_pio().unwrap().io_write(address, &data);
                            self.handle_write_result(id, result);
                        }
                        DeviceRequest::PciConfigRead(ReadRequest { id, address, size }) => {
                            assert_eq!(size, 4);
                            let mut data = 0;
                            let result = self
                                .device
                                .supports_pci()
                                .unwrap()
                                .pci_cfg_read(address, &mut data);
                            self.handle_read_result(id, result, data.to_ne_bytes().to_vec());
                        }
                        DeviceRequest::PciConfigWrite(WriteRequest { id, address, data }) => {
                            let result = self
                                .device
                                .supports_pci()
                                .unwrap()
                                .pci_cfg_write(address, data);
                            self.handle_write_result(id, result);
                        }
                        DeviceRequest::Save(rpc) => {
                            rpc.handle_failable_sync(|()| self.device.save())
                        }
                        DeviceRequest::Restore(rpc) => {
                            rpc.handle_failable_sync(|state| self.device.restore(state))
                        }
                    },
                }
            }
        })
    }
}

impl RemoteChipsetDeviceWorker {
    fn handle_read_result(&mut self, id: usize, result: IoResult, data: Vec<u8>) {
        match result {
            IoResult::Ok => self.resp_send.send(DeviceResponse::Read {
                id,
                result: Ok(data),
            }),
            IoResult::Err(io_error) => self.resp_send.send(DeviceResponse::Read {
                id,
                result: Err(io_error),
            }),
            IoResult::Defer(token) => self.deferred_reads.push(DeferredRead {
                id,
                token,
                size: data.len(),
            }),
        }
    }

    fn handle_write_result(&mut self, id: usize, result: IoResult) {
        match result {
            IoResult::Ok => self
                .resp_send
                .send(DeviceResponse::Write { id, result: Ok(()) }),
            IoResult::Err(io_error) => self.resp_send.send(DeviceResponse::Write {
                id,
                result: Err(io_error),
            }),
            IoResult::Defer(token) => self.deferred_writes.push(DeferredWrite { id, token }),
        }
    }
}
