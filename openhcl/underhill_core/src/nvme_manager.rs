// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Multi-threaded NVMe device manager for user-mode VFIO drivers.
//!
//! # Architecture Overview
//!
//! This module implements a multi-threaded actor-based architecture for managing NVMe devices:
//!
//! ```text
//! NvmeManager (coordinator)
//!   ├── NvmeManagerWorker (device registry via mesh RPC)
//!   │   └── Arc<RwLock<HashMap<String, NvmeDriverManager>>> (device lookup)
//!   │
//!   └── Per-device: NvmeDriverManager
//!       └── NvmeDriverManagerWorker (serialized per device via mesh RPC)
//!           └── VfioNvmeDevice (wraps nvme_driver::NvmeDriver<VfioDevice>)
//! ```
//!
//! # Key Objects
//!
//! - **`NvmeManager`**: Main coordinator, creates worker task and provides client interface
//! - **`NvmeManagerWorker`**: Handles device registry, spawns tasks for concurrent operations  
//! - **`NvmeDriverManager`**: Per-device manager with dedicated worker task for serialization
//! - **`NvmeDriverManagerWorker`**: Serializes requests per device, handles driver lifecycle
//! - **`VfioNvmeDevice`**: Implements `NvmeDevice` trait, wraps actual NVMe VFIO driver
//! - **`VfioNvmeDriverSpawner`**: Implements `CreateNvmeDriver` trait for device creation
//! - **`NvmeDiskResolver`**: Resource resolver for converting NVMe configs to resolved disks
//! - **`NvmeDiskConfig`**: Configuration for NVMe disk resources (PCI ID + namespace ID)
//!
//! # Concurrency Model
//!
//! - **Cross-device operations**: Run concurrently via spawned tasks
//! - **Same-device operations**: Serialized through per-device worker tasks
//! - **Device registry**: Protected by `Arc<RwLock<HashMap<String, NvmeDriverManager>>>`
//! - **Shutdown coordination**: `Arc<AtomicBool>` prevents new operations during shutdown
//!
//! # Lock Order (to prevent deadlocks)
//!
//! 1. `context.devices.read()` - Fast path for existing devices
//! 2. `context.devices.write()` - Only for device creation/removal
//! 3. No nested locks - mesh RPC calls made outside lock scope
//!
//! # Subtle Behaviors
//!
//! - **Idempotent operations**: Multiple `load_driver()` calls are safe (mesh serialization)
//! - **Graceful shutdown**: Mesh RPC handles shutdown races, devices drain before exit
//! - **Error propagation**: Mesh channel errors indicate shutdown
//! - **Save/restore**: Supported when `save_restore_supported=true`, enables nvme_keepalive
//!

use crate::nvme_manager::device_manager::NvmeDriverManager;
use crate::nvme_manager::device_manager::NvmeDriverManagerClient;
use crate::nvme_manager::save_restore::NvmeManagerSavedState;
use crate::nvme_manager::save_restore::NvmeSavedDiskConfig;
use crate::servicing::NvmeSavedState;
use anyhow::Context;
use async_trait::async_trait;
use disk_backend::resolve::ResolveDiskParameters;
use disk_backend::resolve::ResolvedDisk;
use futures::StreamExt;
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
use parking_lot::RwLock;
use std::collections::HashMap;
use std::collections::hash_map;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
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

/// Abstraction over NVMe device drivers that the [`NvmeManager`] manages.
/// This trait provides a uniform interface for different NVMe driver implementations,
/// making it easier to test the [`NvmeManager`] with mock drivers.
#[async_trait]
pub trait NvmeDevice: Inspect + Send + Sync {
    async fn namespace(
        &self,
        nsid: u32,
    ) -> Result<nvme_driver::Namespace, nvme_driver::NamespaceError>;
    async fn save(&mut self) -> anyhow::Result<nvme_driver::NvmeDriverSavedState>;
    async fn shutdown(mut self: Box<Self>);
    fn update_servicing_flags(&mut self, keep_alive: bool);
}

#[derive(Inspect)]
struct VfioNvmeDevice {
    pci_id: String,
    /// The underlying NVMe driver instance that manages the VFIO device.
    driver: nvme_driver::NvmeDriver<VfioDevice>,
}

#[async_trait]
impl NvmeDevice for VfioNvmeDevice {
    /// Get an instance of the supplied namespace (an nvme `nsid`).
    async fn namespace(
        &self,
        nsid: u32,
    ) -> Result<nvme_driver::Namespace, nvme_driver::NamespaceError> {
        self.driver.namespace(nsid).await
    }

    /// Save the NVMe driver state.
    async fn save(&mut self) -> anyhow::Result<nvme_driver::NvmeDriverSavedState> {
        self.driver
            .save()
            .await
            .with_context(|| format!("failed to save NVMe driver state: {}", self.pci_id))
    }

    async fn shutdown(mut self: Box<Self>) {
        self.driver.shutdown().await;
    }

