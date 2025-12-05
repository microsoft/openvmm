// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! An implementation of UEFI spec 8.2 - Variable Services
//!
//! This implementation is a direct implementation / transcription of the UEFI
//! spec, and does not contain any Hyper-V specific features* (i.e: injecting
//! various nvram vars related to secure boot, boot order, etc...).
//!
//! *that isn't _entirely_ true just yet, as there is one bit of code
//! that enforce read-only access to certain Hyper-V specific vars, but if the
//! need arises, those code paths can be refactored.

pub use nvram_services_ext::NvramServicesExt;

use bitfield_struct::bitfield;
use guid::Guid;
use inspect::Inspect;
use mesh::payload::Protobuf;
use std::borrow::Cow;
use thiserror::Error;
use ucs2::Ucs2LeSlice;
use ucs2::Ucs2ParseError;
use uefi_nvram_specvars::signature_list;
use uefi_nvram_specvars::signature_list::ParseSignatureLists;
use uefi_nvram_storage::NextVariable;
use uefi_nvram_storage::NvramStorageError;
use uefi_nvram_storage::VmmNvramStorage;
use uefi_specs::uefi::common::EfiStatus;
use uefi_specs::uefi::nvram::EfiVariableAttributes;
use uefi_specs::uefi::time::EFI_TIME;
use zerocopy::FromBytes;
use zerocopy::FromZeros;

#[cfg(feature = "fuzzing")]
pub mod auth_var_crypto;
#[cfg(not(feature = "fuzzing"))]
mod auth_var_crypto;
mod nvram_services_ext;

