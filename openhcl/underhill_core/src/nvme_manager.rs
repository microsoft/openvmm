// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Provides access to NVMe namespaces that are backed by the user-mode NVMe
//! VFIO driver. Keeps track of all the NVMe drivers.

use crate::servicing::NvmeSavedState;
use anyhow::Context;
use async_trait::async_trait;
use disk_backend::resolve::ResolveDiskParameters;
use disk_backend::resolve::ResolvedDisk;
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
use user_driver::memory::MemoryBlock;
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
    #[error("failed to create dma buffer for device")]
    DmaBuffer(#[source] anyhow::Error),
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
        dma_buffer_spawner: Box<dyn Fn(String) -> anyhow::Result<Arc<dyn VfioDmaBuffer>> + Send>,
        mem_block: MemoryBlock,
        saved_state: Option<NvmeSavedState>,
    ) -> Self {
        let (send, recv) = mesh::channel();
        let driver = driver_source.simple();
        let mut worker = NvmeManagerWorker {
            driver_source: driver_source.clone(),
            devices: HashMap::new(),
            vp_count,
            dma_buffer_spawner,
            mem_block,
            mem_next_offset: 0,
            nvme_keepalive: false, // Will be set to true after save completed.
        };
        let task = driver.spawn(
            "nvme-manager",
            async move {
                // Restore saved data (if present) before async worker thread runs.
                if saved_state.is_some() {
                    let _ = NvmeManager::restore(
                        &mut worker,
                        dma_buffer.clone(),
                        saved_state.as_ref().unwrap())
                    .instrument(tracing::info_span!("nvme_manager_restore"))
                    .await;
                };
                worker.run(recv).await
            }
        );
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
            Err(anyhow::Error::from(SaveRestoreError::ExplicitlyDisabled {}))
        }
    }

    /// Restore NVMe manager's state after servicing.
    async fn restore(
        worker: &mut NvmeManagerWorker,
        dma_buffer: Arc<dyn VfioDmaBuffer>,
        saved_state: &NvmeSavedState,
    ) -> anyhow::Result<()> {
        worker.restore(
            dma_buffer,
            &saved_state.nvme_state,
        )
        .instrument(tracing::info_span!("nvme_worker_restore"))
        .await?;

        Ok(())
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
    // TODO: Revisit this Box<fn> into maybe a trait, once we refactor DMA to a
    // central manager.
    #[inspect(skip)]
    dma_buffer_spawner: Box<dyn Fn(String) -> anyhow::Result<Arc<dyn VfioDmaBuffer>> + Send>,
    vp_count: u32,
    /// Contiguous DMA memory block to be sliced per queue.
    #[inspect(skip)]
    mem_block: MemoryBlock,
    /// Next available offset to use.
    mem_next_offset: usize,
    /// Bypass device shutdown.
    nvme_keepalive: bool,
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

        // When nvme_keepalive flag is set then this block is unreachable
        // because the Shutdown request is never sent.
        //
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
            hash_map::Entry::Occupied(entry) => {
                let existing_driver = entry.into_mut();
                let _ = existing_driver.attach()
                    .instrument(tracing::info_span!(
                    "nvme_driver_attach",
                    pci_id = pci_id
                ))
                .await
                .map_err(InnerError::Vfio);

                existing_driver
            },
            hash_map::Entry::Vacant(entry) => {
                let device = VfioDevice::new(
                    &self.driver_source,
                    entry.key(),
                    (self.dma_buffer_spawner)(format!("nvme_{}", entry.key()))
                        .map_err(InnerError::DmaBuffer)?,
                )
                .instrument(tracing::info_span!("vfio_device_open", pci_id))
                .await
                .map_err(InnerError::Vfio)?;

                // TODO: This is temporary way to obtain the DMA buffer size.
                //       Alternatives: use VTL2 configuation data for fixed size
                //       or for the target queue count.
                let mem_required_size =
                    nvme_driver::NvmeDriver::<VfioDevice>::required_dma_size(
                        self.vp_count as usize + 1
                    );
                let next_offset = self.mem_next_offset;

                let driver =
                    nvme_driver::NvmeDriver::new(
                            &self.driver_source,
                            self.vp_count,
                            self.mem_block.subblock(next_offset, mem_required_size),
                            device)
                        .instrument(tracing::info_span!(
                            "nvme_driver_init",
                            pci_id = entry.key()
                        ))
                        .await
                        .map_err(InnerError::DeviceInitFailed)?;

                self.mem_next_offset += mem_required_size;
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
            mem_buffer: Some(NvmeDmaBufferSavedState {
                dma_base: self.mem_block.base_va(),
                dma_size: self.mem_block.len(),
                pfns: self.mem_block.pfns().to_vec(),
            }),
            mem_next_offset: self.mem_next_offset,
            nvme_disks,
        };

        // Bypass device shutdown.
        self.nvme_keepalive = true;

        Ok(nvme_state)
    }

    /// Restore NVMe manager and device states from the buffer after servicing.
    pub async fn restore(
        &mut self,
        dma_buffer: Arc<dyn VfioDmaBuffer>,
        saved_state: &NvmeManagerSavedState,
    ) -> anyhow::Result<()> {
        self.devices = HashMap::new();
        for disk in &saved_state.nvme_disks {
            let vfio_device =
                // This code can wait on each VFIO device until it is arrived.
                // A potential optimization would be to delay VFIO operation
                // until it is ready, but a redesign of VfioDevice is needed.
                VfioDevice::restore(
                    &self.driver_source,
                    &disk.pci_id.clone(),
                    dma_buffer.clone(),
                    Some(&disk.driver_state.vfio_state),
                )
                .instrument(tracing::info_span!("vfio_device_restore", pci_id = disk.pci_id.clone()))
                .await?;

            // FIXME: Count for multiple drivers.
            let driver_block_len = self.mem_block.len();
            let driver_mem_block = self
                .mem_block
                .subblock(0, driver_block_len);
            let nvme_driver = nvme_driver::NvmeDriver::restore(
                &self.driver_source,
                saved_state.cpu_count,
                driver_mem_block,
                vfio_device,
                &disk.driver_state,
            )
            .instrument(tracing::info_span!("nvme_driver_restore"))
            .await?;

            self.devices.insert(disk.pci_id.clone(), nvme_driver);
        }
        self.mem_next_offset = saved_state.mem_next_offset;
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

        Ok(ResolvedDisk::new(disk_nvme::NvmeDisk::new(namespace).clone()).context("invalid disk")?)
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
    /// NVMe DMA buffer saved state.
    #[mesh(2)]
    pub mem_buffer: Option<NvmeDmaBufferSavedState>,
    /// NVMe DMA buffer next offset.
    #[mesh(3)]
    pub mem_next_offset: usize,
    #[mesh(4)]
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
