// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Client-side proxy for communicating with a remote ChipsetDevice.
//!
//! This module provides a [`ChipsetDeviceProxy`] that implements the
//! [`ChipsetDevice`] trait and forwards all operations over a channel.

use crate::guestmem::GuestMemoryProxy;
use crate::protocol::*;
use anyhow::Context;
use async_trait::async_trait;
use chipset_device::ChipsetDevice;
use chipset_device::io::IoResult;
use chipset_device::io::deferred::DeferredRead;
use chipset_device::io::deferred::DeferredWrite;
use chipset_device::io::deferred::defer_read;
use chipset_device::io::deferred::defer_write;
use chipset_device::mmio::MmioIntercept;
use chipset_device::pci::ByteEnabledDwordRead;
use chipset_device::pci::ByteEnabledDwordWrite;
use chipset_device::pci::PciConfigSpace;
use chipset_device::pio::PortIoIntercept;
use chipset_device::poll_device::PollDevice;
use chipset_device_resources::ResolveChipsetDeviceHandleParams;
use futures::StreamExt;
use inspect::Inspect;
use inspect::InspectMut;
use mesh::rpc::RpcSend;
use mesh_worker::WorkerHandle;
use slab::Slab;
use std::task::Poll;
use vmcore::device_state::ChangeDeviceState;
use vmcore::save_restore::AsyncProtobufSaveRestore;
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
                .map(|(name, start, end)| {
                    let mut region = input.register_mmio.new_io_region(&name, end - start + 1);
                    region.map(start);
                    region
                })
                .collect(),
        });

        let pio = pio.map(|PioInit { static_regions }| PioProxy {
            static_regions: static_regions
                .into_iter()
                .map(|(name, start, end)| {
                    let mut region = input.register_pio.new_io_region(&name, end - start + 1);
                    region.map(start);
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
    fn pci_cfg_read(&mut self, offset: u16, value: ByteEnabledDwordRead<'_>) -> IoResult {
        let (read, token) = defer_read();
        let id = self.in_flight_reads.insert(read);
        self.req_send.send(DeviceRequest::PciConfigRead(
            ReadRequest {
                id,
                address: offset,
                size: 4,
            },
            value.byte_enable(),
        ));
        IoResult::Defer(token)
    }

    fn pci_cfg_write(&mut self, offset: u16, value: ByteEnabledDwordWrite) -> IoResult {
        let (write, token) = defer_write();
        let id = self.in_flight_writes.insert(write);
        self.req_send.send(DeviceRequest::PciConfigWrite(
            WriteRequest {
                id,
                address: offset,
                data: value.extract(),
            },
            value.byte_enable(),
        ));
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
                    // The remote device has disappeared, fail loudly.
                    panic!("remote device channel closed");
                }
            }
        }
    }
}

impl ChangeDeviceState for ChipsetDeviceProxy {
    fn start(&mut self) {
        self.req_send.send(DeviceRequest::Start)
    }

    async fn quiesce_input(&mut self) -> anyhow::Result<()> {
        self.req_send
            .call_failable(DeviceRequest::QuiesceInput, ())
            .await?;
        Ok(())
    }

    async fn resume_input(&mut self) -> anyhow::Result<()> {
        self.req_send
            .call_failable(DeviceRequest::ResumeInput, ())
            .await?;
        Ok(())
    }

    async fn stop(&mut self) {
        self.req_send
            .call(DeviceRequest::Stop, ())
            .await
            .expect("failed to stop remote device")
    }

    async fn reset(&mut self) {
        self.req_send
            .call(DeviceRequest::Reset, ())
            .await
            .expect("failed to reset remote device");

        self.in_flight_reads.clear();
        self.in_flight_writes.clear();
        while self.resp_recv.try_recv().is_ok() {}
    }
}

#[async_trait]
impl AsyncProtobufSaveRestore for ChipsetDeviceProxy {
    async fn save(&mut self) -> Result<SavedStateBlob, SaveError> {
        // TODO: Do we need to include any state from ourselves?
        self.req_send
            .call(DeviceRequest::Save, ())
            .await
            .map_err(|e: mesh::rpc::RpcError| SaveError::Other(anyhow::anyhow!(e)))?
            .map_err(|e: mesh::error::RemoteError| SaveError::Other(anyhow::anyhow!(e)))
    }

