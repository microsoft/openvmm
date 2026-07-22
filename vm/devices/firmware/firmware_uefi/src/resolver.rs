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
use hcl_compat_uefi_nvram_storage::BaseSecureBootTemplateVariables;
use hcl_compat_uefi_nvram_storage::HclCompatNvram;
use std::borrow::Cow;
use thiserror::Error;
use uefi_nvram_specvars::signature_list::SignatureData;
use uefi_nvram_specvars::signature_list::SignatureList;
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

fn base_secure_boot_template_variables(
    custom_vars: &firmware_uefi_custom_vars::CustomVars,
) -> Option<BaseSecureBootTemplateVariables> {
    let signatures = custom_vars.signatures.as_ref()?;

    let mut pk = Vec::new();
    extend_signature_var(std::iter::once(&signatures.pk), &mut pk);

    let mut kek = Vec::new();
    extend_signature_var(&signatures.kek, &mut kek);

    let mut db = Vec::new();
    extend_signature_var(&signatures.db, &mut db);

    let mut dbx = Vec::new();
    extend_signature_var(&signatures.dbx, &mut dbx);

    Some(BaseSecureBootTemplateVariables::new(pk, kek, db, dbx))
}

fn extend_signature_var<'a>(
    signatures: impl IntoIterator<Item = &'a firmware_uefi_custom_vars::Signature>,
    data: &mut Vec<u8>,
) {
    use firmware_uefi_custom_vars::Signature;
    use uefi_specs::hyperv::nvram::vars::MSFT_SECURE_BOOT_PRODUCTION_GUID;

    for signature in signatures {
        match signature {
            Signature::X509(certs) => {
                for cert in certs {
                    SignatureList::X509(SignatureData::new_x509(
                        MSFT_SECURE_BOOT_PRODUCTION_GUID,
                        Cow::Borrowed(cert.0.as_slice()),
                    ))
                    .extend_as_spec_signature_list(data);
                }
            }
            Signature::Sha256(digests) => {
                SignatureList::Sha256(
                    digests
                        .iter()
                        .map(|digest| {
                            SignatureData::new_sha256(
                                MSFT_SECURE_BOOT_PRODUCTION_GUID,
                                Cow::Borrowed(&digest.0),
                            )
                        })
                        .collect(),
                )
                .extend_as_spec_signature_list(data);
            }
        }
    }
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

        let nvram_storage = HclCompatNvram::new(
            vmm_core::emuplat::hcl_compat_uefi_nvram_storage::VmgsStorageBackendAdapter(
                nvram_storage,
            ),
            storage_quirks,
        );
        let nvram_storage = if config.secure_boot {
            tracing::info!(
                baseline_configured = config.base_secure_boot_template_vars.signatures.is_some(),
                baseline_revision = config
                    .base_secure_boot_template_vars
                    .baseline_revision()
                    .unwrap_or("none"),
                custom_uefi_config_present = config.custom_uefi_vars.custom_uefi_config_present(),
                "secure boot configuration"
            );

            match base_secure_boot_template_variables(&config.base_secure_boot_template_vars) {
                Some(template) => nvram_storage.with_base_secure_boot_template_variables(template),
                None => nvram_storage,
            }
        } else {
            nvram_storage
        };
        let nvram_storage = Box::new(nvram_storage);

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
