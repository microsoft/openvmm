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

    /// The remote device's saved state, fetched by `prepare_save` and
    /// consumed by the next `save`.
    #[inspect(skip)]
    pending_save: Option<SavedStateBlob>,
    /// The saved state passed to `restore`, consumed by the next
    /// `finish_restore`, which sends it to the remote device.
    #[inspect(skip)]
    pending_restore: Option<SavedStateBlob>,
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
            pending_save: None,
            pending_restore: None,
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

    async fn prepare_save(&mut self) -> Result<(), SaveError> {
        // Drop any state from an earlier call so that `save` cannot return
        // stale state if this call fails.
        self.pending_save = None;
        let state = self
            .req_send
            .call(DeviceRequest::Save, ())
            .await
            .map_err(|e: mesh::rpc::RpcError| SaveError::Other(anyhow::anyhow!(e)))?
            .map_err(|e: mesh::error::RemoteError| SaveError::Other(anyhow::anyhow!(e)))?;
        self.pending_save = Some(state);
        Ok(())
    }

    async fn finish_restore(&mut self) -> Result<(), RestoreError> {
        let state = self.pending_restore.take().ok_or_else(|| {
            RestoreError::Other(anyhow::anyhow!(
                "restore must be called before finish_restore"
            ))
        })?;
        self.req_send
            .call(DeviceRequest::Restore, state)
            .await
            .map_err(|e: mesh::rpc::RpcError| RestoreError::Other(anyhow::anyhow!(e)))?
            .map_err(|e: mesh::error::RemoteError| RestoreError::Other(anyhow::anyhow!(e)))
    }
}

// The remote device is saved by `prepare_save` and restored by
// `finish_restore`, which wait for the worker asynchronously. These methods
// only hand off the saved state.
impl ProtobufSaveRestore for ChipsetDeviceProxy {
    fn save(&mut self) -> Result<SavedStateBlob, SaveError> {
        // TODO: Do we need to include any state from ourselves?
        self.pending_save.take().ok_or_else(|| {
            SaveError::Other(anyhow::anyhow!("prepare_save must succeed before save"))
        })
    }

