// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Resolver for the PIIX4 PCI-ISA bridge device.

use super::PciIsaBridge;
use async_trait::async_trait;
use chipset_device_resources::ResolveChipsetDeviceHandleParams;
use chipset_device_resources::ResolvedChipsetDevice;
use chipset_resources::piix4_pci_isa_bridge::Piix4PciIsaBridgeDeviceHandle;
use power_resources::PowerRequest;
use power_resources::PowerRequestHandleKind;
use thiserror::Error;
use vm_resource::AsyncResolveResource;
use vm_resource::IntoResource;
use vm_resource::PlatformResource;
use vm_resource::ResolveError;
use vm_resource::ResourceResolver;
use vm_resource::declare_static_async_resolver;
use vm_resource::kind::ChipsetDeviceHandleKind;

/// A resolver for the PIIX4 PCI-ISA bridge device.
pub struct Piix4PciIsaBridgeResolver;

declare_static_async_resolver! {
    Piix4PciIsaBridgeResolver,
    (ChipsetDeviceHandleKind, Piix4PciIsaBridgeDeviceHandle),
}

/// Errors that can occur when resolving the PIIX4 PCI-ISA bridge.
#[derive(Debug, Error)]
#[expect(missing_docs)]
pub enum ResolvePiix4PciIsaBridgeError {
    #[error("failed to resolve power request")]
    ResolvePowerRequest(#[source] ResolveError),
}

#[async_trait]
impl AsyncResolveResource<ChipsetDeviceHandleKind, Piix4PciIsaBridgeDeviceHandle>
    for Piix4PciIsaBridgeResolver
{
    type Output = ResolvedChipsetDevice;
    type Error = ResolvePiix4PciIsaBridgeError;

    async fn resolve(
        &self,
        resolver: &ResourceResolver,
        _resource: Piix4PciIsaBridgeDeviceHandle,
        _input: ResolveChipsetDeviceHandleParams<'_>,
    ) -> Result<Self::Output, Self::Error> {
        let power_request = resolver
            .resolve::<PowerRequestHandleKind, _>(PlatformResource.into_resource(), ())
            .await
            .map_err(ResolvePiix4PciIsaBridgeError::ResolvePowerRequest)?;

        let reset = Box::new(move || {
            power_request.power_request(PowerRequest::Reset);
        });

        let set_a20_signal = Box::new(move |active| {
            tracelimit::info_ratelimited!(active, "setting stubbed A20 signal")
        });

        Ok(PciIsaBridge::new(reset, set_a20_signal).into())
    }
}
