// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Resolver for a SCSI controller.

use super::StorageDevice;
use crate::ScsiController;
use crate::ScsiControllerDisk;
use crate::ScsiControllerState;
use crate::ScsiPathInUse;
use anyhow::Context;
use async_trait::async_trait;
use futures::StreamExt;
use pal_async::task::Spawn;
use scsi_core::ResolveScsiDeviceHandleParams;
use std::sync::Arc;
use std::sync::Weak;
use storvsp_resources::ScsiControllerHandle;
use storvsp_resources::ScsiControllerRequest;
use storvsp_resources::ScsiDeviceAndPath;
use storvsp_resources::ScsiPath;
use thiserror::Error;
use vm_resource::AsyncResolveResource;
use vm_resource::ResolveError;
use vm_resource::ResourceResolver;
use vm_resource::declare_static_async_resolver;
use vm_resource::kind::VmbusDeviceHandleKind;
use vmbus_channel::resources::ResolveVmbusDeviceHandleParams;
use vmbus_channel::resources::ResolvedVmbusDevice;
use vmcore::vm_task::VmTaskDriverSource;

/// The resolver for [`ScsiControllerHandle`].
pub struct StorvspResolver;

declare_static_async_resolver! {
    StorvspResolver,
    (VmbusDeviceHandleKind, ScsiControllerHandle),
}

/// An error returned by [`StorvspResolver`].
#[derive(Debug, Error)]
pub enum Error {
    #[error(transparent)]
    ScsiPathInUse(ScsiPathInUse),
    #[error("failed to resolve scsi device at {path}")]
    Device {
        path: ScsiPath,
        #[source]
        source: ResolveError,
    },
}

#[async_trait]
impl AsyncResolveResource<VmbusDeviceHandleKind, ScsiControllerHandle> for StorvspResolver {
    type Output = ResolvedVmbusDevice;
    type Error = Error;

    async fn resolve(
        &self,
        resolver: &ResourceResolver,
        resource: ScsiControllerHandle,
        input: ResolveVmbusDeviceHandleParams<'_>,
    ) -> Result<Self::Output, Self::Error> {
        let controller =
            ScsiController::new_with_poll_mode_queue_depth(resource.poll_mode_queue_depth);
        let device = StorageDevice::build_scsi(
            input.driver_source,
            &controller,
            resource.instance_id,
            resource.max_sub_channel_count,
            resource.io_queue_depth.unwrap_or(256),
        );

        for ScsiDeviceAndPath { path, device } in resource.devices {
            let device = resolver
                .resolve(
                    device,
                    ResolveScsiDeviceHandleParams {
                        driver_source: input.driver_source,
                    },
                )
                .await
                .map_err(|err| Error::Device { path, source: err })?;

            controller
                .attach(path, ScsiControllerDisk { disk: device.0 })
                .map_err(Error::ScsiPathInUse)?;
        }

        let driver = input.driver_source.simple();
        if let Some(requests) = resource.requests {
            driver
                .spawn(
                    "storvsp-requests",
                    handle_requests(
                        input.driver_source.clone(),
                        Arc::downgrade(&controller.state),
                        resolver.clone(),
                        requests,
                    ),
                )
                .detach();
        }

        Ok(device.into())
    }
}

async fn handle_requests(
    driver_source: VmTaskDriverSource,
    state: Weak<ScsiControllerState>,
    resolver: ResourceResolver,
    mut requests: mesh::Receiver<ScsiControllerRequest>,
) {
    while let Some(req) = requests.next().await {
        match req {
            ScsiControllerRequest::AddDevice(rpc) => {
                rpc.handle_failable(async |ScsiDeviceAndPath { path, device }| {
                    let device = resolver
                        .resolve(
                            device,
                            ResolveScsiDeviceHandleParams {
                                driver_source: &driver_source,
                            },
                        )
                        .await
                        .context("failed to resolve media")?;

                    if let Some(state) = state.upgrade() {
                        ScsiController { state }
                            .attach(path, ScsiControllerDisk::new(device.0))
                            .context("failed to attach device")?;
                    }
                    anyhow::Ok(())
                })
                .await
            }
            ScsiControllerRequest::RemoveDevice(rpc) => rpc.handle_failable_sync(|path| {
                if let Some(state) = state.upgrade() {
                    ScsiController { state }
                        .remove(path)
                        .context("failed to remove device")?;
                }
                anyhow::Ok(())
            }),
        }
    }
}