    fn restore(&mut self, state: SavedStateBlob) -> Result<(), RestoreError> {
        self.pending_restore = Some(state);
        Ok(())
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
    use test_with_tracing::test;
    use vm_resource::IntoResource;
    use vm_resource::ResolveResource;
    use vm_resource::ResourceId;
    use vm_resource::ResourceResolver;
    use vm_resource::kind::ChipsetDeviceHandleKind;
    use vmcore::device_state::ChangeDeviceState;
    use vmcore::save_restore::ProtobufSaveRestore;
    use vmcore::save_restore::RestoreError;
    use vmcore::save_restore::SaveError;
    use vmcore::save_restore::SaveRestore;
    use vmcore::save_restore::SavedStateBlob;
    use vmcore::save_restore::SavedStateRoot;
    use vmcore::vmtime::VmTime;
    use vmcore::vmtime::VmTimeKeeper;

    /// A value that makes the test device fail `prepare_save`.
    const FAIL_PREPARE_SAVE: u32 = 0xdead;
    /// A value that makes the test device fail `finish_restore`.
    const FAIL_FINISH_RESTORE: u32 = 0xbad;

    #[derive(Protobuf, SavedStateRoot)]
    #[mesh(package = "test.chipset_device_worker")]
    struct TestSavedState {
        #[mesh(1)]
        value: u32,
    }

    /// A device that only saves state captured by `prepare_save` and only
    /// applies restored state in `finish_restore`, so that saving and
    /// restoring it through the worker fails if the worker skips either hook.
    #[derive(Default, InspectMut)]
    struct TestDevice {
        value: u32,
        #[inspect(skip)]
        prepared: Option<u32>,
        #[inspect(skip)]
        restored: Option<u32>,
    }

    impl ChipsetDevice for TestDevice {}

    impl ChangeDeviceState for TestDevice {
        fn start(&mut self) {}

        async fn stop(&mut self) {}

        async fn reset(&mut self) {}

        async fn prepare_save(&mut self) -> Result<(), SaveError> {
            if self.value == FAIL_PREPARE_SAVE {
                return Err(SaveError::Other(anyhow::anyhow!("prepare_save failed")));
            }
            self.prepared = Some(self.value);
            Ok(())
        }

        async fn finish_restore(&mut self) -> Result<(), RestoreError> {
            let value = self
                .restored
                .take()
                .ok_or_else(|| RestoreError::Other(anyhow::anyhow!("not restored")))?;
            if value == FAIL_FINISH_RESTORE {
                return Err(RestoreError::Other(anyhow::anyhow!(
                    "finish_restore failed"
                )));
            }
            self.value = value;
            Ok(())
        }
    }

    impl SaveRestore for TestDevice {
        type SavedState = TestSavedState;

        fn save(&mut self) -> Result<TestSavedState, SaveError> {
            let value = self
                .prepared
                .take()
                .ok_or_else(|| SaveError::Other(anyhow::anyhow!("not prepared")))?;
            Ok(TestSavedState { value })
        }

        fn restore(&mut self, state: TestSavedState) -> Result<(), RestoreError> {
            self.restored = Some(state.value);
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

    /// Launches a worker running a [`TestDevice`] on another thread and
    /// returns a proxy for it.
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

        // The device has no MMIO, PIO, or PCI to register, so build the proxy
        // directly instead of through `ChipsetDeviceProxy::new`.
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
            pending_save: None,
            pending_restore: None,
        }
    }

    async fn stop_worker(mut proxy: ChipsetDeviceProxy) {
        proxy.worker.stop();
        proxy.worker.join().await.unwrap();
    }

    fn state(value: u32) -> SavedStateBlob {
        SavedStateBlob::new(TestSavedState { value })
    }

    /// Saves the remote device through the proxy, returning its value.
    async fn save(proxy: &mut ChipsetDeviceProxy) -> Result<u32, SaveError> {
        proxy.prepare_save().await?;
        let state: TestSavedState = proxy.save()?.parse().unwrap();
        Ok(state.value)
    }

    #[async_test]
    async fn test_save_restore(driver: DefaultDriver) {
        let vmtime = VmTimeKeeper::new(&driver, VmTime::from_100ns(0));
        let mut proxy = new_proxy(&vmtime).await;

        assert_eq!(save(&mut proxy).await.unwrap(), 0);
        proxy.restore(state(5)).unwrap();
        proxy.finish_restore().await.unwrap();
        assert_eq!(save(&mut proxy).await.unwrap(), 5);

        stop_worker(proxy).await;
    }

    #[async_test]
    async fn test_save_without_prepare_save(driver: DefaultDriver) {
        let vmtime = VmTimeKeeper::new(&driver, VmTime::from_100ns(0));
        let mut proxy = new_proxy(&vmtime).await;
        let unprepared = "prepare_save must succeed before save";

        assert_eq!(proxy.save().unwrap_err().to_string(), unprepared);

        // Prepared state is returned only once.
        proxy.prepare_save().await.unwrap();
        proxy.save().unwrap();
        assert_eq!(proxy.save().unwrap_err().to_string(), unprepared);

        stop_worker(proxy).await;
    }

    #[async_test]
    async fn test_finish_restore_without_restore(driver: DefaultDriver) {
        let vmtime = VmTimeKeeper::new(&driver, VmTime::from_100ns(0));
        let mut proxy = new_proxy(&vmtime).await;
        let unrestored = "restore must be called before finish_restore";

        let err = proxy.finish_restore().await.unwrap_err();
        assert_eq!(err.to_string(), unrestored);

        // Restored state is sent only once.
        proxy.restore(state(5)).unwrap();
        proxy.finish_restore().await.unwrap();
        let err = proxy.finish_restore().await.unwrap_err();
        assert_eq!(err.to_string(), unrestored);

        stop_worker(proxy).await;
    }

    #[async_test]
    async fn test_remote_hook_failures(driver: DefaultDriver) {
        let vmtime = VmTimeKeeper::new(&driver, VmTime::from_100ns(0));
        let mut proxy = new_proxy(&vmtime).await;

        // A failed remote `finish_restore` fails the restore.
        proxy.restore(state(FAIL_FINISH_RESTORE)).unwrap();
        let err = proxy.finish_restore().await.unwrap_err();
        assert_eq!(err.to_string(), "finish_restore failed");
        assert_eq!(save(&mut proxy).await.unwrap(), 0);

        // A failed remote `prepare_save` fails the save, and `save` does not
        // return the state fetched by an earlier `prepare_save`.
        proxy.prepare_save().await.unwrap();
        proxy.restore(state(FAIL_PREPARE_SAVE)).unwrap();
        proxy.finish_restore().await.unwrap();
        let err = proxy.prepare_save().await.unwrap_err();
        assert_eq!(err.to_string(), "prepare_save failed");
        assert_eq!(
            proxy.save().unwrap_err().to_string(),
            "prepare_save must succeed before save"
        );

        stop_worker(proxy).await;
    }
}
