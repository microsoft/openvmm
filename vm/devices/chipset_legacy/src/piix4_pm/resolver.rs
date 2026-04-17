// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Resolver for the PIIX4 power management device.

use super::Piix4Pm;
use async_trait::async_trait;
use chipset_device::interrupt::LineInterruptTarget;
use chipset_device_resources::GPE0_LINE_SET;
use chipset_device_resources::IRQ_LINE_SET;
use chipset_device_resources::ResolveChipsetDeviceHandleParams;
use chipset_device_resources::ResolvedChipsetDevice;
use chipset_resources::pm::Piix4PowerManagementDeviceHandle;
use chipset_resources::pm::PmTimerAssistHandleKind;
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

/// A resolver for the PIIX4 power management device.
pub struct Piix4PowerManagementResolver;

declare_static_async_resolver! {
    Piix4PowerManagementResolver,
    (ChipsetDeviceHandleKind, Piix4PowerManagementDeviceHandle),
}

/// Errors that can occur when resolving the PIIX4 power management device.
#[derive(Debug, Error)]
#[expect(missing_docs)]
pub enum ResolvePiix4PmError {
    #[error("failed to resolve power request")]
    ResolvePowerRequest(#[source] ResolveError),
    #[error("failed to resolve PM timer assist")]
    ResolvePmTimerAssist(#[source] ResolveError),
}

#[async_trait]
impl AsyncResolveResource<ChipsetDeviceHandleKind, Piix4PowerManagementDeviceHandle>
    for Piix4PowerManagementResolver
{
    type Output = ResolvedChipsetDevice;
    type Error = ResolvePiix4PmError;

    async fn resolve(
        &self,
        resolver: &ResourceResolver,
        resource: Piix4PowerManagementDeviceHandle,
        input: ResolveChipsetDeviceHandleParams<'_>,
    ) -> Result<Self::Output, Self::Error> {
        // Hard-coded to IRQ line 9, as per PIIX4 spec.
        let interrupt = input.configure.new_line(IRQ_LINE_SET, "acpi", 9);

        let power_request = resolver
            .resolve::<PowerRequestHandleKind, _>(PlatformResource.into_resource(), ())
            .await
            .map_err(ResolvePiix4PmError::ResolvePowerRequest)?;

        let pm_timer_assist = if let Some(assist_resource) = resource.pm_timer_assist {
            let resolved = resolver
                .resolve::<PmTimerAssistHandleKind, _>(assist_resource, ())
                .await
                .map_err(ResolvePiix4PmError::ResolvePmTimerAssist)?;
            Some(resolved.0)
        } else {
            None
        };

        let pm = Piix4Pm::new(
            Box::new(move |action| {
                let req = match action {
                    chipset::pm::PowerAction::PowerOff => PowerRequest::PowerOff,
                    chipset::pm::PowerAction::Hibernate => PowerRequest::Hibernate,
                    chipset::pm::PowerAction::Reboot => PowerRequest::Reset,
                };
                power_request.power_request(req);
            }),
            interrupt,
            input.register_pio,
            input.vmtime.access("piix4-pm"),
            pm_timer_assist,
        );

        for range in pm.valid_lines() {
            input
                .configure
                .add_line_target(GPE0_LINE_SET, range.clone(), *range.start());
        }

        Ok(pm.into())
    }
}
