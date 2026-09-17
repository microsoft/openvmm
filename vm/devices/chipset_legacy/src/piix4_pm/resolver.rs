// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Resolver for the PIIX4 power management device.

use super::Piix4Pm;
use async_trait::async_trait;
use chipset::pm::resolver::ResolvePmError;
use chipset::pm::resolver::register_gpe0_lines;
use chipset::pm::resolver::resolve_pm_deps;
use chipset_device_resources::IRQ_LINE_SET;
use chipset_device_resources::ResolveChipsetDeviceHandleParams;
use chipset_device_resources::ResolvedChipsetDevice;
use chipset_resources::pm::Piix4PowerManagementDeviceHandle;
use vm_resource::AsyncResolveResource;
use vm_resource::ResourceResolver;
use vm_resource::declare_static_async_resolver;
use vm_resource::kind::ChipsetDeviceHandleKind;

/// A resolver for the PIIX4 power management device.
pub struct Piix4PowerManagementResolver;

declare_static_async_resolver! {
    Piix4PowerManagementResolver,
    (ChipsetDeviceHandleKind, Piix4PowerManagementDeviceHandle),
}

#[async_trait]
impl AsyncResolveResource<ChipsetDeviceHandleKind, Piix4PowerManagementDeviceHandle>
    for Piix4PowerManagementResolver
{
    type Output = ResolvedChipsetDevice;
    type Error = ResolvePmError;

    async fn resolve(
        &self,
        resolver: &ResourceResolver,
        resource: Piix4PowerManagementDeviceHandle,
        input: ResolveChipsetDeviceHandleParams<'_>,
    ) -> Result<Self::Output, Self::Error> {
        // Hard-coded to IRQ line 9, as per PIIX4 spec.
        let interrupt = input.configure.new_line(IRQ_LINE_SET, "acpi", 9);

        // Resolve common PM dependencies through the shared resolver
        // infrastructure, then construct the inner PowerManagementDevice.
        let deps = resolve_pm_deps(resolver, resource.pm_timer_assist).await?;

        let inner = chipset::pm::PowerManagementDevice::new(
            deps.power_action,
            interrupt,
            input.register_pio,
            input.vmtime.access("piix4-pm"),
            None, // PIIX4 manages ACPI mode via PCI config space
            deps.pm_timer_assist,
        );

        let pm = Piix4Pm::new(inner, input.register_pio);

        register_gpe0_lines(input.configure, &pm);

        Ok(pm.into())
    }
}
