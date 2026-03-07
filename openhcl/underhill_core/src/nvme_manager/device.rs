// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use crate::nvme_manager::CreateNvmeDriver;
use crate::nvme_manager::NamespaceError;
use crate::nvme_manager::NvmeDevice;
use crate::nvme_manager::NvmeSpawnerError;
use anyhow::Context;
use async_trait::async_trait;
use futures::StreamExt;
use inspect::Deferred;
use inspect::Inspect;
use mesh::rpc::Rpc;
use mesh::rpc::RpcError;
use mesh::rpc::RpcSend;
use nvme_driver::save_restore::NvmeDriverSavedState;
use openhcl_dma_manager::AllocationVisibility;
use openhcl_dma_manager::DmaClientParameters;
use openhcl_dma_manager::DmaClientSpawner;
use openhcl_dma_manager::LowerVtlPermissionPolicy;
use openhcl_dma_manager::OpenhclDmaClient;
use pal_async::task::Spawn;
use pal_async::task::Task;
use std::sync::Arc;
use tracing::Instrument;
use tracing::Span;
use user_driver::DmaClient;
use user_driver::vfio::PciDeviceResetMethod;
use user_driver::vfio::VfioDevice;
use user_driver::vfio::VfioDmaClients;
use user_driver::vfio::vfio_set_device_reset_method;
use vmcore::vm_task::VmTaskDriverSource;

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
        &mut self,
        nsid: u32,
    ) -> Result<nvme_driver::NamespaceHandle, nvme_driver::NamespaceError> {
        self.driver.namespace(nsid).await
    }

    /// Save the NVMe driver state.
    async fn save(&mut self) -> anyhow::Result<NvmeDriverSavedState> {
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
        mut saved_state: Option<&NvmeDriverSavedState>,
    ) -> Result<Box<dyn NvmeDevice>, NvmeSpawnerError> {
        // Gracefully tear down old state & reset device if a saved state is
        // present but the host doesn't support keepalive. The device should be
        // reset automatically when the Vfio device handle is dropped.
        if saved_state.is_some() && !save_restore_supported {
            tracing::warn!(
                pci_id = pci_id,
                "received a saved state but keepalive is not supported; clearing existing device state"
            );
            // Create a persistent DMA client to attach & drop pending buffers.
            let persistent_dma_client = self.create_dma_client(pci_id, true)?;
            let _ = persistent_dma_client.attach_pending_buffers().map_err(|e| {
                tracing::warn!(
                    pci_id = pci_id,
                    error = %e,
                    "failed to attach pending DMA buffers when clering existing nvme device state",
                )
            });

            let vfio_device = match VfioDevice::restore(
                driver_source,
                pci_id,
                true,
                VfioDmaClients::PersistentOnly(persistent_dma_client),
            )
            .instrument(tracing::info_span!(
                "nvme_vfio_device_restore",
                pci_id,
                clearing_device = true
            ))
            .await
            {
                Ok(dev) => Some(dev),
                Err(err) => {
                    tracing::warn!(
                        pci_id = pci_id,
                        error = %err,
                        "failed to restore VFIO device for clearing existing NVMe state. proceeding without clearing"
                    );
                    None
                }
            };

            if let Some(vfio_device) = vfio_device {
                if let Err(e) =
                    nvme_driver::NvmeDriver::clear_existing_state(driver_source, vfio_device).await
                {
                    tracing::warn!(
                        pci_id = pci_id,
                        error = %e,
                        "failed to clear existing NVMe device state",
                    );
                }
            }

            // Always discard existing state to prevent restore (because keepalive is
            // not supported)
            let _ = saved_state.take();
        }

        let dma_client = self.create_dma_client(pci_id, save_restore_supported)?;
        let dma_clients = if save_restore_supported {
            VfioDmaClients::PersistentOnly(dma_client)
        } else {
            VfioDmaClients::EphemeralOnly(dma_client)
        };

        let nvme_driver = if let Some(saved_state) = saved_state {
            // On restore, always disable FLR. This isn't necessary for the attach path (setting "keepalive"
            // is sufficient), but *is* necessary to avoid Vfio issuing a reset to the device on shutdown.
            tracing::debug!(pci_id = pci_id, "Disabling FLR for NVMe device restore");
            Self::try_update_reset_method(pci_id, PciDeviceResetMethod::NoReset, "nvme_restore");

            let vfio_device = VfioDevice::restore(driver_source, pci_id, true, dma_clients)
                .instrument(tracing::info_span!(
                    "nvme_vfio_device_restore",
                    pci_id,
                    clearing_device = false
                ))
                .await
                .with_context(|| format!("failed to restore vfio device for {}", pci_id))
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
                dma_clients,
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
    fn create_dma_client(
        &self,
        pci_id: &str,
        save_restore_supported: bool,
    ) -> Result<Arc<OpenhclDmaClient>, NvmeSpawnerError> {
        self.dma_client_spawner
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
            .map_err(NvmeSpawnerError::DmaClient)
    }

    async fn create_nvme_device(
        driver_source: &VmTaskDriverSource,
        pci_id: &str,
        vp_count: u32,
        nvme_always_flr: bool,
        is_isolated: bool,
        dma_clients: VfioDmaClients,
    ) -> Result<nvme_driver::NvmeDriver<VfioDevice>, NvmeSpawnerError> {
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
            Self::try_update_reset_method(pci_id, *reset_method, "nvme_create");
            match Self::try_create_nvme_device(
                driver_source,
                pci_id,
                vp_count,
                is_isolated,
                dma_clients.clone(),
            )
            .await
            {
                Ok(device) => {
                    if !nvme_always_flr && !matches!(reset_method, PciDeviceResetMethod::NoReset) {
                        // For shutdown: set the reset method to NoReset now that the device
                        // has been successfully created.
                        Self::try_update_reset_method(
                            pci_id,
                            PciDeviceResetMethod::NoReset,
                            "nvme_create_post",
                        );
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
        dma_clients: VfioDmaClients,
    ) -> Result<nvme_driver::NvmeDriver<VfioDevice>, NvmeSpawnerError> {
        let device = VfioDevice::new(driver_source, pci_id, dma_clients)
            .instrument(tracing::info_span!("nvme_vfio_device_open", pci_id))
            .await
            .with_context(|| format!("failed to create vfio device for {}", pci_id))
            .map_err(NvmeSpawnerError::Vfio)?;

        // TODO: For now, any isolation means use bounce buffering. This
        // needs to change when we have nvme devices that support DMA to
        // confidential memory.
        nvme_driver::NvmeDriver::new(driver_source, vp_count, device, is_isolated)
            .instrument(tracing::info_span!("nvme_driver_new", pci_id))
            .await
            .map_err(NvmeSpawnerError::DeviceInitFailed)
    }

    fn try_update_reset_method(pci_id: &str, method: PciDeviceResetMethod, label: &str) {
        if let Err(err) = vfio_set_device_reset_method(pci_id, method) {
            tracing::warn!(
                label,
                ?pci_id,
                ?method,
                err = &err as &dyn std::error::Error,
                "failed to update reset_method",
            );
        }
    }
}

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
    GetNamespace(Rpc<(Span, u32), Result<nvme_driver::NamespaceHandle, NamespaceError>>),
    Save(Rpc<Span, anyhow::Result<NvmeDriverSavedState>>),
    /// Shutdown the NVMe driver, and the manager of that driver.
    /// Takes the span, and a set of options.
    Shutdown(Rpc<(Span, NvmeDriverShutdownOptions), ()>),
}

pub struct NvmeDriverManager {
    task: Task<()>,
    pci_id: String,
    client: NvmeDriverManagerClient,
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
    #[inspect(skip)]
    pci_id: String,
    #[inspect(flatten, send = "NvmeDriverRequest::Inspect")]
    sender: mesh::Sender<NvmeDriverRequest>,
}

impl NvmeDriverManagerClient {
    pub async fn get_namespace(&self, nsid: u32) -> anyhow::Result<nvme_driver::NamespaceHandle> {
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
                        .await
                }
                NvmeDriverRequest::GetNamespace(rpc) => {
                    rpc.handle(async |(_, nsid)| {
                        self.driver
                            .as_mut()
                            .unwrap()
                            .namespace(nsid)
                            .await
                            .map_err(|source| NamespaceError {
                                pci_id: self.pci_id.clone(),
                                source: NvmeSpawnerError::Namespace { nsid, source },
                            })
                    })
                    .await
                }
                NvmeDriverRequest::Save(rpc) => {
                    rpc.handle(async |_span| self.driver.as_mut().unwrap().save().await)
                        .await
                }
                NvmeDriverRequest::Shutdown(rpc) => {
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
                        .await;

                    break;
                }
            }
        }
    }
}