    /// Configure how the underlying driver should behave during servicing operations.
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
                .instrument(tracing::info_span!("nvme_vfio_device_restore", pci_id))
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
            pci_id: pci_id.to_string(),
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
                    "failed to update reset_method"
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
            .instrument(tracing::info_span!("nvme_vfio_device_open", pci_id))
            .await
            .map_err(NvmeSpawnerError::Vfio)?;

        // TODO: For now, any isolation means use bounce buffering. This
        // needs to change when we have nvme devices that support DMA to
        // confidential memory.
        nvme_driver::NvmeDriver::new(driver_source, vp_count, device, is_isolated)
            .instrument(tracing::info_span!("nvme_driver_new", pci_id))
            .await
            .map_err(NvmeSpawnerError::DeviceInitFailed)
    }
}

mod device_manager {
    use super::*;
    use inspect::Deferred;
    use mesh::rpc::RpcError;
    use nvme_driver::NvmeDriverSavedState;
    use tracing::Span;

    #[derive(Debug, Clone)]
    pub struct NvmeDriverShutdownOptions {
        /// If true, the device will not reset on shutdown.
        pub do_not_reset: bool,

        /// If true, skip the underlying nvme device shutdown path when tearing
        /// down the driver. Used for NVMe keepalive.
        pub skip_device_shutdown: bool,
    }

    enum NvmeDriverRequest {
        Inspect(Deferred),
        LoadDriver(Rpc<Span, anyhow::Result<()>>),
        /// Get an instance of the supplied namespace (an nvme `nsid`).
        GetNamespace(Rpc<(Span, u32), Result<nvme_driver::Namespace, NamespaceError>>),
        Save(Rpc<Span, anyhow::Result<NvmeDriverSavedState>>),
        /// Shutdown the NVMe driver, and the manager of that driver.
        /// Takes the span, and a set of options.
        Shutdown(Rpc<(Span, NvmeDriverShutdownOptions), ()>),
    }

    pub struct NvmeDriverManager {
        task: Task<()>,
        pci_id: String,
        pub client: NvmeDriverManagerClient,
    }

    impl Inspect for NvmeDriverManager {
        fn inspect(&self, req: inspect::Request<'_>) {
            let mut resp = req.respond();
            // Pull out the field that force loads a driver on a device and handle
            // it separately.
            resp.child("pci_id", |req| req.value(&self.pci_id));

            // Send the remaining fields directly to the worker.
            resp.merge(inspect::adhoc(|req| {
                self.client
                    .sender
                    .send(NvmeDriverRequest::Inspect(req.defer()))
            }));
        }
    }

    impl NvmeDriverManager {
        pub fn client(&self) -> &NvmeDriverManagerClient {
            &self.client
        }

        /// Creates the [`NvmeDriverManager`].
        pub fn new(
            driver_source: &VmTaskDriverSource,
            pci_id: &str,
            vp_count: u32,
            save_restore_supported: bool,
            device: Option<Box<dyn NvmeDevice>>,
            nvme_driver_spawner: Arc<dyn CreateNvmeDriver>,
        ) -> anyhow::Result<Self> {
            let (send, recv) = mesh::channel();
            let driver = driver_source.simple();

            let mut worker = NvmeDriverManagerWorker {
                driver_source: driver_source.clone(),
                pci_id: pci_id.into(),
                vp_count,
                save_restore_supported,
                driver: device,
                nvme_driver_spawner,
            };
            let task = driver.spawn("nvme-driver-manager", async move { worker.run(recv).await });
            Ok(Self {
                task,
                pci_id: pci_id.into(),
                client: NvmeDriverManagerClient {
                    pci_id: pci_id.into(),
                    sender: send,
                },
            })
        }

        pub async fn shutdown(self, opts: NvmeDriverShutdownOptions) {
            // Early return is faster way to skip shutdown.
            // but we need to thoroughly test the data integrity.
            // TODO: Enable this once tested and approved.
            //
            // if self.nvme_keepalive { return }

            let span = tracing::info_span!(
                "nvme_device_manager_shutdown",
                pci_id = self.pci_id,
                do_not_reset = opts.do_not_reset,
                skip_device_shutdown = opts.skip_device_shutdown
            );

            if let Err(e) = self
                .client()
                .sender
                .call(NvmeDriverRequest::Shutdown, (span.clone(), opts.clone()))
                .instrument(span)
                .await
            {
                tracing::warn!(
                    pci_id = self.pci_id,
                    error = &e as &dyn std::error::Error,
                    "nvme device manager already shut down"
                );
            }

            self.task.await;
        }
    }

    #[derive(Inspect, Debug, Clone)]
    pub struct NvmeDriverManagerClient {
        pci_id: String,
        #[inspect(skip)]
        sender: mesh::Sender<NvmeDriverRequest>,
    }

    impl NvmeDriverManagerClient {
        pub fn send_inspect(&self, deferred: Deferred) {
            self.sender.send(NvmeDriverRequest::Inspect(deferred));
        }

