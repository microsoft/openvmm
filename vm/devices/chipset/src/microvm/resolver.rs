// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Resource resolver for microVM chipset devices.

use super::MicrovmPortb;
use super::MicrovmShutdown;
use super::MicrovmSnapshotRequest;
use async_trait::async_trait;
use chipset_device_resources::ResolveChipsetDeviceHandleParams;
use chipset_device_resources::ResolvedChipsetDevice;
use chipset_resources::microvm::MicrovmPortbHandle;
use chipset_resources::microvm::MicrovmShutdownHandle;
use chipset_resources::microvm::MicrovmSnapshotRequestHandle;
use power_resources::PowerRequestHandleKind;
use serial_core::resources::ResolveSerialBackendParams;
use thiserror::Error;
use vm_resource::AsyncResolveResource;
use vm_resource::IntoResource;
use vm_resource::PlatformResource;
use vm_resource::ResolveError;
use vm_resource::ResolveResource;
use vm_resource::ResourceResolver;
use vm_resource::declare_static_async_resolver;
use vm_resource::declare_static_resolver;
use vm_resource::kind::ChipsetDeviceHandleKind;

/// Resolver for the microVM portb console.
pub struct MicrovmPortbResolver;

declare_static_async_resolver! {
    MicrovmPortbResolver,
    (ChipsetDeviceHandleKind, MicrovmPortbHandle),
}

#[derive(Debug, Error)]
pub enum ResolveMicrovmPortbError {
    #[error("failed to resolve microVM portb backend")]
    ResolveBackend(#[source] ResolveError),
}

#[async_trait]
impl AsyncResolveResource<ChipsetDeviceHandleKind, MicrovmPortbHandle> for MicrovmPortbResolver {
    type Output = ResolvedChipsetDevice;
    type Error = ResolveMicrovmPortbError;

    async fn resolve(
        &self,
        resolver: &ResourceResolver,
        resource: MicrovmPortbHandle,
        input: ResolveChipsetDeviceHandleParams<'_>,
    ) -> Result<Self::Output, Self::Error> {
        let io = resolver
            .resolve(
                resource.io,
                ResolveSerialBackendParams {
                    driver: Box::new(input.task_driver_source.simple()),
                    _async_trait_workaround: &(),
                },
            )
            .await
            .map_err(ResolveMicrovmPortbError::ResolveBackend)?;
        Ok(MicrovmPortb::new(io.0.into_io()).into())
    }
}

/// Resolver for the microVM shutdown port.
pub struct MicrovmShutdownResolver;

declare_static_async_resolver! {
    MicrovmShutdownResolver,
    (ChipsetDeviceHandleKind, MicrovmShutdownHandle),
}

/// Resolver for the microVM snapshot-request port.
pub struct MicrovmSnapshotRequestResolver;

declare_static_resolver! {
    MicrovmSnapshotRequestResolver,
    (ChipsetDeviceHandleKind, MicrovmSnapshotRequestHandle),
}

#[async_trait]
impl AsyncResolveResource<ChipsetDeviceHandleKind, MicrovmShutdownHandle>
    for MicrovmShutdownResolver
{
    type Output = ResolvedChipsetDevice;
    type Error = ResolveMicrovmPortbError;

    async fn resolve(
        &self,
        resolver: &ResourceResolver,
        _resource: MicrovmShutdownHandle,
        _input: ResolveChipsetDeviceHandleParams<'_>,
    ) -> Result<Self::Output, Self::Error> {
        let power_request = resolver
            .resolve::<PowerRequestHandleKind, _>(PlatformResource.into_resource(), ())
            .await
            .map_err(ResolveMicrovmPortbError::ResolveBackend)?;
        Ok(MicrovmShutdown::new(power_request).into())
    }
}

impl ResolveResource<ChipsetDeviceHandleKind, MicrovmSnapshotRequestHandle>
    for MicrovmSnapshotRequestResolver
{
    type Output = ResolvedChipsetDevice;
    type Error = std::convert::Infallible;

    fn resolve(
        &self,
        resource: MicrovmSnapshotRequestHandle,
        _input: ResolveChipsetDeviceHandleParams<'_>,
    ) -> Result<Self::Output, Self::Error> {
        Ok(MicrovmSnapshotRequest::new(resource.notify, resource.input_gate_timeout).into())
    }
}
