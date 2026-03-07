// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! The resolver for remote chipset devices.

use crate::RemoteDynamicResolvers;
use crate::guestmem::GuestMemoryProxy;
use crate::proxy::ChipsetDeviceProxy;
use crate::worker::RemoteChipsetDeviceHandleParams;
use crate::worker::RemoteChipsetDeviceWorkerParameters;
use crate::worker::remote_chipset_device_worker_id;
use async_trait::async_trait;
use chipset_device_resources::ResolveChipsetDeviceHandleParams;
use chipset_device_resources::ResolvedChipsetDevice;
use chipset_device_worker_defs::RemoteChipsetDeviceHandle;
use thiserror::Error;
use vm_resource::AsyncResolveResource;
use vm_resource::ResourceResolver;
use vm_resource::kind::ChipsetDeviceHandleKind;

/// The resolver for remote chipset devices.
/// T is the type of the dynamic resolvers needed for the remote chipset device.
// FUTURE: Create a way to store a Vec of all registered dynamic resolvers
// and transfer them, instead of maintaining a list of just a few.
pub struct RemoteChipsetDeviceResolver<T: RemoteDynamicResolvers>(pub T);

/// Errors that can occur while resolving a remote chipset device.
#[derive(Debug, Error)]
pub enum ResolveRemoteChipsetDeviceError {
    /// Error launching the worker.
    #[error("error launching worker")]
    LaunchWorker(#[source] anyhow::Error),
    /// Error constructing the proxy.
    #[error("error constructing proxy device")]
    ConstructProxy(#[source] anyhow::Error),
}

#[async_trait]
impl<T: RemoteDynamicResolvers>
    AsyncResolveResource<ChipsetDeviceHandleKind, RemoteChipsetDeviceHandle>
    for RemoteChipsetDeviceResolver<T>
{
    type Error = ResolveRemoteChipsetDeviceError;
    type Output = ResolvedChipsetDevice;

    async fn resolve(
        &self,
        _resolver: &ResourceResolver,
        resource: RemoteChipsetDeviceHandle,
        input: ResolveChipsetDeviceHandleParams<'_>,
    ) -> Result<Self::Output, Self::Error> {
        let RemoteChipsetDeviceHandle {
            device,
            worker_host,
        } = resource;

        let (req_send, req_recv) = mesh::channel();
        let (resp_send, resp_recv) = mesh::channel();
        let (cap_send, cap_recv) = mesh::oneshot();

        let (gm_proxy, gm_remote) = GuestMemoryProxy::new(input.guest_memory.clone());
        let (enc_gm_proxy, enc_gm_remote) =
            GuestMemoryProxy::new(input.encrypted_guest_memory.clone());

        let worker = worker_host
            .launch_worker(
                remote_chipset_device_worker_id(),
                RemoteChipsetDeviceWorkerParameters {
                    device,
                    dyn_resolvers: self.0.clone(),
                    inputs: RemoteChipsetDeviceHandleParams {
                        device_name: input.device_name.to_string(),
                        vmtime: input.vmtime.builder().clone(),
                        is_restoring: input.is_restoring,
                        guest_memory: gm_remote,
                        encrypted_guest_memory: enc_gm_remote,
                    },
                    req_recv,
                    resp_send,
                    cap_send,
                },
            )
            .await
            .map_err(ResolveRemoteChipsetDeviceError::LaunchWorker)?;

        let proxy = ChipsetDeviceProxy::new(
            req_send,
            resp_recv,
            cap_recv,
            worker,
            gm_proxy,
            enc_gm_proxy,
            input,
        )
        .await
        .map_err(ResolveRemoteChipsetDeviceError::ConstructProxy)?;

        Ok(proxy.into())
    }
}