        pub async fn get_namespace(&self, nsid: u32) -> anyhow::Result<nvme_driver::Namespace> {
            let span = tracing::info_span!(
                "nvme_device_manager_get_namespace",
                pci_id = self.pci_id,
                nsid
            );
            match self
                .sender
                .call_failable(NvmeDriverRequest::GetNamespace, (span.clone(), nsid))
                .instrument(span)
                .await
            {
                Err(RpcError::Channel(_)) => Err(anyhow::anyhow!(format!(
                    "nvme device manager worker is shut down: {}",
                    self.pci_id
                ))),
                Err(RpcError::Call(e)) => Err(anyhow::Error::from(e)),
                Ok(ns) => Ok(ns),
            }
        }

        pub async fn load_driver(&self) -> anyhow::Result<()> {
            let span = tracing::info_span!("nvme_driver_client_load_driver", pci_id = self.pci_id);
            match self
                .sender
                .call_failable(NvmeDriverRequest::LoadDriver, span.clone())
                .instrument(span)
                .await
            {
                Err(RpcError::Channel(_)) => Err(anyhow::anyhow!(format!(
                    "nvme device manager worker is shut down: {}",
                    self.pci_id
                ))),
                Err(RpcError::Call(e)) => Err(e),
                Ok(()) => Ok(()),
            }
        }

        pub(crate) async fn save(&self) -> anyhow::Result<NvmeDriverSavedState> {
            let span = tracing::info_span!("nvme_driver_client_save", pci_id = self.pci_id);
            match self
                .sender
                .call_failable(NvmeDriverRequest::Save, span.clone())
                .instrument(span)
                .await
            {
                Err(RpcError::Channel(_)) => Err(anyhow::anyhow!(format!(
                    "nvme device manager worker is shut down: {}",
                    self.pci_id
                ))),
                Err(RpcError::Call(e)) => Err(e),
                Ok(state) => Ok(state),
            }
        }
    }

    #[derive(Inspect)]
    struct NvmeDriverManagerWorker {
        #[inspect(skip)]
        driver_source: VmTaskDriverSource,
        pci_id: String,
        vp_count: u32,
        /// Whether the running environment (specifically the VTL2 memory layout) allows save/restore.
        save_restore_supported: bool,
        #[inspect(skip)]
        nvme_driver_spawner: Arc<dyn CreateNvmeDriver>,
        driver: Option<Box<dyn NvmeDevice>>,
    }

    impl NvmeDriverManagerWorker {
        async fn run(&mut self, mut recv: mesh::Receiver<NvmeDriverRequest>) {
            loop {
                let Some(req) = recv.next().await else {
                    break;
                };
                // Handle requests for this specific NVMe device. Each device has its own
                // worker task, so requests are naturally serialized per device.
                match req {
                    NvmeDriverRequest::Inspect(deferred) => deferred.inspect(&self),
                    NvmeDriverRequest::LoadDriver(rpc) => {
                        let load_driver_span = tracing::debug_span!(parent: rpc.input(),
                            "nvme_device_manager_load_driver",
                            pci_id = %self.pci_id
                        );

                        rpc.handle(async |_span| {
                            // Multiple threads could have raced to call this driver.
                            // Just let the winning thread create the driver.
                            if self.driver.is_some() {
                                tracing::debug!(
                                    "nvme device manager worker load driver called for {} with existing driver",
                                    self.pci_id
                                );
                                return Ok(());
                            }

                            let driver = self
                                .nvme_driver_spawner
                                .create_driver(
                                    &self.driver_source,
                                    &self.pci_id,
                                    self.vp_count,
                                    self.save_restore_supported,
                                    None,
                                )
                                .await?;
                            self.driver = Some(driver);

                            Ok(())
                        })
                        .instrument(load_driver_span)
                        .await
                    }
                    NvmeDriverRequest::GetNamespace(rpc) => {
                        let namespace_span = tracing::debug_span!(parent: &rpc.input().0,
                            "nvme_device_manager_get_namespace",
                            pci_id = %self.pci_id,
                            nsid = rpc.input().1
                        );

                        rpc.handle(async |(_, nsid)| {
                            self.driver
                                .as_ref()
                                .unwrap()
                                .namespace(nsid)
                                .await
                                .map_err(|source| NamespaceError {
                                    pci_id: self.pci_id.clone(),
                                    source: NvmeSpawnerError::Namespace { nsid, source },
                                })
                        })
                        .instrument(namespace_span)
                        .await
                    }
                    NvmeDriverRequest::Save(rpc) => {
                        rpc.handle(async |_span| self.driver.as_mut().unwrap().save().await)
                            .await
                    }
                    NvmeDriverRequest::Shutdown(rpc) => {
                        let shutdown_span = tracing::debug_span!(parent: &rpc.input().0,
                            "nvme_device_manager_shutdown",
                            pci_id = %self.pci_id,
                        );
                        rpc.handle(async |(_span, options)| {
                            // Driver may be `None` here if there was a failure during driver creation.
                            // In that case, we just skip the shutdown rather than panic.
                            match self.driver.take() {
                                None => {
                                    tracing::debug!(
                                        "nvme device manager worker shutdown called for {pci_id} with no driver",
                                        pci_id = self.pci_id
                                    );
                                },
                                Some(mut driver) => {
                                    driver.update_servicing_flags(options.do_not_reset);

                                    if !options.skip_device_shutdown {
                                        driver.shutdown()
                                            .instrument(
                                                tracing::info_span!("shutdown_nvme_device", pci_id = %self.pci_id),
                                            )
                                            .await;
                                    }
                                }
                            }
                        })
                        .instrument(shutdown_span)
                        .await;

                        break;
                    }
                }
            }
        }
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
        nvme_driver_spawner: Arc<dyn CreateNvmeDriver>,
    ) -> Self {
        let (send, recv) = mesh::channel();
        let driver = driver_source.simple();
        let mut worker = NvmeManagerWorker {
            tasks: Vec::new(),
            context: NvmeWorkerContext {
                shutdown: Arc::new(AtomicBool::new(false)),
                vp_count,
                save_restore_supported,
                driver_source: driver_source.clone(),
                devices: Arc::new(RwLock::new(HashMap::new())),
                nvme_driver_spawner: nvme_driver_spawner.clone(),
            },
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
            .instrument(tracing::info_span!("nvme_manager_worker_restore"))
            .await?;

        Ok(())
    }
}

enum Request {
    Inspect(inspect::Deferred),
    ForceLoadDriver(inspect::DeferredUpdate),
    GetNamespace(Rpc<(String, u32), anyhow::Result<nvme_driver::Namespace>>),
    Save(Rpc<(), anyhow::Result<NvmeManagerSavedState>>),
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
        self.sender
            .call(Request::GetNamespace, (pci_id.clone(), nsid))
            .instrument(tracing::info_span!(
                "nvme_manager_get_namespace",
                %pci_id,
                nsid
            ))
            .await
            .context("nvme manager is shut down")?
    }

