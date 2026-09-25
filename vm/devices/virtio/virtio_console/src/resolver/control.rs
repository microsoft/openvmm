// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Resource resolver for the control console.
//!
//! The control console is a virtio-console device whose worker runs the
//! control-session broker (see [`VirtioConsoleDevice::new_broker`]).

use super::attachment::validate_attachment;
use crate::VirtioConsoleDevice;
use async_trait::async_trait;
use serial_core::resources::ResolveSerialBackendParams;
use virtio::resolve::ResolvedVirtioDevice;
use virtio::resolve::VirtioResolveInput;
use virtio_resources::console::control::VirtioControlConsoleBrokerConfig;
use virtio_resources::console::control::VirtioControlConsoleHandle;
use vm_resource::AsyncResolveResource;
use vm_resource::ResourceResolver;
use vm_resource::declare_static_async_resolver;
use vm_resource::kind::VirtioDeviceHandle;

/// Resolver for the microVM control console resource identity.
pub struct VirtioControlConsoleResolver;

declare_static_async_resolver! {
    VirtioControlConsoleResolver,
    (VirtioDeviceHandle, VirtioControlConsoleHandle),
}

#[async_trait]
impl AsyncResolveResource<VirtioDeviceHandle, VirtioControlConsoleHandle>
    for VirtioControlConsoleResolver
{
    type Output = ResolvedVirtioDevice;
    type Error = anyhow::Error;

    async fn resolve(
        &self,
        resolver: &ResourceResolver,
        resource: VirtioControlConsoleHandle,
        input: VirtioResolveInput<'_>,
    ) -> Result<Self::Output, Self::Error> {
        validate_attachment(resource.attachment.as_ref())?;
        validate_broker_config(&resource.broker_config)?;
        let io = resolve_backend(resolver, &input, resource.backend).await?;
        Ok(VirtioConsoleDevice::new_broker(input.driver_source, io, resource.broker_config).into())
    }
}

fn validate_broker_config(config: &VirtioControlConsoleBrokerConfig) -> anyhow::Result<()> {
    anyhow::ensure!(
        config.instance_id != [0; 16],
        "control-console broker instance ID must not be zero"
    );
    anyhow::ensure!(
        config.capability != [0; 32],
        "control-console broker capability must not be zero"
    );
    anyhow::ensure!(
        (1..=60_000).contains(&config.auth_timeout_ms),
        "control-console broker authentication timeout must be between 1 and 60000 ms"
    );
    Ok(())
}

async fn resolve_backend(
    resolver: &ResourceResolver,
    input: &VirtioResolveInput<'_>,
    backend: vm_resource::Resource<vm_resource::kind::SerialBackendHandle>,
) -> anyhow::Result<Box<dyn serial_core::SerialIo>> {
    let io = resolver
        .resolve(
            backend,
            ResolveSerialBackendParams {
                driver: Box::new(input.driver_source.simple()),
                _async_trait_workaround: &(),
            },
        )
        .await?;
    Ok(io.0.into_io())
}
