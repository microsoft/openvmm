// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Provides access to NVMe namespaces that are backed by the user-mode NVMe
//! VFIO driver. Keeps track of all the NVMe drivers.

use crate::dma_manager::DmaClientSpawner;
use crate::nvme_manager::save_restore::NvmeManagerSavedState;
use crate::nvme_manager::save_restore::NvmeSavedDiskConfig;
use crate::servicing::NvmeSavedState;
use anyhow::Context;
use async_trait::async_trait;
use disk_backend::resolve::ResolveDiskParameters;
use disk_backend::resolve::ResolvedDisk;
use futures::future::join_all;
use futures::StreamExt;
use futures::TryFutureExt;
use inspect::Inspect;
use mesh::rpc::Rpc;
use mesh::rpc::RpcSend;
use mesh::MeshPayload;
use pal_async::task::Spawn;
use pal_async::task::Task;
use std::collections::hash_map;
use std::collections::HashMap;
use std::sync::Arc;
use thiserror::Error;
use tracing::Instrument;
use user_driver::vfio::VfioDevice;
use vm_resource::kind::DiskHandleKind;
use vm_resource::AsyncResolveResource;
use vm_resource::ResourceId;
use vm_resource::ResourceResolver;
use vmcore::vm_task::VmTaskDriverSource;

#[derive(Debug, Error)]
#[error("nvme device {pci_id} error")]
pub struct NamespaceError {
    pci_id: String,
    #[source]
    source: InnerError,
}

#[derive(Debug, Error)]
enum InnerError {
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
            Err(req) => req.value("".into()),
        });
        // Send the remaining fields directly to the worker.
        self.client
            .sender
            .send(Request::Inspect(resp.request().defer()));
    }
}

impl NvmeManager {
    pub fn new(
        driver_source: &VmTaskDriverSource,
        vp_count: u32,
        save_restore_supported: bool,
        saved_state: Option<NvmeSavedState>,
        dma_client_spawner: DmaClientSpawner,
    ) -> Self {
        let (send, recv) = mesh::channel();
        let driver = driver_source.simple();
        let mut worker = NvmeManagerWorker {
            driver_source: driver_source.clone(),
            devices: HashMap::new(),
            vp_count,
            save_restore_supported,
            dma_client_spawner,
        };
        let task = driver.spawn("nvme-manager", async move {
            // Restore saved data (if present) before async worker thread runs.
            if saved_state.is_some() {
                let _ = NvmeManager::restore(&mut worker, saved_state.as_ref().unwrap())
                    .instrument(tracing::info_span!("nvme_manager_restore"))
                    .await;
            };
            worker.run(recv).await
        });
        Self {
            task,
            client: NvmeManagerClient {
                sender: Arc::new(send),
            },
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
    sender: Arc<mesh::Sender<Request>>,
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
    devices: HashMap<String, nvme_driver::NvmeDriver<VfioDevice>>,
    vp_count: u32,
    /// Running environment (memory layout) allows save/restore.
    save_restore_supported: bool,
    #[inspect(skip)]
    dma_client_spawner: DmaClientSpawner,
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
                            let pci_id = update.new_value().into();
                            update.succeed(pci_id);
                        }
                        Err(err) => {
                            update.fail(err);
                        }
                    }
                }
                Request::GetNamespace(rpc) => {
                    rpc.handle(|(pci_id, nsid)| {
                        self.get_namespace(pci_id.clone(), nsid)
                            .map_err(|source| NamespaceError { pci_id, source })
                    })
                    .await
                }
                // Request to save worker data for servicing.
                Request::Save(rpc) => {
                    rpc.handle(|_| self.save())
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
                join_all(self.devices.drain().map(|(pci_id, driver)| {
                    driver
                        .shutdown()
                        .instrument(tracing::info_span!("shutdown_nvme_driver", pci_id))
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
    ) -> Result<&mut nvme_driver::NvmeDriver<VfioDevice>, InnerError> {
        let driver = match self.devices.entry(pci_id.to_owned()) {
            hash_map::Entry::Occupied(entry) => entry.into_mut(),
            hash_map::Entry::Vacant(entry) => {
                let dma_client = self
                    .dma_client_spawner
                    .create_client(format!("nvme_{}", pci_id))
                    .map_err(InnerError::DmaClient)?;

                let device = VfioDevice::new(&self.driver_source, entry.key(), dma_client)
                    .instrument(tracing::info_span!("vfio_device_open", pci_id))
                    .await
                    .map_err(InnerError::Vfio)?;

                let driver =
                    nvme_driver::NvmeDriver::new(&self.driver_source, self.vp_count, device)
                        .instrument(tracing::info_span!(
                            "nvme_driver_init",
                            pci_id = entry.key()
                        ))
                        .await
                        .map_err(InnerError::DeviceInitFailed)?;

                entry.insert(driver)
            }
        };
        Ok(driver)
    }

    async fn get_namespace(
        &mut self,
        pci_id: String,
        nsid: u32,
    ) -> Result<nvme_driver::Namespace, InnerError> {
        let driver = self.get_driver(pci_id.to_owned()).await?;
        driver
            .namespace(nsid)
            .await
            .map_err(|source| InnerError::Namespace { nsid, source })
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

            let dma_client = self
                .dma_client_spawner
                .create_client(format!("nvme_{}", pci_id))?;
            let vfio_device =
                // This code can wait on each VFIO device until it is arrived.
                // A potential optimization would be to delay VFIO operation
                // until it is ready, but a redesign of VfioDevice is needed.
                VfioDevice::restore(
                    &self.driver_source,
                    &disk.pci_id.clone(),
                    true,
                    dma_client,
                )
                .instrument(tracing::info_span!("vfio_device_restore", pci_id))
                .await?;

            let nvme_driver = nvme_driver::NvmeDriver::restore(
                &self.driver_source,
                saved_state.cpu_count,
                vfio_device,
                &disk.driver_state,
            )
            .instrument(tracing::info_span!("nvme_driver_restore"))
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
