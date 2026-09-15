// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! UEFI Nvram variable services subsystem.
//!
//! Special care has been taken to keep all Hyper-V specific interfaces and
//! extensions in a separate layer from the underlying UEFI spec mandated
//! functionality.
//!
//! e.g: things like injecting various nvram vars related to secure boot, boot
//! order, etc... are not part of the UEFI spec, and are therefore implemented
//! _outside_ of the [`spec_services`] module.

pub use spec_services::NvramError;
pub use spec_services::NvramResult;
pub use spec_services::NvramServicesExt;
pub use spec_services::NvramSpecServices;

use crate::UefiDevice;
use cvm_tracing::CVM_ALLOWED;
use cvm_tracing::CVM_CONFIDENTIAL;
use firmware_uefi_custom_vars::BaseTemplate;
use firmware_uefi_custom_vars::BaseTemplateVars;
use firmware_uefi_custom_vars::FinalVars;
use firmware_uefi_custom_vars::Signature;
use firmware_uefi_custom_vars::UefiVarsDeltaJson;
use firmware_uefi_resources::platform::VsmConfig;
use guestmem::GuestMemoryError;
use guid::Guid;
use inspect::Inspect;
use std::borrow::Cow;
use std::collections::BTreeSet;
use std::fmt::Debug;
use std::mem::size_of;
use std::mem::size_of_val;
use thiserror::Error;
use uefi_nvram_specvars::signature_list::ParseError as SignatureListParseError;
use uefi_nvram_specvars::signature_list::ParseSignatureList;
use uefi_nvram_specvars::signature_list::ParseSignatureLists;
use uefi_nvram_storage::VmmNvramStorage;
use uefi_specs::uefi::common::EfiStatus;
use uefi_specs::uefi::nvram::EFI_VARIABLE_AUTHENTICATION_2;
use uefi_specs::uefi::nvram::EfiVariableAttributes;
use uefi_specs::uefi::nvram::signature_list::EFI_SIGNATURE_DATA;
use uefi_specs::uefi::nvram::vars;
use uefi_specs::uefi::signing::EFI_CERT_TYPE_PKCS7_GUID;
use uefi_specs::uefi::signing::WIN_CERT_TYPE_EFI_GUID;
use uefi_specs::uefi::signing::WIN_CERTIFICATE_UEFI_GUID;
use zerocopy::FromBytes;
use zerocopy::IntoBytes;

#[cfg(feature = "fuzzing")]
pub mod spec_services;
#[cfg(not(feature = "fuzzing"))]
mod spec_services;

const WIN_CERT_REVISION_2_0: u16 = 0x0200;

/// A Secure Boot signature's type, owner, and payload used for set comparison.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
enum SignatureValue {
    X509(EFI_SIGNATURE_DATA, Vec<u8>),
    Sha256(EFI_SIGNATURE_DATA, Vec<u8>),
}

/// Unique Secure Boot signatures contained in a variable or base template.
type SignatureSet = BTreeSet<SignatureValue>;

/// Secure Boot telemetry schema mirrored by legacy HCL's `UefiNvramStore.cpp`.
struct SecureBootVariableReport<'a> {
    name: &'a ucs2::Ucs2LeSlice,
    in_nvram: bool,
    has_custom_uefi: bool,
    loaded_bytes: usize,
    loaded_entries: usize,
    template_entries: usize,
    missing_entries: usize,
}

impl Debug for SecureBootVariableReport<'_> {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let Self {
            name,
            in_nvram,
            has_custom_uefi,
            loaded_bytes,
            loaded_entries,
            template_entries,
            missing_entries,
        } = self;

        formatter
            .debug_struct("SecureBootVariable")
            .field("name", name)
            .field("in_nvram", in_nvram)
            .field("has_custom_uefi", has_custom_uefi)
            .field("loaded_bytes", &format_args!("{loaded_bytes:#x}"))
            .field("loaded_entries", &format_args!("{loaded_entries:#x}"))
            .field("template_entries", &format_args!("{template_entries:#x}"))
            .field("missing_entries", &format_args!("{missing_entries:#x}"))
            .finish()
    }
}