    async fn restore(&mut self, state: SavedStateBlob) -> Result<(), RestoreError> {
        self.req_send
            .call(DeviceRequest::Restore, state)
            .await
            .map_err(|e: mesh::rpc::RpcError| RestoreError::Other(anyhow::anyhow!(e)))?
            .map_err(|e: mesh::error::RemoteError| RestoreError::Other(anyhow::anyhow!(e)))
    }
}

#[cfg(test)]
mod tests {
    use super::ChipsetDeviceProxy;
    use crate::RemoteDynamicResolvers;
    use crate::guestmem::GuestMemoryProxy;
    use crate::protocol::DeviceInit;
    use crate::worker::RemoteChipsetDeviceHandleParams;
    use crate::worker::RemoteChipsetDeviceWorker;
    use crate::worker::RemoteChipsetDeviceWorkerParameters;
    use chipset_device::ChipsetDevice;
    use chipset_device_resources::ResolveChipsetDeviceHandleParams;
    use chipset_device_resources::ResolvedChipsetDevice;
    use guestmem::GuestMemory;
    use inspect::InspectMut;
    use mesh::MeshPayload;
    use mesh::payload::Protobuf;
    use pal_async::DefaultDriver;
    use pal_async::async_test;
    use slab::Slab;
    use std::convert::Infallible;
    use std::task::Poll;
    use test_with_tracing::test;
    use vm_resource::IntoResource;
    use vm_resource::ResolveResource;
    use vm_resource::ResourceId;
    use vm_resource::ResourceResolver;
    use vm_resource::kind::ChipsetDeviceHandleKind;
    use vmcore::device_state::ChangeDeviceState;
    use vmcore::save_restore::AsyncProtobufSaveRestore;
    use vmcore::save_restore::AsyncSaveRestore;
    use vmcore::save_restore::RestoreError;
    use vmcore::save_restore::SaveError;
    use vmcore::save_restore::SavedStateBlob;
    use vmcore::save_restore::SavedStateRoot;
    use vmcore::vmtime::VmTime;
    use vmcore::vmtime::VmTimeKeeper;

    #[derive(Protobuf, SavedStateRoot)]
    #[mesh(package = "test.chipset_device_worker")]
    struct TestSavedState {
        #[mesh(1)]
        value: u32,
    }

    #[derive(Default, InspectMut)]
    struct TestDevice(u32);

    impl ChipsetDevice for TestDevice {}

    impl ChangeDeviceState for TestDevice {
        fn start(&mut self) {}
        async fn stop(&mut self) {}
        async fn reset(&mut self) {}
    }

    async fn yield_once() {
        let mut yielded = false;
        std::future::poll_fn(|cx| {
            if std::mem::replace(&mut yielded, true) {
                Poll::Ready(())
            } else {
                cx.waker().wake_by_ref();
                Poll::Pending
            }
        })
        .await;
    }

    impl AsyncSaveRestore for TestDevice {
        type SavedState = TestSavedState;

        async fn save(&mut self) -> Result<TestSavedState, SaveError> {
            yield_once().await;
            if self.0 == 0xdead {
                return Err(SaveError::Other(anyhow::anyhow!("save failed")));
            }
            Ok(TestSavedState { value: self.0 })
        }

        async fn restore(&mut self, state: TestSavedState) -> Result<(), RestoreError> {
            yield_once().await;
            if state.value == u32::MAX {
                return Err(RestoreError::InvalidSavedState(anyhow::anyhow!(
                    "invalid value"
                )));
            }
            self.0 = state.value;
            Ok(())
        }
    }

    #[derive(MeshPayload)]
    struct TestDeviceHandle;

    impl ResourceId<ChipsetDeviceHandleKind> for TestDeviceHandle {
        const ID: &'static str = "test";
    }

    struct TestDeviceResolver;