    /// Send an RPC call to save NVMe worker data.
    pub async fn save(&self) -> Option<NvmeManagerSavedState> {
        match self.sender.call(Request::Save, ()).await {
            Ok(s) => s.ok(),
            Err(_) => None,
        }
    }
}

#[derive(Clone, Inspect)]
struct NvmeWorkerContext {
    /// Shutdown flag, set to true when the worker is shutting down.
    shutdown: Arc<AtomicBool>,
    vp_count: u32,
    /// Running environment (memory layout) allows save/restore.
    save_restore_supported: bool,
    #[inspect(skip)]
    driver_source: VmTaskDriverSource,
    #[inspect(skip)]
    devices: Arc<RwLock<HashMap<String, NvmeDriverManager>>>,
    #[inspect(skip)]
    nvme_driver_spawner: Arc<dyn CreateNvmeDriver>,
}

#[derive(Inspect)]
#[inspect(extra = "NvmeManagerWorker::inspect_extra")]
struct NvmeManagerWorker {
    #[inspect(skip)]
    tasks: Vec<Task<()>>,
    context: NvmeWorkerContext,
}

impl NvmeManagerWorker {
    fn inspect_extra(&self, resp: &mut inspect::Response<'_>) {
        resp.child("outstanding-tasks", |req| {
            req.value(self.tasks.len());
        });

        resp.child("devices", |req| {
            let devices = self.context.devices.read();
            let mut resp = req.respond();
            for (pci_id, driver) in devices.iter() {
                resp.field(
                    pci_id,
                    inspect::adhoc(|req| {
                        driver.client().send_inspect(req.defer());
                    }),
                );
            }
        });
    }

