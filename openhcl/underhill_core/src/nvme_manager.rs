// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Provides access to NVMe namespaces that are backed by the user-mode NVMe
//! VFIO driver. Keeps track of all the NVMe drivers.

use crate::nvme_manager::save_restore::NvmeManagerSavedState;
use crate::nvme_manager::save_restore::NvmeSavedDiskConfig;
use crate::servicing::NvmeSavedState;
use anyhow::Context;
use async_trait::async_trait;
use disk_backend::resolve::ResolveDiskParameters;
use disk_backend::resolve::ResolvedDisk;
use futures::StreamExt;
use futures::TryFutureExt;
use futures::future::join_all;
use inspect::Inspect;
use mesh::MeshPayload;
use mesh::rpc::Rpc;
use mesh::rpc::RpcSend;
use openhcl_dma_manager::AllocationVisibility;
use openhcl_dma_manager::DmaClientParameters;
use openhcl_dma_manager::DmaClientSpawner;
use openhcl_dma_manager::LowerVtlPermissionPolicy;
use pal_async::task::Spawn;
use pal_async::task::Task;
use std::collections::HashMap;
use std::collections::hash_map;
use std::sync::Arc;
use thiserror::Error;
use tracing::Instrument;
use user_driver::vfio::PciDeviceResetMethod;
use user_driver::vfio::VfioDevice;
use user_driver::vfio::vfio_set_device_reset_method;
use vm_resource::AsyncResolveResource;
use vm_resource::ResourceId;
use vm_resource::ResourceResolver;
use vm_resource::kind::DiskHandleKind;
use vmcore::vm_task::VmTaskDriverSource;

#[derive(Debug, Error)]
#[error("nvme device {pci_id} error")]
pub struct NamespaceError {
    pci_id: String,
    #[source]
    source: NvmeSpawnerError,
}

