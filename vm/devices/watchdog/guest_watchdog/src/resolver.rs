// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Resource resolver for the Hyper-V guest watchdog chipset device.

use crate::GuestWatchdogServices;
use async_trait::async_trait;
use chipset_device_resources::ResolveChipsetDeviceHandleParams;
use chipset_device_resources::ResolvedChipsetDevice;
use chipset_resources::hyperv_guest_watchdog::HyperVGuestWatchdogDeviceHandle;
use thiserror::Error;
use vm_resource::AsyncResolveResource;
use vm_resource::IntoResource;
use vm_resource::PlatformResource;
use vm_resource::ResolveError;
use vm_resource::ResourceResolver;
use vm_resource::declare_static_async_resolver;
use vm_resource::kind::ChipsetDeviceHandleKind;
use watchdog_core::platform::WatchdogCallback;
use watchdog_core::platform::WatchdogPlatform;
use watchdog_core::resources::WatchdogPlatformHandleKind;

/// Resolver for the Hyper-V guest watchdog device.
pub struct HyperVGuestWatchdogResolver;

declare_static_async_resolver! {
    HyperVGuestWatchdogResolver,
    (ChipsetDeviceHandleKind, HyperVGuestWatchdogDeviceHandle),
}

/// Errors that can occur while resolving a guest watchdog handle.
#[derive(Debug, Error)]
pub enum ResolveGuestWatchdogError {
    /// Failed to resolve the watchdog platform capability.
    #[error("failed to resolve watchdog platform capability")]
    ResolvePlatform(#[source] ResolveError),
}

#[async_trait]
impl AsyncResolveResource<ChipsetDeviceHandleKind, HyperVGuestWatchdogDeviceHandle>
    for HyperVGuestWatchdogResolver
{
    type Output = ResolvedChipsetDevice;
    type Error = ResolveGuestWatchdogError;

    async fn resolve(
        &self,
        resolver: &ResourceResolver,
        resource: HyperVGuestWatchdogDeviceHandle,
        input: ResolveChipsetDeviceHandleParams<'_>,
    ) -> Result<Self::Output, Self::Error> {
        let mut pio_static_wdat_port = input.register_pio.new_io_region("wdat_port", 8);
        pio_static_wdat_port.map(resource.port_base);

        let mut watchdog_platform = resolver
            .resolve::<WatchdogPlatformHandleKind, _>(PlatformResource.into_resource(), ())
            .await
            .map_err(ResolveGuestWatchdogError::ResolvePlatform)?
            .into_inner();
        if resource.log_only {
            watchdog_platform = Box::new(NoActionWatchdog(watchdog_platform));
        }

        let device = GuestWatchdogServices::new(
            input.vmtime.access("guest-watchdog-time"),
            watchdog_platform,
            pio_static_wdat_port,
            input.is_restoring,
        )
        .await;

        Ok(device.into())
    }
}

/// Logs watchdog timeouts without notifying the platform, so the VM keeps
/// running and no watchdog boot failure is recorded.
struct NoActionWatchdog(Box<dyn WatchdogPlatform>);

#[async_trait]
impl WatchdogPlatform for NoActionWatchdog {
    async fn on_timeout(&mut self) {
        tracelimit::warn_ratelimited!("guest watchdog expired; configured to take no action");
    }

    async fn read_and_clear_boot_status(&mut self) -> bool {
        self.0.read_and_clear_boot_status().await
    }

    fn add_callback(&mut self, callback: Box<dyn WatchdogCallback>) {
        self.0.add_callback(callback);
    }
}
