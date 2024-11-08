// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Provides access to NVMe namespaces that are backed by the user-mode NVMe
//! VFIO driver. Keeps track of all the NVMe drivers.

use anyhow::Context;
use async_trait::async_trait;
use disk_backend::resolve::ResolveDiskParameters;
use disk_backend::resolve::ResolvedSimpleDisk;
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
use user_driver::vfio::VfioDmaBuffer;
use virt_mshv_vtl::UhPartition;
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
        dma_bounce_buffer_pages_per_queue: u64,
        dma_bounce_buffer_pages_per_io_threshold: Option<u32>,
        partition: Option<Arc<UhPartition>>,
    ) -> Self {
        let (send, recv) = mesh::channel();
        let driver = driver_source.simple();
        let mut worker = NvmeManagerWorker {
            driver_source: driver_source.clone(),
            devices: HashMap::new(),
            vp_count,
            dma_buffer,
            dma_bounce_buffer_pages_per_queue,
            dma_bounce_buffer_pages_per_io_threshold,
            partition,
        };
        let task = driver.spawn("nvme-manager", async move { worker.run(recv).await });
        Self {
            task,
            client: NvmeManagerClient {
                sender: Arc::new(send),
            },
        }
    }

    pub fn client(&self) -> &NvmeManagerClient {
        &self.client
    }

    pub async fn shutdown(self, nvme_keepalive: bool) {
        //
        // Early return would be the fastest way to skip shutdown.
        // Unfortunately, then there is no good way to prevent
        // controller reset in the drop() fn if we early return here.
        //
        // TODO: Figure out how to uncomment this. Maybe just don't reset the ctrl in drop().
        //
        // if nvme_keepalive == true { return }
        self.client.sender.send(Request::Shutdown {
            span: tracing::info_span!("shutdown_nvme_manager"),
            nvme_keepalive,
        });
        self.task.await;
    }
}

enum Request {
    Inspect(inspect::Deferred),
    ForceLoadDriver(inspect::DeferredUpdate),
    GetNamespace(Rpc<(String, u32, tracing::Span), Result<nvme_driver::Namespace, NamespaceError>>),
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
    dma_bounce_buffer_pages_per_queue: u64,
    dma_bounce_buffer_pages_per_io_threshold: Option<u32>,
    partition: Option<Arc<UhPartition>>,
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

                let driver = nvme_driver::NvmeDriver::new(
                    &self.driver_source,
                    self.vp_count,
                    device,
                    self.dma_bounce_buffer_pages_per_queue,
                    self.dma_bounce_buffer_pages_per_io_threshold,
                    self.partition.clone(),
                )
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

        Ok(disk_nvme::NvmeDisk::new(namespace).into())
    }
}

#[derive(MeshPayload)]
pub struct NvmeDiskConfig {
    pub pci_id: String,
    pub nsid: u32,
}

impl ResourceId<DiskHandleKind> for NvmeDiskConfig {
    const ID: &'static str = "nvme";
}
