// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Resolver for the Hyper-V power management device.

use super::EnableAcpiMode;
use super::PowerManagementDevice;
use async_trait::async_trait;
use chipset_device::interrupt::LineInterruptTarget;
use chipset_device_resources::GPE0_LINE_SET;
use chipset_device_resources::IRQ_LINE_SET;
use chipset_device_resources::ResolveChipsetDeviceHandleParams;
use chipset_device_resources::ResolvedChipsetDevice;
use chipset_resources::pm::HyperVPowerManagementDeviceHandle;
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

/// A resolver for the Hyper-V power management device.
pub struct HyperVPowerManagementResolver;

declare_static_async_resolver! {
    HyperVPowerManagementResolver,
    (ChipsetDeviceHandleKind, HyperVPowerManagementDeviceHandle),
}

/// Errors that can occur when resolving the Hyper-V power management device.
#[derive(Debug, Error)]
#[expect(missing_docs)]
pub enum ResolveHyperVPmError {
    #[error("failed to resolve power request")]
    ResolvePowerRequest(#[source] ResolveError),
    #[error("failed to resolve PM timer assist")]
    ResolvePmTimerAssist(#[source] ResolveError),
}

#[async_trait]
impl AsyncResolveResource<ChipsetDeviceHandleKind, HyperVPowerManagementDeviceHandle>
    for HyperVPowerManagementResolver
{
    type Output = ResolvedChipsetDevice;
    type Error = ResolveHyperVPmError;

    async fn resolve(
        &self,
        resolver: &ResourceResolver,
        resource: HyperVPowerManagementDeviceHandle,
        input: ResolveChipsetDeviceHandleParams<'_>,
    ) -> Result<Self::Output, Self::Error> {
        let acpi_interrupt = input
            .configure
            .new_line(IRQ_LINE_SET, "gpe0", resource.acpi_irq);

        let power_request = resolver
            .resolve::<PowerRequestHandleKind, _>(PlatformResource.into_resource(), ())
            .await
            .map_err(ResolveHyperVPmError::ResolvePowerRequest)?;

        let pm_timer_assist = if let Some(assist_resource) = resource.pm_timer_assist {
            let resolved = resolver
                .resolve::<PmTimerAssistHandleKind, _>(assist_resource, ())
                .await
                .map_err(ResolveHyperVPmError::ResolvePmTimerAssist)?;
            Some(resolved.0)
        } else {
            None
        };

        let pm = PowerManagementDevice::new(
            Box::new(move |action| {
                let req = match action {
                    super::PowerAction::PowerOff => PowerRequest::PowerOff,
                    super::PowerAction::Hibernate => PowerRequest::Hibernate,
                    super::PowerAction::Reboot => PowerRequest::Reset,
                };
                power_request.power_request(req);
            }),
            acpi_interrupt,
            input.register_pio,
            input.vmtime.access("pm"),
            Some(EnableAcpiMode {
                default_pio_dynamic: resource.pio_base,
            }),
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
