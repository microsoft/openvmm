// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! A worker for running ChipsetDevice implementations in a separate process.
//!
//! This worker provides process isolation for any device implementing the
//! ChipsetDevice trait. It handles serialization and deserialization of
//! device operations across process boundaries.

#![forbid(unsafe_code)]

use crate::DeviceRequest;
use crate::ReadResult;
use chipset_device::ChipsetDevice;
use chipset_device::io::IoResult;
use chipset_device_resources::ErasedChipsetDevice;
use chipset_device_resources::ResolveChipsetDeviceHandleParams;
use futures::FutureExt;
use mesh::MeshPayload;
use mesh_worker::Worker;
use mesh_worker::WorkerId;
use mesh_worker::WorkerRpc;
use pal_async::local::block_on;
use vm_resource::Resource;
use vm_resource::ResourceResolver;
use vm_resource::kind::ChipsetDeviceHandleKind;

/// Worker ID for ChipsetDevice workers.
pub const CHIPSET_DEVICE_WORKER_ID: WorkerId<ChipsetDeviceWorkerParameters> =
    WorkerId::new("ChipsetDeviceWorker");

/// Parameters for launching a chipset device worker.
#[derive(MeshPayload)]
pub struct ChipsetDeviceWorkerParameters {
    pub(crate) device: Resource<ChipsetDeviceHandleKind>,
    pub(crate) recv: mesh::Receiver<DeviceRequest>,
}

/// The chipset device worker.
///
/// This worker wraps any device implementing ChipsetDevice and handles
/// device operations sent via mesh channels.
pub struct ChipsetDeviceWorker {
    device: ErasedChipsetDevice,
    recv: mesh::Receiver<DeviceRequest>,
}

impl Worker for ChipsetDeviceWorker {
    type Parameters = ChipsetDeviceWorkerParameters;
    type State = ChipsetDeviceWorkerParameters;
    const ID: WorkerId<Self::Parameters> = CHIPSET_DEVICE_WORKER_ID;

    fn new(params: Self::Parameters) -> anyhow::Result<Self> {
        let ChipsetDeviceWorkerParameters { device, recv } = params;
        let resolver = ResourceResolver::new();
        // TODO: Add dynamic resolvers: GET, VMGS.

        let device = block_on(resolver.resolve(
            device,
            ResolveChipsetDeviceHandleParams {
                device_name: todo!(),
                guest_memory: todo!(),
                encrypted_guest_memory: todo!(),
                vmtime: todo!(),
                is_restoring: todo!(),
                configure: todo!(),
                task_driver_source: todo!(),
                register_mmio: todo!(),
                register_pio: todo!(),
            },
        ))?;

        Ok(Self {
            device: device.0,
            recv,
        })
    }

    fn restart(state: Self::State) -> anyhow::Result<Self> {
        Self::new(state)
    }

    fn run(mut self, mut rpc_recv: mesh::Receiver<WorkerRpc<Self::State>>) -> anyhow::Result<()> {
        block_on(async move {
            loop {
                enum WorkerEvent {
                    Rpc(WorkerRpc<ChipsetDeviceWorkerParameters>),
                    DeviceRequest(DeviceRequest),
                }

                let event = futures::select! { // merge semantics
                    r = rpc_recv.recv().fuse() => WorkerEvent::Rpc(r?),
                    r = self.recv.recv().fuse() => WorkerEvent::DeviceRequest(r?),
                };

                match event {
                    WorkerEvent::Rpc(rpc) => match rpc {
                        WorkerRpc::Stop => {
                            return Ok(());
                        }
                        WorkerRpc::Inspect(deferred) => {
                            deferred.inspect(&mut self.device);
                        }
                        WorkerRpc::Restart(response) => {
                            todo!();
                            // let state = ChipsetDeviceWorkerParameters {
                            //     device: self.device,
                            //     recv: self.recv,
                            // };
                            // response.complete(Ok(state));
                            // return Ok(());
                        }
                    },
                    WorkerEvent::DeviceRequest(req) => match req {
                        DeviceRequest::Inspect(deferred) => deferred.inspect(&mut self.device),
                        DeviceRequest::MmioRead(rpc) => {
                            rpc.handle_sync(|read_req| {
                                let mut data = vec![0; read_req.size as usize];
                                let result = self
                                    .device
                                    .supports_mmio()
                                    .unwrap()
                                    .mmio_read(read_req.address, &mut data);
                                io_result_to_read_result(result, data)
                            });
                        }
                        DeviceRequest::MmioWrite(rpc) => {
                            rpc.handle_sync(|write_req| {
                                self.device
                                    .supports_mmio()
                                    .unwrap()
                                    .mmio_write(write_req.address, &write_req.data)
                            });
                        }
                        DeviceRequest::PioRead(rpc) => {
                            rpc.handle_sync(|read_req| {
                                let mut data = vec![0; read_req.size as usize];
                                let result = self
                                    .device
                                    .supports_pio()
                                    .unwrap()
                                    .io_read(read_req.address, &mut data);
                                io_result_to_read_result(result, data)
                            });
                        }
                        DeviceRequest::PioWrite(rpc) => {
                            rpc.handle_sync(|write_req| {
                                self.device
                                    .supports_pio()
                                    .unwrap()
                                    .io_write(write_req.address, &write_req.data)
                            });
                        }
                        DeviceRequest::PciConfigRead(rpc) => {
                            rpc.handle_sync(|read_req| {
                                let mut data = 0;
                                let result = self
                                    .device
                                    .supports_pci()
                                    .unwrap()
                                    .pci_cfg_read(read_req.address, &mut data);
                                io_result_to_read_result(result, data)
                            });
                        }
                        DeviceRequest::PciConfigWrite(rpc) => {
                            rpc.handle_sync(|write_req| {
                                self.device
                                    .supports_pci()
                                    .unwrap()
                                    .pci_cfg_write(write_req.address, write_req.data)
                            });
                        }
                        DeviceRequest::Poll => {}
                    },
                }
            }
        })
    }
}

fn io_result_to_read_result<T>(result: IoResult, value: T) -> ReadResult<T> {
    match result {
        IoResult::Ok => ReadResult::Ok(value),
        IoResult::Err(err) => ReadResult::Err(err),
        IoResult::Defer(token) => ReadResult::Defer(token),
    }
}
