// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Resource resolver for the nvme controller.

use crate::NsidConflict;
use crate::NvmeFaultController;
use crate::NvmeFaultControllerCaps;
use crate::QueueFaultBehavior;
use async_trait::async_trait;
use disk_backend::resolve::ResolveDiskParameters;
use nvme_resources::NamespaceDefinition;
use nvme_resources::NvmeFaultControllerHandle;
use pci_resources::ResolvePciDeviceHandleParams;
use pci_resources::ResolvedPciDevice;
use thiserror::Error;
use vm_resource::AsyncResolveResource;
use vm_resource::ResolveError;
use vm_resource::ResourceResolver;
use vm_resource::declare_static_async_resolver;
use vm_resource::kind::PciDeviceHandleKind;

/// Resource resolver for [`NvmeFaultControllerHandle`].
pub struct NvmeFaultControllerResolver;

declare_static_async_resolver! {
    NvmeFaultControllerResolver,
    (PciDeviceHandleKind, NvmeFaultControllerHandle),
}

/// Error returned by [`NvmeFaultControllerResolver`].
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

struct AdminSubQueueFault {
    pub signal: Cell<bool>,
}

#[async_trait::async_trait]
impl crate::QueueFault for AdminSubQueueFault {
    async fn fault_submission_queue(&self, command: Command) -> QueueFaultBehavior<Command> {
        tracing::info!("Faulting submission queue by now allowing io completion queue creation");
        let opcode = nvme_spec::AdminOpcode(command.cdw0.opcode());
        match opcode {
            nvme_spec::AdminOpcode::IDENTIFY => {
                if !self.signal.get() {
                    panic!("Found an identify command");
                    QueueFaultBehavior::Drop
                } else {
                    QueueFaultBehavior::Default
                }
            }
            _ => panic!("Found a command"),
        }
    }

    async fn fault_completion_queue(
        &self,
        _completion: Completion,
    ) -> QueueFaultBehavior<Completion> {
        QueueFaultBehavior::Default
    }
}

#[async_trait]
impl AsyncResolveResource<PciDeviceHandleKind, NvmeFaultControllerHandle>
    for NvmeFaultControllerResolver
{
    type Output = ResolvedPciDevice;
    type Error = Error;

    async fn resolve(
        &self,
        resolver: &ResourceResolver,
        resource: NvmeFaultControllerHandle,
        input: ResolvePciDeviceHandleParams<'_>,
    ) -> Result<Self::Output, Self::Error> {
        let controller = NvmeFaultController::new(
            input.driver_source,
            input.guest_memory.clone(),
            input.register_msi,
            input.register_mmio,
            NvmeFaultControllerCaps {
                msix_count: resource.msix_count,
                max_io_queues: resource.max_io_queues,
                subsystem_id: resource.subsystem_id,
            },
            resource.fault_config,
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
        Ok(controller.into())
    }
}
