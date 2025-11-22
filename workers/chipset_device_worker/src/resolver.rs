// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use crate::ChipsetDeviceWorkerHandle;
use crate::proxy::ChipsetDeviceProxy;
use crate::worker::CHIPSET_DEVICE_WORKER_ID;
use crate::worker::ChipsetDeviceWorkerParameters;
use async_trait::async_trait;
use chipset_device_resources::ResolveChipsetDeviceHandleParams;
use chipset_device_resources::ResolvedChipsetDevice;
use thiserror::Error;
use vm_resource::AsyncResolveResource;
use vm_resource::ResourceResolver;
use vm_resource::declare_static_async_resolver;
use vm_resource::kind::ChipsetDeviceHandleKind;

/// The resolver for chipset device workers.
pub struct ChipsetDeviceWorkerResolver;

declare_static_async_resolver! {
    ChipsetDeviceWorkerResolver,
    (ChipsetDeviceHandleKind, ChipsetDeviceWorkerHandle),
}

/// Errors that can occur while resolving a chipset device worker.
#[derive(Debug, Error)]
pub enum ResolveChipsetWorkerError {
    /// Error launching the worker.
    #[error("error launching worker")]
    LaunchWorker(#[source] anyhow::Error),
}

#[async_trait]
impl AsyncResolveResource<ChipsetDeviceHandleKind, ChipsetDeviceWorkerHandle>
    for ChipsetDeviceWorkerResolver
{
    type Error = ResolveChipsetWorkerError;
    type Output = ResolvedChipsetDevice;

    async fn resolve(
        &self,
        _resolver: &ResourceResolver,
        resource: ChipsetDeviceWorkerHandle,
        _input: ResolveChipsetDeviceHandleParams<'_>,
    ) -> Result<Self::Output, Self::Error> {
        let ChipsetDeviceWorkerHandle {
            device,
            worker_host,
        } = resource;

        let (send, recv) = mesh::channel();

        let worker = worker_host
            .launch_worker(
                CHIPSET_DEVICE_WORKER_ID,
                ChipsetDeviceWorkerParameters { device, recv },
            )
            .await
            .map_err(ResolveChipsetWorkerError::LaunchWorker)?;

        let proxy = ChipsetDeviceProxy::new(send, worker);

        Ok(proxy.into())
    }
}
