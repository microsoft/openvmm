// Copyright (C) Microsoft Corporation. All rights reserved.

//! Provides access to NVMe namespaces that are backed by the user-mode NVMe
//! VFIO driver. Keeps track of all the NVMe drivers.

use crate::servicing::NvmeSavedState;
use anyhow::Context;
use async_trait::async_trait;
use disk_backend::resolve::ResolveDiskParameters;
use disk_backend::resolve::ResolvedSimpleDisk;
use futures::future::join_all;
use futures::StreamExt;
use futures::TryFutureExt;
use inspect::Inspect;
use mesh::payload::Protobuf;
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
use user_driver::vfio::VfioDmaBuffer;
use vm_resource::kind::DiskHandleKind;
use vm_resource::AsyncResolveResource;
use vm_resource::ResourceId;
use vm_resource::ResourceResolver;
use vmcore::save_restore::SavedStateRoot;
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
    #[error("failed to get namespace {nsid}")]
    Namespace {
        nsid: u32,
        #[source]
        source: nvme_driver::NamespaceError,
    },
}

/// Save/restore errors.
#[derive(Debug, Error)]
pub enum SaveRestoreError {
    #[error("save explicitly disabled")]
    ExplicitlyDisabled,
}

#[derive(Debug)]
pub struct NvmeManager {
    task: Task<()>,
    client: NvmeManagerClient,
    /// Flags controlling servicing behavior.
    nvme_keepalive: bool,
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
        dma_buffer: Arc<dyn VfioDmaBuffer>,
        saved_state: Option<NvmeSavedState>,
    ) -> Self {
        let (send, recv) = mesh::channel();
        let driver = driver_source.simple();
        let mut worker = NvmeManagerWorker {
            driver_source: driver_source.clone(),
            devices: HashMap::new(),
            vp_count,
            dma_buffer,
        };
        worker
            .restore(saved_state)
            .expect("unable to restore nvme state");
        let task = driver.spawn("nvme-manager", async move { worker.run(recv).await });
        Self {
            task,
            client: NvmeManagerClient {
                sender: Arc::new(send),
            },
            nvme_keepalive: true,
        }
    }

    pub fn client(&self) -> &NvmeManagerClient {
        &self.client
    }

    pub async fn shutdown(self) {
        // Early return would be the fastest way to skip shutdown.
        // Unfortunately, then there is no good way to prevent
        // controller reset in the drop() fn if we early return here.
        //
        // TODO: Figure out how to uncomment this. Maybe just don't reset the ctrl in drop().
        //
        // if nvme_keepalive == true { return }
        self.client.sender.send(Request::Shutdown {
            span: tracing::info_span!("shutdown_nvme_manager"),
            nvme_keepalive: self.nvme_keepalive,
        });
        self.task.await;
    }

    /// Save NVMe manager's state during servicing.
    pub async fn save(&self) -> anyhow::Result<NvmeManagerSavedState> {
        // NVMe manager has no own data to save, everything will be done
        // in the Worker task which can be contacted through Client.
        if self.nvme_keepalive {
            self.client().save().await
        } else {
            // If nvme_keepalive was explicitly disabled,
            // return an error which is non-fatal indication
            // that there is no save data.
            return Err(anyhow::Error::from(SaveRestoreError::ExplicitlyDisabled {}));
        }
    }

    /// Control servicing behavior: to keep the attached device intact or not.
    pub fn set_nvme_keepalive(&mut self, nvme_keepalive: bool) {
        self.nvme_keepalive = nvme_keepalive;
    }
}

enum Request {
    Inspect(inspect::Deferred),
    ForceLoadDriver(inspect::DeferredUpdate),
    GetNamespace(
        Rpc<(String, u32, tracing::Span), Result<Arc<nvme_driver::Namespace>, NamespaceError>>,
    ),
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
    ) -> anyhow::Result<Arc<nvme_driver::Namespace>> {
        Ok(self
            .sender
            .call(
                Request::GetNamespace,
                (
                    pci_id.clone(),
                    nsid,
                    tracing::info_span!("nvme_get_namespace", pci_id, nsid),
                ),
            )
            .await
            .context("nvme manager is shut down")??)
    }

    /// Send an RPC call to save NVMe worker data.
    pub async fn save(&self) -> anyhow::Result<NvmeManagerSavedState> {
        self.sender.call(Request::Save, ()).await?
    }
}

#[derive(Inspect)]
struct NvmeManagerWorker {
    #[inspect(skip)]
    driver_source: VmTaskDriverSource,
    #[inspect(iter_by_key)]
    devices: HashMap<String, nvme_driver::NvmeDriver<VfioDevice>>,
    #[inspect(skip)]
    dma_buffer: Arc<dyn VfioDmaBuffer>,
    vp_count: u32,
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
                    rpc.handle(|(pci_id, nsid, span)| {
                        self.get_namespace(pci_id.clone(), nsid)
                            .map_err(|source| NamespaceError { pci_id, source })
                            .instrument(span)
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
                    // Update the flag for all connected devices.
                    for (_s, dev) in self.devices.iter_mut() {
                        // Prevent devices from originating controller reset in drop().
                        dev.update_servicing_flags(nvme_keepalive);
                    }
                    break (span, nvme_keepalive);
                }
            }
        };

        // Tear down all the devices if nvme_keepalive is not set.
        if !nvme_keepalive {
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
                let device =
                    VfioDevice::new(&self.driver_source, entry.key(), self.dma_buffer.clone())
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
    ) -> Result<Arc<nvme_driver::Namespace>, InnerError> {
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
                driver_state: driver.save().await?,
            });
        }

        let nvme_state = NvmeManagerSavedState {
            cpu_count: self.vp_count,
            nvme_disks,
        };

        Ok(nvme_state)
    }

    /// Restore NVMe manager worker state after servicing.
    pub fn restore(&mut self, _saved_state: Option<NvmeSavedState>) -> anyhow::Result<()> {
        // TODO: A placeholder for the next update.
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
    type Output = ResolvedSimpleDisk;
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

        Ok(disk_nvme::NvmeDisk::new(namespace.clone()).into())
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

#[derive(Protobuf)]
#[mesh(package = "underhill")]
pub struct NvmeDmaBufferSavedState {
    /// GVA of the DMA buffer assigned to NVMe device(s).
    #[mesh(1)]
    pub dma_base: u64,
    /// Total size of DMA buffer in bytes.
    #[mesh(2)]
    pub dma_size: usize,
    /// List of PFNs for this DMA buffer.
    #[mesh(3)]
    pub pfns: Vec<u64>,
}