#[derive(Debug, Error)]
pub enum NvramError {
    #[error("storage backend error")]
    NvramStorage(#[source] NvramStorageError),
    #[error("variable name cannot be null/None")]
    NameNull,
    #[error("variable data of non-zero len cannot be null")]
    DataNull,
    #[error("variable name validation failed")]
    NameValidation(#[from] Ucs2ParseError),
    #[error("cannot pass empty string to SetVariable")]
    NameEmpty,
    #[error("attributes include non-spec values")]
    AttributeNonSpec,
    #[error("invalid runtime access")]
    InvalidRuntimeAccess,
    #[error("invalid attr: hardware error records are not supported")]
    UnsupportedHardwareErrorRecord,
    #[error("invalid attr: enhanced authenticated access unsupported")]
    UnsupportedEnhancedAuthAccess,
    #[error("invalid attr: volatile variables unsupported")]
    UnsupportedVolatile,
    #[error("attribute mismatch with existing variable")]
    AttributeMismatch,
    #[error("authenticated variable error")]
    AuthError(#[from] AuthError),
    #[error("updating SetupMode variable")]
    UpdateSetupMode(#[source] NvramStorageError),
    #[error("parsing signature list")]
    SignatureList(#[from] signature_list::ParseError),
}

#[derive(Debug, Error)]
pub enum AuthError {
    #[error("data too short (cannot extract EFI_VARIABLE_AUTHENTICATION_2 header)")]
    NotEnoughHdrData,
    #[error("data too short (cannot extract WIN_CERTIFICATE_UEFI_GUID cert)")]
    NotEnoughCertData,
    #[error("invalid WIN_CERTIFICATE Header")]
    InvalidWinCertHeader,
    #[error("invalid WIN_CERTIFICATE_UEFI_GUID Header")]
    InvalidWinCertUefiGuidHeader,
    #[error("incorrect cert type (must be WIN_CERTIFICATE_UEFI_GUID)")]
    IncorrectCertType,
    #[error("incorrect timestamp values")]
    IncorrectTimestamp,
    #[error("new timestamp is not later than current timestamp")]
    OldTimestamp,

    #[error("current implementation cannot authenticate specified var")]
    UnsupportedAuthVar,

    #[error("could not verify auth var")]
    CryptoError,
    #[cfg(feature = "auth-var-verify-crypto")]
    #[error("error in crypto payload format")]
    CryptoFormat(#[from] auth_var_crypto::FormatError),
}

/// `SetVariable` validation is incredibly tricky, since there are a _lot_ of
/// subtle logic branches that are predicated on the presence (or lack thereof)
/// of various attribute bits.
///
/// In order to make the implementation a bit easier to understand and maintain,
/// we switch from using the full-featured `EfiVariableAttributes` bitflags type
/// to a restricted subset of these flags described by `SupportedAttrs` part-way
/// through SetVariable.
#[bitfield(u32)]
#[derive(PartialEq)]
pub struct SupportedAttrs {
    pub non_volatile: bool,
    pub bootservice_access: bool,
    pub runtime_access: bool,
    pub hardware_error_record: bool,
    _reserved: bool,
    pub time_based_authenticated_write_access: bool,
    #[bits(26)]
    _reserved2: u32,
}

impl SupportedAttrs {
    pub fn contains_unsupported_bits(&self) -> bool {
        u32::from(*self)
            & !u32::from(
                Self::new()
                    .with_non_volatile(true)
                    .with_bootservice_access(true)
                    .with_runtime_access(true)
                    .with_hardware_error_record(true)
                    .with_time_based_authenticated_write_access(true),
            )
            != 0
    }
}

/// Helper struct to collect various properties of a parsed authenticated var
#[allow(dead_code)]
#[derive(Debug, Clone, Copy)]
pub struct ParsedAuthVar<'a> {
    pub name: &'a Ucs2LeSlice,
    pub vendor: Guid,
    pub attr: u32,
    pub timestamp: EFI_TIME,
    pub pkcs7_data: &'a [u8],
    pub var_data: &'a [u8],
}

/// Unlike a typical result type, NvramErrors contain _both_ a payload _and_ an
/// error code. Depending on the error code, an optional `NvramError` might be
/// included as well, which provides more context.
///
/// Notably, **this result types cannot be propagated via the `?` operator!**
#[derive(Debug)]
pub struct NvramResult<T>(pub T, pub EfiStatus, pub Option<NvramError>);

impl<T> NvramResult<T> {
    pub fn is_success(&self) -> bool {
        matches!(self.1, EfiStatus::SUCCESS)
    }
}

impl<T> std::fmt::Display for NvramResult<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match &self.2 {
            Some(_) => write!(f, "{:?} (with error context)", self.1),
            None => write!(f, "{:?}", self.1),
        }
    }
}

impl<T> std::error::Error for NvramResult<T>
where
    T: std::fmt::Debug,
{
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        self.2
            .as_ref()
            .map(|s| s as &(dyn std::error::Error + 'static))
    }
}

#[derive(Clone, Copy, Debug, Protobuf, Inspect)]
enum RuntimeState {
    /// Implementation-specific state, whereby certain read-only and
    /// authenticated variable checks are bypassed.
    ///
    /// Transitions into `Boot` once all pre-boot nvram variables have been
    /// successfully injected.
    PreBoot,
    /// UEFI firmware hasn't called `ExitBootServices`
    Boot,
    /// UEFI firmware has called `ExitBootServices`
    Runtime,
}

impl RuntimeState {
    fn is_pre_boot(&self) -> bool {
        matches!(&self, RuntimeState::PreBoot)
    }

    fn is_boot(&self) -> bool {
        matches!(&self, RuntimeState::Boot)
    }

    fn is_runtime(&self) -> bool {
        matches!(&self, RuntimeState::Runtime)
    }
}

/// An implementation of UEFI spec 8.2 - Variable Services
///
/// This API tries to match the API defined by the UEFI spec 1:1, hence why it
/// doesn't look very "Rust-y".
///
/// If you need to interact with `NvramServices` outside the context of the UEFI
/// device itself, consider importing the [`NvramServicesExt`] trait. This trait
/// provides various helper methods that make it easier to get/set nvram
/// variables, without worrying about the nitty-gritty details of UCS-2 string
/// encoding, pointer sizes/nullness, etc...
///
/// Instead of returning a typical `Result` type, these methods all return a
/// tuple of `(Option<T>, EfiStatus, Option<NvramError>)`, where the `EfiStatus`
/// field should be unconditionally returned to the guest, while the
/// `NvramError` type provides additional context as to what error occurred in
/// OpenVMM (i.e: for logging purposes).
#[derive(Debug, Inspect)]
pub struct NvramSpecServices<S: VmmNvramStorage> {
    storage: S,
    runtime_state: RuntimeState,
}

impl<S: VmmNvramStorage> NvramSpecServices<S> {
    /// Construct a new NvramServices instance from an existing storage backend.
    pub fn new(storage: S) -> NvramSpecServices<S> {
        NvramSpecServices {
            storage,
            runtime_state: RuntimeState::PreBoot,
        }
    }

    /// Check if the nvram store is empty.
    pub async fn is_empty(&mut self) -> Result<bool, NvramStorageError> {
        self.storage.is_empty().await
    }

    /// Update "SetupMode" based on the current value of "PK"
    ///
    /// From UEFI spec section 32.3
    ///
    /// While no Platform Key is enrolled, the SetupMode variable shall be equal
    /// to 1. While SetupMode == 1, the platform firmware shall not require
    /// authentication in order to modify the Platform Key, Key Enrollment Key,
    /// OsRecoveryOrder, OsRecovery####, and image security databases.
    ///
    /// After the Platform Key is enrolled, the SetupMode variable shall be
    /// equal to 0. While SetupMode == 0, the platform firmware shall require
    /// authentication in order to modify the Platform Key, Key Enrollment Key,
    /// OsRecoveryOrder, OsRecovery####, and image security databases.
    pub async fn update_setup_mode(&mut self) -> Result<(), NvramStorageError> {
        use uefi_specs::uefi::nvram::vars::PK;
        use uefi_specs::uefi::nvram::vars::SETUP_MODE;

        let (pk_vendor, pk_name) = PK();
        let (setup_mode_vendor, setup_mode_name) = SETUP_MODE();

        let attr = EfiVariableAttributes::DEFAULT_ATTRIBUTES;
        let timestamp = EFI_TIME::new_zeroed();
        let data = match self.storage.get_variable(pk_name, pk_vendor).await? {
            Some(_) => [0x00],
            None => [0x01],
        };

        self.storage
            .set_variable(
                setup_mode_name,
                setup_mode_vendor,
                attr.into(),
                data.to_vec(),
                timestamp,
            )
            .await?;

        Ok(())
    }

    /// Nvram behavior changes after the guest signals that ExitBootServices has
    /// been called (e.g: hiding variables that are only accessible at
    /// boot-time).
    pub fn exit_boot_services(&mut self) {
        assert!(self.runtime_state.is_boot());
        tracing::trace!("NVRAM has entered runtime mode");
        self.runtime_state = RuntimeState::Runtime;
    }

    /// Called when the VM resets to return to the preboot state.
    pub fn reset(&mut self) {
        self.runtime_state = RuntimeState::PreBoot;
    }

    /// Called after injecting any pre-boot nvram vars, transitioning the nvram
    /// store to start accepting calls from guest UEFI.
    pub fn prepare_for_boot(&mut self) {
        assert!(self.runtime_state.is_pre_boot());
        tracing::trace!("NVRAM has entered boot mode");
        self.runtime_state = RuntimeState::Boot;
    }

    async fn get_setup_mode(&mut self) -> Result<bool, NvramStorageError> {
        use uefi_specs::uefi::nvram::vars::SETUP_MODE;

        let (setup_mode_vendor, setup_mode_name) = SETUP_MODE();
        let in_setup_mode = match self
            .storage
            .get_variable(setup_mode_name, setup_mode_vendor)
            .await?
        {
            None => false,
            Some((_, data, _)) => data.first().map(|b| *b == 0x01).unwrap_or(false),
        };

        Ok(in_setup_mode)
    }

    /// Get a variable identified by `name` + `vendor`, returning the variable's
    /// attributes and data.
    ///
    /// - `in_name`
    ///     - (In) Variable name (a null-terminated UTF-16 string, or `None` if
    ///       the guest passed a `nullptr`)
    /// - `in_vendor`
    ///     - (In) Variable vendor guid
    /// - `out_attr`
    ///     - (Out) Variable's attributes
    ///     - _Note:_ According to the UEFI spec: `attr` will be populated on
    ///       both EFI_SUCCESS _and_ when EFI_BUFFER_TOO_SMALL is returned.
    /// - `in_out_data_size`
    ///     - (In) Size of available data buffer (provided by guest)
    ///     - (Out) Size of data to be written into buffer
    ///     - _Note:_ If `data_is_null` is `true`, and `in_out_data_size` is set
    ///       to `0`, `in_out_data_size` will be updated with the size required
    ///       to store the variable.
    /// - `data_is_null`
    ///     - (In) bool indicating if guest passed `nullptr` as the data addr
    pub async fn uefi_get_variable(
        &mut self,
        name: Option<&[u8]>,
        in_vendor: Guid,
        out_attr: &mut u32,
        in_out_data_size: &mut u32,
        data_is_null: bool,
    ) -> NvramResult<Option<Vec<u8>>> {
        let name = match name {
            Some(name) => {
                Ucs2LeSlice::from_slice_with_nul(name).map_err(NvramError::NameValidation)
            }
            None => Err(NvramError::NameNull),
        };

        let name = match name {
            Ok(name) => name,
            Err(e) => return NvramResult(None, EfiStatus::INVALID_PARAMETER, Some(e)),
        };

        tracing::trace!(
            ?in_vendor,
            ?name,
            in_out_data_size,
            data_is_null,
            "Get NVRAM variable",
        );

        let (attr, data) = match self.get_variable_inner(name, in_vendor).await {
            Ok(Some((attr, data, _))) => (attr, data),
            Ok(None) => return NvramResult(None, EfiStatus::NOT_FOUND, None),
            Err((status, err)) => return NvramResult(None, status, err),
        };

        if self.runtime_state.is_runtime() && !attr.runtime_access() {
            // From UEFI spec section 8.2:
            //
            // If EFI_BOOT_SERVICES.ExitBootServices() has already been
            // executed, data variables without the EFI_VARIABLE_RUNTIME_ACCESS
            // attribute set will not be visible to GetVariable() and will
            // return an EFI_NOT_FOUND error.
            return NvramResult(
                None,
                EfiStatus::NOT_FOUND,
                Some(NvramError::InvalidRuntimeAccess),
            );
        }

        *out_attr = attr.into();
        match (*in_out_data_size, data_is_null) {
            (0, true) => *in_out_data_size = data.len() as u32,
            (_, true) => return NvramResult(None, EfiStatus::INVALID_PARAMETER, None),
            (_, false) => {
                let guest_buf_len = *in_out_data_size as usize;
                *in_out_data_size = data.len() as u32;
                if guest_buf_len < data.len() {
                    return NvramResult(None, EfiStatus::BUFFER_TOO_SMALL, None);
                }
            }
        }

        NvramResult(Some(data), EfiStatus::SUCCESS, None)
    }

    async fn get_variable_inner(
        &mut self,
        name: &Ucs2LeSlice,
        vendor: Guid,
    ) -> Result<Option<(SupportedAttrs, Vec<u8>, EFI_TIME)>, (EfiStatus, Option<NvramError>)> {
        match self.storage.get_variable(name, vendor).await {
            Ok(None) => Ok(None),
            Ok(Some((attr, data, timestamp))) => {
                let attr = SupportedAttrs::from(attr);
                assert!(
                    !attr.contains_unsupported_bits(),
                    "underlying storage should only ever contain valid attributes"
                );

                Ok(Some((attr, data, timestamp)))
            }
            Err(e) => {
                let status = match &e {
                    NvramStorageError::Deserialize => EfiStatus::DEVICE_ERROR,
                    _ => panic!("unexpected NvramStorageError from get_variable"),
                };
                Err((status, Some(NvramError::NvramStorage(e))))
            }
        }
    }

    /// Set a variable identified by `name` + `vendor` with the specified `attr`
    /// and `data`
    ///
    /// - `name`
    ///     - (In) Variable name (a null-terminated UTF-16 string, or `None` if
    ///       the guest passed a `nullptr`)
    ///     - _Note:_ `name` must contain one or more character.
    /// - `in_vendor`
    ///     - (In) Variable vendor guid
    /// - `in_attr`
    ///     - (In) Variable's attributes
    /// - `in_data_size`
    ///     - (In) Length of data to be written
    ///     - If len in `0`, and the EFI_VARIABLE_APPEND_WRITE,
    ///       EFI_VARIABLE_AUTHENTICATED_WRITE_ACCESS,
    ///       EFI_VARIABLE_ENHANCED_AUTHENTICATED_ACCESS, or
    ///       EFI_VARIABLE_TIME_BASED_AUTHENTICATED_WRITE_ACCESS are not set,
    ///       the variable will be deleted.
    /// - `data`
    ///     - (In) Variable data (or `None` if the guest passed a `nullptr`)
    pub async fn uefi_set_variable(
        &mut self,
        name: Option<&[u8]>,
        in_vendor: Guid,
        in_attr: u32,
        in_data_size: u32,
        data: Option<Vec<u8>>,
    ) -> NvramResult<()> {
        let name = match name {
            Some(name) => {
                Ucs2LeSlice::from_slice_with_nul(name).map_err(NvramError::NameValidation)
            }
            None => Err(NvramError::NameNull),
        };

        let name = match name {
            Ok(name) => name,
            Err(e) => return NvramResult((), EfiStatus::INVALID_PARAMETER, Some(e)),
        };

        if name.as_bytes() == [0, 0] {
            return NvramResult(
                (),
                EfiStatus::INVALID_PARAMETER,
                Some(NvramError::NameEmpty),
            );
        }

        tracing::trace!(
            %in_vendor,
            %name,
            in_attr,
            in_data_size,
            data = if data.is_some() { "Some([..])" } else { "None" },
            "Set NVRAM variable",
        );

        // Perform some basic attribute validation
        let attr = {
            // Validate that set bits correspond to valid attribute flags
            let attr = EfiVariableAttributes::from(in_attr);
            if attr.contains_unsupported_bits() {
                return NvramResult(
                    (),
                    EfiStatus::INVALID_PARAMETER,
                    Some(NvramError::AttributeNonSpec),
                );
            }

            // From UEFI spec section 8.2:
            //
            // Runtime access to a data variable implies boot service access.
            // Attributes that have EFI_VARIABLE_RUNTIME_ACCESS set must also
            // have EFI_VARIABLE_BOOTSERVICE_ACCESS set. The caller is
            // responsible for following this rule.
            if attr.runtime_access() && !attr.bootservice_access() {
                return NvramResult((), EfiStatus::INVALID_PARAMETER, None);
            }

            // From UEFI spec section 8.2:
            //
            // If both the EFI_VARIABLE_TIME_BASED_AUTHENTICATED_WRITE_ACCESS
            // and the EFI_VARIABLE_ENHANCED_AUTHENTICATED_ACCESS attribute are
            // set in a SetVariable() call, then the firmware must return
            // EFI_INVALID_PARAMETER.
            if attr.time_based_authenticated_write_access() && attr.enhanced_authenticated_access()
            {
                return NvramResult((), EfiStatus::INVALID_PARAMETER, None);
            }

            attr
        };

        // Report EFI_UNSUPPORTED for any attributes our implementation doesn't
        // support
        {
            if attr.hardware_error_record() {
                return NvramResult(
                    (),
                    EfiStatus::UNSUPPORTED,
                    Some(NvramError::UnsupportedHardwareErrorRecord),
                );
            }

            if attr.enhanced_authenticated_access() {
                return NvramResult(
                    (),
                    EfiStatus::UNSUPPORTED,
                    Some(NvramError::UnsupportedEnhancedAuthAccess),
                );
            }

            // From UEFI spec section 8.2:
            //
            // EFI_VARIABLE_AUTHENTICATED_WRITE_ACCESS is deprecated and should
            // not be used. Platforms should return EFI_UNSUPPORTED if a caller
            // to SetVariable() specifies this attribute.
            if attr.authenticated_write_access() {
                return NvramResult((), EfiStatus::UNSUPPORTED, None);
            }
        }

        // From UEFI spec section 32.3, Figure 32-4
        //
        // There are various nvram variables that determine what part of secure
        // boot flow we are in. These get used later on in validation, but we'll
        // go ahead and fetch them here...
        //
        // TODO: implement logic around AuditMode and DeployedMode
        let in_setup_mode = match self.get_setup_mode().await {
            Ok(val) => val,
            Err(err) => {
                return NvramResult(
                    (),
                    EfiStatus::DEVICE_ERROR,
                    Some(NvramError::NvramStorage(err)),
                );
            }
        };

        // From UEFI spec section 8.2:
        //
        // Once ExitBootServices() is performed, only variables that have
        // EFI_VARIABLE_RUNTIME_ACCESS and EFI_VARIABLE_NON_VOLATILE set can be
        // set with SetVariable(). Variables that have runtime access but that
        // are not nonvolatile are readonly data variables once
        // ExitBootServices() is performed.
        if self.runtime_state.is_runtime() {
            // At first glance, this seems like a pretty straightforward
            // conditional, but unfortunately, we need to consider the
            // interaction with this other clause:
            //
            //   From UEFI spec section 8.2:
            //
            //   If a preexisting variable is rewritten with no access
            //   attributes specified, the variable will be deleted.
            //
            // As such, if neither access attribute is set, we punt this runtime
            // access check to the implementation of the delete operation,
            // whereby it will make sure the variable being deleted has the
            // correct attributes.
            let missing_access_attrs = !(attr.runtime_access() || attr.bootservice_access());

            if !missing_access_attrs {
                if !attr.runtime_access() || !attr.non_volatile() {
                    return NvramResult(
                        (),
                        EfiStatus::INVALID_PARAMETER,
                        Some(NvramError::InvalidRuntimeAccess),
                    );
                }
            }
        }

        // Check if variable being set is read-only from the Guest
        //
        // Note: these checks are bypassed during pre-boot in order to set the
        // vars' initial values.
        if !self.runtime_state.is_pre_boot() {
            use uefi_specs::hyperv::nvram::vars as hyperv_vars;
            use uefi_specs::uefi::nvram::vars as spec_vars;

            // In true UEFI spec fashion, there are always exceptions...
            enum Exception {
                None,
                SetupMode,
                // TODO: add more exception variants as new RO vars are added
            }

            #[rustfmt::skip]
            let read_only_vars = [
                // UEFI Spec - Table 3-1 Global Variables
                //
                // NOTE: Does not implement all of the read-only
                // variables defined by the UEFI spec in section 3.3
                (spec_vars::SECURE_BOOT(), Exception::None),
                (spec_vars::SETUP_MODE(),  Exception::None),
                (spec_vars::KEK(),         Exception::SetupMode),
                (spec_vars::PK(),          Exception::SetupMode),
                (spec_vars::DBDEFAULT(),   Exception::None),
                // Hyper-V also uses some read-only vars that aren't specified
                // in the UEFI spec
                (hyperv_vars::SECURE_BOOT_ENABLE(),              Exception::None),
                (hyperv_vars::CURRENT_POLICY(),                  Exception::None),
                (hyperv_vars::OS_LOADER_INDICATIONS_SUPPORTED(), Exception::None),
            ];

            let is_readonly = read_only_vars.into_iter().any(|(v, exception)| {
                let skip_check = match exception {
                    Exception::None => false,
                    Exception::SetupMode => in_setup_mode,
                };

                // NOTE: The HCL and worker process implementations perform a
                // case-insensitive comparisons here. A better fix would've
                // been to make all comparisons case _sensitive_, rather than
                // introducing bits of case _insensitivity_ around the nvram
                // implementation. Hindsight is 20-20.
                //
                // In OpenVMM, we don't consider nvram variable names as strings
                // with semantic meaning. Instead, they are akin to a
                // bag-of-bytes that _just so happen_ to have a convenient debug
                // representation when printed out at a UCS-2 string.
                //
                // Case-sensitive comparisons has been confirmed correct with
                // the UEFI team, and as such, it may be worthwhile to backport
                // this change into the C++ implementation as well.
                if !skip_check {
                    v == (in_vendor, name)
                } else {
                    false
                }
            });

            if is_readonly {
                return NvramResult((), EfiStatus::WRITE_PROTECTED, None);
            }
        }

        // The behavior of various operations changes depending on whether or
        // not the specified variable already exists, so go ahead and try to
        // fetch it
        let existing_var = match self.get_variable_inner(name, in_vendor).await {
            Ok(v) => v,
            Err((status, err)) => return NvramResult((), status, err),
        };

        let (in_data_size, data, timestamp) = {
            if !attr.time_based_authenticated_write_access() {
                // nothing fancy here, just some regular 'ol data...
                let timestamp = EFI_TIME::new_zeroed();

                (in_data_size, data, timestamp)
            } else {
                // the payload includes an authenticated variable header
                //
                // UEFI spec 8.2.2 - Using the EFI_VARIABLE_AUTHENTICATION_2 descriptor
                use uefi_specs::uefi::nvram::EFI_VARIABLE_AUTHENTICATION_2;
                use uefi_specs::uefi::signing::EFI_CERT_TYPE_PKCS7_GUID;
                use uefi_specs::uefi::signing::WIN_CERT_TYPE_EFI_GUID;
                use uefi_specs::uefi::signing::WIN_CERTIFICATE_UEFI_GUID;

                tracing::trace!(
                    "variable is attempting to use TIME_BASED_AUTHENTICATED_WRITE_ACCESS"
                );

                // data cannot be null
                let data = match data {
                    Some(data) => data,
                    None => {
                        return NvramResult(
                            (),
                            EfiStatus::INVALID_PARAMETER,
                            Some(NvramError::DataNull),
                        );
                    }
                };

                // extract EFI_VARIABLE_AUTHENTICATION_2 header
                // TODO: zerocopy: err (https://github.com/microsoft/openvmm/issues/759)
                let auth_hdr =
                    match EFI_VARIABLE_AUTHENTICATION_2::read_from_prefix(data.as_slice()).ok() {
                        Some((hdr, _)) => hdr,
                        None => {
                            return NvramResult(
                                (),
                                EfiStatus::SECURITY_VIOLATION,
                                Some(NvramError::AuthError(AuthError::NotEnoughHdrData)),
                            );
                        }
                    };
                let timestamp = auth_hdr.timestamp;
                let auth_info = auth_hdr.auth_info;

                // split off the variable-length WIN_CERTIFICATE_UEFI_GUID cert
                // data from the variable length payload
                let (pkcs7_data, var_data) = {
                    let auth_info_offset = size_of_val(&auth_hdr.timestamp);

                    // use the header's length value to extract the
                    // WIN_CERTIFICATE_UEFI_GUID struct + variable length payload
                    if data[auth_info_offset..].len() < (auth_info.header.length as usize) {
                        return NvramResult(
                            (),
                            EfiStatus::SECURITY_VIOLATION,
                            Some(NvramError::AuthError(AuthError::NotEnoughCertData)),
                        );
                    }
                    let (auth_info_hdr_and_cert, var_data) =
                        data[auth_info_offset..].split_at(auth_info.header.length as usize);

                    // ...and then strip off the WIN_CERTIFICATE_UEFI_GUID
                    // struct from the variable length payload
                    let pkcs7_data = match auth_info_hdr_and_cert
                        .get(size_of::<WIN_CERTIFICATE_UEFI_GUID>()..)
                    {
                        Some(data) => data,
                        None => {
                            return NvramResult(
                                (),
                                EfiStatus::SECURITY_VIOLATION,
                                Some(NvramError::AuthError(AuthError::NotEnoughCertData)),
                            );
                        }
                    };

                    (pkcs7_data, var_data)
                };

                // validate WIN_CERTIFICATE header construction
                if auth_info.header.revision != 0x0200 {
                    return NvramResult(
                        (),
                        EfiStatus::SECURITY_VIOLATION,
                        Some(NvramError::AuthError(AuthError::InvalidWinCertHeader)),
                    );
                }

                // validate correct cert type is being used
                if auth_info.header.certificate_type != WIN_CERT_TYPE_EFI_GUID
                    || auth_info.cert_type != EFI_CERT_TYPE_PKCS7_GUID
                {
                    return NvramResult(
                        (),
                        EfiStatus::SECURITY_VIOLATION,
                        Some(NvramError::AuthError(AuthError::IncorrectCertType)),
                    );
                }

                // validate timestamp according to spec
                if timestamp.pad1 != 0
                    || timestamp.nanosecond != 0
                    || timestamp.timezone.0 != 0
                    || u8::from(timestamp.daylight) != 0
                    || timestamp.pad2 != 0
                {
                    return NvramResult(
                        (),
                        EfiStatus::SECURITY_VIOLATION,
                        Some(NvramError::AuthError(AuthError::IncorrectTimestamp)),
                    );
                }

                // if a variable already exists, make sure the timestamp is
                // newer (or in the case of Append, clamp the timestamp to the
                // existing timestamp)
                let orig_timestamp = timestamp; // original value must be used when performing variable auth
                let timestamp = {
                    let mut timestamp = timestamp;
                    if let Some((_, _, existing_timestamp)) = existing_var {
                        let is_newer = (
                            timestamp.year,
                            timestamp.month,
                            timestamp.day,
                            timestamp.hour,
                            timestamp.minute,
                            timestamp.second,
                            timestamp.nanosecond,
                        )
                            .cmp(&(
                                existing_timestamp.year,
                                existing_timestamp.month,
                                existing_timestamp.day,
                                existing_timestamp.hour,
                                existing_timestamp.minute,
                                existing_timestamp.second,
                                existing_timestamp.nanosecond,
                            ))
                            .is_gt();

                        if !is_newer {
                            if !attr.append_write() {
                                return NvramResult(
                                    (),
                                    EfiStatus::SECURITY_VIOLATION,
                                    Some(NvramError::AuthError(AuthError::OldTimestamp)),
                                );
                            } else {
                                timestamp = existing_timestamp
                            }
                        }
                    }
                    timestamp
                };

                // If PK is present, then we need to authenticate the payload with KEK or PK.
                let pk_var = {
                    let (pk_vendor, pk_name) = uefi_specs::uefi::nvram::vars::PK();
                    match self.get_variable_inner(pk_name, pk_vendor).await {
                        Ok(v) => v,
                        Err((status, err)) => return NvramResult((), status, err),
                    }
                };

                // From UEFI spec section 8.2.2:
                //
                // If the variable SetupMode==1, and the variable is a secure
                // boot policy variable, then the firmware implementation shall
                // consider the checks in the following steps 4 and 5 to have
                // passed, and proceed with updating the variable value as
                // outlined below.
                //
                // (our implementation extends this condition to include
                // "is nvram currently in the pre-boot state")
                let bypass_auth = self.runtime_state.is_pre_boot()
                    || (in_setup_mode
                        && uefi_specs::uefi::nvram::is_secure_boot_policy_var(in_vendor, name));

                if pk_var.is_some() && !bypass_auth {
                    tracing::trace!("pk exists, attempting to actually authenticate var...");

                    let parsed_auth_var = ParsedAuthVar {
                        name,
                        vendor: in_vendor,
                        attr: attr.into(),
                        timestamp: orig_timestamp,
                        pkcs7_data,
                        var_data,
                    };

                    // The UEFI spec has several special-cased authenticated vars.
                    // At the moment, our implementation only supports a handful of these cases.
                    enum AuthVarKind {
                        Db,
                        PkKek,
                        Unsupported,
                    }

                    let var_kind = match (in_vendor, name) {
                        v if v == uefi_specs::uefi::nvram::vars::DB() => AuthVarKind::Db,
                        v if v == uefi_specs::uefi::nvram::vars::DBX() => AuthVarKind::Db,
                        v if v == uefi_specs::uefi::nvram::vars::PK() => AuthVarKind::PkKek,
                        v if v == uefi_specs::uefi::nvram::vars::KEK() => AuthVarKind::PkKek,
                        // TODO: add support for:
                        // - dbr, dbt
                        // - OsRecoveryOrder, OsRecovery####
                        // - private auth vars
                        _ => AuthVarKind::Unsupported,
                    };

                    let auth_res = match var_kind {
                        AuthVarKind::Db => {
                            // UEFI Spec - 8.2.2 Using the EFI_VARIABLE_AUTHENTICATION_2 descriptor
                            //
                            // If the variable is the “db”, “dbt”, “dbr”, or “dbx” variable mentioned
                            // in step 3, verify that the signer’s certificate chains to a certificate
                            // in the Key Exchange Key database (or that the signature was made with
                            // the current Platform Key).
                            match self
                                .authenticate_var(
                                    uefi_specs::uefi::nvram::vars::KEK(),
                                    parsed_auth_var,
                                )
                                .await
                            {
                                Ok(res) => Ok(res),
                                // If authentication with KEK fails, then try PK authentication.
                                Err(_) => {
                                    self.authenticate_var(
                                        uefi_specs::uefi::nvram::vars::PK(),
                                        parsed_auth_var,
                                    )
                                    .await
                                }
                            }
                        }
                        AuthVarKind::PkKek => {
                            // UEFI Spec - 8.2.2 Using the EFI_VARIABLE_AUTHENTICATION_2 descriptor
                            //
                            // If the variable is the global PK variable or the global KEK variable,
                            // verify that the signature has been made with the current Platform Key.
                            self.authenticate_var(
                                uefi_specs::uefi::nvram::vars::PK(),
                                parsed_auth_var,
                            )
                            .await
                        }
                        AuthVarKind::Unsupported => {
                            // TODO: the HCL treats this case the same as the `PkKek` case, but that
                            // seems wrong...
                            return NvramResult(
                                (),
                                EfiStatus::SECURITY_VIOLATION,
                                Some(NvramError::AuthError(AuthError::UnsupportedAuthVar)),
                            );
                        }
                    };

                    if let Err((status, err)) = auth_res {
                        return NvramResult((), status, err);
                    }
                }

                // now that everything has been validated, we can strip off the
                // auth header and go on to actually performing the requested
                // operation of the remaining payload.
                let total_auth_hdr_len =
                    size_of_val(&auth_hdr.timestamp) + (auth_info.header.length as usize);

                (
                    in_data_size - total_auth_hdr_len as u32,
                    Some({
                        let mut data = data;
                        data.drain(..total_auth_hdr_len);
                        data
                    }),
                    timestamp,
                )
            }
        };

        // SetVariable is pretty weird, as it overloads a single method to
        // perform a whole bunch of different variable operations, such as
        // removing, updating, appending, and setting variables.
        //
        // Determining which specific operation is being requested requires
        // navigating a hodgepodge of various rules and indicators, such as the
        // length of the data passed in, what attributes are set, etc...
        #[derive(Debug)]
        enum VariableOperation {
            Set,
            Append,
            Delete,
        }

        let op = {
            let is_doing_append = attr.append_write();
            let is_doing_delete = {
                // From UEFI spec section 8.2:
                //
                // If a preexisting variable is rewritten with no access attributes
                // specified, the variable will be deleted.
                let missing_access_attrs = !(attr.runtime_access() || attr.bootservice_access());

                // From UEFI spec section 8.2:
                //
                // Unless the EFI_VARIABLE_APPEND_WRITE,
                // EFI_VARIABLE_TIME_BASED_AUTHENTICATED_WRITE_ACCESS, or
                // EFI_VARIABLE_ENHANCED_AUTHENTICATED_ACCESS attribute is set (see
                // below), using SetVariable() with a DataSize of zero will cause the
                // entire variable to be deleted
                let zero_data_size = in_data_size == 0 && !is_doing_append;

                missing_access_attrs || zero_data_size
            };

            // append takes precedence over delete/set
            if is_doing_append {
                VariableOperation::Append
            } else if is_doing_delete {
                VariableOperation::Delete
            } else {
                VariableOperation::Set
            }
        };

        tracing::trace!(?op, "SetVariable is performing");

        // normalize attr bits (i.e: strip off APPEND_WRITE indicator)
        let attr = attr.with_append_write(false);

        // Drop down to using `SupportedAttrs` instead of
        // `EfiVariableAttributes` to make things easier to follow.
        let attr = SupportedAttrs::from(u32::from(attr));

        let res = match op {
            VariableOperation::Append => {
                // This implementation only supports non-volatile variables.
                // Volatile variables should be handled within UEFI itself.
                if !attr.non_volatile() {
                    return NvramResult(
                        (),
                        EfiStatus::UNSUPPORTED,
                        Some(NvramError::UnsupportedVolatile),
                    );
                }

                // data *might* get modified in the case that it contains an
                // EFI_SIGNATURE_LIST, and duplicates need to get filtered out
                // (hence the use of `mut`)
                let mut data = match (in_data_size, data) {
                    // Appending with zero data will silently do nothing,
                    // regardless if a variable already exists
                    (0, _) => return NvramResult((), EfiStatus::SUCCESS, None),
                    // If data len is non-zero, data cannot be nullptr
                    (_, None) => {
                        return NvramResult((), EfiStatus::SUCCESS, Some(NvramError::DataNull));
                    }
                    (_, Some(data)) => data,
                };

                if let Some((existing_attr, existing_data, _)) = existing_var {
                    // attempting to fetch a boot-time variable at runtime
                    if self.runtime_state.is_runtime() && !existing_attr.runtime_access() {
                        // ...will fail, since the variable "doesn't exist" at runtime
                        return NvramResult(
                            (),
                            EfiStatus::NOT_FOUND,
                            Some(NvramError::InvalidRuntimeAccess),
                        );
                    }

                    // From UEFI spec section 8.2:
                    //
                    // If a preexisting variable is rewritten with different
                    // attributes, SetVariable() shall not modify the variable
                    // and shall return EFI_INVALID_PARAMETER.
                    if attr != existing_attr {
                        return NvramResult(
                            (),
                            EfiStatus::INVALID_PARAMETER,
                            Some(NvramError::AttributeMismatch),
                        );
                    }

                    // From UEFI spec section 8.2:
                    //
                    // For variables with the GUID EFI_IMAGE_SECURITY_DATABASE_GUID
                    // (i.e. where the data buffer is formatted as EFI_SIGNATURE_LIST),
                    // the driver shall not perform an append of EFI_SIGNATURE_DATA
                    // values that are already part of the existing variable value.
                    //
                    // Note: This situation is not considered an error, and shall in itself
                    // not cause a status code other than EFI_SUCCESS to be returned or the
                    // timestamp associated with the variable not to be updated.
                    if attr.time_based_authenticated_write_access() {
                        use signature_list::SignatureDataPayload;

                        let existing_signatures = ParseSignatureLists::new(&existing_data)
                            .collect_signature_set()
                            .expect("existing var must contain valid list of EFI_SIGNATURE_LIST");

                        // NOTE: the Hyper-V implementation filter signature lists in-place. While
                        // that *would* be more efficient, it also makes the code a _lot_ harder to
                        // understand, so in OpenVMM, lets keep things simple and just allocate a new
                        // buffer for the filtered signatures.
                        let filtered_signatures = ParseSignatureLists::new(&data)
                            .collect_signature_lists(|header, sig| {
                                let sig: &[u8] = match sig {
                                    SignatureDataPayload::X509(buf) => buf,
                                    SignatureDataPayload::Sha256(buf) => buf,
                                };

                                !existing_signatures.contains(&(header, Cow::Borrowed(sig)))
                            });

                        // it *is* an error if the provided signature list is malformed
                        let filtered_signatures = match filtered_signatures {
                            Ok(sigs) => sigs,
                            Err(e) => {
                                return NvramResult(
                                    (),
                                    EfiStatus::INVALID_PARAMETER,
                                    Some(NvramError::SignatureList(e)),
                                );
                            }
                        };

                        let mut new_data = Vec::new();
                        for list in filtered_signatures {
                            list.extend_as_spec_signature_list(&mut new_data);
                        }

                        // update data to point at the new signature list we just created
                        data = new_data;
                    }
                }

                // All validation checks have passed, so perform the operation
                match self
                    .storage
                    .append_variable(name, in_vendor, data.clone(), timestamp)
                    .await
                {
                    Ok(true) => NvramResult((), EfiStatus::SUCCESS, None),
                    Ok(false) => NvramResult((), EfiStatus::NOT_FOUND, None),
                    Err(e) => {
                        let status = match &e {
                            NvramStorageError::Commit(_) => EfiStatus::DEVICE_ERROR,
                            NvramStorageError::OutOfSpace => EfiStatus::OUT_OF_RESOURCES,
                            NvramStorageError::VariableNameTooLong => EfiStatus::INVALID_PARAMETER,
                            NvramStorageError::VariableDataTooLong => EfiStatus::INVALID_PARAMETER,
                            _ => {
                                panic!("unexpected NvramStorageError from append_variable")
                            }
                        };

                        NvramResult((), status, Some(NvramError::NvramStorage(e)))
                    }
                }
            }
            VariableOperation::Delete => {
                if let Some((existing_attr, _, _)) = existing_var {
                    // attempting to delete an existing boot-time variable at runtime
                    if self.runtime_state.is_runtime() && !existing_attr.runtime_access() {
                        // ...will fail, since the variable "doesn't exist" at runtime
                        return NvramResult(
                            (),
                            EfiStatus::NOT_FOUND,
                            Some(NvramError::InvalidRuntimeAccess),
                        );
                    }
                }

                // All validation checks have passed, so perform the operation
                match self.storage.remove_variable(name, in_vendor).await {
                    Ok(true) => NvramResult((), EfiStatus::SUCCESS, None),
                    Ok(false) => NvramResult((), EfiStatus::NOT_FOUND, None),
                    Err(e) => {
                        let status = match &e {
                            NvramStorageError::Commit(_) => EfiStatus::DEVICE_ERROR,
                            NvramStorageError::OutOfSpace => EfiStatus::OUT_OF_RESOURCES,
                            NvramStorageError::VariableNameTooLong => EfiStatus::INVALID_PARAMETER,
                            NvramStorageError::VariableDataTooLong => EfiStatus::INVALID_PARAMETER,
                            _ => {
                                panic!("unexpected NvramStorageError from remove_variable")
                            }
                        };

                        NvramResult((), status, Some(NvramError::NvramStorage(e)))
                    }
                }
            }
            VariableOperation::Set => {
                // This implementation only supports non-volatile variables.
                // Volatile variables should be handled within UEFI itself.
                //
                // The exceptions are variables that are controlled/injected by the loader.
                // This includes secure boot enablement (volatile by specification),
                // as well as the private Hyper-V OsLoaderIndications and
                // OsLoaderIndicationsSupported variables, which are volatile variables
                // that are injected via the non-volatile store. The dbDefault variable
                // is also an exception.
                if !attr.non_volatile() {
                    use uefi_specs::hyperv::nvram::vars as hyperv_vars;
                    use uefi_specs::uefi::nvram::vars::DBDEFAULT;
                    use uefi_specs::uefi::nvram::vars::SECURE_BOOT;
                    let allowed_volatile = [
                        hyperv_vars::OS_LOADER_INDICATIONS(),
                        hyperv_vars::OS_LOADER_INDICATIONS_SUPPORTED(),
                        DBDEFAULT(),
                        SECURE_BOOT(),
                    ];

                    let is_allowed = allowed_volatile.into_iter().any(|v| v == (in_vendor, name));

                    if !is_allowed {
                        return NvramResult(
                            (),
                            EfiStatus::UNSUPPORTED,
                            Some(NvramError::UnsupportedVolatile),
                        );
                    }
                }

                // if we are doing a variable set, then data cannot be a nullptr
                let data = match data {
                    Some(data) => data,
                    None => {
                        return NvramResult(
                            (),
                            EfiStatus::INVALID_PARAMETER,
                            Some(NvramError::DataNull),
                        );
                    }
                };

                if let Some((existing_attr, _, _)) = existing_var {
                    // attempting to overwrite an existing boot-time variable
                    if self.runtime_state.is_runtime() && !existing_attr.runtime_access() {
                        // This is a weird case, since calling GetVariable would
                        // actually return `EFI_NOT_FOUND` (as the variable is
                        // "hidden" at runtime), implying that it should be
                        // _fine_ to set the variable.
                        //
                        // It seems that unless we have some kind of "runtime
                        // shadow variable" support, it's possible to use
                        // `SetVariable` as a way to check if boot-time
                        // variables _actually_ exist...
                        //
                        // The UEFI folks seem to think this gap is _fine_, as
                        // it doesn't give access to protected data - just the
                        // fact that that the boot time var exists.
                        //
                        // So... while this isn't a _great_ solution, it matches
                        // all existing implementations (both within and outside
                        // Hyper-V)
                        return NvramResult(
                            (),
                            EfiStatus::WRITE_PROTECTED,
                            Some(NvramError::InvalidRuntimeAccess),
                        );
                    }

                    // From UEFI spec section 8.2:
                    //
                    // If a preexisting variable is rewritten with different
                    // attributes, SetVariable() shall not modify the
                    // variable and shall return EFI_INVALID_PARAMETER.
                    if attr != existing_attr {
                        return NvramResult(
                            (),
                            EfiStatus::INVALID_PARAMETER,
                            Some(NvramError::AttributeMismatch),
                        );
                    }
                }

                // All validation checks have passed, so perform the operation
                match self
                    .storage
                    .set_variable(name, in_vendor, attr.into(), data.clone(), timestamp)
                    .await
                {
                    Ok(_) => NvramResult((), EfiStatus::SUCCESS, None),
                    Err(e) => {
                        let status = match &e {
                            NvramStorageError::Commit(_) => EfiStatus::DEVICE_ERROR,
                            NvramStorageError::OutOfSpace => EfiStatus::OUT_OF_RESOURCES,
                            NvramStorageError::VariableNameTooLong => EfiStatus::INVALID_PARAMETER,
                            NvramStorageError::VariableDataTooLong => EfiStatus::INVALID_PARAMETER,
                            _ => panic!("unexpected NvramStorageError from set_variable"),
                        };

                        NvramResult((), status, Some(NvramError::NvramStorage(e)))
                    }
                }
            }
        };

        // If we modified the PK variable, we need to update the SetupMode
        // variable accordingly.
        if res.is_success() && (in_vendor, name) == uefi_specs::uefi::nvram::vars::PK() {
            if let Err(e) = self.update_setup_mode().await {
                return NvramResult(
                    (),
                    EfiStatus::DEVICE_ERROR,
                    Some(NvramError::UpdateSetupMode(e)),
                );
            }
        }

        res
    }

    #[cfg(not(feature = "auth-var-verify-crypto"))]
    async fn authenticate_var(
        &mut self,
        // NOTE: Due to a compiler limitation with async fn, 'static bound was removed here
        // https://github.com/rust-lang/rust/issues/63033#issuecomment-521234696
        _: (Guid, &Ucs2LeSlice),
        _: ParsedAuthVar<'_>,
    ) -> Result<(), (EfiStatus, Option<NvramError>)> {
        tracing::warn!(
            "compiled without 'auth-var-verify-crypto' - unconditionally failing auth var validation!"
        );
        Err((EfiStatus::SECURITY_VIOLATION, None))
    }

    /// Authenticate the given variable against the signatures stored in the
    /// specified EFI_SIGNATURE_LIST
    #[cfg(feature = "auth-var-verify-crypto")]
    async fn authenticate_var(
        &mut self,
        // NOTE: Due to a compiler limitation with async fn, 'static bound was removed here
        // https://github.com/rust-lang/rust/issues/63033#issuecomment-521234696
        (key_var_name, key_var_vendor): (Guid, &Ucs2LeSlice),
        auth_var: ParsedAuthVar<'_>,
    ) -> Result<(), (EfiStatus, Option<NvramError>)> {
        let signature_lists = match self
            .get_variable_inner(key_var_vendor, key_var_name)
            .await?
        {
            Some((_, data, _)) => data,
            None => return Err((EfiStatus::SECURITY_VIOLATION, None)),
        };

        // the nitty-gritty of how authentication works is best left to a separate module...
        match auth_var_crypto::authenticate_variable(&signature_lists, auth_var) {
            Ok(true) => Ok(()),
            Ok(false) => Err((
                EfiStatus::SECURITY_VIOLATION,
                Some(NvramError::AuthError(AuthError::CryptoError)),
            )),
            Err(e) if e.key_var_error() => {
                panic!("existing signature list must contain valid data: {}", e)
            }
            // all other errors are due to malformed auth_var data
            Err(e) => Err((
                EfiStatus::SECURITY_VIOLATION,
                Some(NvramError::AuthError(AuthError::CryptoFormat(e))),
            )),
        }
    }

    /// Return the variable immediately following the variable identified by
    /// `name` + `vendor` `key`.
    ///
    /// If `name` is an empty string, the first variable is returned.
    ///
    /// - `name`
    ///     - (In) Variable name (a null-terminated UTF-16 string, or `None` if
    ///       the guest passed a `nullptr`)
    /// - `in_out_name_size`
    ///     - (In) Length of the provided `name`
    ///     - (Out) Length of the next variable name
    ///     - _Note:_ If there is insufficient space in the name buffer to store
    ///       the next variable, `in_out_name_size` will be updated with the
    ///       size required to store the variable.
    /// - `vendor`
    ///     - (In) Variable vendor guid
    pub async fn uefi_get_next_variable(
        &mut self,
        in_out_name_size: &mut u32,
        name: Option<&[u8]>,
        vendor: Guid,
    ) -> NvramResult<Option<(Vec<u8>, Guid)>> {
        let name = match name {
            Some(name) => {
                Ucs2LeSlice::from_slice_with_nul(name).map_err(NvramError::NameValidation)
            }
            None => Err(NvramError::NameNull),
        };

        let name = match name {
            Ok(name) => name,
            Err(e) => return NvramResult(None, EfiStatus::INVALID_PARAMETER, Some(e)),
        };

        tracing::trace!(?vendor, ?name, in_out_name_size, "Next NVRAM variable",);

        // As per UEFI spec: if an empty null-terminated string is passed to
        // GetNextVariable, the first variable should be returned
        let mut res = if name.as_bytes() == [0, 0] {
            self.storage.next_variable(None).await
        } else {
            self.storage.next_variable(Some((name, vendor))).await
        };

        loop {
            match res {
                Ok(NextVariable::EndOfList) => {
                    return NvramResult(None, EfiStatus::NOT_FOUND, None);
                }
                Ok(NextVariable::InvalidKey) => {
                    return NvramResult(None, EfiStatus::INVALID_PARAMETER, None);
                }
                Ok(NextVariable::Exists { name, vendor, attr }) => {
                    let attr = EfiVariableAttributes::from(attr);
                    assert!(
                        !attr.contains_unsupported_bits(),
                        "underlying storage should only ever contain valid attributes"
                    );

                    // From UEFI spec section 8.2:
                    //
                    // Once EFI_BOOT_SERVICES.ExitBootServices() is performed,
                    // variables that are only visible during boot services will
                    // no longer be returned.
                    //
                    // i.e: continue iterating
                    if self.runtime_state.is_runtime() && !attr.runtime_access() {
                        res = self
                            .storage
                            .next_variable(Some((name.as_ref(), vendor)))
                            .await;
                        continue;
                    }

                    let guest_buf_len = *in_out_name_size as usize;
                    *in_out_name_size = name.as_bytes().len() as u32;
                    if guest_buf_len < name.as_bytes().len() {
                        return NvramResult(None, EfiStatus::BUFFER_TOO_SMALL, None);
                    }

                    return NvramResult(
                        Some((name.into_inner(), vendor)),
                        EfiStatus::SUCCESS,
                        None,
                    );
                }
                Err(e) => {
                    let status = match &e {
                        NvramStorageError::Deserialize => EfiStatus::DEVICE_ERROR,
                        _ => panic!("unexpected NvramStorageError from next_variable"),
                    };

                    return NvramResult(None, status, Some(NvramError::NvramStorage(e)));
                }
            }
        }
    }
}

mod save_restore {
    use super::*;
    use vmcore::save_restore::RestoreError;
    use vmcore::save_restore::SaveError;
    use vmcore::save_restore::SaveRestore;

    mod state {
        use mesh::payload::Protobuf;
        use uefi_nvram_storage::in_memory::InMemoryNvram;
        use vmcore::save_restore::SaveRestore;

        #[derive(Protobuf)]
        #[mesh(package = "firmware.uefi.nvram.spec")]
        pub enum SavedRuntimeState {
            #[mesh(1)]
            PreBoot,
            #[mesh(2)]
            Boot,
            #[mesh(3)]
            Runtime,
        }

        #[derive(Protobuf)]
        #[mesh(package = "firmware.uefi.nvram.spec")]
        pub struct SavedState {
            #[mesh(1)]
            pub runtime_state: SavedRuntimeState,
            #[mesh(2)]
            pub storage: <InMemoryNvram as SaveRestore>::SavedState,
        }
    }

    impl<S: VmmNvramStorage> SaveRestore for NvramSpecServices<S> {
        type SavedState = state::SavedState;

        fn save(&mut self) -> Result<Self::SavedState, SaveError> {
            Ok(state::SavedState {
                runtime_state: match self.runtime_state {
                    RuntimeState::PreBoot => state::SavedRuntimeState::PreBoot,
                    RuntimeState::Boot => state::SavedRuntimeState::Boot,
                    RuntimeState::Runtime => state::SavedRuntimeState::Runtime,
                },
                storage: self.storage.save()?,
            })
        }

        fn restore(&mut self, state: Self::SavedState) -> Result<(), RestoreError> {
            let state::SavedState {
                runtime_state,
                storage,
            } = state;

            self.runtime_state = match runtime_state {
                state::SavedRuntimeState::PreBoot => RuntimeState::PreBoot,
                state::SavedRuntimeState::Boot => RuntimeState::Boot,
                state::SavedRuntimeState::Runtime => RuntimeState::Runtime,
            };
            self.storage.restore(storage)?;

            Ok(())
        }
    }
}

#[cfg(test)]
mod test {
    use super::*;
    use uefi_nvram_storage::in_memory::InMemoryNvram;
    // TODO: wchz returns UTF-16 strings, _not_ UCS-2 strings. This works fine
    // when using english variable names, but things will _not_ work as expected
    // if one tries to use any particularly "exotic" chars (that cannot be
    // represented in UCS-2).
    use pal_async::async_test;
    use wchar::wchz;

    use zerocopy::IntoBytes;

    /// Extension trait around `NvramServices` that makes it easier to use the
    /// API outside the context of the UEFI device
    #[async_trait::async_trait]
    trait NvramServicesTestExt {
        async fn set_test_var(&mut self, name: &[u8], attr: u32, data: &[u8]) -> NvramResult<()>;
        async fn get_test_var(&mut self, name: &[u8]) -> NvramResult<(u32, Option<Vec<u8>>)>;
        async fn get_next_test_var(
            &mut self,
            name: Option<Vec<u8>>,
        ) -> NvramResult<Option<Vec<u8>>>;
    }

    #[async_trait::async_trait]
    impl<S: VmmNvramStorage> NvramServicesTestExt for NvramSpecServices<S> {
        async fn set_test_var(&mut self, name: &[u8], attr: u32, data: &[u8]) -> NvramResult<()> {
            let vendor = Guid::default();

            self.uefi_set_variable(
                Some(name),
                vendor,
                attr,
                data.len() as u32,
                Some(data.to_vec()),
            )
            .await
        }

        async fn get_test_var(&mut self, name: &[u8]) -> NvramResult<(u32, Option<Vec<u8>>)> {
            let vendor = Guid::default();

            let mut attr = 0;
            let NvramResult(data, status, err) = self
                .uefi_get_variable(Some(name), vendor, &mut attr, &mut 256, false)
                .await;

            NvramResult((attr, data), status, err)
        }

        async fn get_next_test_var(
            &mut self,
            name: Option<Vec<u8>>,
        ) -> NvramResult<Option<Vec<u8>>> {
            let vendor = Guid::default();

            let NvramResult(name_guid, status, err) = self
                .uefi_get_next_variable(&mut 256, name.as_deref(), vendor)
                .await;

            NvramResult(name_guid.map(|(n, _)| n.clone()), status, err)
        }
    }

    trait NvramRetTestExt<T> {
        fn unwrap_efi_success(self) -> T;
    }

    impl<T> NvramRetTestExt<T> for NvramResult<T> {
        #[track_caller]
        fn unwrap_efi_success(self) -> T {
            let NvramResult(val, status, err) = self;
            if let Some(err) = err {
                panic!("{}", err)
            }
            assert_eq!(status, EfiStatus::SUCCESS);
            val
        }
    }

    #[async_test]
    async fn runtime_vars() {
        let nvram_storage = InMemoryNvram::new();
        let mut nvram = NvramSpecServices::new(nvram_storage);

        nvram.prepare_for_boot();

        let name1 = wchz!(u16, "var1").as_bytes();
        let name2 = wchz!(u16, "var2").as_bytes();
        let name3 = wchz!(u16, "var3").as_bytes();
        let name4 = wchz!(u16, "var4").as_bytes();

        let dummy_data = b"dummy data".to_vec();

        let runtime_attr = (EfiVariableAttributes::DEFAULT_ATTRIBUTES).into();
        let no_runtime_attr = EfiVariableAttributes::DEFAULT_ATTRIBUTES
            .with_runtime_access(false)
            .into();

        // set some vars
        nvram
            .set_test_var(name1, runtime_attr, &dummy_data)
            .await
            .unwrap_efi_success();
        nvram
            .set_test_var(name2, no_runtime_attr, &dummy_data)
            .await
            .unwrap_efi_success();
        nvram
            .set_test_var(name3, runtime_attr, &dummy_data)
            .await
            .unwrap_efi_success();
        nvram
            .set_test_var(name4, no_runtime_attr, &dummy_data)
            .await
            .unwrap_efi_success();

        // ensure they can all be accessed in pre-runtime environment

        // access them individually
        let (attr, data) = nvram.get_test_var(name1).await.unwrap_efi_success();
        assert_eq!(attr, runtime_attr);
        assert_eq!(data, Some(dummy_data.clone()));

        let (attr, data) = nvram.get_test_var(name2).await.unwrap_efi_success();
        assert_eq!(attr, no_runtime_attr);
        assert_eq!(data, Some(dummy_data.clone()));

        let (attr, data) = nvram.get_test_var(name3).await.unwrap_efi_success();
        assert_eq!(attr, runtime_attr);
        assert_eq!(data, Some(dummy_data.clone()));

        let (attr, data) = nvram.get_test_var(name4).await.unwrap_efi_success();
        assert_eq!(attr, no_runtime_attr);
        assert_eq!(data, Some(dummy_data.clone()));

        // access them sequentially
        let mut name = Some(wchz!(u16, "").as_bytes().into());
        name = nvram.get_next_test_var(name).await.unwrap_efi_success();
        assert_eq!(name, Some(name1.into()));

        name = nvram.get_next_test_var(name).await.unwrap_efi_success();
        assert_eq!(name, Some(name2.into()));

        name = nvram.get_next_test_var(name).await.unwrap_efi_success();
        assert_eq!(name, Some(name3.into()));

        name = nvram.get_next_test_var(name).await.unwrap_efi_success();
        assert_eq!(name, Some(name4.into()));

        let NvramResult(name, status, err) = nvram.get_next_test_var(name).await;
        assert!(name.is_none());
        assert_eq!(status, EfiStatus::NOT_FOUND);
        assert!(err.is_none());

        // ensure vars are hidden once runtime toggle is set
        nvram.exit_boot_services();

        // try to set non-runtime access var
        let NvramResult(_, status, err) = nvram
            .set_test_var(
                wchz!(u16, "non-volatile").as_bytes(),
                no_runtime_attr,
                &dummy_data,
            )
            .await;
        assert_eq!(status, EfiStatus::INVALID_PARAMETER);
        assert!(matches!(err, Some(NvramError::InvalidRuntimeAccess)));

        // access them individually
        let (attr, data) = nvram.get_test_var(name1).await.unwrap_efi_success();
        assert_eq!(attr, runtime_attr);
        assert_eq!(data, Some(dummy_data.clone()));

        let NvramResult((attr, data), status, err) = nvram.get_test_var(name2).await;
        assert_eq!(attr, 0);
        assert_eq!(data, None);
        assert_eq!(status, EfiStatus::NOT_FOUND);
        assert!(matches!(err, Some(NvramError::InvalidRuntimeAccess)));

        let (attr, data) = nvram.get_test_var(name3).await.unwrap_efi_success();
        assert_eq!(attr, runtime_attr);
        assert_eq!(data, Some(dummy_data));

        let NvramResult((attr, data), status, err) = nvram.get_test_var(name4).await;
        assert_eq!(attr, 0);
        assert_eq!(data, None);
        assert_eq!(status, EfiStatus::NOT_FOUND);
        assert!(matches!(err, Some(NvramError::InvalidRuntimeAccess)));

        // access them sequentially
        let mut name = Some(wchz!(u16, "").as_bytes().into());
        name = nvram.get_next_test_var(name).await.unwrap_efi_success();
        assert_eq!(name, Some(name1.into()));

        // DON'T read name2

        name = nvram.get_next_test_var(name).await.unwrap_efi_success();
        assert_eq!(name, Some(name3.into()));

        // DON'T read name4

        let NvramResult(name, status, err) = nvram.get_next_test_var(name).await;
        assert!(name.is_none());
        assert_eq!(status, EfiStatus::NOT_FOUND);
        assert!(err.is_none());
    }
}
