// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Provides access to storvsc driver instances shared between disks on each vStor controller.

use crate::servicing::StorvscSavedState;
use crate::storvsc_manager::save_restore::StorvscManagerSavedState;
use crate::storvsc_manager::save_restore::StorvscSavedDriverConfig;
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
use storvsc_driver::StorvscDriver;
use thiserror::Error;
use tracing::Instrument;
use vm_resource::AsyncResolveResource;
use vm_resource::ResourceId;
use vm_resource::ResourceResolver;
use vm_resource::kind::DiskHandleKind;
use vmbus_user_channel::MappedRingMem;
use vmcore::vm_task::VmTaskDriverSource;

#[derive(Debug, Error)]
#[error("storvsc driver {instance_guid} error")]
pub struct DriverError {
    instance_guid: guid::Guid,
    #[source]
    source: InnerError,
}

#[derive(Debug, Error)]
enum InnerError {
    #[error("failed to initialize vmbus channel")]
    Vmbus(#[source] vmbus_user_channel::Error),
    #[error("failed to initialize storvsc driver")]
    DriverInitFailed(#[source] storvsc_driver::StorvscError),
    #[error("failed to create dma client for device")]
    DmaClient(#[source] anyhow::Error),
}

#[derive(Debug)]
pub struct StorvscManager {
    task: Task<()>,
    client: StorvscManagerClient,
    /// Running environment (memory layout) supports save/restore.
    save_restore_supported: bool,
}

impl Inspect for StorvscManager {
    fn inspect(&self, req: inspect::Request<'_>) {
        let mut resp = req.respond();
        resp.merge(inspect::adhoc(|req| {
            self.client.sender.send(Request::Inspect(req.defer()))
        }));
    }
}

impl StorvscManager {
    pub fn new(
        driver_source: &VmTaskDriverSource,
        save_restore_supported: bool,
        is_isolated: bool,
        saved_state: Option<StorvscSavedState>,
        dma_client_spawner: DmaClientSpawner,
    ) -> Self {
        let (send, recv) = mesh::channel();
        let driver = driver_source.simple();
        let mut worker = StorvscManagerWorker {
            driver_source: driver_source.clone(),
            drivers: HashMap::new(),
            save_restore_supported,
            is_isolated,
            dma_client_spawner,
        };
        let task = driver.spawn("storvsc-manager", async move {
            // Restore saved data (if present) before async worker thread runs.
            if let Some(s) = saved_state.as_ref() {
                if let Err(e) = StorvscManager::restore(&mut worker, s)
                    .instrument(tracing::info_span!("storvsc_manager_restore"))
                    .await
                {
                    tracing::error!(
                        error = e.as_ref() as &dyn std::error::Error,
                        "failed to restore storvsc manager"
                    );
                }
            };
            worker.run(recv).await
        });
        Self {
            task,
            client: StorvscManagerClient { sender: send },
            save_restore_supported,
        }
    }

    pub fn client(&self) -> &StorvscManagerClient {
        &self.client
    }

    pub async fn shutdown(self) {
        self.client.sender.send(Request::Shutdown {
            span: tracing::info_span!("shutdown_storvsc_manager"),
        });
        self.task.await;
    }

    /// Save storvsc manager's state during servicing.
    pub async fn save(&self) -> Option<StorvscManagerSavedState> {
        // The manager has no data to save, so everything will be done in the Worker task which can
        // be contacted through the Client.
        if self.save_restore_supported {
            Some(self.client().save().await?)
        } else {
            None
        }
    }

    /// Restore the storvsc manager's state after servicing.
    async fn restore(
        worker: &mut StorvscManagerWorker,
        saved_state: &StorvscSavedState,
    ) -> anyhow::Result<()> {
        worker
            .restore(&saved_state.storvsc_state)
            .instrument(tracing::info_span!("storvsc_worker_restore"))
            .await?;

        Ok(())
    }
}

enum Request {
    Inspect(inspect::Deferred),
    GetDriver(Rpc<guid::Guid, Result<Arc<StorvscDriver<MappedRingMem>>, DriverError>>),
    Save(Rpc<(), Result<StorvscManagerSavedState, anyhow::Error>>),
    Shutdown { span: tracing::Span },
}

#[derive(Debug, Clone)]
pub struct StorvscManagerClient {
    sender: mesh::Sender<Request>,
}

impl StorvscManagerClient {
    pub async fn get_driver(
        &self,
        instance_guid: guid::Guid,
    ) -> anyhow::Result<Arc<StorvscDriver<MappedRingMem>>> {
        Ok(self
            .sender
            .call(Request::GetDriver, instance_guid)
            .instrument(tracing::info_span!(
                "storvsc_get_driver",
                instance_guid = instance_guid.to_string()
            ))
            .await
            .context("storvsc manager is shutdown")??)
    }

    pub async fn save(&self) -> Option<StorvscManagerSavedState> {
        match self.sender.call(Request::Save, ()).await {
            Ok(s) => s.ok(),
            Err(_) => None,
        }
    }
}

#[derive(Inspect)]
struct StorvscManagerWorker {
    #[inspect(skip)]
    driver_source: VmTaskDriverSource,
    #[inspect(iter_by_key)]
    drivers: HashMap<guid::Guid, Arc<StorvscDriver<MappedRingMem>>>,
    /// Running environment (memory layout) allows save/restore.
    save_restore_supported: bool,
    /// If this VM is isolated or not. This influences DMA client allocations.
    is_isolated: bool,
    #[inspect(skip)]
    dma_client_spawner: DmaClientSpawner,
}

impl StorvscManagerWorker {
    async fn run(&mut self, mut recv: mesh::Receiver<Request>) {
        let join_span = loop {
            let Some(req) = recv.next().await else {
                break tracing::Span::none();
            };
            match req {
                Request::Inspect(deferred) => deferred.inspect(&self),
                Request::GetDriver(rpc) => {
                    rpc.handle(async |instance_guid| {
                        self.get_driver(instance_guid)
                            .map_err(|source| DriverError {
                                instance_guid,
                                source,
                            })
                            .await
                    })
                    .await
                }
                Request::Save(rpc) => {
                    rpc.handle(async |_| self.save().await)
                        .instrument(tracing::info_span!("storvsc_save_state"))
                        .await
                }
                Request::Shutdown { span } => {
                    break span;
                }
            }
        };

        if !self.save_restore_supported {
            async {
                join_all(self.drivers.drain().map(|(guid, driver)| {
                    let guid_str = guid.to_string();
                    async move {
                        driver
                            .stop()
                            .instrument(tracing::info_span!(
                                "shutdown_storvsc_driver",
                                guid = guid_str
                            ))
                            .await
                    }
                }))
                .await
            }
            .instrument(join_span)
            .await;
        }
    }

    async fn get_driver(
        &mut self,
        instance_guid: guid::Guid,
    ) -> Result<Arc<StorvscDriver<MappedRingMem>>, InnerError> {
        let storvsc = match self.drivers.entry(instance_guid) {
            hash_map::Entry::Occupied(entry) => entry.get().clone(),
            hash_map::Entry::Vacant(entry) => {
                let file = vmbus_user_channel::open_uio_device(&instance_guid)
                    .map_err(InnerError::Vmbus)?;
                let channel = vmbus_user_channel::channel(&self.driver_source.simple(), file)
                    .map_err(InnerError::Vmbus)?;

                let dma_client = self
                    .dma_client_spawner
                    .new_client(DmaClientParameters {
                        device_name: format!("storvsc_{}", instance_guid),
                        lower_vtl_policy: LowerVtlPermissionPolicy::Any,
                        allocation_visibility: if self.is_isolated {
                            AllocationVisibility::Shared
                        } else {
                            AllocationVisibility::Private
                        },
                        persistent_allocations: self.save_restore_supported,
                    })
                    .map_err(InnerError::DmaClient)?;

                let mut driver = Arc::new(StorvscDriver::new(dma_client));

                Arc::get_mut(&mut driver)
                    .unwrap()
                    .run(
                        &self.driver_source,
                        channel,
                        storvsp_protocol::ProtocolVersion {
                            major_minor: storvsp_protocol::VERSION_BLUE,
                            reserved: 0,
                        },
                        0, // TODO: Pick right VP
                    )
                    .map_err(InnerError::DriverInitFailed)
                    .await?;

                entry.insert(driver).clone()
            }
        };
        Ok(storvsc)
    }

    /// Saves storvsc driver states into buffer during servicing.
    pub async fn save(&mut self) -> anyhow::Result<StorvscManagerSavedState> {
        let mut storvsc_drivers: Vec<StorvscSavedDriverConfig> = Vec::new();
        for (guid, driver) in self.drivers.iter_mut() {
            storvsc_drivers.push(StorvscSavedDriverConfig {
                instance_guid: *guid,
                driver_state: driver
                    .save()
                    .instrument(tracing::info_span!(
                        "storvsc_driver_save",
                        instance_guid = guid.to_string()
                    ))
                    .await?,
            });
        }

        Ok(StorvscManagerSavedState { storvsc_drivers })
    }

    /// Restores storvsc manager and driver states from the buffer after servicing.
    pub async fn restore(&mut self, saved_state: &StorvscManagerSavedState) -> anyhow::Result<()> {
        self.drivers = HashMap::new();
        for driver_state in &saved_state.storvsc_drivers {
            let file = vmbus_user_channel::open_uio_device(&driver_state.instance_guid)
                .map_err(InnerError::Vmbus)?;
            let channel = vmbus_user_channel::channel(&self.driver_source.simple(), file)
                .map_err(InnerError::Vmbus)?;

            let dma_client = self
                .dma_client_spawner
                .new_client(DmaClientParameters {
                    device_name: format!("storvsc_{}", driver_state.instance_guid),
                    lower_vtl_policy: LowerVtlPermissionPolicy::Any,
                    allocation_visibility: if self.is_isolated {
                        AllocationVisibility::Shared
                    } else {
                        AllocationVisibility::Private
                    },
                    persistent_allocations: self.save_restore_supported,
                })
                .map_err(InnerError::DmaClient)?;

            self.drivers.insert(
                driver_state.instance_guid,
                Arc::new(
                    StorvscDriver::restore(
                        &driver_state.driver_state,
                        &self.driver_source,
                        channel,
                        0,
                        dma_client,
                    )
                    .await?,
                ), // TODO: Pick right VP
            );
        }
        Ok(())
    }
}

pub struct StorvscDiskResolver {
    manager: StorvscManagerClient,
}

impl StorvscDiskResolver {
    pub fn new(manager: StorvscManagerClient) -> Self {
        Self { manager }
    }
}

#[async_trait]
impl AsyncResolveResource<DiskHandleKind, StorvscDiskConfig> for StorvscDiskResolver {
    type Output = ResolvedDisk;
    type Error = anyhow::Error;

    async fn resolve(
        &self,
        _resolver: &ResourceResolver,
        rsrc: StorvscDiskConfig,
        _input: ResolveDiskParameters<'_>,
    ) -> Result<Self::Output, Self::Error> {
        let disk = self
            .manager
            .get_driver(rsrc.instance_guid)
            .await
            .context("could not open storvsc disk")?;

        Ok(
            ResolvedDisk::new(disk_storvsc::StorvscDisk::new(disk, rsrc.lun))
                .context("invalid disk")?,
        )
    }
}

#[derive(MeshPayload, Default)]
pub struct StorvscDiskConfig {
    pub instance_guid: guid::Guid,
    pub lun: u8,
}

impl ResourceId<DiskHandleKind> for StorvscDiskConfig {
    const ID: &'static str = "storvsc";
}

pub mod save_restore {
    use mesh::payload::Protobuf;
    use vmcore::save_restore::SavedStateRoot;

    #[derive(Protobuf, SavedStateRoot)]
    #[mesh(package = "underhill")]
    pub struct StorvscManagerSavedState {
        #[mesh(1)]
        pub storvsc_drivers: Vec<StorvscSavedDriverConfig>,
    }

    #[derive(Protobuf, Clone)]
    #[mesh(package = "underhill")]
    pub struct StorvscSavedDriverConfig {
        #[mesh(1)]
        pub instance_guid: guid::Guid,
        #[mesh(2)]
        pub driver_state: storvsc_driver::save_restore::StorvscDriverSavedState,
    }
}