#[derive(Debug, Error)]
pub enum NvmeSpawnerError {
    #[error("failed to initialize vfio device")]
    Vfio(#[source] anyhow::Error),
    #[error("failed to initialize nvme device")]
    DeviceInitFailed(#[source] anyhow::Error),
    #[error("failed to create dma client for device")]
    DmaClient(#[source] anyhow::Error),
    #[error("failed to get namespace {nsid}")]
    Namespace {
        nsid: u32,
        #[source]
        source: nvme_driver::NamespaceError,
    },
    #[cfg(test)]
    #[error("failed to create mock nvme driver")]
    MockDriverCreationFailed(#[source] anyhow::Error),
}

/// The object that the [`NvmeManager`] manages. This is an NVMe driver.
/// Abstracted away to make it easier to test the [`NvmeManager`].
#[async_trait]
pub trait NvmeDevice: Inspect + Send + Sync {
    async fn namespace(
        &self,
        nsid: u32,
    ) -> Result<nvme_driver::Namespace, nvme_driver::NamespaceError>;
    async fn save(&mut self) -> anyhow::Result<nvme_driver::NvmeDriverSavedState>;
    async fn shutdown(mut self: Box<Self>);
    fn update_servicing_flags(&mut self, do_not_restart: bool);
}

#[derive(Inspect)]
struct VfioNvmeDevice {
    /// The NVMe driver that this object manages.
    driver: nvme_driver::NvmeDriver<VfioDevice>,
}

#[async_trait]
impl NvmeDevice for VfioNvmeDevice {
    /// Get an instance of the supplied namespace (an nvme `nsid`).
    /// Panics if the driver is not loaded (this is a programming error).
    async fn namespace(
        &self,
        nsid: u32,
    ) -> Result<nvme_driver::Namespace, nvme_driver::NamespaceError> {
        self.driver.namespace(nsid).await
    }

    /// Save the NVMe driver state.
    /// Panics if the driver is not loaded (this is a programming error).
    async fn save(&mut self) -> anyhow::Result<nvme_driver::NvmeDriverSavedState> {
        self.driver
            .save()
            .await
            .context("failed to save NVMe driver state")
    }

    async fn shutdown(mut self: Box<Self>) {
        // Shutdown can be called in the event of a failure to create the driver, so do not panic in this case.
        self.driver.shutdown().await;
    }

    /// Configure how the underlying driver should behave during servicing operations.
    /// Panics if the driver is not loaded (this is a programming error).
    fn update_servicing_flags(&mut self, keep_alive: bool) {
        self.driver.update_servicing_flags(keep_alive);
    }
}

#[async_trait]
pub trait CreateNvmeDriver: Inspect + Send + Sync {
    async fn create_driver(
        &self,
        driver_source: &VmTaskDriverSource,
        pci_id: &str,
        vp_count: u32,
        save_restore_supported: bool,
        saved_state: Option<&nvme_driver::NvmeDriverSavedState>,
    ) -> Result<Box<dyn NvmeDevice>, NvmeSpawnerError>;
}

#[derive(Inspect)]
pub struct VfioNvmeDriverSpawner {
    pub nvme_always_flr: bool,
    pub is_isolated: bool,
    #[inspect(skip)]
    pub dma_client_spawner: DmaClientSpawner,
}

#[async_trait]
impl CreateNvmeDriver for VfioNvmeDriverSpawner {
    async fn create_driver(
        &self,
        driver_source: &VmTaskDriverSource,
        pci_id: &str,
        vp_count: u32,
        save_restore_supported: bool,
        saved_state: Option<&nvme_driver::NvmeDriverSavedState>,
    ) -> Result<Box<dyn NvmeDevice>, NvmeSpawnerError> {
        let dma_client = self
            .dma_client_spawner
            .new_client(DmaClientParameters {
                device_name: format!("nvme_{}", pci_id),
                lower_vtl_policy: LowerVtlPermissionPolicy::Any,
                allocation_visibility: if self.is_isolated {
                    AllocationVisibility::Shared
                } else {
                    AllocationVisibility::Private
                },
                persistent_allocations: save_restore_supported,
            })
            .map_err(NvmeSpawnerError::DmaClient)?;

        let nvme_driver = if let Some(saved_state) = saved_state {
            let vfio_device = VfioDevice::restore(driver_source, pci_id, true, dma_client)
                .instrument(tracing::info_span!("vfio_device_restore", pci_id))
                .await
                .map_err(NvmeSpawnerError::Vfio)?;

            // TODO: For now, any isolation means use bounce buffering. This
            // needs to change when we have nvme devices that support DMA to
            // confidential memory.
            nvme_driver::NvmeDriver::restore(
                driver_source,
                vp_count,
                vfio_device,
                saved_state,
                self.is_isolated,
            )
            .instrument(tracing::info_span!("nvme_driver_restore"))
            .await
            .map_err(NvmeSpawnerError::DeviceInitFailed)?
        } else {
            Self::create_nvme_device(
                driver_source,
                pci_id,
                vp_count,
                self.nvme_always_flr,
                self.is_isolated,
                dma_client,
            )
            .await?
        };

        Ok(Box::new(VfioNvmeDevice {
            driver: nvme_driver,
        }))
    }
}

impl VfioNvmeDriverSpawner {
    async fn create_nvme_device(
        driver_source: &VmTaskDriverSource,
        pci_id: &str,
        vp_count: u32,
        nvme_always_flr: bool,
        is_isolated: bool,
        dma_client: Arc<dyn user_driver::DmaClient>,
    ) -> Result<nvme_driver::NvmeDriver<VfioDevice>, NvmeSpawnerError> {
        // Disable FLR on vfio attach/detach; this allows faster system
        // startup/shutdown with the caveat that the device needs to be properly
        // sent through the shutdown path during servicing operations, as that is
        // the only cleanup performed. If the device fails to initialize, turn FLR
        // on and try again, so that the reset is invoked on the next attach.
        let update_reset = |method: PciDeviceResetMethod| {
            if let Err(err) = vfio_set_device_reset_method(pci_id, method) {
                tracing::warn!(
                    ?method,
                    err = &err as &dyn std::error::Error,
                    "Failed to update reset_method"
                );
            }
        };
        let mut last_err = None;
        let reset_methods = if nvme_always_flr {
            &[PciDeviceResetMethod::Flr][..]
        } else {
            // If this code can't create a device without resetting it, then still try to issue an FLR
            // in case that unwedges something weird in the device state.
            // (This is implicit when the code in [`try_create_nvme_device`] opens a handle to the
            // Vfio device).
            &[PciDeviceResetMethod::NoReset, PciDeviceResetMethod::Flr][..]
        };
        for reset_method in reset_methods {
            update_reset(*reset_method);
            match Self::try_create_nvme_device(
                driver_source,
                pci_id,
                vp_count,
                is_isolated,
                dma_client.clone(),
            )
            .await
            {
                Ok(device) => {
                    if !nvme_always_flr && !matches!(reset_method, PciDeviceResetMethod::NoReset) {
                        update_reset(PciDeviceResetMethod::NoReset);
                    }
                    return Ok(device);
                }
                Err(err) => {
                    tracing::error!(
                        pci_id,
                        ?reset_method,
                        %err,
                        "failed to create nvme device"
                    );
                    last_err = Some(err);
                }
            }
        }
        // Return the most reliable error (this code assumes that the reset methods are in increasing order
        // of reliability).
        Err(last_err.unwrap())
    }

    async fn try_create_nvme_device(
        driver_source: &VmTaskDriverSource,
        pci_id: &str,
        vp_count: u32,
        is_isolated: bool,
        dma_client: Arc<dyn user_driver::DmaClient>,
    ) -> Result<nvme_driver::NvmeDriver<VfioDevice>, NvmeSpawnerError> {
        let device = VfioDevice::new(driver_source, pci_id, dma_client)
            .instrument(tracing::info_span!("vfio_device_open", pci_id))
            .await
            .map_err(NvmeSpawnerError::Vfio)?;

        // TODO: For now, any isolation means use bounce buffering. This
        // needs to change when we have nvme devices that support DMA to
        // confidential memory.
        nvme_driver::NvmeDriver::new(driver_source, vp_count, device, is_isolated)
            .instrument(tracing::info_span!("nvme_driver_init", pci_id))
            .await
            .map_err(NvmeSpawnerError::DeviceInitFailed)
    }
}

#[derive(Debug)]
pub struct NvmeManager {
    task: Task<()>,
    client: NvmeManagerClient,
    /// Running environment (memory layout) supports save/restore.
    save_restore_supported: bool,
}

impl Inspect for NvmeManager {
    fn inspect(&self, req: inspect::Request<'_>) {
        let mut resp = req.respond();
        // Pull out the field that force loads a driver on a device and handle
        // it separately.
        resp.child("force_load_pci_id", |req| match req.update() {
            Ok(update) => {
                self.client
                    .sender
                    .send(Request::ForceLoadDriver(update.defer()));
            }
            Err(req) => req.value(""),
        });
        // Send the remaining fields directly to the worker.
        resp.merge(inspect::adhoc(|req| {
            self.client.sender.send(Request::Inspect(req.defer()))
        }));
    }
}

impl NvmeManager {
    pub fn new(
        driver_source: &VmTaskDriverSource,
        vp_count: u32,
        save_restore_supported: bool,
        saved_state: Option<NvmeSavedState>,
        factory: Arc<dyn CreateNvmeDriver>,
    ) -> Self {
        let (send, recv) = mesh::channel();
        let driver = driver_source.simple();
        let mut worker = NvmeManagerWorker {
            driver_source: driver_source.clone(),
            devices: HashMap::new(),
            vp_count,
            save_restore_supported,
            factory,
        };
        let task = driver.spawn("nvme-manager", async move {
            // Restore saved data (if present) before async worker thread runs.
            if let Some(s) = saved_state.as_ref() {
                if let Err(e) = NvmeManager::restore(&mut worker, s)
                    .instrument(tracing::info_span!("nvme_manager_restore"))
                    .await
                {
                    tracing::error!(
                        error = e.as_ref() as &dyn std::error::Error,
                        "failed to restore nvme manager"
                    );
                }
            };
            worker.run(recv).await
        });
        Self {
            task,
            client: NvmeManagerClient { sender: send },
            save_restore_supported,
        }
    }

    pub fn client(&self) -> &NvmeManagerClient {
        &self.client
    }

    pub async fn shutdown(self, nvme_keepalive: bool) {
        // Early return is faster way to skip shutdown.
        // but we need to thoroughly test the data integrity.
        // TODO: Enable this once tested and approved.
        //
        // if self.nvme_keepalive { return }
        self.client.sender.send(Request::Shutdown {
            span: tracing::info_span!("shutdown_nvme_manager"),
            nvme_keepalive,
        });
        self.task.await;
    }

    /// Save NVMe manager's state during servicing.
    pub async fn save(&self, nvme_keepalive: bool) -> Option<NvmeManagerSavedState> {
        // NVMe manager has no own data to save, everything will be done
        // in the Worker task which can be contacted through Client.
        if self.save_restore_supported && nvme_keepalive {
            Some(self.client().save().await?)
        } else {
            // Do not save any state if nvme_keepalive
            // was explicitly disabled.
            None
        }
    }

    /// Restore NVMe manager's state after servicing.
    async fn restore(
        worker: &mut NvmeManagerWorker,
        saved_state: &NvmeSavedState,
    ) -> anyhow::Result<()> {
        worker
            .restore(&saved_state.nvme_state)
            .instrument(tracing::info_span!("nvme_worker_restore"))
            .await?;

        Ok(())
    }
}

enum Request {
    Inspect(inspect::Deferred),
    ForceLoadDriver(inspect::DeferredUpdate),
    GetNamespace(Rpc<(String, u32), Result<nvme_driver::Namespace, NamespaceError>>),
    Save(Rpc<(), Result<NvmeManagerSavedState, anyhow::Error>>),
    Shutdown {
        span: tracing::Span,
        nvme_keepalive: bool,
    },
}

#[derive(Debug, Clone)]
pub struct NvmeManagerClient {
    sender: mesh::Sender<Request>,
}

impl NvmeManagerClient {
    pub async fn get_namespace(
        &self,
        pci_id: String,
        nsid: u32,
    ) -> anyhow::Result<nvme_driver::Namespace> {
        Ok(self
            .sender
            .call(Request::GetNamespace, (pci_id.clone(), nsid))
            .instrument(tracing::info_span!("nvme_get_namespace", pci_id, nsid))
            .await
            .context("nvme manager is shut down")??)
    }

    /// Send an RPC call to save NVMe worker data.
    pub async fn save(&self) -> Option<NvmeManagerSavedState> {
        match self.sender.call(Request::Save, ()).await {
            Ok(s) => s.ok(),
            Err(_) => None,
        }
    }
}

#[derive(Inspect)]
struct NvmeManagerWorker {
    #[inspect(skip)]
    driver_source: VmTaskDriverSource,
    #[inspect(iter_by_key)]
    devices: HashMap<String, Box<dyn NvmeDevice>>,
    vp_count: u32,
    /// Running environment (memory layout) allows save/restore.
    save_restore_supported: bool,
    factory: Arc<dyn CreateNvmeDriver>,
}

impl NvmeManagerWorker {
    async fn run(&mut self, mut recv: mesh::Receiver<Request>) {
        let (join_span, nvme_keepalive) = loop {
            let Some(req) = recv.next().await else {
                break (tracing::Span::none(), false);
            };
            match req {
                Request::Inspect(deferred) => deferred.inspect(&self),
                Request::ForceLoadDriver(update) => {
                    match self.get_driver(update.new_value().to_owned()).await {
                        Ok(_) => {
                            let pci_id = update.new_value().to_string();
                            update.succeed(pci_id);
                        }
                        Err(err) => {
                            update.fail(err);
                        }
                    }
                }
                Request::GetNamespace(rpc) => {
                    rpc.handle(async |(pci_id, nsid)| {
                        self.get_namespace(pci_id.clone(), nsid)
                            .map_err(|source| NamespaceError { pci_id, source })
                            .await
                    })
                    .await
                }
                // Request to save worker data for servicing.
                Request::Save(rpc) => {
                    rpc.handle(async |_| self.save().await)
                        .instrument(tracing::info_span!("nvme_save_state"))
                        .await
                }
                Request::Shutdown {
                    span,
                    nvme_keepalive,
                } => {
                    // nvme_keepalive is received from host but it is only valid
                    // when memory pool allocator supports save/restore.
                    let do_not_reset = nvme_keepalive && self.save_restore_supported;
                    // Update the flag for all connected devices.
                    for (_s, dev) in self.devices.iter_mut() {
                        // Prevent devices from originating controller reset in drop().
                        dev.update_servicing_flags(do_not_reset);
                    }
                    break (span, nvme_keepalive);
                }
            }
        };

        // When nvme_keepalive flag is set then this block is unreachable
        // because the Shutdown request is never sent.
        //
        // Tear down all the devices if nvme_keepalive is not set.
        if !nvme_keepalive || !self.save_restore_supported {
            async {
                join_all(self.devices.drain().map(|(pci_id, driver)| async move {
                    driver
                        .shutdown()
                        .instrument(tracing::info_span!("shutdown_nvme_driver", pci_id))
                        .await
                }))
                .await
            }
            .instrument(join_span)
            .await;
        }
    }

    async fn get_driver(
        &mut self,
        pci_id: String,
    ) -> Result<&mut Box<dyn NvmeDevice>, NvmeSpawnerError> {
        let driver = match self.devices.entry(pci_id.to_owned()) {
            hash_map::Entry::Occupied(entry) => entry.into_mut(),
            hash_map::Entry::Vacant(entry) => {
                let driver = self
                    .factory
                    .create_driver(
                        &self.driver_source,
                        &pci_id,
                        self.vp_count,
                        self.save_restore_supported,
                        None, // No saved state for new devices.
                    )
                    .instrument(tracing::info_span!("create_nvme_device", %pci_id))
                    .await?;

                entry.insert(driver)
            }
        };
        Ok(driver)
    }

    async fn get_namespace(
        &mut self,
        pci_id: String,
        nsid: u32,
    ) -> Result<nvme_driver::Namespace, NvmeSpawnerError> {
        let driver = self.get_driver(pci_id.to_owned()).await?;
        driver
            .namespace(nsid)
            .await
            .map_err(|source| NvmeSpawnerError::Namespace { nsid, source })
    }

    /// Saves NVMe device's states into buffer during servicing.
    pub async fn save(&mut self) -> anyhow::Result<NvmeManagerSavedState> {
        let mut nvme_disks: Vec<NvmeSavedDiskConfig> = Vec::new();
        for (pci_id, driver) in self.devices.iter_mut() {
            nvme_disks.push(NvmeSavedDiskConfig {
                pci_id: pci_id.clone(),
                driver_state: driver
                    .save()
                    .instrument(tracing::info_span!("nvme_driver_save", %pci_id))
                    .await?,
            });
        }

        Ok(NvmeManagerSavedState {
            cpu_count: self.vp_count,
            nvme_disks,
        })
    }

    /// Restore NVMe manager and device states from the buffer after servicing.
    pub async fn restore(&mut self, saved_state: &NvmeManagerSavedState) -> anyhow::Result<()> {
        self.devices = HashMap::new();
        for disk in &saved_state.nvme_disks {
            let pci_id = disk.pci_id.clone();

            // This code can wait on each VFIO device until it is arrived.
            // A potential optimization would be to delay VFIO operation
            // until it is ready, but a redesign of VfioDevice is needed.
            let nvme_driver = self
                .factory
                .create_driver(
                    &self.driver_source,
                    &disk.pci_id,
                    self.vp_count,
                    self.save_restore_supported,
                    Some(&disk.driver_state),
                )
                .instrument(tracing::info_span!("nvme_driver_restore", pci_id))
                .await?;

            self.devices.insert(disk.pci_id.clone(), nvme_driver);
        }
        Ok(())
    }
}

pub struct NvmeDiskResolver {
    manager: NvmeManagerClient,
}

impl NvmeDiskResolver {
    pub fn new(manager: NvmeManagerClient) -> Self {
        Self { manager }
    }
}

#[async_trait]
impl AsyncResolveResource<DiskHandleKind, NvmeDiskConfig> for NvmeDiskResolver {
    type Output = ResolvedDisk;
    type Error = anyhow::Error;

    async fn resolve(
        &self,
        _resolver: &ResourceResolver,
        rsrc: NvmeDiskConfig,
        _input: ResolveDiskParameters<'_>,
    ) -> Result<Self::Output, Self::Error> {
        let namespace = self
            .manager
            .get_namespace(rsrc.pci_id, rsrc.nsid)
            .await
            .context("could not open nvme namespace")?;

        Ok(ResolvedDisk::new(disk_nvme::NvmeDisk::new(namespace)).context("invalid disk")?)
    }
}

#[derive(MeshPayload, Default)]
pub struct NvmeDiskConfig {
    pub pci_id: String,
    pub nsid: u32,
}

impl ResourceId<DiskHandleKind> for NvmeDiskConfig {
    const ID: &'static str = "nvme";
}

pub mod save_restore {
    use mesh::payload::Protobuf;
    use vmcore::save_restore::SavedStateRoot;

    #[derive(Protobuf, SavedStateRoot)]
    #[mesh(package = "underhill")]
    pub struct NvmeManagerSavedState {
        #[mesh(1)]
        pub cpu_count: u32,
        #[mesh(2)]
        pub nvme_disks: Vec<NvmeSavedDiskConfig>,
    }

    #[derive(Protobuf, Clone)]
    #[mesh(package = "underhill")]
    pub struct NvmeSavedDiskConfig {
        #[mesh(1)]
        pub pci_id: String,
        #[mesh(2)]
        pub driver_state: nvme_driver::NvmeDriverSavedState,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures::future::join_all;
    use inspect::Inspect;
    use inspect::InspectionBuilder;
    use nvme_driver::Namespace;
    use nvme_driver::NvmeDriverSavedState;
    use pal_async::DefaultDriver;
    use pal_async::async_test;
    use parking_lot::RwLock;
    use std::sync::atomic::AtomicBool;
    use std::sync::atomic::AtomicU32;
    use std::sync::atomic::Ordering;
    use std::time::Duration;
    use std::time::Instant;
    use test_with_tracing::test;
    use vmcore::vm_task::VmTaskDriverSource;
    use vmcore::vm_task::thread::ThreadDriverBackend;

    /// Mock NVMe driver for testing that simulates realistic delays and tracks call patterns
    #[derive(Inspect, Clone)]
    struct MockNvmeDriver {
        pci_id: String,
        /// Simulated delay for namespace operations
        #[inspect(skip)]
        namespace_delay: Duration,
        /// Simulated delay for shutdown operations
        #[inspect(skip)]
        shutdown_delay: Duration,
        /// Track when operations start (for concurrency validation)
        #[inspect(skip)]
        namespace_start_times: Arc<RwLock<Vec<Instant>>>,
        #[inspect(skip)]
        shutdown_start_time: Arc<RwLock<Option<Instant>>>,
        /// Counters for verification
        namespace_call_count: Arc<AtomicU32>,
        shutdown_call_count: Arc<AtomicU32>,
        save_call_count: Arc<AtomicU32>,
        /// Allow tests to inject failures
        fail_namespace: Arc<AtomicBool>,
        namespace_delay_sync: Arc<AtomicBool>,
        fail_save: Arc<AtomicBool>,
        /// Success mode for testing
        success_mode: Arc<AtomicBool>,
        /// Driver source for creating timers
        #[inspect(skip)]
        driver_source: VmTaskDriverSource,
    }

    impl MockNvmeDriver {
        fn new(
            pci_id: &str,
            namespace_delay: Duration,
            shutdown_delay: Duration,
            driver_source: VmTaskDriverSource,
        ) -> Self {
            Self {
                pci_id: pci_id.to_string(),
                namespace_delay,
                shutdown_delay,
                namespace_start_times: Arc::new(RwLock::new(Vec::new())),
                shutdown_start_time: Arc::new(RwLock::new(None)),
                namespace_call_count: Arc::new(AtomicU32::new(0)),
                shutdown_call_count: Arc::new(AtomicU32::new(0)),
                save_call_count: Arc::new(AtomicU32::new(0)),
                fail_namespace: Arc::new(AtomicBool::new(false)),
                namespace_delay_sync: Arc::new(AtomicBool::new(false)), // FUTURE: tests fail when this is `true` since NvmeManager does not force this to run on multiple VPs.
                fail_save: Arc::new(AtomicBool::new(false)),
                success_mode: Arc::new(AtomicBool::new(false)),
                driver_source,
            }
        }

        fn namespace_call_count(&self) -> u32 {
            self.namespace_call_count.load(Ordering::SeqCst)
        }

        fn shutdown_call_count(&self) -> u32 {
            self.shutdown_call_count.load(Ordering::SeqCst)
        }

        fn set_fail_namespace(&self, fail: bool) {
            self.fail_namespace.store(fail, Ordering::SeqCst);
        }
    }

    #[async_trait]
    impl NvmeDevice for MockNvmeDriver {
        async fn namespace(&self, _nsid: u32) -> Result<Namespace, nvme_driver::NamespaceError> {
            // Record start time for concurrency analysis
            {
                let mut start_times = self.namespace_start_times.write();
                start_times.push(Instant::now());
            }

            self.namespace_call_count.fetch_add(1, Ordering::SeqCst);

            if self.fail_namespace.load(Ordering::SeqCst) {
                return Err(nvme_driver::NamespaceError::NotFound);
            }

            // Simulate realistic work with delay
            // namespace_delay_sync == true simulates cases where the vmexit is blocked
            if self.namespace_delay_sync.load(Ordering::SeqCst) {
                std::thread::sleep(self.namespace_delay);
            } else {
                let mut timer = pal_async::timer::PolledTimer::new(&self.driver_source.simple());
                timer.sleep(self.namespace_delay).await;
            }

            if self.success_mode.load(Ordering::SeqCst) {
                // For successful tests, we can't return a real Namespace easily
                // So we'll just return an error but with a success indicator in the message
                tracing::warn!(
                    "MOCK_SUCCESS: namespace operation completed for {}",
                    self.pci_id
                );
                return Err(nvme_driver::NamespaceError::NotFound);
            } else {
                return Err(nvme_driver::NamespaceError::NotFound);
            }
        }

        async fn save(&mut self) -> anyhow::Result<NvmeDriverSavedState> {
            self.save_call_count.fetch_add(1, Ordering::SeqCst);

            if self.fail_save.load(Ordering::SeqCst) {
                anyhow::bail!("Mock save failure for {}", self.pci_id);
            }

            // Simulate work
            let mut timer = pal_async::timer::PolledTimer::new(&self.driver_source.simple());
            timer.sleep(Duration::from_millis(10)).await;

            anyhow::bail!("MOCK_SUCCESS: save operation completed for {}", self.pci_id);
        }

        async fn shutdown(mut self: Box<Self>) {
            // Record shutdown start time
            {
                let mut shutdown_time = self.shutdown_start_time.write();
                *shutdown_time = Some(Instant::now());
            }

            self.shutdown_call_count.fetch_add(1, Ordering::SeqCst);

            // Simulate shutdown work
            let mut timer = pal_async::timer::PolledTimer::new(&self.driver_source.simple());
            timer.sleep(self.shutdown_delay).await;
        }

        fn update_servicing_flags(&mut self, _do_not_restart: bool) {
            // No-op for testing
        }
    }

    /// Mock factory that creates MockNvmeDriver instances
    #[derive(Inspect)]
    struct MockNvmeDriverFactory {
        #[inspect(skip)]
        namespace_delay: Duration,
        #[inspect(skip)]
        shutdown_delay: Duration,
        #[inspect(skip)]
        /// Store references to created drivers for test verification
        created_drivers: Arc<RwLock<Vec<Arc<MockNvmeDriver>>>>,
        #[inspect(skip)]
        /// Allow injection of creation failures
        fail_create: Arc<AtomicBool>,
    }

    impl MockNvmeDriverFactory {
        fn new(namespace_delay: Duration, shutdown_delay: Duration) -> Self {
            Self {
                namespace_delay,
                shutdown_delay,
                created_drivers: Arc::new(RwLock::new(Vec::new())),
                fail_create: Arc::new(AtomicBool::new(false)),
            }
        }

        fn get_driver(&self, pci_id: &str) -> Option<Arc<MockNvmeDriver>> {
            let drivers = self.created_drivers.read();
            drivers.iter().find(|d| d.pci_id == pci_id).cloned()
        }

        fn set_fail_create(&self, fail: bool) {
            self.fail_create.store(fail, Ordering::SeqCst);
        }

        fn driver_count(&self) -> usize {
            self.created_drivers.read().len()
        }
    }

    #[async_trait]
    impl CreateNvmeDriver for MockNvmeDriverFactory {
        async fn create_driver(
            &self,
            driver_source: &VmTaskDriverSource,
            pci_id: &str,
            _vp_count: u32,
            _save_restore_supported: bool,
            _saved_state: Option<&NvmeDriverSavedState>,
        ) -> Result<Box<dyn NvmeDevice>, NvmeSpawnerError> {
            if self.fail_create.load(Ordering::SeqCst) {
                return Err(NvmeSpawnerError::MockDriverCreationFailed(anyhow::anyhow!(
                    "Mock create failure for {}",
                    pci_id
                )));
            }

            let driver = Arc::new(MockNvmeDriver::new(
                pci_id,
                self.namespace_delay,
                self.shutdown_delay,
                driver_source.clone(),
            ));

            // Store reference for test verification
            {
                let mut drivers = self.created_drivers.write();
                drivers.push(driver.clone());
            }

            Ok(Box::new((*driver).clone()))
        }
    }

    // Helper to create test VmTaskDriverSource
    fn create_test_driver_source(driver: DefaultDriver) -> VmTaskDriverSource {
        VmTaskDriverSource::new(ThreadDriverBackend::new(driver))
    }

    #[ignore = "This test validates that the NvmeManager GetNamespace path is concurrent, but that concurrency is not yet implemented."]
    #[async_test]
    async fn test_concurrent_get_namespace_calls(driver: DefaultDriver) {
        // Test that multiple GetNamespace calls to different devices run concurrently
        let driver_source = create_test_driver_source(driver);

        // Create factory with realistic delays to observe concurrency
        let factory = Arc::new(MockNvmeDriverFactory::new(
            Duration::from_millis(100), // namespace delay
            Duration::from_millis(50),  // shutdown delay
        ));

        let manager = NvmeManager::new(
            &driver_source,
            4,     // vp_count
            false, // save_restore_supported
            None,  // no saved state
            factory.clone(),
        );

        let client = manager.client().clone();

        // Launch multiple concurrent GetNamespace calls to different devices
        let start_time = Instant::now();
        let tasks = (0..3).map(|i| {
            let client = client.clone();
            let pci_id = format!("test-device-{}", i);
            async move { client.get_namespace(pci_id, 1).await }
        });

        // Wait for all to complete
        let results: Vec<_> = join_all(tasks).await;
        let total_time = start_time.elapsed();

        // Verify all completed (even if they "failed" with our mock)
        assert_eq!(results.len(), 3);

        // Verify concurrency: total time should be much less than 3 * 100ms if concurrent
        assert!(
            total_time < Duration::from_millis(250),
            "Total time {:?} suggests operations were not concurrent",
            total_time
        );

        // Verify we created 3 separate drivers
        assert_eq!(factory.driver_count(), 3);

        manager.shutdown(false).await;
    }

    #[async_test]
    async fn test_concurrent_shutdown(driver: DefaultDriver) {
        // Test that shutdown operations on multiple devices run concurrently
        let driver_source = create_test_driver_source(driver);

        let factory = Arc::new(MockNvmeDriverFactory::new(
            Duration::from_millis(10),  // namespace delay
            Duration::from_millis(100), // shutdown delay - this is what we're testing
        ));

        let manager = NvmeManager::new(&driver_source, 4, false, None, factory.clone());

        let client = manager.client().clone();

        // First, create several devices by calling GetNamespace
        for i in 0..4 {
            let pci_id = format!("test-device-{}", i);
            let _ = client.get_namespace(pci_id, 1).await; // Ignore the mock "error"
        }

        // Verify we have 4 drivers
        assert_eq!(factory.driver_count(), 4);

        // Now test concurrent shutdown
        let start_time = Instant::now();
        manager.shutdown(false).await;
        let shutdown_time = start_time.elapsed();

        // Verify concurrency: with 4 devices each taking 100ms to shutdown,
        // serial would take 400ms, concurrent should be ~100ms
        assert!(
            shutdown_time < Duration::from_millis(200),
            "Shutdown time {:?} suggests shutdowns were not concurrent",
            shutdown_time
        );

        // Verify all drivers were shutdown exactly once
        for i in 0..4 {
            let pci_id = format!("test-device-{}", i);
            let driver = factory.get_driver(&pci_id).unwrap();
            assert_eq!(driver.shutdown_call_count(), 1);
        }
    }

    #[async_test]
    async fn test_same_device_namespace_serialization(driver: DefaultDriver) {
        // Test that multiple calls to the same device are properly handled
        let driver_source = create_test_driver_source(driver);

        let factory = Arc::new(MockNvmeDriverFactory::new(
            Duration::from_millis(50),
            Duration::from_millis(10),
        ));

        let manager = NvmeManager::new(&driver_source, 4, false, None, factory.clone());
        let client = manager.client().clone();

        let pci_id = "test-device-same".to_string();

        // Launch multiple concurrent calls to the same device
        let tasks = (0..3).map(|nsid| {
            let client = client.clone();
            let pci_id = pci_id.clone();
            async move { client.get_namespace(pci_id, nsid + 1).await }
        });

        let results: Vec<_> = join_all(tasks).await;

        // All should complete
        assert_eq!(results.len(), 3);

        // Should have created only one driver (same device)
        assert_eq!(factory.driver_count(), 1);

        let driver = factory.get_driver(&pci_id).unwrap();
        // Should have received 3 namespace calls
        assert_eq!(driver.namespace_call_count(), 3);

        manager.shutdown(false).await;
    }

    #[async_test]
    async fn test_error_handling(driver: DefaultDriver) {
        // Test error handling in various scenarios
        let driver_source = create_test_driver_source(driver);

        let factory = Arc::new(MockNvmeDriverFactory::new(
            Duration::from_millis(10),
            Duration::from_millis(10),
        ));

        let manager = NvmeManager::new(&driver_source, 4, false, None, factory.clone());
        let client = manager.client().clone();

        // Test factory creation failure
        factory.set_fail_create(true);
        let result = client.get_namespace("failing-device".to_string(), 1).await;
        assert!(result.is_err());

        // Reset and create a working device
        factory.set_fail_create(false);
        let _ = client.get_namespace("working-device".to_string(), 1).await;

        // Test namespace operation failure
        let driver = factory.get_driver("working-device").unwrap();
        driver.set_fail_namespace(true);

        let result = client.get_namespace("working-device".to_string(), 2).await;
        assert!(result.is_err());

        manager.shutdown(false).await;
    }

    #[async_test]
    async fn test_shutdown_before_operations(driver: DefaultDriver) {
        // Test that operations fail gracefully after shutdown
        let driver_source = create_test_driver_source(driver);

        let factory = Arc::new(MockNvmeDriverFactory::new(
            Duration::from_millis(10),
            Duration::from_millis(10),
        ));

        let manager = NvmeManager::new(&driver_source, 4, false, None, factory.clone());
        let client = manager.client().clone();

        // Shutdown immediately
        manager.shutdown(false).await;

        // Now try to use the client - should fail gracefully
        let result = client.get_namespace("test-device".to_string(), 1).await;
        assert!(result.is_err());
    }

    #[ignore = "This test validates that the NvmeManager GetNamespace path is concurrent, but that concurrency is not yet implemented."]
    #[async_test]
    async fn test_concurrent_namespace_timing(driver: DefaultDriver) {
        // More focused test on timing to prove concurrency
        let driver_source = create_test_driver_source(driver);

        let factory = Arc::new(MockNvmeDriverFactory::new(
            Duration::from_millis(200), // Longer delay to make timing differences clear
            Duration::from_millis(50),
        ));

        let manager = NvmeManager::new(&driver_source, 4, false, None, factory.clone());
        let client = manager.client().clone();

        // Test concurrent calls to different devices
        let start_time = Instant::now();
        let tasks = (0..4).map(|i| {
            let client = client.clone();
            let pci_id = format!("timing-device-{}", i);
            async move {
                let start = Instant::now();
                let _ = client.get_namespace(pci_id, 1).await;
                (i, start.elapsed())
            }
        });

        let results: Vec<_> = join_all(tasks).await;
        let total_time = start_time.elapsed();

        // If sequential: 4 * 200ms = 800ms
        // If concurrent: ~200ms (all running in parallel)
        println!("Total time for 4 concurrent calls: {:?}", total_time);
        assert!(
            total_time < Duration::from_millis(400),
            "Total time {:?} suggests operations were sequential, not concurrent",
            total_time
        );

        // Verify each call took approximately the expected time
        for (i, duration) in results {
            println!("Device {} took {:?}", i, duration);
            assert!(
                duration >= Duration::from_millis(190) && duration <= Duration::from_millis(250),
                "Device {} timing {:?} outside expected range",
                i,
                duration
            );
        }

        manager.shutdown(false).await;
    }

    #[async_test]
    async fn test_nvme_manager_inspect(driver: DefaultDriver) {
        // Test that NvmeManager's Inspect implementation provides access to device information
        let driver_source = create_test_driver_source(driver);

        let factory = Arc::new(MockNvmeDriverFactory::new(
            Duration::from_millis(10),
            Duration::from_millis(10),
        ));

        let manager = NvmeManager::new(&driver_source, 4, false, None, factory.clone());
        let client = manager.client().clone();

        // Create some devices by calling GetNamespace
        let device_ids = vec!["inspect-device-1", "inspect-device-2", "inspect-device-3"];
        for pci_id in device_ids {
            let _ = client.get_namespace(pci_id.into(), 1).await; // Ignore mock "error"
        }

        // Verify devices were created
        assert_eq!(factory.driver_count(), 3);

        let mut i = InspectionBuilder::new("/").inspect(&manager);

        i.resolve().await;

        // For example:
        // {"devices":{"inspect-device-1":{..},"inspect-device-2":{..},"inspect-device-3":{..}},"factory":{},"force_load_pci_id":"","save_restore_supported":false,"vp_count":4}
        let results = i.results();
        let string = results.to_string();
        assert!(string.contains("devices"));
        assert!(string.contains("inspect-device-1"));
        assert!(string.contains("inspect-device-2"));
        assert!(string.contains("inspect-device-3"));
        assert!(string.contains("vp_count"));

        manager.shutdown(false).await;
    }
}