#[derive(Debug, Error)]
pub enum NvramSetupError {
    #[error("could not query backing nvram storage")]
    BadNvramStorage(#[source] uefi_nvram_storage::NvramStorageError),
    #[error("failed to apply custom UEFI template variables")]
    ApplyCustomTemplate(#[source] firmware_uefi_custom_vars::ApplyDeltaError),
    #[error("failed to load built-in UEFI template")]
    LoadBaseTemplate(#[source] hyperv_uefi_custom_vars_json::ParseJsonError),
    #[error("failed to load custom UEFI variable JSON")]
    LoadCustomUefiJson(#[source] hyperv_uefi_custom_vars_json::ParseJsonError),
    #[error("could not inject pre-boot var '{0}': {1:?}")]
    InjectPreBootVar(
        Cow<'static, ucs2::Ucs2LeSlice>,
        EfiStatus,
        #[source] Option<NvramError>,
    ),
    #[error("could not inject signature var '{0}': {1:?}")]
    InjectSigVar(
        Cow<'static, ucs2::Ucs2LeSlice>,
        EfiStatus,
        #[source] Option<NvramError>,
    ),
    #[error("could not inject UEFI var '{0}': {1:?}")]
    InjectUefiVar(String, EfiStatus, #[source] Option<NvramError>),
    #[error("UEFI variable name is not valid UCS-2")]
    UefiVarNotUcs2,
}

/// Implements Hyper-V specific nvram service interfaces, extensions, and
/// functionality, deferring to the underlying [`NvramSpecServices`] object to
/// implement any UEFI spec mandated nvram service functionality.
#[derive(Inspect)]
pub struct NvramServices {
    // Runtime glue
    #[inspect(skip)]
    vsm_config: Option<Box<dyn VsmConfig>>,

    // Sub-emulators
    #[inspect(flatten)]
    services: NvramSpecServices<Box<dyn VmmNvramStorage>>,
}

impl NvramServices {
    pub async fn new(
        nvram_storage: Box<dyn VmmNvramStorage>,
        base_template: Option<BaseTemplate>,
        custom_uefi_json: Option<UefiVarsDeltaJson>,
        secure_boot_enabled: bool,
        vsm_config: Option<Box<dyn VsmConfig>>,
        is_restoring: bool,
    ) -> Result<NvramServices, NvramSetupError> {
        let mut nvram = NvramServices {
            services: NvramSpecServices::new(nvram_storage),
            vsm_config,
        };

        if !is_restoring {
            let is_nvram_empty = nvram
                .services
                .is_empty()
                .await
                .map_err(NvramSetupError::BadNvramStorage)?;
            let base_template = match base_template
                .filter(|_| is_nvram_empty || secure_boot_enabled)
                .map(|template| {
                    hyperv_uefi_custom_vars_json::parse_template_json(template.json.as_bytes())
                })
                .transpose()
            {
                Ok(vars) => vars,
                Err(error) if !is_nvram_empty => {
                    tracing::warn!(
                        CVM_ALLOWED,
                        error = &error as &dyn std::error::Error,
                        "secure boot configuration report skipped: failed to parse baseline template"
                    );
                    None
                }
                Err(error) => return Err(NvramSetupError::LoadBaseTemplate(error)),
            };

            // Top-level configuration reads the VMGS CUSTOM_UEFI entry on every boot and
            // populates this value when the entry is non-empty, matching legacy HCL. The delta
            // itself is only applied below when NVRAM is empty.
            let has_custom_uefi = custom_uefi_json
                .as_ref()
                .is_some_and(|json| !json.as_bytes().is_empty());

            if is_nvram_empty {
                nvram
                    .inject_initial_vars(base_template.as_ref(), custom_uefi_json)
                    .await?;
            }
            nvram.inject_hyperv_vars().await?;
            nvram.setup_secure_boot(secure_boot_enabled).await?;

            if secure_boot_enabled {
                nvram
                    .report_secure_boot_configuration(base_template.as_ref(), has_custom_uefi)
                    .await;
            }
        }

        nvram.services.prepare_for_boot();

        Ok(nvram)
    }

    pub fn reset(&mut self) {
        self.services.reset();
        self.services.prepare_for_boot();
    }

    /// Inject hard-coded and configured UEFI variables into empty NVRAM.
    async fn inject_initial_vars(
        &mut self,
        base_template_vars: Option<&BaseTemplateVars>,
        custom_uefi_json: Option<UefiVarsDeltaJson>,
    ) -> Result<(), NvramSetupError> {
        let custom_template_delta = custom_uefi_json
            .map(|json| hyperv_uefi_custom_vars_json::parse_delta_json(json.as_bytes()))
            .transpose()
            .map_err(NvramSetupError::LoadCustomUefiJson)?;
        let final_vars = FinalVars::resolve(base_template_vars.cloned(), custom_template_delta)
            .map_err(NvramSetupError::ApplyCustomTemplate)?;

        tracing::info!("No NVRAM variables (first boot). Loading in initial NVRAM values.");

        // Windows uses CurrentPolicy to protect secure boot policy
        tracing::trace!("Injecting 'CurrentPolicy'");
        {
            use uefi_specs::hyperv::nvram::vars::CURRENT_POLICY;

            let (vendor, name) = CURRENT_POLICY();
            const CURRENT_POLICY_AUTHENTICATED_MARKER: u8 = 0x02;
            let data = [CURRENT_POLICY_AUTHENTICATED_MARKER];
            let attr = EfiVariableAttributes::DEFAULT_ATTRIBUTES_TIME_BASED_AUTH;

            // because this variable is set with time based auth, it needs a
            // `EFI_VARIABLE_AUTHENTICATION_2`. fortunately, we are still in
            // pre-boot, which means it suffices to use a dummy header.
            let data = {
                let mut v = Vec::new();
                v.extend(EFI_VARIABLE_AUTHENTICATION_2::DUMMY.as_bytes());
                v.extend(data);
                v
            };

            self.services
                .set_variable_ucs2(vendor, name, attr.into(), data.clone())
                .await
                .map_err(|(status, err)| {
                    NvramSetupError::InjectPreBootVar(name.into(), status, err)
                })?;
        }

        tracing::trace!("Updating 'SetupMode'");
        {
            use uefi_specs::uefi::nvram::vars::SETUP_MODE;
            let (_, name) = SETUP_MODE();

            self.services.update_setup_mode().await.map_err(|e| {
                NvramSetupError::InjectPreBootVar(
                    name.into(),
                    EfiStatus::DEVICE_ERROR,
                    Some(NvramError::NvramStorage(e)),
                )
            })?
        }

        self.inject_final_vars(final_vars).await?;

        Ok(())
    }

    /// Compare one loaded Secure Boot variable with its base-template signatures.
    async fn get_secure_boot_variable_report<'a>(
        &mut self,
        (vendor, name): (Guid, &'a ucs2::Ucs2LeSlice),
        template_signatures: &[Signature],
        has_custom_uefi: bool,
    ) -> Option<SecureBootVariableReport<'a>> {
        let base_signatures = base_signature_set(template_signatures);
        if base_signatures.is_empty() {
            tracing::warn!(
                CVM_ALLOWED,
                %name,
                "secure boot variable omitted from configuration report: baseline contains no signatures"
            );
            return None;
        }

        match self.services.get_variable_ucs2(vendor, name).await {
            // The variable exists in NVRAM but is empty
            Ok((_, data)) if data.is_empty() => Some(SecureBootVariableReport {
                name,
                in_nvram: true,
                has_custom_uefi,
                loaded_bytes: 0,
                loaded_entries: 0,
                template_entries: base_signatures.len(),
                missing_entries: base_signatures.len(),
            }),
            // The variable exists in NVRAM and has data
            Ok((_, data)) => match collect_signature_set(&data) {
                // The data is valid
                Ok(loaded_signatures) => Some(SecureBootVariableReport {
                    name,
                    in_nvram: true,
                    has_custom_uefi,
                    loaded_bytes: data.len(),
                    loaded_entries: loaded_signatures.len(),
                    template_entries: base_signatures.len(),
                    missing_entries: base_signatures.difference(&loaded_signatures).count(),
                }),
                // The data cannot be parsed
                Err(error) => {
                    tracing::warn!(
                        CVM_CONFIDENTIAL,
                        %name,
                        error = &error as &dyn std::error::Error,
                        "secure boot variable omitted from configuration report: failed to parse variable"
                    );
                    None
                }
            },
            // The variable does not exist in NVRAM
            Err((EfiStatus::NOT_FOUND, _)) => Some(SecureBootVariableReport {
                name,
                in_nvram: false,
                has_custom_uefi,
                loaded_bytes: 0,
                loaded_entries: 0,
                template_entries: base_signatures.len(),
                missing_entries: base_signatures.len(),
            }),
            // The variable exists in NVRAM but cannot be read for some other reason
            Err((status, error)) => {
                tracing::warn!(
                    CVM_CONFIDENTIAL,
                    %name,
                    ?status,
                    ?error,
                    "secure boot variable omitted from configuration report: failed to read variable"
                );
                None
            }
        }
    }

    /// Report the loaded Secure Boot configuration against the selected base template.
    async fn report_secure_boot_configuration(
        &mut self,
        base_template: Option<&BaseTemplateVars>,
        has_custom_uefi: bool,
    ) {
        let Some(signatures) = base_template.and_then(BaseTemplateVars::signatures) else {
            tracing::warn!(
                CVM_ALLOWED,
                "secure boot configuration report skipped: baseline signatures are unavailable"
            );
            return;
        };

        // Report each secure boot variable's loaded configuration against the baseline template
        for (variable, template_signatures) in [
            (vars::PK(), std::slice::from_ref(&signatures.pk)),
            (vars::KEK(), signatures.kek.as_slice()),
            (vars::DB(), signatures.db.as_slice()),
            (vars::DBX(), signatures.dbx.as_slice()),
        ] {
            if let Some(variable_report) = self
                .get_secure_boot_variable_report(variable, template_signatures, has_custom_uefi)
                .await
            {
                tracing::info!(CVM_ALLOWED, "{variable_report:?}");
            }
        }
    }

    async fn inject_hyperv_vars(&mut self) -> Result<(), NvramSetupError> {
        // Always inject these, in case the vmgs file was first booted RS1.86
        tracing::trace!("Injecting 'OsLoaderIndications'");
        {
            use uefi_specs::hyperv::nvram::vars::OS_LOADER_INDICATIONS;

            let (vendor, name) = OS_LOADER_INDICATIONS();
            let data = 0u32.as_bytes();
            let attr = EfiVariableAttributes::new().with_bootservice_access(true);

            self.services
                .set_variable_ucs2(vendor, name, attr.into(), data.to_vec())
                .await
                .map_err(|(status, err)| {
                    NvramSetupError::InjectPreBootVar(name.into(), status, err)
                })?;
        }

        tracing::trace!("Injecting 'OsLoaderIndicationsSupported'");
        {
            use uefi_specs::hyperv::nvram::vars::OS_LOADER_INDICATIONS_SUPPORTED;

            let (vendor, name) = OS_LOADER_INDICATIONS_SUPPORTED();
            // All VM versions capable of running the HCL support VSM
            let data = 1u32.as_bytes();
            let attr = EfiVariableAttributes::new().with_bootservice_access(true);

            self.services
                .set_variable_ucs2(vendor, name, attr.into(), data.to_vec())
                .await
                .map_err(|(status, err)| {
                    NvramSetupError::InjectPreBootVar(name.into(), status, err)
                })?;
        }

        Ok(())
    }

    async fn inject_final_vars(&mut self, final_vars: FinalVars) -> Result<(), NvramSetupError> {
        use firmware_uefi_custom_vars::Sha256Digest;
        use firmware_uefi_custom_vars::Signature;
        use firmware_uefi_custom_vars::UefiVar;
        use firmware_uefi_custom_vars::X509Cert;
        use uefi_nvram_specvars::signature_list::SignatureData;
        use uefi_nvram_specvars::signature_list::SignatureList;
        use uefi_specs::hyperv::nvram::vars::MSFT_SECURE_BOOT_PRODUCTION_GUID;
        use uefi_specs::uefi::nvram::EFI_VARIABLE_AUTHENTICATION_2;

        let firmware_uefi_custom_vars::UefiVars {
            signatures,
            non_signature_vars,
        } = final_vars.into_uefi_vars();

        // Inject non-signature vars first, as some may require an auth bypass.
        for (name, UefiVar { guid, attr, value }) in non_signature_vars {
            tracing::trace!(%name, "Injecting UEFI var");

            // the value might need to be prepended with an auth header,
            // depending on what auth mode the variable is using.
            let value = {
                let attr = EfiVariableAttributes::from(attr);
                if attr.contains_unsupported_bits() {
                    return Err(NvramSetupError::InjectUefiVar(
                        name,
                        EfiStatus::INVALID_PARAMETER,
                        Some(NvramError::AttributeNonSpec),
                    ));
                }

                if attr.time_based_authenticated_write_access() {
                    let mut new_value = Vec::new();
                    // a dummy header needs to be present, even through no
                    // actual validation will be performed while nvram is still
                    // in SetupMode (i.e: until `pk` is injected).
                    new_value.extend(EFI_VARIABLE_AUTHENTICATION_2::DUMMY.as_bytes());
                    new_value.extend(value);
                    new_value
                } else {
                    value
                }
            };

            self.services
                .set_variable(guid, &name, attr, value)
                .await
                .map_err(|(status, err)| NvramSetupError::InjectUefiVar(name, status, err))?;
        }

        // inject structured signature vars
        if let Some(sigs) = signatures {
            use uefi_specs::linux::nvram::vars as linux_vars;
            use uefi_specs::uefi::nvram::vars as uefi_vars;

            // `dbDefault` is a read-only copy of the initial `db`
            let dbdefault_sig = sigs.db.clone();

            // for each of the signatures, construct the variable payload
            // (in the form of a signature list), and inject it into nvram.
            #[rustfmt::skip]
            let sigs_loop = [
                (uefi_vars::KEK(),        sigs.kek,      EfiVariableAttributes::DEFAULT_ATTRIBUTES_TIME_BASED_AUTH),
                (uefi_vars::DB(),         sigs.db,       EfiVariableAttributes::DEFAULT_ATTRIBUTES_TIME_BASED_AUTH),
                (uefi_vars::DBX(),        sigs.dbx,      EfiVariableAttributes::DEFAULT_ATTRIBUTES_TIME_BASED_AUTH),
                // Two notes:
                //
                // 1. Why the `vec![]`? Well, while there can only ever be a
                //    single PK, it still ends up getting stored in a signature
                //    _list_, so we may as well reuse the existing logic (rather
                //    than having a special cased block just for PK).
                //
                // 2. pk _must_ be injected after kek, db, and dbx, as once pk
                //    is injected, nvram switches out of SetupMode, and requires
                //    non-dummy auth var headers to update those vars.
                (uefi_vars::PK(),         vec![sigs.pk], EfiVariableAttributes::DEFAULT_ATTRIBUTES_TIME_BASED_AUTH),
                (uefi_vars::DBDEFAULT(),  dbdefault_sig, EfiVariableAttributes::DEFAULT_ATTRIBUTES_VOLATILE),
                (linux_vars::MOK_LIST(),  sigs.moklist,  EfiVariableAttributes::DEFAULT_ATTRIBUTES),
                (linux_vars::MOK_LISTX(), sigs.moklistx, EfiVariableAttributes::DEFAULT_ATTRIBUTES),
            ];

            for ((vendor, name), sigs, attr) in sigs_loop {
                tracing::trace!(?name, "Injecting");

                let mut var_data: Vec<u8> = Vec::new();

                for sig in sigs {
                    match sig {
                        Signature::X509(certs) => {
                            // x509 is weird, since every cert in the array
                            // actually ends up as a _separate_ signature list!
                            for X509Cert(data) in certs {
                                let sig_list = SignatureList::X509(SignatureData::new_x509(
                                    MSFT_SECURE_BOOT_PRODUCTION_GUID,
                                    Cow::Owned(data),
                                ));
                                sig_list.extend_as_spec_signature_list(&mut var_data);
                            }
                        }
                        Signature::Sha256(digests) => {
                            let sig_list = SignatureList::Sha256(
                                digests
                                    .into_iter()
                                    .map(|Sha256Digest(data)| {
                                        SignatureData::new_sha256(
                                            MSFT_SECURE_BOOT_PRODUCTION_GUID,
                                            Cow::Owned(data),
                                        )
                                    })
                                    .collect(),
                            );
                            sig_list.extend_as_spec_signature_list(&mut var_data);
                        }
                    }
                }

                if var_data.is_empty() {
                    continue;
                }

                if attr.time_based_authenticated_write_access() {
                    // a dummy header needs to be present, even through no
                    // actual validation will be performed while nvram is still
                    // in SetupMode (i.e: until `pk` is injected).
                    let mut authenticated_var_data =
                        EFI_VARIABLE_AUTHENTICATION_2::DUMMY.as_bytes().to_vec();
                    authenticated_var_data.extend(var_data);
                    var_data = authenticated_var_data;
                }

                self.services
                    .set_variable_ucs2(vendor, name, attr.into(), var_data)
                    .await
                    .map_err(|(status, err)| {
                        NvramSetupError::InjectSigVar(name.into(), status, err)
                    })?;
            }
        }

        Ok(())
    }

    /// Inject secure boot configuration vars.
    async fn setup_secure_boot(&mut self, enabled: bool) -> Result<(), NvramSetupError> {
        tracing::info!(enabled, "configuring secure boot");

        let data = if enabled { [0x01] } else { [0x00] };

        tracing::trace!("Injecting 'SecureBoot'");
        {
            use uefi_specs::uefi::nvram::vars::SECURE_BOOT;

            let (vendor, name) = SECURE_BOOT();

            // Older versions of OpenHCL (and Hyper-V, closed-source HCL, etc. ) may have created
            // a SecureBoot variable with the NV attribute, which doesn't match the UEFI spec.
            // Delete this variable (if it exists).
            let delete_attr = EfiVariableAttributes::new();
            let _ = self
                .services
                .set_variable_ucs2(vendor, name, delete_attr.into(), data.to_vec())
                .await;

            // TODO: For compatibility with older OpenHCL images that cannot handle a volatile
            // variable, we still need to create with NV for now.  Once the above variable
            // deletion code is deployed everywhere, replace with:
            // let attr = EfiVariableAttributes::DEFAULT_ATTRIBUTES_VOLATILE;
            let attr = EfiVariableAttributes::DEFAULT_ATTRIBUTES;
            self.services
                .set_variable_ucs2(vendor, name, attr.into(), data.to_vec())
                .await
                .map_err(|(status, err)| {
                    NvramSetupError::InjectPreBootVar(name.into(), status, err)
                })?;
        }

        tracing::trace!("Injecting 'SecureBootEnabled'");
        {
            use uefi_specs::hyperv::nvram::vars::SECURE_BOOT_ENABLE;

            let (vendor, name) = SECURE_BOOT_ENABLE();
            let attr = EfiVariableAttributes::DEFAULT_ATTRIBUTES;

            self.services
                .set_variable_ucs2(vendor, name, attr.into(), data.to_vec())
                .await
                .map_err(|(status, err)| {
                    NvramSetupError::InjectPreBootVar(name.into(), status, err)
                })?;
        }

        Ok(())
    }
}

/// Convert typed base-template signatures into their injected signature identities.
fn base_signature_set<'a>(signatures: impl IntoIterator<Item = &'a Signature>) -> SignatureSet {
    use uefi_specs::hyperv::nvram::vars::MSFT_SECURE_BOOT_PRODUCTION_GUID;

    let header = EFI_SIGNATURE_DATA {
        signature_owner: MSFT_SECURE_BOOT_PRODUCTION_GUID,
    };
    let mut values = SignatureSet::new();
    for signature in signatures {
        match signature {
            Signature::X509(certs) => values.extend(
                certs
                    .iter()
                    .map(|cert| SignatureValue::X509(header, cert.0.clone())),
            ),
            Signature::Sha256(digests) => values.extend(
                digests
                    .iter()
                    .map(|digest| SignatureValue::Sha256(header, digest.0.to_vec())),
            ),
        }
    }
    values
}

/// Parse a serialized Secure Boot variable into unique signature identities.
fn collect_signature_set(data: &[u8]) -> Result<SignatureSet, SignatureListParseError> {
    let mut signatures = SignatureSet::new();

    for list in ParseSignatureLists::new(signature_list_payload(data)) {
        match list? {
            ParseSignatureList::X509(certs) => {
                for cert in certs {
                    let cert = cert?;
                    signatures.insert(SignatureValue::X509(
                        cert.header,
                        cert.data.0.as_ref().to_vec(),
                    ));
                }
            }
            ParseSignatureList::Sha256(digests) => {
                for digest in digests {
                    let digest = digest?;
                    signatures.insert(SignatureValue::Sha256(
                        digest.header,
                        digest.data.0.as_ref().to_vec(),
                    ));
                }
            }
        }
    }

    Ok(signatures)
}

/// Return the signature-list payload, skipping a valid authentication header.
///
/// Malformed or unrecognized headers are treated as part of the payload so the
/// signature-list parser can report the underlying format error.
fn signature_list_payload(data: &[u8]) -> &[u8] {
    let Ok((auth, _)) = EFI_VARIABLE_AUTHENTICATION_2::read_from_prefix(data) else {
        return data;
    };

    if auth.auth_info.header.revision != WIN_CERT_REVISION_2_0
        || auth.auth_info.header.certificate_type != WIN_CERT_TYPE_EFI_GUID
        || auth.auth_info.cert_type != EFI_CERT_TYPE_PKCS7_GUID
    {
        return data;
    }

    let cert_len = auth.auth_info.header.length as usize;
    if cert_len < size_of::<WIN_CERTIFICATE_UEFI_GUID>() {
        return data;
    }

    let Some(auth_len) = size_of_val(&auth.timestamp).checked_add(cert_len) else {
        return data;
    };
    data.get(auth_len..).unwrap_or(data)
}

#[cfg(test)]
mod tests {
    use super::*;
    use firmware_uefi_custom_vars::ApplyDeltaError;
    use pal_async::async_test;
    use ucs2::Ucs2LeSlice;
    use uefi_nvram_specvars::signature_list::SignatureData;
    use uefi_nvram_specvars::signature_list::SignatureList;
    use uefi_nvram_storage::EFI_TIME;
    use uefi_nvram_storage::NvramStorage;
    use uefi_nvram_storage::in_memory::InMemoryNvram;
    use wchar::wchz;

    const TEST_SIGNATURE_OWNER: Guid = guid::guid!("00000000-0000-0000-0000-000000000001");

    fn x509_variable(certs: &[&'static [u8]]) -> Vec<u8> {
        let mut data = Vec::new();
        for cert in certs {
            SignatureList::X509(SignatureData::new_x509(
                TEST_SIGNATURE_OWNER,
                Cow::Borrowed(*cert),
            ))
            .extend_as_spec_signature_list(&mut data);
        }
        data
    }

    fn append_without_base_json() -> Vec<u8> {
        br#"{
            "type": "Microsoft.Compute/disks",
            "properties": {
                "uefiSettings": {
                    "signatureMode": "Append",
                    "signatures": {}
                }
            }
        }"#
        .to_vec()
    }

    fn invalid_base_template() -> BaseTemplate {
        BaseTemplate {
            json: b"not json".to_vec().into(),
        }
    }

    fn nvram_services(storage: InMemoryNvram) -> NvramServices {
        let storage: Box<dyn VmmNvramStorage> = Box::new(storage);
        NvramServices {
            vsm_config: None,
            services: NvramSpecServices::new(storage),
        }
    }

    #[test]
    fn authenticated_and_raw_signature_payloads_parse_identically() {
        let raw = x509_variable(&[b"cert1"]);
        let mut authenticated = EFI_VARIABLE_AUTHENTICATION_2::DUMMY.as_bytes().to_vec();
        authenticated.extend_from_slice(&raw);

        assert_eq!(
            collect_signature_set(&authenticated).unwrap(),
            collect_signature_set(&raw).unwrap()
        );
    }

    #[test]
    fn signature_set_difference_ignores_extra_loaded_signatures() {
        let base = collect_signature_set(&x509_variable(&[b"cert1", b"cert2"])).unwrap();
        let loaded = collect_signature_set(&x509_variable(&[b"cert1", b"cert3"])).unwrap();

        assert_eq!(base.difference(&loaded).count(), 1);
    }

    #[test]
    fn secure_boot_variable_report_fits_get_trace_limit() {
        const GET_TRACE_MESSAGE_MAX_BYTES: usize = 256;

        let report = SecureBootVariableReport {
            name: vars::KEK().1,
            in_nvram: false,
            has_custom_uefi: false,
            loaded_bytes: usize::MAX,
            loaded_entries: usize::MAX,
            template_entries: usize::MAX,
            missing_entries: usize::MAX,
        };
        let message = format!("{report:?}");

        assert!(
            message.len() <= GET_TRACE_MESSAGE_MAX_BYTES,
            "Secure Boot variable report is {} bytes: {message}",
            message.len()
        );
    }

    #[async_test]
    async fn missing_secure_boot_variable_is_reported_as_not_present() {
        let mut nvram = nvram_services(InMemoryNvram::new());
        let template_signatures = [Signature::X509(vec![firmware_uefi_custom_vars::X509Cert(
            b"cert1".to_vec(),
        )])];

        let report = nvram
            .get_secure_boot_variable_report(vars::DB(), &template_signatures, true)
            .await
            .unwrap();

        assert!(!report.in_nvram);
        assert!(report.has_custom_uefi);
        assert_eq!(report.loaded_bytes, 0);
        assert_eq!(report.loaded_entries, 0);
        assert_eq!(report.template_entries, 1);
        assert_eq!(report.missing_entries, 1);
    }

    #[async_test]
    async fn empty_secure_boot_variable_reports_all_entries_missing() {
        let (vendor, name) = vars::DB();
        let mut storage = InMemoryNvram::new();
        storage
            .set_variable(name, vendor, 0, Vec::new(), EFI_TIME::default())
            .await
            .unwrap();
        let mut nvram = nvram_services(storage);
        let template_signatures = [Signature::X509(vec![
            firmware_uefi_custom_vars::X509Cert(b"cert1".to_vec()),
            firmware_uefi_custom_vars::X509Cert(b"cert2".to_vec()),
        ])];

        let report = nvram
            .get_secure_boot_variable_report((vendor, name), &template_signatures, false)
            .await
            .unwrap();

        assert!(report.in_nvram);
        assert!(!report.has_custom_uefi);
        assert_eq!(report.loaded_bytes, 0);
        assert_eq!(report.loaded_entries, 0);
        assert_eq!(report.template_entries, 2);
        assert_eq!(report.missing_entries, 2);
    }

    #[async_test]
    async fn populated_secure_boot_variable_reports_signature_counts() {
        use uefi_specs::hyperv::nvram::vars::MSFT_SECURE_BOOT_PRODUCTION_GUID;

        let (vendor, name) = vars::DB();
        let mut data = Vec::new();
        for cert in [b"cert1".as_slice(), b"cert3".as_slice()] {
            SignatureList::X509(SignatureData::new_x509(
                MSFT_SECURE_BOOT_PRODUCTION_GUID,
                Cow::Borrowed(cert),
            ))
            .extend_as_spec_signature_list(&mut data);
        }
        let loaded_bytes = data.len();
        let mut storage = InMemoryNvram::new();
        storage
            .set_variable(name, vendor, 0, data, EFI_TIME::default())
            .await
            .unwrap();
        let mut nvram = nvram_services(storage);
        let template_signatures = [Signature::X509(vec![
            firmware_uefi_custom_vars::X509Cert(b"cert1".to_vec()),
            firmware_uefi_custom_vars::X509Cert(b"cert2".to_vec()),
        ])];

        let report = nvram
            .get_secure_boot_variable_report((vendor, name), &template_signatures, false)
            .await
            .unwrap();

        assert_eq!(report.loaded_bytes, loaded_bytes);
        assert_eq!(report.loaded_entries, 2);
        assert_eq!(report.template_entries, 2);
        assert_eq!(report.missing_entries, 1);
    }

    #[async_test]
    async fn invalid_templates_are_ignored_after_first_boot_without_secure_boot() {
        let mut storage = InMemoryNvram::new();
        let name = Ucs2LeSlice::from_slice_with_nul(wchz!(u16, "existing").as_bytes()).unwrap();
        storage
            .set_variable(name, Guid::default(), 0, vec![1], EFI_TIME::default())
            .await
            .unwrap();

        NvramServices::new(
            Box::new(storage),
            Some(invalid_base_template()),
            Some(b"not json".to_vec().into()),
            false,
            None,
            false,
        )
        .await
        .unwrap();
    }

    #[async_test]
    async fn invalid_base_template_is_ignored_after_first_boot_with_secure_boot() {
        let mut storage = InMemoryNvram::new();
        let name = Ucs2LeSlice::from_slice_with_nul(wchz!(u16, "existing").as_bytes()).unwrap();
        storage
            .set_variable(name, Guid::default(), 0, vec![1], EFI_TIME::default())
            .await
            .unwrap();

        NvramServices::new(
            Box::new(storage),
            Some(invalid_base_template()),
            None,
            true,
            None,
            false,
        )
        .await
        .unwrap();
    }

    #[async_test]
    async fn invalid_base_template_fails_on_first_boot() {
        assert!(matches!(
            NvramServices::new(
                Box::new(InMemoryNvram::new()),
                Some(invalid_base_template()),
                None,
                false,
                None,
                false,
            )
            .await,
            Err(NvramSetupError::LoadBaseTemplate(_))
        ));
    }

    #[async_test]
    async fn invalid_delta_fails_on_first_boot() {
        let mut nvram = nvram_services(InMemoryNvram::new());

        assert!(matches!(
            nvram
                .inject_initial_vars(None, Some(append_without_base_json().into()))
                .await,
            Err(NvramSetupError::ApplyCustomTemplate(
                ApplyDeltaError::AppendWithoutBase
            ))
        ));
    }

    #[async_test]
    async fn malformed_json_fails_on_first_boot() {
        let mut nvram = nvram_services(InMemoryNvram::new());

        assert!(matches!(
            nvram
                .inject_initial_vars(None, Some(b"not json".to_vec().into()))
                .await,
            Err(NvramSetupError::LoadCustomUefiJson(_))
        ));
    }

    #[async_test]
    async fn deferred_base_template_parses_on_first_boot() {
        let mut nvram = nvram_services(InMemoryNvram::new());
        let base_template = firmware_uefi_resources::x64_secure_boot_templates::microsoft_windows();
        let base_template_vars =
            hyperv_uefi_custom_vars_json::parse_template_json(base_template.json.as_bytes())
                .unwrap();

        nvram
            .inject_initial_vars(Some(&base_template_vars), None)
            .await
            .unwrap();

        let (vendor, name) = vars::PK();
        assert!(nvram.services.get_variable_ucs2(vendor, name).await.is_ok());
    }

    #[async_test]
    async fn omitted_dbx_is_not_injected_on_first_boot() {
        let custom_vars = br#"{
            "type": "Microsoft.Compute/disks",
            "properties": {
                "uefiSettings": {
                    "signatureMode": "Replace",
                    "signatures": {
                        "PK": { "type": "x509", "value": ["Jw=="] },
                        "KEK": [{ "type": "x509", "value": ["Jw=="] }],
                        "db": [{ "type": "x509", "value": ["Jw=="] }]
                    }
                }
            }
        }"#;
        let mut nvram = nvram_services(InMemoryNvram::new());

        nvram
            .inject_initial_vars(None, Some(custom_vars.to_vec().into()))
            .await
            .unwrap();

        let (pk_vendor, pk_name) = vars::PK();
        assert!(
            nvram
                .services
                .get_variable_ucs2(pk_vendor, pk_name)
                .await
                .is_ok()
        );

        let (dbx_vendor, dbx_name) = vars::DBX();
        assert!(matches!(
            nvram.services.get_variable_ucs2(dbx_vendor, dbx_name).await,
            Err((EfiStatus::NOT_FOUND, _))
        ));
    }
}

impl UefiDevice {
    pub(crate) async fn nvram_handle_command(&mut self, desc_addr: u64) {
        use uefi_specs::hyperv::nvram::NvramCommandDescriptor;

        let mut desc: NvramCommandDescriptor = match self.gm.read_plain(desc_addr) {
            Ok(desc) => desc,
            Err(err) => {
                tracelimit::warn_ratelimited!(
                    error = &err as &dyn std::error::Error,
                    "Could not read NvramCommandDescriptor from guest memory",
                );
                return;
            }
        };

        let status = match self.handle_nvram_command_inner(desc_addr, desc).await {
            Ok(status) => status,
            Err(err) => {
                tracelimit::warn_ratelimited!(
                    error = &err as &dyn std::error::Error,
                    "Guest memory error while handling nvram command"
                );
                EfiStatus::DEVICE_ERROR
            }
        };

        // write back status into guest memory
        desc.status = status.into();

        if let Err(err) = self.gm.write_plain(desc_addr, &desc) {
            tracelimit::warn_ratelimited!(
                error = &err as &dyn std::error::Error,
                "Could not write NvramCommandDescriptor into guest memory",
            );
        }
    }

    async fn handle_nvram_command_inner(
        &mut self,
        desc_addr: u64,
        desc: uefi_specs::hyperv::nvram::NvramCommandDescriptor,
    ) -> Result<EfiStatus, GuestMemoryError> {
        use uefi_specs::hyperv::nvram::NvramCommand;
        use uefi_specs::hyperv::nvram::NvramVariableCommand;

        let command_addr = desc_addr + size_of_val(&desc) as u64;

        let (status, err) = match desc.command {
            NvramCommand::GET_VARIABLE => {
                let mut command: NvramVariableCommand = self.gm.read_plain(command_addr)?;

                let name = if command.name_address.get() != 0 {
                    let mut buf = vec![0; command.name_bytes as usize];
                    self.gm
                        .read_at(command.name_address.into(), buf.as_mut_slice())?;
                    Some(buf)
                } else {
                    None
                };

                let NvramResult(data, status, err) = self
                    .service
                    .nvram
                    .services
                    .uefi_get_variable(
                        name.as_deref(),
                        command.vendor_guid,
                        &mut command.attributes,
                        &mut command.data_bytes,
                        command.data_address.get() == 0,
                    )
                    .await;

                // writeback updated command struct
                self.gm.write_plain(command_addr, &command)?;

                // write any data to provided guest memory location
                // (bounds checking is performed within `nvram.get_variable`)
                if let Some(data) = data {
                    self.gm
                        .write_at(command.data_address.get(), data.as_bytes())?;
                }

                (status, err)
            }
            NvramCommand::SET_VARIABLE => {
                let command: NvramVariableCommand = self.gm.read_plain(command_addr)?;

                let name = if command.name_address.get() != 0 {
                    let mut buf = vec![0; command.name_bytes as usize];
                    self.gm
                        .read_at(command.name_address.into(), buf.as_mut_slice())?;
                    Some(buf)
                } else {
                    None
                };

                let data = if command.data_address.get() != 0 {
                    let mut buf = vec![0; command.data_bytes as usize];
                    self.gm
                        .read_at(command.data_address.into(), buf.as_mut_slice())?;
                    Some(buf)
                } else {
                    None
                };

                let NvramResult((), status, err) = self
                    .service
                    .nvram
                    .services
                    .uefi_set_variable(
                        name.as_deref(),
                        command.vendor_guid,
                        command.attributes,
                        command.data_bytes,
                        data,
                    )
                    .await;

                (status, err)
            }
            NvramCommand::GET_FIRST_VARIABLE_NAME | NvramCommand::GET_NEXT_VARIABLE_NAME => {
                let mut command: NvramVariableCommand = self.gm.read_plain(command_addr)?;

                let name = if desc.command == NvramCommand::GET_NEXT_VARIABLE_NAME {
                    if command.name_address.get() != 0 {
                        let mut buf = vec![0; command.name_bytes as usize];
                        self.gm
                            .read_at(command.name_address.into(), buf.as_mut_slice())?;
                        Some(buf)
                    } else {
                        None
                    }
                } else {
                    // If the command is GET_FIRST_VARIABLE_NAME, then we should
                    // ignore the name provided in the NvramVariableCommand
                    // struct, and just pass along a empty UTF-16 string to
                    // `get_next_variable`, which will fetch the first variable
                    // name (as specified by the official UEFI spec)
                    Some(vec![0, 0])
                };

                let NvramResult(data, status, err) = self
                    .service
                    .nvram
                    .services
                    .uefi_get_next_variable(
                        &mut command.name_bytes,
                        name.as_deref(),
                        command.vendor_guid,
                    )
                    .await;

                // write new name data to provided guest memory location
                // (bounds checking is performed within `nvram.get_next_variable`)
                if let Some((name, vendor)) = data {
                    command.vendor_guid = vendor;

                    self.gm
                        .write_at(command.name_address.get(), name.as_bytes())?;
                }

                // writeback updated command struct
                self.gm.write_at(command_addr, command.as_bytes())?;

                (status, err)
            }
            NvramCommand::QUERY_INFO => (EfiStatus::UNSUPPORTED, None),
            NvramCommand::SIGNAL_RUNTIME => {
                use uefi_specs::hyperv::nvram::NvramSignalRuntimeCommand;
                let command: NvramSignalRuntimeCommand = self.gm.read_plain(command_addr)?;

                if !command.flags.vsm_aware() {
                    if let Some(vsm) = &self.service.nvram.vsm_config {
                        tracelimit::info_ratelimited!("Revoking guest vsm");
                        vsm.revoke_guest_vsm()
                    }
                }
                self.service.nvram.services.exit_boot_services();

                (EfiStatus::SUCCESS, None)
            }
            NvramCommand::DEBUG_STRING => {
                let command: uefi_specs::hyperv::nvram::NvramDebugStringCommand =
                    self.gm.read_plain(command_addr)?;

                let mut data = vec![0u16; command.len as usize / 2];
                self.gm
                    .read_at(command.address.into(), data.as_mut_bytes())?;

                tracing::trace!(
                    target: "uefi-nvram-guest-debug",
                    data = %String::from_utf16_lossy(&data),
                    "nvram guest debug",
                );
                (EfiStatus::SUCCESS, None)
            }
            command => {
                tracelimit::warn_ratelimited!(?command, "unknown nvram command");
                (EfiStatus::UNSUPPORTED, None)
            }
        };

        // log any errors which may have occurred
        if let Some(err) = err {
            let err: &(dyn std::error::Error + 'static) = &err;
            tracelimit::warn_ratelimited!(
                command = ?desc.command,
                ?status,
                error = err,
                "nvram error"
            )
        }

        if status != EfiStatus::SUCCESS {
            tracing::trace!(?status, "nvram status");
        }

        Ok(status)
    }
}

mod save_restore {
    use super::*;
    use vmcore::save_restore::RestoreError;
    use vmcore::save_restore::SaveError;
    use vmcore::save_restore::SaveRestore;

    mod state {
        use crate::service::nvram::NvramSpecServices;
        use mesh::payload::Protobuf;
        use uefi_nvram_storage::VmmNvramStorage;
        use vmcore::save_restore::SaveRestore;

        #[derive(Protobuf)]
        #[mesh(package = "firmware.uefi.nvram")]
        pub struct SavedState {
            #[mesh(1)]
            pub services: <NvramSpecServices<Box<dyn VmmNvramStorage>> as SaveRestore>::SavedState,
        }
    }

    impl SaveRestore for NvramServices {
        type SavedState = state::SavedState;

        fn save(&mut self) -> Result<Self::SavedState, SaveError> {
            let NvramServices {
                vsm_config: _,
                services,
            } = self;

            let saved_state = state::SavedState {
                services: services.save()?,
            };

            Ok(saved_state)
        }

        fn restore(&mut self, state: Self::SavedState) -> Result<(), RestoreError> {
            let state::SavedState { services } = state;

            self.services.restore(services)?;

            Ok(())
        }
    }
}