    async fn run(&mut self, mut recv: mesh::Receiver<Request>) {
        let (join_span, nvme_keepalive) = loop {
            let Some(req) = recv.next().await else {
                break (tracing::Span::none(), false);
            };
            match req {
                Request::Inspect(deferred) => deferred.inspect(&self),
                Request::ForceLoadDriver(update) => {
                    match Self::load_driver(update.new_value().to_owned(), self.context.clone())
                        .await
                    {
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
                    let context = self.context.clone();
                    self.tasks.push(self.context.driver_source.simple().spawn(
                        "get-namespace",
                        rpc.handle(async move |(pci_id, nsid)| {
                            Self::get_namespace(pci_id.clone(), nsid, context).await
                        }),
                    ));
                }
                // Request to save worker data for servicing.
                Request::Save(rpc) => rpc.handle(async |_| self.save().await).await,
                Request::Shutdown {
                    span,
                    nvme_keepalive,
                } => {
                    // Make sure shutdown is only called once, and then flag that no further requests should
                    // be processed.
                    assert!(
                        !self
                            .context
                            .shutdown
                            .load(std::sync::atomic::Ordering::SeqCst)
                    );
                    self.context
                        .shutdown
                        .store(true, std::sync::atomic::Ordering::SeqCst);
                    tracing::info!(nvme_keepalive, "nvme manager worker shutdown requested");
                    break (span, nvme_keepalive);
                }
            }
        };

        // Wait for any pending tasks to complete. Otherwise, we will see the tasks get dropped
        // without completions.
        //
        // This is not strictly required for correctness (a dropped task will drop any in-progress state,
        // and this code is written to handle that). But, it's reasonable to do this so that we don't
        // see things that look like missing telemetry in our production logs.
        join_all(self.tasks.drain(..))
            .instrument(tracing::info_span!("nvme_manager_worker_wait_for_tasks"))
            .await;

        // Send, and wait for completion, any shutdown requests to the individual drivers.
        // After this completes, the `NvmeDriverManager` instances will remain alive, but the
        // drivers they control will be shutdown (as appropriate).
        //
        // This is required even if `nvme_keepalive` is set, since the underlying drivers
        // need to be told to not reset. In that case, the shutdown is ultimately a no-op.
        let mut devices_to_shutdown: Vec<(String, NvmeDriverManager)> = Vec::new();
        {
            let mut guard = self.context.devices.write();
            devices_to_shutdown.reserve(guard.len());
            guard.drain().for_each(|(pci_id, driver)| {
                devices_to_shutdown.push((pci_id.clone(), driver));
            });
        }

        async {
            join_all(devices_to_shutdown.into_iter().map(|(pci_id, driver)| {
                driver
                    .shutdown(device_manager::NvmeDriverShutdownOptions {
                        // nvme_keepalive is received from host but it is only valid
                        // when memory pool allocator supports save/restore.
                        do_not_reset: nvme_keepalive && self.context.save_restore_supported,
                        skip_device_shutdown: nvme_keepalive && self.context.save_restore_supported,
                    })
                    .instrument(tracing::info_span!("shutdown_nvme_driver", %pci_id))
            }))
            .await
        }
        .instrument(join_span)
        .await;
    }

    async fn load_driver(pci_id: String, context: NvmeWorkerContext) -> anyhow::Result<()> {
        if context.shutdown.load(std::sync::atomic::Ordering::SeqCst) {
            anyhow::bail!(
                "nvme device manager worker is shut down, cannot load driver for {}",
                pci_id
            );
        }

        // If the driver is already loaded, we can just return.
        {
            let guard = context.devices.read();
            if guard.get(&pci_id).is_some() {
                // If the driver is already loaded, we can just return.
                return Ok(());
            }
        }

        // Now we don't think there is a driver yet, so we need to create one. Get exclusive access
        // to update the hash map. If a shutdown call comes in while the lock is not held, then
        // this code will add an entry for the device in the hashmap, but the `load_driver` call
        // will return an appropriate error.
        //
        // Note: `client` exists outside of the devices write lock. This is safe:
        // the mesh client will fail appropriately if shutdown comes in between inserting
        // this entry and the call to `load_driver()`.
        let client = {
            let mut guard = context.devices.write();

            // Check if another thread created the driver while we were waiting for the lock.
            if let Some(driver) = guard.get(&pci_id) {
                Ok::<_, anyhow::Error>(driver.client().clone())
            } else if context.shutdown.load(std::sync::atomic::Ordering::SeqCst) {
                // No driver AND there's now a shutdown in progress, just bail.
                anyhow::bail!(
                    "nvme device manager worker is shut down, cannot load driver for {}",
                    pci_id
                );
            } else {
                // We're first! Create a new driver manager and place it in the map.
                match guard.entry(pci_id.to_owned()) {
                    hash_map::Entry::Occupied(_) => unreachable!(), // We checked above that this entry does not exist.
                    hash_map::Entry::Vacant(entry) => {
                        let driver = NvmeDriverManager::new(
                            &context.driver_source,
                            &pci_id,
                            context.vp_count,
                            context.save_restore_supported,
                            None, // No device yet,
                            context.nvme_driver_spawner.clone(),
                        )?;

                        Ok(entry.insert(driver).client().clone())
                    }
                }
            }
        }?;

        // At this point, there may be multiple threads who will execute this call. That's fine: `load_driver`
        // is idempotent.
        //
        // If a shutdown came in between dropping the lock and executing this call: mesh will notice and
        // return an error.
        client.load_driver().await
    }

    async fn get_namespace(
        pci_id: String,
        nsid: u32,
        context: NvmeWorkerContext,
    ) -> anyhow::Result<nvme_driver::Namespace> {
        // If the driver is already created, use it.
        let mut client: Option<NvmeDriverManagerClient> = None;
        {
            let guard = context.devices.read();
            if let Some(manager) = guard.get(&pci_id) {
                client = Some(manager.client().clone());
            }
        }

        if client.is_none() {
            // No driver loaded yet, so load it.
            Self::load_driver(pci_id.to_owned(), context.clone()).await?;

            // This time, if there is no entry, then we know that the driver failed to load OR a shutdown came in
            // since we loaded the driver (so we should fail).
            {
                let guard = context.devices.read();
                if let Some(manager) = guard.get(&pci_id) {
                    client = Some(manager.client().clone());
                }
            }
        }

        match client {
            Some(client) => client.get_namespace(nsid).await,
            None => Err(anyhow::anyhow!(
                "nvme device manager worker is shut down, can't get namespace {} for {}",
                nsid,
                pci_id
            )),
        }
    }

    /// Saves NVMe device's states into buffer during servicing.
    pub async fn save(&mut self) -> anyhow::Result<NvmeManagerSavedState> {
        let mut nvme_disks: Vec<NvmeSavedDiskConfig> = Vec::new();
        let mut devices_to_save: HashMap<String, NvmeDriverManagerClient> = self
            .context
            .devices
            .write()
            .iter()
            .map(|(pci_id, driver)| (pci_id.clone(), driver.client().clone()))
            .collect();
        for (pci_id, client) in devices_to_save.iter_mut() {
            nvme_disks.push(NvmeSavedDiskConfig {
                pci_id: pci_id.clone(),
                driver_state: client.save().await?,
            });
        }

        Ok(NvmeManagerSavedState {
            cpu_count: self.context.vp_count,
            nvme_disks,
        })
    }

    /// Restore NVMe manager and device states from the buffer after servicing.
    pub async fn restore(&mut self, saved_state: &NvmeManagerSavedState) -> anyhow::Result<()> {
        let mut restored_devices: HashMap<String, NvmeDriverManager> = HashMap::new();

        for disk in &saved_state.nvme_disks {
            let pci_id = disk.pci_id.clone();
            let nvme_driver = self
                .context
                .nvme_driver_spawner
                .create_driver(
                    &self.context.driver_source,
                    &pci_id,
                    saved_state.cpu_count,
                    true, // save_restore_supported is always `true` when restoring.
                    Some(&disk.driver_state),
                )
                .await?;

            restored_devices.insert(
                disk.pci_id.clone(),
                NvmeDriverManager::new(
                    &self.context.driver_source,
                    &pci_id,
                    self.context.vp_count,
                    true, // save_restore_supported is always `true` when restoring.
                    Some(nvme_driver),
                    self.context.nvme_driver_spawner.clone(),
                )?,
            );
        }

        tracing::info!(
            "nvme manager worker restored {} devices",
            restored_devices.len()
        );

        self.context.devices = Arc::new(RwLock::new(restored_devices));

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
                namespace_delay_sync: Arc::new(AtomicBool::new(false)), // todo: variant
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

        fn update_servicing_flags(&mut self, _keep_alive: bool) {
            // No-op for testing
        }
    }

    #[derive(Inspect)]
    #[inspect(skip)]
    /// Mock spawner that creates MockNvmeDriver instances
    struct MockNvmeDriverSpawner {
        namespace_delay: Duration,
        shutdown_delay: Duration,
        /// Store references to created drivers for test verification
        created_drivers: Arc<RwLock<Vec<Arc<MockNvmeDriver>>>>,
        /// Allow injection of creation failures
        fail_create: Arc<AtomicBool>,
    }

    impl MockNvmeDriverSpawner {
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
    impl CreateNvmeDriver for MockNvmeDriverSpawner {
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

    #[async_test]
    async fn test_concurrent_get_namespace_calls(driver: DefaultDriver) {
        // Test that multiple GetNamespace calls to different devices run concurrently
        let driver_source = create_test_driver_source(driver);

        // Create spawner with realistic delays to observe concurrency
        let spawner = Arc::new(MockNvmeDriverSpawner::new(
            Duration::from_millis(100), // namespace delay
            Duration::from_millis(50),  // shutdown delay
        ));

        let manager = NvmeManager::new(
            &driver_source,
            4,     // vp_count
            false, // save_restore_supported
            None,  // no saved state
            spawner.clone(),
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
        assert_eq!(spawner.driver_count(), 3);

        manager.shutdown(false).await;
    }

    #[async_test]
    async fn test_concurrent_shutdown(driver: DefaultDriver) {
        // Test that shutdown operations on multiple devices run concurrently
        let driver_source = create_test_driver_source(driver);

        let spawner = Arc::new(MockNvmeDriverSpawner::new(
            Duration::from_millis(10),  // namespace delay
            Duration::from_millis(100), // shutdown delay - this is what we're testing
        ));

        let manager = NvmeManager::new(&driver_source, 4, false, None, spawner.clone());

        let client = manager.client().clone();

        // First, create several devices by calling GetNamespace
        for i in 0..4 {
            let pci_id = format!("test-device-{}", i);
            let _ = client.get_namespace(pci_id, 1).await; // Ignore the mock "error"
        }

        // Verify we have 4 drivers
        assert_eq!(spawner.driver_count(), 4);

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
            let driver = spawner.get_driver(&pci_id).unwrap();
            assert_eq!(driver.shutdown_call_count(), 1);
        }
    }

    #[async_test]
    async fn test_same_device_namespace_serialization(driver: DefaultDriver) {
        // Test that multiple calls to the same device are properly handled
        let driver_source = create_test_driver_source(driver);

        let spawner = Arc::new(MockNvmeDriverSpawner::new(
            Duration::from_millis(50),
            Duration::from_millis(10),
        ));

        let manager = NvmeManager::new(&driver_source, 4, false, None, spawner.clone());
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
        assert_eq!(spawner.driver_count(), 1);

        let driver = spawner.get_driver(&pci_id).unwrap();
        // Should have received 3 namespace calls
        assert_eq!(driver.namespace_call_count(), 3);

        manager.shutdown(false).await;
    }

    #[async_test]
    async fn test_error_handling(driver: DefaultDriver) {
        // Test error handling in various scenarios
        let driver_source = create_test_driver_source(driver);

        let spawner = Arc::new(MockNvmeDriverSpawner::new(
            Duration::from_millis(10),
            Duration::from_millis(10),
        ));

        let manager = NvmeManager::new(&driver_source, 4, false, None, spawner.clone());
        let client = manager.client().clone();

        // Test spawner creation failure
        spawner.set_fail_create(true);
        let result = client.get_namespace("failing-device".to_string(), 1).await;
        assert!(result.is_err());

        // Reset and create a working device
        spawner.set_fail_create(false);
        let _ = client.get_namespace("working-device".to_string(), 1).await;

        // Test namespace operation failure
        let driver = spawner.get_driver("working-device").unwrap();
        driver.set_fail_namespace(true);

        let result = client.get_namespace("working-device".to_string(), 2).await;
        assert!(result.is_err());

        manager.shutdown(false).await;
    }

    #[async_test]
    async fn test_shutdown_before_operations(driver: DefaultDriver) {
        // Test that operations fail gracefully after shutdown
        let driver_source = create_test_driver_source(driver);

        let spawner = Arc::new(MockNvmeDriverSpawner::new(
            Duration::from_millis(10),
            Duration::from_millis(10),
        ));

        let manager = NvmeManager::new(&driver_source, 4, false, None, spawner.clone());
        let client = manager.client().clone();

        // Shutdown immediately
        manager.shutdown(false).await;

        // Now try to use the client - should fail gracefully
        let result = client.get_namespace("test-device".to_string(), 1).await;
        assert!(result.is_err());
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("nvme manager is shut down")
        );
    }

    #[async_test]
    async fn test_concurrent_namespace_timing(driver: DefaultDriver) {
        // More focused test on timing to prove concurrency
        let driver_source = create_test_driver_source(driver);

        let spawner = Arc::new(MockNvmeDriverSpawner::new(
            Duration::from_millis(200), // Longer delay to make timing differences clear
            Duration::from_millis(50),
        ));

        let manager = NvmeManager::new(&driver_source, 4, false, None, spawner.clone());
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

        let spawner = Arc::new(MockNvmeDriverSpawner::new(
            Duration::from_millis(10),
            Duration::from_millis(10),
        ));

        let manager = NvmeManager::new(&driver_source, 4, false, None, spawner.clone());
        let client = manager.client().clone();

        // Create some devices by calling GetNamespace
        let device_ids = vec!["inspect-device-1", "inspect-device-2", "inspect-device-3"];
        for pci_id in device_ids {
            let _ = client.get_namespace(pci_id.into(), 1).await; // Ignore mock "error"
        }

        // Verify devices were created
        assert_eq!(spawner.driver_count(), 3);

        let mut i = InspectionBuilder::new("/").inspect(&manager);

        i.resolve().await;

        // For example:
        // {"devices":{"inspect-device-1":{..},"inspect-device-2":{..},"inspect-device-3":{..}},"spawner":{},"force_load_pci_id":"","save_restore_supported":false,"vp_count":4}
        let results = i.results();
        let string = results.to_string();
        assert!(string.contains("devices"));
        assert!(string.contains("inspect-device-1"));
        assert!(string.contains("inspect-device-2"));
        assert!(string.contains("inspect-device-3"));
        assert!(string.contains("vp_count"));

        manager.shutdown(false).await;
    }

    #[async_test]
    async fn test_rpc_channel_errors_after_driver_manager_shutdown(driver: DefaultDriver) {
        // Test RPC channel errors when trying to use client after driver manager shutdown
        let driver_source = create_test_driver_source(driver);
        let spawner = Arc::new(MockNvmeDriverSpawner::new(
            Duration::from_millis(10),
            Duration::from_millis(10),
        ));

        // Create a driver manager
        let driver_manager =
            NvmeDriverManager::new(&driver_source, "0000:00:04.0", 4, false, None, spawner)
                .unwrap();

        let client = driver_manager.client().clone();

        // First load the driver successfully
        client.load_driver().await.unwrap();

        // Shutdown the driver manager (closes the worker and mesh channel)
        driver_manager
            .shutdown(device_manager::NvmeDriverShutdownOptions {
                do_not_reset: false,
                skip_device_shutdown: false,
            })
            .await;

        // Now try to use the client - should get channel error
        let result = client.get_namespace(1).await;
        assert!(result.is_err());
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("nvme device manager worker is shut down")
        );

        let result = client.load_driver().await;
        assert!(result.is_err());
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("nvme device manager worker is shut down")
        );

        let result = client.save().await;
        assert!(result.is_err());
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("nvme device manager worker is shut down")
        );
    }

    #[async_test]
    async fn test_shutdown_flag_errors_during_load(driver: DefaultDriver) {
        // Test shutdown flag check errors when manager is shut down
        let driver_source = create_test_driver_source(driver);
        let spawner = Arc::new(MockNvmeDriverSpawner::new(
            Duration::from_millis(10),
            Duration::from_millis(10),
        ));

        let manager = NvmeManager::new(&driver_source, 4, false, None, spawner);

        let client = manager.client().clone();

        // Shutdown the manager (sets shutdown flag)
        manager.shutdown(false).await;

        // Try to get namespace for new device - will try to load driver and hit shutdown check
        let result = client.get_namespace("0000:00:04.0".to_string(), 1).await;
        assert!(result.is_err());
        let error_msg = result.unwrap_err().to_string();
        assert!(
            error_msg.contains("nvme device manager worker is shut down")
                || error_msg.contains("nvme manager is shut down")
        );
    }

    #[async_test]
    async fn test_driver_manager_shutdown_during_operations(driver: DefaultDriver) {
        // Test multiple concurrent operations when driver manager is shut down
        let driver_source = create_test_driver_source(driver);
        let spawner = Arc::new(MockNvmeDriverSpawner::new(
            Duration::from_millis(20),
            Duration::from_millis(10),
        ));

        let driver_manager = NvmeDriverManager::new(
            &driver_source,
            "0000:00:05.0",
            4,
            true, // save_restore_supported
            None,
            spawner,
        )
        .unwrap();

        let client = driver_manager.client().clone();

        // Load driver first
        client.load_driver().await.unwrap();

        // Start operations concurrently
        let get_ns_future = {
            let client = client.clone();
            async move { client.get_namespace(1).await }
        };

        let save_future = {
            let client = client.clone();
            async move { client.save().await }
        };

        // Start shutdown
        let shutdown_future = async move {
            driver_manager
                .shutdown(device_manager::NvmeDriverShutdownOptions {
                    do_not_reset: true,
                    skip_device_shutdown: false,
                })
                .await;
        };

        // Run concurrently using basic join
        let (operations_result, _) = futures::future::join(
            async {
                let get_ns_result = get_ns_future.await;
                let save_result = save_future.await;
                (get_ns_result, save_result)
            },
            shutdown_future,
        )
        .await;

        let (get_ns_result, save_result) = operations_result;

        // At least one operation should fail with shutdown error
        let mut has_shutdown_error = false;

        if let Err(e) = get_ns_result {
            if e.to_string()
                .contains("nvme device manager worker is shut down")
            {
                has_shutdown_error = true;
            }
        }

        if let Err(e) = save_result {
            if e.to_string()
                .contains("nvme device manager worker is shut down")
            {
                has_shutdown_error = true;
            }
        }

        // We expect at least one operation to see the shutdown
        assert!(has_shutdown_error, "Expected at least one shutdown error");
    }

    #[async_test]
    async fn test_get_namespace_no_client_after_failed_load(driver: DefaultDriver) {
        // Test the specific case where get_namespace can't get a client after load fails
        let driver_source = create_test_driver_source(driver);
        let spawner = Arc::new(MockNvmeDriverSpawner::new(
            Duration::from_millis(10),
            Duration::from_millis(10),
        ));

        // Set spawner to fail creation
        spawner.set_fail_create(true);

        let manager = NvmeManager::new(&driver_source, 4, false, None, spawner);

        let client = manager.client().clone();

        // Shutdown manager to ensure load_driver fails
        manager.shutdown(false).await;

        // Try to get namespace - load_driver will fail, leaving no client
        let result = client.get_namespace("0000:00:07.0".to_string(), 1).await;
        assert!(result.is_err());
        let error = result.unwrap_err().to_string();
        assert!(
            error.contains("nvme device manager worker is shut down")
                || error.contains("nvme manager is shut down")
        );
    }

    #[async_test]
    async fn test_multiple_shutdown_scenarios(driver: DefaultDriver) {
        // Test various shutdown timing scenarios
        let driver_source = create_test_driver_source(driver);
        let spawner = Arc::new(MockNvmeDriverSpawner::new(
            Duration::from_millis(10),
            Duration::from_millis(10),
        ));

        // Test 1: Shutdown before any operations
        {
            let manager = NvmeManager::new(&driver_source, 4, false, None, spawner.clone());
            let client = manager.client().clone();

            manager.shutdown(false).await;

            let result = client.get_namespace("test1".to_string(), 1).await;
            assert!(result.is_err());
            assert!(result.unwrap_err().to_string().contains("shut down"));
        }

        // Test 2: Shutdown after successful operations
        {
            let manager = NvmeManager::new(&driver_source, 4, false, None, spawner.clone());
            let client = manager.client().clone();

            // This will fail due to mock, but should create the driver manager
            let _ = client.get_namespace("test2".to_string(), 1).await;

            manager.shutdown(false).await;

            // Try another operation after shutdown
            let result = client.get_namespace("test3".to_string(), 1).await;
            assert!(result.is_err());
            assert!(result.unwrap_err().to_string().contains("shut down"));
        }
    }
}
