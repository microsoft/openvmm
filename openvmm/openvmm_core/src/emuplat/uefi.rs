// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Resolvers for the per-platform [`firmware_uefi`] dependencies.

use crate::partition::HvlitePartition;
use crate::vmgs_non_volatile_store::HvLiteVmgsNonVolatileStore;
use anyhow::Context as _;
use async_trait::async_trait;
use firmware_uefi_resources::EphemeralNvramStorageHandle;
use firmware_uefi_resources::ResolvedUefiNvramStorage;
use firmware_uefi_resources::ResolvedUefiWatchdogPlatform;
use firmware_uefi_resources::UefiNvramStorageHandleKind;
use firmware_uefi_resources::UefiWatchdogPlatformHandleKind;
use firmware_uefi_resources::VmgsNvramStorageHandle;
use hcl_compat_uefi_nvram_storage::HclCompatNvram;
use std::sync::Arc;
use uefi_nvram_storage::VmmNvramStorage;
use uefi_nvram_storage::in_memory::InMemoryNvram;
use vm_resource::AsyncResolveResource;
use vm_resource::PlatformResource;
use vm_resource::ResolveResource;
use vmcore::non_volatile_store::EphemeralNonVolatileStore;
use vmm_core::emuplat::hcl_compat_uefi_nvram_storage::VmgsStorageBackendAdapter;
use vmm_core::partition_unit::Halt;
use watchdog_core::platform::BaseWatchdogPlatform;
use watchdog_core::platform::WatchdogCallback;
use watchdog_core::platform::WatchdogPlatform;

/// Resolver that produces UEFI NVRAM storage backed by the host VMGS file.
pub struct VmgsUefiNvramStorageResolver {
    vmgs_client: vmgs_broker::VmgsClient,
}

impl VmgsUefiNvramStorageResolver {
    pub fn new(vmgs_client: vmgs_broker::VmgsClient) -> Self {
        Self { vmgs_client }
    }
}

impl ResolveResource<UefiNvramStorageHandleKind, VmgsNvramStorageHandle>
    for VmgsUefiNvramStorageResolver
{
    type Output = ResolvedUefiNvramStorage;
    type Error = anyhow::Error;

    fn resolve(
        &self,
        _resource: VmgsNvramStorageHandle,
        _input: (),
    ) -> Result<Self::Output, Self::Error> {
        let storage: Box<dyn VmmNvramStorage> = Box::new(HclCompatNvram::new(
            VmgsStorageBackendAdapter(
                self.vmgs_client
                    .as_non_volatile_store(vmgs::FileId::BIOS_NVRAM, true)
                    .context("failed to instantiate UEFI NVRAM store")?,
            ),
            None,
        ));
        Ok(ResolvedUefiNvramStorage(storage))
    }
}

/// Resolver that produces a fresh ephemeral in-memory UEFI NVRAM store.
pub struct EphemeralUefiNvramStorageResolver;

impl ResolveResource<UefiNvramStorageHandleKind, EphemeralNvramStorageHandle>
    for EphemeralUefiNvramStorageResolver
{
    type Output = ResolvedUefiNvramStorage;
    type Error = std::convert::Infallible;

    fn resolve(
        &self,
        _resource: EphemeralNvramStorageHandle,
        _input: (),
    ) -> Result<Self::Output, Self::Error> {
        Ok(ResolvedUefiNvramStorage(Box::new(InMemoryNvram::new())))
    }
}

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