    impl ResolveResource<ChipsetDeviceHandleKind, TestDeviceHandle> for TestDeviceResolver {
        type Output = ResolvedChipsetDevice;
        type Error = Infallible;

        fn resolve(
            &self,
            _resource: TestDeviceHandle,
            _input: ResolveChipsetDeviceHandleParams<'_>,
        ) -> Result<Self::Output, Self::Error> {
            Ok(TestDevice::default().into())
        }
    }

    #[derive(Clone, MeshPayload)]
    struct TestResolvers;

    impl RemoteDynamicResolvers for TestResolvers {
        const WORKER_ID_STR: &str = "test_remote_chipset_worker";

        async fn register_remote_dynamic_resolvers(
            self,
            resolver: &mut ResourceResolver,
        ) -> anyhow::Result<()> {
            resolver.add_resolver(TestDeviceResolver);
            Ok(())
        }
    }

    async fn new_proxy(vmtime: &VmTimeKeeper) -> ChipsetDeviceProxy {
        let (req_send, req_recv) = mesh::channel();
        let (resp_send, resp_recv) = mesh::channel();
        let (cap_send, cap_recv) = mesh::oneshot();
        let (gm_proxy, gm_remote) = GuestMemoryProxy::new(GuestMemory::empty());
        let (enc_gm_proxy, enc_gm_remote) = GuestMemoryProxy::new(GuestMemory::empty());
        let worker = mesh_worker::launch_local_worker::<RemoteChipsetDeviceWorker<TestResolvers>>(
            RemoteChipsetDeviceWorkerParameters {
                device: TestDeviceHandle.into_resource(),
                dyn_resolvers: TestResolvers,
                inputs: RemoteChipsetDeviceHandleParams {
                    device_name: "test".to_owned(),
                    is_restoring: false,
                    vmtime: vmtime.builder().clone(),
                    guest_memory: gm_remote,
                    encrypted_guest_memory: enc_gm_remote,
                },
                req_recv,
                resp_send,
                cap_send,
            },
        )
        .await
        .unwrap();

        let DeviceInit { mmio, pio, pci } = cap_recv.await.unwrap();
        assert!(mmio.is_none() && pio.is_none() && pci.is_none());
        ChipsetDeviceProxy {
            req_send,
            resp_recv,
            worker,
            in_flight_reads: Slab::new(),
            in_flight_writes: Slab::new(),
            gm_proxy,
            enc_gm_proxy,
            mmio: None,
            pio: None,
            pci: None,
        }
    }

    fn state(value: u32) -> SavedStateBlob {
        SavedStateBlob::new(TestSavedState { value })
    }

    #[async_test]
    async fn async_typed_device_round_trip_through_worker(driver: DefaultDriver) {
        let vmtime = VmTimeKeeper::new(&driver, VmTime::from_100ns(0));
        let mut proxy = new_proxy(&vmtime).await;

        for value in [0, 5, 42] {
            proxy.restore(state(value)).await.unwrap();
            let saved = proxy.save().await.unwrap();
            assert_eq!(saved.parse::<TestSavedState>().unwrap().value, value);
        }

        proxy.worker.stop();
        proxy.worker.join().await.unwrap();
    }

    #[async_test]
    async fn remote_save_restore_errors_are_reported(driver: DefaultDriver) {
        let vmtime = VmTimeKeeper::new(&driver, VmTime::from_100ns(0));
        let mut proxy = new_proxy(&vmtime).await;

        let error = proxy.restore(state(u32::MAX)).await.unwrap_err();
        assert!(format!("{error:#}").contains("invalid value"));
        assert_eq!(
            proxy
                .save()
                .await
                .unwrap()
                .parse::<TestSavedState>()
                .unwrap()
                .value,
            0
        );

        proxy.restore(state(0xdead)).await.unwrap();
        assert_eq!(proxy.save().await.unwrap_err().to_string(), "save failed");

        proxy.worker.stop();
        proxy.worker.join().await.unwrap();
        assert!(proxy.save().await.is_err());
        assert!(proxy.restore(state(0)).await.is_err());
    }
}
