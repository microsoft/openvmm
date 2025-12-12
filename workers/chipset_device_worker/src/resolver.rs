// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use crate::guestmem::GuestMemoryProxy;
use crate::proxy::ChipsetDeviceProxy;
use crate::worker::REMOTE_CHIPSET_DEVICE_WORKER_ID;
use crate::worker::RemoteChipsetDeviceHandleParams;
use crate::worker::RemoteChipsetDeviceWorkerParameters;
use crate::worker::RemoteDynamicResolvers;
use async_trait::async_trait;
use chipset_device_resources::ResolveChipsetDeviceHandleParams;
use chipset_device_resources::ResolvedChipsetDevice;
use chipset_device_worker_defs::RemoteChipsetDeviceHandle;
use thiserror::Error;
use vm_resource::AsyncResolveResource;
use vm_resource::IntoResource;
use vm_resource::PlatformResource;
use vm_resource::ResourceResolver;
use vm_resource::declare_static_async_resolver;
use vm_resource::kind::ChipsetDeviceHandleKind;
use vmgs_broker::resolver::VmgsClientKind;

/// The resolver for remote chipset devices.
pub struct RemoteChipsetDeviceResolver;

declare_static_async_resolver! {
    RemoteChipsetDeviceResolver,
    (ChipsetDeviceHandleKind, RemoteChipsetDeviceHandle),
}

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
impl AsyncResolveResource<ChipsetDeviceHandleKind, RemoteChipsetDeviceHandle>
    for RemoteChipsetDeviceResolver
{
    type Error = ResolveRemoteChipsetDeviceError;
    type Output = ResolvedChipsetDevice;

    async fn resolve(
        &self,
        resolver: &ResourceResolver,
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
                REMOTE_CHIPSET_DEVICE_WORKER_ID,
                RemoteChipsetDeviceWorkerParameters {
                    device,
                    dyn_resolvers: RemoteDynamicResolvers {
                        #[cfg(target_os = "linux")]
                        get: resolver
                            .resolve::<guest_emulation_transport::resolver::GetClientKind, _>(
                                PlatformResource.into_resource(),
                                (),
                            )
                            .await
                            .ok(),
                        vmgs: resolver
                            .resolve::<VmgsClientKind, _>(PlatformResource.into_resource(), ())
                            .await
                            .ok(),
                    },
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
