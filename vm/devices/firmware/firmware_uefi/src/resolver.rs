// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Resource resolver for the Hyper-V UEFI helper chipset device.

use crate::UefiDevice;
use crate::UefiRuntimeDeps;
use async_trait::async_trait;
use chipset_device_resources::GPE0_LINE_SET;
use chipset_device_resources::IRQ_LINE_SET;
use chipset_device_resources::ResolveChipsetDeviceHandleParams;
use chipset_device_resources::ResolvedChipsetDevice;
use chipset_resources::CmosRtcTimeSourceHandleKind;
use firmware_uefi_resources::ResolvedUefiWatchdogPlatform;
use firmware_uefi_resources::UefiCommandSet;
use firmware_uefi_resources::UefiDeviceHandle;
use firmware_uefi_resources::UefiLoggerHandleKind;
use firmware_uefi_resources::UefiVsmConfigHandleKind;
use firmware_uefi_resources::UefiWatchdogPlatformHandleKind;
use hcl_compat_uefi_nvram_storage::HclCompatNvram;
use thiserror::Error;
use vm_resource::AsyncResolveResource;
use vm_resource::ResolveError;
use vm_resource::ResourceResolver;
use vm_resource::declare_static_async_resolver;
use vm_resource::kind::ChipsetDeviceHandleKind;
use vm_resource::kind::NonVolatileStoreKind;

/// Resolver for the Hyper-V UEFI helper device.
pub struct UefiDeviceResolver;

declare_static_async_resolver! {
    UefiDeviceResolver,
    (ChipsetDeviceHandleKind, UefiDeviceHandle),
}

/// Errors that can occur while resolving a UEFI device handle.
#[derive(Debug, Error)]
pub enum ResolveUefiDeviceError {
    /// Failed to resolve the UEFI logger.
    #[error("failed to resolve UEFI logger")]
    ResolveLogger(#[source] ResolveError),
    /// Failed to resolve the UEFI NVRAM storage.
    #[error("failed to resolve UEFI NVRAM storage")]
    ResolveNvramStorage(#[source] ResolveError),
    /// Failed to resolve the UEFI watchdog platform.
    #[error("failed to resolve UEFI watchdog platform")]
    ResolveWatchdogPlatform(#[source] ResolveError),
    /// Failed to resolve the UEFI VSM configuration.
    #[error("failed to resolve UEFI VSM configuration")]
    ResolveVsmConfig(#[source] ResolveError),
    /// Failed to resolve the UEFI time source.
    #[error("failed to resolve UEFI time source")]
    ResolveTimeSource(#[source] ResolveError),
    /// Failed to initialize the UEFI device.
    #[error("failed to initialize UEFI device")]
    Init(#[from] crate::UefiInitError),
}

// The ACPI GPE0 line to use for generation ID. This must match the value in
// the DSDT.
const GPE0_LINE_GENERATION_ID: u32 = 0;
// For ARM64, 3 + 32 (SPI range start) = 35, the SYSTEM_SPI_GENCOUNTER vector
// for the GIC.
const GENERATION_ID_IRQ: u32 = 3;

#[async_trait]
impl AsyncResolveResource<ChipsetDeviceHandleKind, UefiDeviceHandle> for UefiDeviceResolver {
    type Output = ResolvedChipsetDevice;
    type Error = ResolveUefiDeviceError;

    async fn resolve(
        &self,
        resolver: &ResourceResolver,
        resource: UefiDeviceHandle,
        input: ResolveChipsetDeviceHandleParams<'_>,
    ) -> Result<Self::Output, Self::Error> {
        let UefiDeviceHandle {
            config,
            storage_quirks,
            generation_id_recv,
            logger,
            nvram_storage,
            watchdog_platform,
            vsm_config,
            time_source,
        } = resource;

        let logger = resolver
            .resolve::<UefiLoggerHandleKind, _>(logger, ())
            .await
            .map_err(ResolveUefiDeviceError::ResolveLogger)?
            .0;
        let nvram_storage = resolver
            .resolve::<NonVolatileStoreKind, _>(nvram_storage, &())
            .await
            .map_err(ResolveUefiDeviceError::ResolveNvramStorage)?
            .0;
        let ResolvedUefiWatchdogPlatform {
            platform: watchdog_platform,
            watchdog_recv,
        } = resolver
            .resolve::<UefiWatchdogPlatformHandleKind, _>(watchdog_platform, &())
            .await
            .map_err(ResolveUefiDeviceError::ResolveWatchdogPlatform)?;
        let vsm_config = if let Some(vsm_config) = vsm_config {
            Some(
                resolver
                    .resolve::<UefiVsmConfigHandleKind, _>(vsm_config, ())
                    .await
                    .map_err(ResolveUefiDeviceError::ResolveVsmConfig)?
                    .0,
            )
        } else {
            None
        };
        let time_source = resolver
            .resolve::<CmosRtcTimeSourceHandleKind, _>(time_source, ())
            .await
            .map_err(ResolveUefiDeviceError::ResolveTimeSource)?
            .0;

        let notify_interrupt = match config.command_set {
            UefiCommandSet::X64 => {
                input
                    .configure
                    .new_line(GPE0_LINE_SET, "genid", GPE0_LINE_GENERATION_ID)
            }
            UefiCommandSet::Aarch64 => {
                input
                    .configure
                    .new_line(IRQ_LINE_SET, "genid", GENERATION_ID_IRQ)
            }
        };

        let nvram_storage = Box::new(HclCompatNvram::new(
            vmm_core::emuplat::hcl_compat_uefi_nvram_storage::VmgsStorageBackendAdapter(
                nvram_storage,
            ),
            storage_quirks,
        ));

        let gm = input.encrypted_guest_memory.clone();
        let runtime_deps = UefiRuntimeDeps {
            gm: gm.clone(),
            nvram_storage,
            logger,
            vmtime: input.vmtime,
            watchdog_platform,
            watchdog_recv,
            generation_id_deps: generation_id::GenerationIdRuntimeDeps {
                generation_id_recv,
                gm,
                notify_interrupt,
            },
            vsm_config,
            time_source,
        };

        let device = UefiDevice::new(runtime_deps, config, input.is_restoring).await?;
        Ok(device.into())
    }
}
