// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Resolvers for the per-platform [`firmware_uefi`] dependencies.

use crate::partition::HvlitePartition;
use anyhow::Context as _;
use async_trait::async_trait;
use firmware_uefi_resources::ResolvedUefiWatchdogPlatform;
use firmware_uefi_resources::UefiWatchdogPlatformHandleKind;
use std::sync::Arc;
use vm_resource::AsyncResolveResource;
use vm_resource::PlatformResource;
use vmcore::non_volatile_store::EphemeralNonVolatileStore;
use vmm_core::partition_unit::Halt;
use watchdog_core::platform::BaseWatchdogPlatform;
use watchdog_core::platform::WatchdogCallback;
use watchdog_core::platform::WatchdogPlatform;

/// Resolver that produces a fresh [`BaseWatchdogPlatform`] (and the matching
/// receiver) for the UEFI watchdog on each resolution.
#[expect(unused)] // One of these will be unused no matter what
pub struct OpenvmmUefiWatchdogPlatformResolver {
    // TODO: Should this be a weak reference?
    partition: Arc<dyn HvlitePartition>,
    halt_vps: Arc<Halt>,
}

impl OpenvmmUefiWatchdogPlatformResolver {
    pub fn new(partition: Arc<dyn HvlitePartition>, halt_vps: Arc<Halt>) -> Self {
        Self {
            partition,
            halt_vps,
        }
    }
}

#[async_trait]
impl AsyncResolveResource<UefiWatchdogPlatformHandleKind, PlatformResource>
    for OpenvmmUefiWatchdogPlatformResolver
{
    type Output = ResolvedUefiWatchdogPlatform;
    type Error = anyhow::Error;

    async fn resolve(
        &self,
        _resolver: &vm_resource::ResourceResolver,
        _resource: PlatformResource,
        _input: &(),
    ) -> Result<Self::Output, Self::Error> {
        let (watchdog_send, watchdog_recv) = mesh::channel();
        let store = EphemeralNonVolatileStore::new_boxed();
        let mut platform = BaseWatchdogPlatform::new(store)
            .await
            .context("failed to initialize UEFI watchdog platform")?;
        #[cfg(guest_arch = "x86_64")]
        platform.add_callback(Box::new(UefiWatchdogTimeoutNmi {
            partition: self.partition.clone(),
            watchdog_send,
        }));
        #[cfg(guest_arch = "aarch64")]
        platform.add_callback(Box::new(UefiWatchdogTimeoutReset {
            halt_vps: self.halt_vps.clone(),
            watchdog_send,
        }));
        Ok(ResolvedUefiWatchdogPlatform {
            platform: Box::new(platform),
            watchdog_recv,
        })
    }
}

/// On-timeout callback used by the OpenVMM UEFI watchdog: sends an NMI to the
/// BSP on x86_64 and resets the VM on aarch64.
#[cfg(guest_arch = "x86_64")]
struct UefiWatchdogTimeoutNmi {
    // TODO: Should this be a weak?
    partition: Arc<dyn HvlitePartition>,
    watchdog_send: mesh::Sender<()>,
}

#[cfg(guest_arch = "x86_64")]
#[async_trait]
impl WatchdogCallback for UefiWatchdogTimeoutNmi {
    async fn on_timeout(&mut self) {
        self.partition.request_msi(
            hvdef::Vtl::Vtl0,
            virt::irqcon::MsiRequest::new_x86(virt::irqcon::DeliveryMode::NMI, 0, false, 0, false),
        );
        self.watchdog_send.send(());
    }
}

#[cfg(guest_arch = "aarch64")]
struct UefiWatchdogTimeoutReset {
    halt_vps: Arc<Halt>,
    watchdog_send: mesh::Sender<()>,
}

#[cfg(guest_arch = "aarch64")]
#[async_trait]
impl WatchdogCallback for UefiWatchdogTimeoutReset {
    async fn on_timeout(&mut self) {
        self.halt_vps.halt(vmm_core_defs::HaltReason::Reset);
        self.watchdog_send.send(());
    }
}
