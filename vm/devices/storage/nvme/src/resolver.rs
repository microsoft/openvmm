// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Resource resolver for the nvme controller.

use crate::NsidConflict;
use crate::NvmeController;
use crate::NvmeControllerCaps;
use crate::NvmeControllerClient;
use anyhow::Context;
use async_trait::async_trait;
use disk_backend::resolve::ResolveDiskParameters;
use futures::StreamExt;
use nvme_resources::NamespaceDefinition;
use nvme_resources::NvmeControllerHandle;
use nvme_resources::NvmeControllerRequest;
use pal_async::task::Spawn;
use pci_resources::ResolvePciDeviceHandleParams;
use pci_resources::ResolvedPciDevice;
use thiserror::Error;
use vm_resource::AsyncResolveResource;
use vm_resource::ResolveError;
use vm_resource::ResourceResolver;
use vm_resource::declare_static_async_resolver;
use vm_resource::kind::PciDeviceHandleKind;
use vmcore::vm_task::VmTaskDriverSource;

/// Resource resolver for [`NvmeControllerHandle`].
pub struct NvmeControllerResolver;

declare_static_async_resolver! {
    NvmeControllerResolver,
    (PciDeviceHandleKind, NvmeControllerHandle),
}

/// Error returned by [`NvmeControllerResolver`].
#[derive(Debug, Error)]
#[expect(missing_docs)]
pub enum Error {
    #[error("failed to resolve namespace {nsid}")]
    NamespaceResolve {
        nsid: u32,
        #[source]
        source: ResolveError,
    },
    #[error(transparent)]
    NsidConflict(NsidConflict),
}

#[async_trait]
impl AsyncResolveResource<PciDeviceHandleKind, NvmeControllerHandle> for NvmeControllerResolver {
    type Output = ResolvedPciDevice;
    type Error = Error;

    async fn resolve(
        &self,
        resolver: &ResourceResolver,
        resource: NvmeControllerHandle,
        input: ResolvePciDeviceHandleParams<'_>,
    ) -> Result<Self::Output, Self::Error> {
        let controller = NvmeController::new(
            input.driver_source,
            input.guest_memory.clone(),
            input.msi_target,
            input.register_mmio,
            NvmeControllerCaps {
                msix_count: resource.msix_count,
                max_io_queues: resource.max_io_queues,
                subsystem_id: resource.subsystem_id,
            },
        );
        for NamespaceDefinition {
            nsid,
            read_only,
            disk,
        } in resource.namespaces
        {
            let disk = resolver
                .resolve(
                    disk,
                    ResolveDiskParameters {
                        read_only,
                        driver_source: input.driver_source,
                    },
                )
                .await
                .map_err(|source| Error::NamespaceResolve { nsid, source })?;
            controller
                .client()
                .add_namespace(nsid, disk.0)
                .await
                .map_err(Error::NsidConflict)?;
        }

        if let Some(requests) = resource.requests {
            let driver = input.driver_source.simple();
            driver
                .spawn(
                    "nvme-requests",
                    handle_requests(
                        input.driver_source.clone(),
                        controller.client(),
                        resolver.clone(),
                        requests,
                    ),
                )
                .detach();
        }

        Ok(controller.into())
    }
}

async fn handle_requests(
    driver_source: VmTaskDriverSource,
    client: NvmeControllerClient,
    resolver: ResourceResolver,
    mut requests: mesh::Receiver<NvmeControllerRequest>,
) {
    while let Some(req) = requests.next().await {
        match req {
            NvmeControllerRequest::AddNamespace(rpc) => {
                rpc.handle_failable(
                    async |NamespaceDefinition {
                               nsid,
                               read_only,
                               disk,
                           }| {
                        let disk = resolver
                            .resolve(
                                disk,
                                ResolveDiskParameters {
                                    read_only,
                                    driver_source: &driver_source,
                                },
                            )
                            .await
                            .context("failed to resolve disk")?;

                        client
                            .add_namespace(nsid, disk.0)
                            .await
                            .context("failed to add namespace")?;

                        anyhow::Ok(())
                    },
                )
                .await
            }
            NvmeControllerRequest::RemoveNamespace(rpc) => {
                rpc.handle_failable(async |nsid| {
                    let removed = client.remove_namespace(nsid).await;
                    if !removed {
                        anyhow::bail!("namespace {nsid} not found");
                    }
                    anyhow::Ok(())
                })
                .await
            }
        }
    }
}
