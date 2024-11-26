// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Resolver for battery devices.

use super::BatteryDevice;
use super::BatteryRuntimeDeps;
use super::BATTERY_MMIO_REGION_BASE_ADDRESS_ARM;
use super::BATTERY_MMIO_REGION_BASE_ADDRESS_X64;
use super::BATTERY_STATUS_GPE0_LINE;
use super::BATTERY_STATUS_IRQ_NO;
use async_trait::async_trait;
use chipset_device_resources::ResolveChipsetDeviceHandleParams;
use chipset_device_resources::ResolvedChipsetDevice;
use chipset_device_resources::GPE0_LINE_SET;
use chipset_device_resources::IRQ_LINE_SET;
use chipset_resources::battery::BatteryDeviceHandleAArch64;
use chipset_resources::battery::BatteryDeviceHandleX64;
use thiserror::Error;
use vm_resource::declare_static_async_resolver;
use vm_resource::kind::ChipsetDeviceHandleKind;
use vm_resource::AsyncResolveResource;
use vm_resource::ResolveError;
use vm_resource::ResourceResolver;

/// A resolver for battery devices.
pub struct BatteryResolver;

declare_static_async_resolver! {
    BatteryResolver,
    (ChipsetDeviceHandleKind, BatteryDeviceHandleX64),
    (ChipsetDeviceHandleKind, BatteryDeviceHandleAArch64),
}

/// Errors that can occur when resolving a battery device.
/// Currently marked unused to dodge compiler warning despite needing
/// this for the type Error below.
#[derive(Debug, Error)]
pub enum ResolveBatteryError {
    #[error("failed to resolve battery")]
    ResolveBattery(#[source] ResolveError),
}

#[async_trait]
impl AsyncResolveResource<ChipsetDeviceHandleKind, BatteryDeviceHandleAArch64> for BatteryResolver {
    type Output = ResolvedChipsetDevice;
    type Error = ResolveBatteryError;

    async fn resolve(
        &self,
        _resolver: &ResourceResolver,
        resource: BatteryDeviceHandleAArch64,
        input: ResolveChipsetDeviceHandleParams<'_>,
    ) -> Result<Self::Output, Self::Error> {
        Ok(BatteryDevice::new(
            BatteryRuntimeDeps {
                battery_status_recv: resource.battery_status_recv,
                notify_interrupt: input.configure.new_line(
                    IRQ_LINE_SET,
                    "battery_status",
                    BATTERY_STATUS_IRQ_NO,
                ),
            },
            BATTERY_MMIO_REGION_BASE_ADDRESS_ARM,
        )
        .into())
    }
}

#[async_trait]
impl AsyncResolveResource<ChipsetDeviceHandleKind, BatteryDeviceHandleX64> for BatteryResolver {
    type Output = ResolvedChipsetDevice;
    type Error = ResolveBatteryError;

    async fn resolve(
        &self,
        _resolver: &ResourceResolver,
        resource: BatteryDeviceHandleX64,
        input: ResolveChipsetDeviceHandleParams<'_>,
    ) -> Result<Self::Output, Self::Error> {
        Ok(BatteryDevice::new(
            BatteryRuntimeDeps {
                battery_status_recv: resource.battery_status_recv,
                notify_interrupt: input.configure.new_line(
                    GPE0_LINE_SET,
                    "battery_status",
                    BATTERY_STATUS_GPE0_LINE,
                ),
            },
            BATTERY_MMIO_REGION_BASE_ADDRESS_X64,
        )
        .into())
    }
}
