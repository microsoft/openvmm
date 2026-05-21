// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Resolvers for the per-platform [`firmware_uefi`] dependencies (Underhill).

use crate::emuplat::firmware::UnderhillLogger;
use crate::emuplat::firmware::UnderhillVsmConfig;
use crate::emuplat::non_volatile_store::VmgsBrokerNonVolatileStore;
use crate::emuplat::watchdog::UnderhillWatchdogPlatform;
use anyhow::Context as _;
use async_trait::async_trait;
use firmware_uefi_resources::ResolvedUefiLogger;
use firmware_uefi_resources::ResolvedUefiNvramStorage;
use firmware_uefi_resources::ResolvedUefiVsmConfig;
use firmware_uefi_resources::ResolvedUefiWatchdogPlatform;
use firmware_uefi_resources::UefiLoggerHandleKind;
use firmware_uefi_resources::UefiNvramStorageHandleKind;
use firmware_uefi_resources::UefiVsmConfigHandleKind;
use firmware_uefi_resources::UefiWatchdogPlatformHandleKind;
use guest_emulation_transport::GuestEmulationTransportClient;
use hcl_compat_uefi_nvram_storage::HclCompatNvram;
use hcl_compat_uefi_nvram_storage::HclCompatNvramQuirks;
use std::sync::Arc;
use std::sync::Weak;
use uefi_nvram_storage::VmmNvramStorage;
use uefi_nvram_storage::in_memory::InMemoryNvram;
use virt_mshv_vtl::UhPartition;
use vm_resource::AsyncResolveResource;
use vm_resource::PlatformResource;
use vm_resource::ResolveResource;
use vmcore::non_volatile_store::EphemeralNonVolatileStore;
use vmm_core::emuplat::hcl_compat_uefi_nvram_storage::VmgsStorageBackendAdapter;
use vmm_core::partition_unit::Halt;
use watchdog_core::platform::WatchdogCallback;
use watchdog_core::platform::WatchdogPlatform;

/// Resolver that creates a fresh [`UnderhillLogger`] each time it is resolved.
pub struct UnderhillUefiLoggerResolver {
    get: GuestEmulationTransportClient,
}

impl UnderhillUefiLoggerResolver {
    pub fn new(get: GuestEmulationTransportClient) -> Self {
        Self { get }
    }
}

impl ResolveResource<UefiLoggerHandleKind, PlatformResource> for UnderhillUefiLoggerResolver {
    type Output = ResolvedUefiLogger;
    type Error = std::convert::Infallible;

    fn resolve(
        &self,
        _resource: PlatformResource,
        _input: (),
    ) -> Result<Self::Output, Self::Error> {
        Ok(ResolvedUefiLogger(Box::new(UnderhillLogger {
            get: self.get.clone(),
        })))
    }
}

/// Resolver that produces UEFI NVRAM storage backed by the host VMGS, or an
/// in-memory fallback.
pub struct UnderhillUefiNvramStorageResolver {
    vmgs_client: Option<vmgs_broker::VmgsClient>,
}

impl UnderhillUefiNvramStorageResolver {
    pub fn new(vmgs_client: Option<vmgs_broker::VmgsClient>) -> Self {
        Self { vmgs_client }
    }
}

impl ResolveResource<UefiNvramStorageHandleKind, PlatformResource>
    for UnderhillUefiNvramStorageResolver
{
    type Output = ResolvedUefiNvramStorage;
    type Error = anyhow::Error;

    fn resolve(
        &self,
        _resource: PlatformResource,
        _input: (),
    ) -> Result<Self::Output, Self::Error> {
        let storage: Box<dyn VmmNvramStorage> = match &self.vmgs_client {
            Some(vmgs) => Box::new(HclCompatNvram::new(
                VmgsStorageBackendAdapter(
                    vmgs.as_non_volatile_store(vmgs::FileId::BIOS_NVRAM, true)
                        .context("failed to instantiate UEFI NVRAM store")?,
                ),
                Some(HclCompatNvramQuirks {
                    skip_corrupt_vars_with_missing_null_term: true,
                }),
            )),
            None => Box::new(InMemoryNvram::new()),
        };
        Ok(ResolvedUefiNvramStorage(storage))
    }
}

/// Resolver that produces a fresh [`UnderhillWatchdogPlatform`] each time it
/// is resolved, along with the matching receiver.
#[expect(unused)] // One of these will be unused no matter what
pub struct UnderhillUefiWatchdogPlatformResolver {
    get: GuestEmulationTransportClient,
    // TODO: Should this be a weak?
    partition: Arc<UhPartition>,
    halt_vps: Arc<Halt>,
}

impl UnderhillUefiWatchdogPlatformResolver {
    pub fn new(
        get: GuestEmulationTransportClient,
        partition: Arc<UhPartition>,
        halt_vps: Arc<Halt>,
    ) -> Self {
        Self {
            get,
            partition,
            halt_vps,
        }
    }
}

#[async_trait]
impl AsyncResolveResource<UefiWatchdogPlatformHandleKind, PlatformResource>
    for UnderhillUefiWatchdogPlatformResolver
{
    type Output = ResolvedUefiWatchdogPlatform;
    type Error = anyhow::Error;

    async fn resolve(
        &self,
        _resolver: &vm_resource::ResourceResolver,
        _handle: PlatformResource,
        _input: &(),
    ) -> Result<Self::Output, Self::Error> {
        let (watchdog_send, watchdog_recv) = mesh::channel();
        let store = EphemeralNonVolatileStore::new_boxed();
        let mut platform = UnderhillWatchdogPlatform::new(store, self.get.clone())
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

/// Resolver that produces a fresh [`UnderhillVsmConfig`] each time it is
/// resolved.
pub struct UnderhillUefiVsmConfigResolver {
    partition: Weak<UhPartition>,
}

impl UnderhillUefiVsmConfigResolver {
    pub fn new(partition: Weak<UhPartition>) -> Self {
        Self { partition }
    }
}

impl ResolveResource<UefiVsmConfigHandleKind, PlatformResource> for UnderhillUefiVsmConfigResolver {
    type Output = ResolvedUefiVsmConfig;
    type Error = std::convert::Infallible;

    fn resolve(
        &self,
        _resource: PlatformResource,
        _input: (),
    ) -> Result<Self::Output, Self::Error> {
        Ok(ResolvedUefiVsmConfig(Box::new(UnderhillVsmConfig {
            partition: self.partition.clone(),
        })))
    }
}

#[cfg(guest_arch = "x86_64")]
struct UefiWatchdogTimeoutNmi {
    partition: Arc<UhPartition>,
    watchdog_send: mesh::Sender<()>,
}

#[cfg(guest_arch = "x86_64")]
#[async_trait]
impl WatchdogCallback for UefiWatchdogTimeoutNmi {
    async fn on_timeout(&mut self) {
        use virt::Partition;
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
        use vmm_core_defs::HaltReason;
        self.halt_vps.halt(HaltReason::Reset);
        self.watchdog_send.send(());
    }
}
