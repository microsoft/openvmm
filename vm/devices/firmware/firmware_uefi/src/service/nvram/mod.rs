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
use crate::platform::nvram::VsmConfig;
use firmware_uefi_custom_vars::CustomVars;
use guestmem::GuestMemoryError;
use inspect::Inspect;
use std::borrow::Cow;
use std::fmt::Debug;
use thiserror::Error;
use uefi_nvram_storage::VmmNvramStorage;
use uefi_specs::uefi::common::EfiStatus;
use uefi_specs::uefi::nvram::EfiVariableAttributes;
use zerocopy::IntoBytes;

#[cfg(feature = "fuzzing")]
pub mod spec_services;
#[cfg(not(feature = "fuzzing"))]
mod spec_services;

#[derive(Debug, Error)]
pub enum NvramSetupError {
    #[error("could not query backing nvram storage")]
    BadNvramStorage(#[source] crate::platform::nvram::NvramStorageError),
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
    #[error("could not inject custom var '{0}': {1:?}")]
    InjectCustomVar(String, EfiStatus, #[source] Option<NvramError>),
    #[error("custom variable name is not valid UCS-2")]
    CustomVarNotUcs2,
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
        custom_vars: CustomVars,
        secure_boot_enabled: bool,
        vsm_config: Option<Box<dyn VsmConfig>>,
        is_restoring: bool,
    ) -> Result<NvramServices, NvramSetupError> {
        let mut nvram = NvramServices {
            services: NvramSpecServices::new(nvram_storage),
            vsm_config,
        };

        if !is_restoring {
            nvram.inject_vars_on_first_boot(custom_vars).await?;
            nvram.inject_hyperv_vars().await?;
            nvram.setup_secure_boot(secure_boot_enabled).await?;
        }

        nvram.services.prepare_for_boot();

        Ok(nvram)
    }

    pub fn reset(&mut self) {
        self.services.reset();
        self.services.prepare_for_boot();
    }

    /// Check if this is the VM's first boot, and if so, inject various
    /// hard-coded and custom UEFI vars.
    async fn inject_vars_on_first_boot(
        &mut self,
        custom_vars: CustomVars,
    ) -> Result<(), NvramSetupError> {
        // "First boot" is marked by having no variables in nvram storage
        if !self
            .services
            .is_empty()
            .await
            .map_err(NvramSetupError::BadNvramStorage)?
        {
            return Ok(());
        }

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
                v.extend(uefi_specs::uefi::nvram::EFI_VARIABLE_AUTHENTICATION_2::DUMMY.as_bytes());
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

        self.inject_custom_vars(custom_vars).await?;

        Ok(())
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

    async fn inject_custom_vars(&mut self, custom_vars: CustomVars) -> Result<(), NvramSetupError> {
        use firmware_uefi_custom_vars::CustomVar;
        use firmware_uefi_custom_vars::Sha256Digest;
        use firmware_uefi_custom_vars::Signature;
        use firmware_uefi_custom_vars::X509Cert;
        use uefi_nvram_specvars::signature_list::SignatureData;
        use uefi_nvram_specvars::signature_list::SignatureList;
        use uefi_specs::hyperv::nvram::vars::MSFT_SECURE_BOOT_PRODUCTION_GUID;
        use uefi_specs::uefi::nvram::EFI_VARIABLE_AUTHENTICATION_2;

        tracing::trace!(custom_vars = ?custom_vars.custom_vars, "custom uefi vars");

        // inject freeform custom vars first, as some may require an auth bypass
        for (name, CustomVar { guid, attr, value }) in custom_vars.custom_vars {
            tracing::trace!(%name, "Injecting custom var");

            // the value might need to be prepended with an auth header,
            // depending on what auth mode the variable is using.
            let value = {
                let attr = EfiVariableAttributes::from(attr);
                if attr.contains_unsupported_bits() {
                    return Err(NvramSetupError::InjectCustomVar(
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
                .map_err(|(status, err)| NvramSetupError::InjectCustomVar(name, status, err))?;
        }

        // inject structured signature vars
        if let Some(sigs) = custom_vars.signatures {
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

                if attr.time_based_authenticated_write_access() {
                    // a dummy header needs to be present, even through no
                    // actual validation will be performed while nvram is still
                    // in SetupMode (i.e: until `pk` is injected).
                    var_data.extend(EFI_VARIABLE_AUTHENTICATION_2::DUMMY.as_bytes());
                }

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
