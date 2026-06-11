// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Resolver for the Hyper-V power management device, and shared PM resolution
//! logic used by both the Hyper-V and PIIX4 PM resolvers.

use super::EnableAcpiMode;
use super::PowerAction;
use super::PowerManagementDevice;
use async_trait::async_trait;
use chipset_device::interrupt::LineInterruptTarget;
use chipset_device_resources::GPE0_LINE_SET;
use chipset_device_resources::IRQ_LINE_SET;
use chipset_device_resources::ResolveChipsetDeviceHandleParams;
use chipset_device_resources::ResolvedChipsetDevice;
use chipset_resources::pm::HyperVPowerManagementDeviceHandle;
use chipset_resources::pm::PmTimerAssist;
use chipset_resources::pm::PmTimerAssistHandleKind;
use power_resources::PowerRequest;
use power_resources::PowerRequestHandleKind;
use thiserror::Error;
use vm_resource::AsyncResolveResource;
use vm_resource::IntoResource;
use vm_resource::PlatformResource;
use vm_resource::ResolveError;
use vm_resource::Resource;
use vm_resource::ResourceResolver;
use vm_resource::declare_static_async_resolver;
use vm_resource::kind::ChipsetDeviceHandleKind;

/// Errors that can occur when resolving PM device dependencies.
#[derive(Debug, Error)]
#[expect(missing_docs)]
pub enum ResolvePmError {
    #[error("failed to resolve power request")]
    ResolvePowerRequest(#[source] ResolveError),
    #[error("failed to resolve PM timer assist")]
    ResolvePmTimerAssist(#[source] ResolveError),
}

/// Resolved PM dependencies (power action callback and optional timer assist).
///
/// Produced by [`resolve_pm_deps`] and consumed by
/// [`PowerManagementDevice::new`].
pub struct ResolvedPmDeps {
    /// Callback invoked whenever a power action is requested.
    pub power_action: super::PowerActionFn,
    /// Optional PM timer assist, if a resource was provided.
    pub pm_timer_assist: Option<Box<dyn PmTimerAssist>>,
}

/// Resolve the common PM dependencies (power request handler and optional PM
/// timer assist) that are shared by both the Hyper-V and PIIX4 PM resolvers.
///
/// This performs the async resolution work up-front so that the resulting
/// [`ResolvedPmDeps`] can be passed to [`PowerManagementDevice::new`]
/// synchronously.
pub async fn resolve_pm_deps(
    resolver: &ResourceResolver,
    pm_timer_assist_resource: Option<Resource<PmTimerAssistHandleKind>>,
) -> Result<ResolvedPmDeps, ResolvePmError> {
    let power_request = resolver
        .resolve::<PowerRequestHandleKind, _>(PlatformResource.into_resource(), ())
        .await
        .map_err(ResolvePmError::ResolvePowerRequest)?;

    let pm_timer_assist = if let Some(assist_resource) = pm_timer_assist_resource {
        let resolved = resolver
            .resolve::<PmTimerAssistHandleKind, _>(assist_resource, ())
            .await
            .map_err(ResolvePmError::ResolvePmTimerAssist)?;
        Some(resolved.0)
    } else {
        None
    };

    Ok(ResolvedPmDeps {
        power_action: Box::new(move |action| {
            let req = match action {
                PowerAction::PowerOff => PowerRequest::PowerOff,
                PowerAction::Hibernate => PowerRequest::Hibernate,
                PowerAction::Reboot => PowerRequest::Reset,
            };
            power_request.power_request(req);
        }),
        pm_timer_assist,
    })
}

/// Register GPE0 line targets for a device that implements
/// [`LineInterruptTarget`].
pub fn register_gpe0_lines(
    configure: &mut dyn chipset_device_resources::ConfigureChipsetDevice,
    target: &dyn LineInterruptTarget,
) {
    for range in target.valid_lines() {
        configure.add_line_target(GPE0_LINE_SET, range.clone(), *range.start());
    }
}

/// A resolver for the Hyper-V power management device.
pub struct HyperVPowerManagementResolver;

declare_static_async_resolver! {
    HyperVPowerManagementResolver,
    (ChipsetDeviceHandleKind, HyperVPowerManagementDeviceHandle),
}

#[async_trait]
impl AsyncResolveResource<ChipsetDeviceHandleKind, HyperVPowerManagementDeviceHandle>
    for HyperVPowerManagementResolver
{
    type Output = ResolvedChipsetDevice;
    type Error = ResolvePmError;

    async fn resolve(
        &self,
        resolver: &ResourceResolver,
        resource: HyperVPowerManagementDeviceHandle,
        input: ResolveChipsetDeviceHandleParams<'_>,
    ) -> Result<Self::Output, Self::Error> {
        let acpi_interrupt = input
            .configure
            .new_line(IRQ_LINE_SET, "gpe0", resource.acpi_irq);

        let deps = resolve_pm_deps(resolver, resource.pm_timer_assist).await?;

        let pm = PowerManagementDevice::new(
            deps.power_action,
            acpi_interrupt,
            input.register_pio,
            input.vmtime.access("pm"),
            Some(EnableAcpiMode {
                default_pio_dynamic: resource.pio_base,
            }),
            deps.pm_timer_assist,
        );

        register_gpe0_lines(input.configure, &pm);

        Ok(pm.into())
    }
}
