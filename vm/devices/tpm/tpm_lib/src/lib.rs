// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! The module includes the helper functions for sending TPM commands.

#![forbid(unsafe_code)]

use cvm_tracing::CVM_ALLOWED;
use inspect::Inspect;
use inspect::InspectMut;
use std::error::Error as StdError;
use std::fmt;
use thiserror::Error;

use tpm_protocol::TPM_AZURE_AIK_HANDLE;
use tpm_protocol::TPM_DEFAULT_AKCERT_SIZE;
use tpm_protocol::TPM_GUEST_SECRET_HANDLE;
use tpm_protocol::TPM_NV_INDEX_AIK_CERT;
use tpm_protocol::TPM_NV_INDEX_ATTESTATION_REPORT;
use tpm_protocol::TPM_NV_INDEX_MITIGATED;
use tpm_protocol::TPM_RSA_SRK_HANDLE;
use tpm_protocol::expected_ak_attributes;
use tpm_protocol::platform_akcert_attributes;
use tpm_protocol::tpm20proto;
use tpm_protocol::tpm20proto::AlgIdEnum;
use tpm_protocol::tpm20proto::CommandCodeEnum;
use tpm_protocol::tpm20proto::MAX_DIGEST_BUFFER_SIZE;
use tpm_protocol::tpm20proto::ReservedHandle;
use tpm_protocol::tpm20proto::ResponseCode;
use tpm_protocol::tpm20proto::ResponseValidationError;
use tpm_protocol::tpm20proto::SessionTagEnum;
use tpm_protocol::tpm20proto::TPM20_RH_ENDORSEMENT;
use tpm_protocol::tpm20proto::TPM20_RH_OWNER;
use tpm_protocol::tpm20proto::TPM20_RH_PLATFORM;
use tpm_protocol::tpm20proto::TPM20_RS_PW;
use tpm_protocol::tpm20proto::TpmProtoError;
use tpm_protocol::tpm20proto::TpmaNvBits;
use tpm_protocol::tpm20proto::TpmaObjectBits;
use tpm_protocol::tpm20proto::protocol::CreatePrimaryReply;
use tpm_protocol::tpm20proto::protocol::ImportReply;
use tpm_protocol::tpm20proto::protocol::LoadReply;
use tpm_protocol::tpm20proto::protocol::NvReadPublicReply;
use tpm_protocol::tpm20proto::protocol::PcrSelection;
use tpm_protocol::tpm20proto::protocol::ReadPublicReply;
use tpm_protocol::tpm20proto::protocol::StartupType;
use tpm_protocol::tpm20proto::protocol::Tpm2bBuffer;
use tpm_protocol::tpm20proto::protocol::Tpm2bPublic;
use tpm_protocol::tpm20proto::protocol::TpmCommand;
use tpm_protocol::tpm20proto::protocol::TpmsNvPublic;
use tpm_protocol::tpm20proto::protocol::TpmsRsaParams;
use tpm_protocol::tpm20proto::protocol::TpmtPublic;
use tpm_protocol::tpm20proto::protocol::TpmtRsaScheme;
use tpm_protocol::tpm20proto::protocol::TpmtSymDefObject;
use tpm_protocol::tpm20proto::protocol::common::CmdAuth;
use zerocopy::FromZeros;
use zerocopy::IntoBytes;

// The size of command and response buffers.
// DEVNOTE: The specification only requires the size to be large
// enough for the command and response fit into the buffer. We
// would need to scale this value up in case it is not sufficient.
const TPM_PAGE_SIZE: usize = 4096;
const MAX_NV_BUFFER_SIZE: usize = MAX_DIGEST_BUFFER_SIZE;
/// Maximum NV index size supported by the TPM v1.38 reference implementation.
pub const TPM_V138_MAX_NV_INDEX_SIZE: u16 = 4096;
/// Maximum NV index size supported by the TPM v1.85 reference implementation.
pub const TPM_V185_MAX_NV_INDEX_SIZE: u16 = 8500;
// Scale this with maximum attestation payload
pub(crate) const MAX_ATTESTATION_INDEX_SIZE: u16 = 2900;

pub(crate) const RSA_2K_MODULUS_BITS: u16 = 2048;
pub(crate) const RSA_2K_MODULUS_SIZE: usize = (RSA_2K_MODULUS_BITS / 8) as usize;
const RSA_2K_EXPONENT_SIZE: usize = 3;

/// Operation types for provisioning telemetry.
#[derive(Debug)]
enum LogOpType {
    BeginNvWrite,
    NvWrite,
    BeginNvRead,
    NvRead,
}

/// RSA-2048 public key material exposed by the TPM helper utilities.
#[derive(Copy, Clone, Inspect, Debug, PartialEq)]
pub struct TpmRsa2kPublic {
    /// Big-endian RSA modulus bytes (2048 bits).
    pub modulus: [u8; RSA_2K_MODULUS_SIZE],
    /// Big-endian exponent bytes (typically 0x01_00_01).
    pub exponent: [u8; RSA_2K_EXPONENT_SIZE],
}

/// Error returned by TPM engine implementations.
#[derive(Debug)]
pub struct TpmEngineError {
    inner: Box<dyn StdError + Send + Sync>,
}

impl TpmEngineError {
    /// Creates a new [`TpmEngineError`] from the provided error.
    pub fn new<E>(error: E) -> Self
    where
        E: StdError + Send + Sync + 'static,
    {
        Self {
            inner: Box::new(error),
        }
    }

    /// Returns a reference to the wrapped error.
    pub fn as_error(&self) -> &(dyn StdError + Send + Sync + 'static) {
        &*self.inner
    }

    /// Creates a new [`TpmEngineError`] from the provided error.
    pub fn from_error<E>(error: E) -> Self
    where
        E: StdError + Send + Sync + 'static,
    {
        Self::new(error)
    }

    /// Attempts to downcast the wrapped error to the requested type.
    pub fn downcast_ref<E>(&self) -> Option<&E>
    where
        E: StdError + 'static,
    {
        self.inner.downcast_ref::<E>()
    }
}

impl fmt::Display for TpmEngineError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.inner)
    }
}

impl StdError for TpmEngineError {
    fn source(&self) -> Option<&(dyn StdError + 'static)> {
        Some(&*self.inner)
    }
}

/// Abstraction over the TPM execution engine.
pub trait TpmEngine: Send {
    /// Executes a TPM command.
    fn execute_command(
        &mut self,
        command: &mut [u8],
        response: &mut [u8],
    ) -> Result<(), TpmEngineError>;

    /// Maximum size, in bytes, of an NV index this engine can define. This is a
    /// property of the underlying reference implementation, not the protocol.
    fn max_nv_index_size(&self) -> u16;
}

/// TPM command debug information used by error logs.
#[derive(Debug)]
pub struct CommandDebugInfo {
    /// Command code
    pub command_code: CommandCodeEnum,
    /// Optional authorization handle in the command request
    pub auth_handle: Option<ReservedHandle>,
    /// Optional nv index in the command request
    pub nv_index: Option<u32>,
}

/// Top-level error produced by TPM helper routines.
#[expect(missing_docs, clippy::enum_variant_names)] // self-explanatory fields
#[derive(Error, Debug)]
pub enum Error {
    #[error("TPM command error - command code: {:?}, auth handle: {:#x?}, nv index: {:#x?}",
        {.command_debug_info.command_code}, {.command_debug_info.auth_handle}, {.command_debug_info.nv_index})]
    TpmCommandError {
        command_debug_info: CommandDebugInfo,
        #[source]
        error: TpmCommandError,
    },
    #[error("failed to export rsa public from ak handle {ak_handle:#x?}")]
    ExportRsaPublicFromAkHandle {
        ak_handle: u32,
        #[source]
        error: TpmHelperUtilityError,
    },
    #[error("failed to create ak pub template")]
    CreateAkPubTemplateFailed(#[source] TpmHelperUtilityError),
    #[error("failed to create ek pub template")]
    CreateEkPubTemplateFailed(#[source] TpmHelperUtilityError),
    #[error("failed to export rsa public from newly created primary object")]
    ExportRsaPublicFromPrimaryObject(#[source] TpmHelperUtilityError),
    #[error("nv index {0:#x} without owner read flag")]
    NoOwnerReadFlag(u32),
    #[error(
        "nv index {nv_index:#x} without auth write ({auth_write}) or platform created ({platform_created}) flag"
    )]
    InvalidPermission {
        nv_index: u32,
        auth_write: bool,
        platform_created: bool,
    },
    #[error(
        "input size {input_size} to nv write exceeds the allocated size {allocated_size} of nv index {nv_index:#x}"
    )]
    NvWriteInputTooLarge {
        nv_index: u32,
        input_size: usize,
        allocated_size: usize,
    },
    #[error("failed to find SRK {0:#x} from tpm")]
    SrkNotFound(u32),
    #[error("failed to deserialize guest secret key into TPM Import command")]
    DeserializeGuestSecretKey,
}

/// Error surface emitted while issuing commands to the TPM.
#[expect(missing_docs)] // self-explanatory fields
#[derive(Error, Debug)]
pub enum TpmCommandError {
    #[error("failed to execute the TPM command")]
    TpmExecuteCommand(#[source] TpmEngineError),
    #[error("invalid response from the TPM command")]
    InvalidResponse(#[source] ResponseValidationError),
    #[error("invalid input parameter for the TPM command")]
    InvalidInputParameter(#[source] TpmProtoError),
    #[error("TPM command failed, response code: {response_code:#x}")]
    TpmCommandFailed { response_code: u32 },
    #[error("failed to create the TPM command struct")]
    TpmCommandCreationFailed(#[source] TpmProtoError),
}

/// Helper utilities encountered an unexpected condition during processing.
#[expect(missing_docs)] // self-explanatory fields
#[derive(Error, Debug)]
pub enum TpmHelperUtilityError {
    #[error("the RSA exponent returned by TPM is unexpected")]
    UnexpectedRsaExponent,
    #[error("the size of RSA modulus returned by TPM is unexpected")]
    UnexpectedRsaModulusSize,
    #[error("invalid input parameter")]
    InvalidInputParameter(#[source] TpmProtoError),
}

/// Helper that wraps a TPM engine and cached response buffer.
#[derive(InspectMut)]
pub struct TpmEngineHelper<E> {
    /// A TPM engine instance used to service requests.
    #[inspect(skip)]
    pub tpm_engine: E,
    /// Buffer used to hold the command response.
    pub reply_buffer: [u8; TPM_PAGE_SIZE],
}

/// Action of the `evict_or_persist`.
enum EvictOrPersist {
    /// Evict a persistent handle from nv ram
    Evict(ReservedHandle),
    /// Persist a transient object into nv ram
    Persist {
        from: ReservedHandle,
        to: ReservedHandle,
    },
}

/// State of the NV index returned by `read_from_nv_index`
#[derive(Debug)]
pub enum NvIndexState {
    /// The NV index is available to read
    Available,
    /// The NV index does not exist
    Unallocated,
    /// The NV index existed but uninitialized
    Uninitialized,
}

enum AkCertType {
    None,
    PlatformOwned(Vec<u8>),
    OwnerOwned,
}

/// Parameters to allocate_guest_attestation_nv_indices
pub struct AllocateNvIndicesParams {
    /// Preserve the previous AK Cert into the newly-created NV index.
    pub preserve_ak_cert: bool,
    /// Allocate NV index for the attestation report.
    pub support_attestation_report: bool,
    /// Attempt to mitigate a platform-defined AKCert in a legacy TPM.
    pub mitigate_legacy_akcert: bool,
    /// Create the AKCert index if it is not present.
    pub create_if_missing: bool,
}

impl<E: TpmEngine> TpmEngineHelper<E> {
    /// Creates a new helper backed by the provided TPM engine implementation.
    pub fn new(tpm_engine: E) -> Self {
        Self {
            tpm_engine,
            reply_buffer: [0u8; TPM_PAGE_SIZE],
        }
    }

    // === Helper functions built on top of TPM commands === //

    /// Initialize the TPM instance and perform self-tests using Startup and SelfTest commands.
    /// This function should only be invoked after an TPM reset.
    pub fn initialize_tpm_engine(&mut self) -> Result<(), Error> {
        // Set TPM to the default state.
        self.startup(StartupType::Clear)
            .map_err(|error| Error::TpmCommandError {
                command_debug_info: CommandDebugInfo {
                    command_code: CommandCodeEnum::Startup,
                    auth_handle: None,
                    nv_index: None,
                },
                error,
            })?;

        // Perform capabilities test
        self.self_test(true)
            .map_err(|error| Error::TpmCommandError {
                command_debug_info: CommandDebugInfo {
                    command_code: CommandCodeEnum::SelfTest,
                    auth_handle: None,
                    nv_index: None,
                },
                error,
            })?;

        Ok(())
    }

    /// Clear the TPM context under the platform hierarchy using ClearControl and Clear commands.
    /// This function should only be invoked under platform hierarchy (before it's cleared by
    /// the HierarchyControl command).
    ///
    /// Returns the response code in `u32`.
    pub fn clear_tpm_platform_context(&mut self) -> Result<u32, Error> {
        // Use clear control to enable the execution of clear
        if let Err(error) = self.clear_control(TPM20_RH_PLATFORM, false) {
            if let TpmCommandError::TpmCommandFailed { response_code } = error {
                tracelimit::error_ratelimited!(
                    CVM_ALLOWED,
                    err = &error as &dyn std::error::Error,
                    "tpm ClearControlCmd failed"
                );

                // Return the error code to be written to `last_ppi_state`
                return Ok(response_code);
            } else {
                // Unexpected failure
                return Err(Error::TpmCommandError {
                    command_debug_info: CommandDebugInfo {
                        command_code: CommandCodeEnum::ClearControl,
                        auth_handle: Some(TPM20_RH_PLATFORM),
                        nv_index: None,
                    },
                    error,
                });
            }
        }

        // Clear the context associated with `TPM20_RH_PLATFORM`.
        match self.clear(TPM20_RH_PLATFORM) {
            Err(error) => {
                if let TpmCommandError::TpmCommandFailed { response_code } = error {
                    tracelimit::error_ratelimited!(
                        CVM_ALLOWED,
                        err = &error as &dyn std::error::Error,
                        "tpm ClearCmd failed"
                    );

                    // Return the error code to be written to `last_ppi_state`
                    Ok(response_code)
                } else {
                    // Unexpected failure
                    Err(Error::TpmCommandError {
                        command_debug_info: CommandDebugInfo {
                            command_code: CommandCodeEnum::Clear,
                            auth_handle: Some(TPM20_RH_PLATFORM),
                            nv_index: None,
                        },
                        error,
                    })?
                }
            }
            // Return `tpm20proto::ResponseCode::Success`
            Ok(response_code) => Ok(response_code),
        }
    }

    /// Refresh TPM endorsement primary seed (ESP) and platform primary seed (PPS) using ChangeEPS
    /// and ChangePPS commands.
    pub fn refresh_tpm_seeds(&mut self) -> Result<(), Error> {
        // Refresh endorsement primary seed (EPS)
        self.change_seed(TPM20_RH_PLATFORM, CommandCodeEnum::ChangeEPS)
            .map_err(|error| Error::TpmCommandError {
                command_debug_info: CommandDebugInfo {
                    command_code: CommandCodeEnum::ChangeEPS,
                    auth_handle: Some(TPM20_RH_PLATFORM),
                    nv_index: None,
                },
                error,
            })?;

        // Refresh platform primary seed (PPS)
        self.change_seed(TPM20_RH_PLATFORM, CommandCodeEnum::ChangePPS)
            .map_err(|error| Error::TpmCommandError {
                command_debug_info: CommandDebugInfo {
                    command_code: CommandCodeEnum::ChangePPS,
                    auth_handle: Some(TPM20_RH_PLATFORM),
                    nv_index: None,
                },
                error,
            })?;

        Ok(())
    }

    /// Create and persist an Attestation Key (AK) in the tpm.
    ///
    /// # Arguments
    /// * `force_create`: Whether to remove the existing AK and re-create one.
    ///
    /// Returns the AK public in `TpmRsa2kPublic`, and a bool indicating whether AKCert
    /// renewal is allowed.
    pub fn create_ak_pub(&mut self, force_create: bool) -> Result<(TpmRsa2kPublic, bool), Error> {
        if let Some(res) = self.find_object(TPM_AZURE_AIK_HANDLE)? {
            if force_create {
                // Remove existing key before creating a new one
                self.evict_or_persist_handle(EvictOrPersist::Evict(TPM_AZURE_AIK_HANDLE))?;
            } else {
                let expected_attributes = expected_ak_attributes();

                // If an existing key has the wrong attributes, deny renewing the AKCert later.
                // This prevents an attack where the VTL0 admin can replace the AK with their own
                // and get a signed AKCert.
                let actual_attributes = res.out_public.public_area.object_attributes;
                if actual_attributes != expected_attributes {
                    tracing::warn!(
                        CVM_ALLOWED,
                        attrs = actual_attributes.0.get(),
                        "incorrect AK attributes; denying AKCert renewal"
                    );
                }

                return export_rsa_public(&res.out_public)
                    .map_err(|error| Error::ExportRsaPublicFromAkHandle {
                        ak_handle: TPM_AZURE_AIK_HANDLE.0.get(),
                        error,
                    })
                    .map(|ak_pub| (ak_pub, actual_attributes == expected_attributes));
            }
        }

        let in_public = ak_pub_template().map_err(Error::CreateAkPubTemplateFailed)?;

        self.create_key_object(in_public, Some(TPM_AZURE_AIK_HANDLE))
            .map(|res| (res, true))
    }

    /// Create Windows-style Endorsement key (EK) based on the template from the TPM specification. Note that
    /// this function does not persist the EK in the tpm platform. Instead, EK will be created and persisted
    /// using the same template by other software component during guest OS boot.
    ///
    /// Returns the EK public in `TpmRsa2kPublic`.
    pub fn create_ek_pub(&mut self) -> Result<TpmRsa2kPublic, Error> {
        let in_public = ek_pub_template().map_err(Error::CreateEkPubTemplateFailed)?;

        self.create_key_object(in_public, None)
    }

    /// Create EK or AK based on the public key template.
    ///
    /// # Arguments
    /// `in_public` - The public key template.
    /// `ak_handle` - To determine if this is EK or AK.
    ///
    /// Returns the created RSA public in `TpmRsa2kPublic`.
    fn create_key_object(
        &mut self,
        in_public: TpmtPublic,
        ak_handle: Option<ReservedHandle>,
    ) -> Result<TpmRsa2kPublic, Error> {
        let res = match self.create_primary(TPM20_RH_ENDORSEMENT, in_public) {
            Err(error) => {
                if let TpmCommandError::TpmCommandFailed { response_code: _ } = error {
                    // Guest might cause the command to fail (e.g., taking the ownership of a hierarchy).
                    // Making this failure as non-fatal.
                    tracelimit::error_ratelimited!(
                        CVM_ALLOWED,
                        err = &error as &dyn std::error::Error,
                        "tpm CreatePrimaryCmd failed"
                    );

                    return Ok(TpmRsa2kPublic {
                        modulus: [0u8; RSA_2K_MODULUS_SIZE],
                        exponent: [0u8; RSA_2K_EXPONENT_SIZE],
                    });
                } else {
                    // Unexpected failure
                    return Err(Error::TpmCommandError {
                        command_debug_info: CommandDebugInfo {
                            command_code: CommandCodeEnum::CreatePrimary,
                            auth_handle: Some(TPM20_RH_ENDORSEMENT),
                            nv_index: None,
                        },
                        error,
                    });
                }
            }
            Ok(res) => res,
        };

        if res.out_public.size.get() == 0 {
            // Guest might cause the command to fail (e.g., taking the ownership of a hierarchy).
            // Making this failure as non-fatal.
            tracelimit::error_ratelimited!(
                CVM_ALLOWED,
                "No public data in CreatePrimaryCmd response"
            );

            return Ok(TpmRsa2kPublic {
                modulus: [0u8; RSA_2K_MODULUS_SIZE],
                exponent: [0u8; RSA_2K_EXPONENT_SIZE],
            });
        }

        let rsa_public = if let Some(ak_handle) = ak_handle {
            // Make a persistent copy of the transient object
            self.evict_or_persist_handle(EvictOrPersist::Persist {
                from: res.object_handle,
                to: ak_handle,
            })?;

            export_rsa_public(&res.out_public)
        } else {
            // EK already exists, we just re-compute the public key
            export_rsa_public(&res.out_public)
        }
        .map_err(Error::ExportRsaPublicFromPrimaryObject)?;

        if let Err(error) = self.flush_context(res.object_handle) {
            if let TpmCommandError::TpmCommandFailed { response_code: _ } = error {
                // Guest might cause the command to fail (e.g., taking the ownership of a hierarchy).
                // Making this failure as non-fatal.
                tracelimit::error_ratelimited!(
                    CVM_ALLOWED,
                    err = &error as &dyn std::error::Error,
                    "tpm FlushContextCmd failed"
                );
            } else {
                // Unexpected failure
                return Err(Error::TpmCommandError {
                    command_debug_info: CommandDebugInfo {
                        command_code: CommandCodeEnum::FlushContext,
                        auth_handle: None,
                        nv_index: Some(res.object_handle.0.get()),
                    },
                    error,
                });
            }
        }

        Ok(rsa_public)
    }

    /// Evict a persistent object from or persist a transient object to nv ram using EvictControl
    /// command.
    fn evict_or_persist_handle(&mut self, action: EvictOrPersist) -> Result<(), Error> {
        let (object_handle, persistent_handle) = match action {
            EvictOrPersist::Evict(handle) => (handle, handle),
            EvictOrPersist::Persist { from, to } => (from, to),
        };

        if let Err(error) = self.evict_control(TPM20_RH_OWNER, object_handle, persistent_handle) {
            if let TpmCommandError::TpmCommandFailed { response_code: _ } = error {
                // Guest might cause the command to fail (e.g., taking the ownership of a hierarchy).
                // Making this failure as non-fatal.
                tracelimit::error_ratelimited!(
                    CVM_ALLOWED,
                    err = &error as &dyn std::error::Error,
                    "tpm EvictControlCmd failed"
                );
            } else {
                // Unexpected failure
                return Err(Error::TpmCommandError {
                    command_debug_info: CommandDebugInfo {
                        command_code: CommandCodeEnum::EvictControl,
                        auth_handle: Some(TPM20_RH_OWNER),
                        nv_index: Some(object_handle.0.get()),
                    },
                    error,
                });
            }
        }

        Ok(())
    }

    /// Read the existing AK cert and clear the nv index if:
    ///  - the nv index is present, and is platform owned
    ///  - the nv index is present, but has no data
    ///
    /// Owner owned nv index is left as-is.
    fn take_existing_ak_cert(&mut self) -> Result<AkCertType, Error> {
        let max_nv_index_size = self.tpm_engine.max_nv_index_size();
        let mut output = vec![0; max_nv_index_size as usize];

        // Read the AK cert from the index. If the index is not owner owned, the
        // index will be removed.
        match self.read_from_nv_index(TPM_NV_INDEX_AIK_CERT, &mut output)? {
            NvIndexState::Available => {
                let res = self
                    .find_nv_index(TPM_NV_INDEX_AIK_CERT)?
                    .expect("nv index exists");
                let nv_bits = TpmaNvBits::from(res.nv_public.nv_public.attributes.0.get());
                let size = res.nv_public.nv_public.data_size.get();

                // Resize the output vector to match exactly what the nv index
                // size is.
                assert!(size <= max_nv_index_size);
                output.resize(size as usize, 0);

                let platform_cert = nv_bits.nv_platformcreate();
                tracing::info!(platform_cert, "AK cert nv index with available data");

                if nv_bits.nv_platformcreate() {
                    tracing::info!("clearing platform owned AK cert");
                    self.nv_undefine_space(TPM20_RH_PLATFORM, TPM_NV_INDEX_AIK_CERT)
                        .map_err(|error| Error::TpmCommandError {
                            command_debug_info: CommandDebugInfo {
                                command_code: CommandCodeEnum::NV_UndefineSpace,
                                auth_handle: Some(TPM20_RH_PLATFORM),
                                nv_index: Some(TPM_NV_INDEX_AIK_CERT),
                            },
                            error,
                        })?;

                    Ok(AkCertType::PlatformOwned(output))
                } else {
                    tracing::info!("Existing AK cert is owner-defined");
                    Ok(AkCertType::OwnerOwned)
                }
            }
            NvIndexState::Uninitialized => {
                tracing::info!("AK cert nv index allocated but uninitialized");

                self.nv_undefine_space(TPM20_RH_PLATFORM, TPM_NV_INDEX_AIK_CERT)
                    .map_err(|error| Error::TpmCommandError {
                        command_debug_info: CommandDebugInfo {
                            command_code: CommandCodeEnum::NV_UndefineSpace,
                            auth_handle: Some(TPM20_RH_PLATFORM),
                            nv_index: Some(TPM_NV_INDEX_AIK_CERT),
                        },
                        error,
                    })?;

                Ok(AkCertType::None)
            }
            NvIndexState::Unallocated => {
                tracing::info!("AK cert nv index not allocated yet");
                Ok(AkCertType::None)
            }
        }
    }

    /// Allocate NV indices under platform hierarchy that are necessary for guest
    /// attestation.
    ///
    /// # Arguments
    /// * `auth_value`: The password used during the NV indices allocation.
    /// * `params`: Flags that control how and whether indices are allocated.
    ///
    pub fn allocate_guest_attestation_nv_indices(
        &mut self,
        auth_value: u64,
        params: AllocateNvIndicesParams,
    ) -> Result<(), Error> {
        if params.mitigate_legacy_akcert && self.has_mitigation_marker() {
            // VM has a small-vTPM mitigation marker. Don't touch anything, but
            // log whether the AK cert exists, as that previous write might have
            // failed.
            let mut output = vec![0u8; self.tpm_engine.max_nv_index_size() as usize];
            let r = self.read_from_nv_index(TPM_NV_INDEX_AIK_CERT, &mut output);
            tracing::warn!("VM has 16k vTPM mitigation marker");
            match r {
                Err(e) => tracing::error!(
                    err = &e as &dyn std::error::Error,
                    "error reading AKCert index with mitigation marker"
                ),
                Ok(NvIndexState::Available) => {
                    let res = self
                        .find_nv_index(TPM_NV_INDEX_AIK_CERT)?
                        .expect("akcert nv index present");
                    let nv_bits = TpmaNvBits::from(res.nv_public.nv_public.attributes.0.get());
                    let size = res.nv_public.nv_public.data_size.get();

                    tracing::info!(?nv_bits, size, "AKCert index exists");

                    if nv_bits.nv_platformcreate() {
                        tracing::info!("AKCert index is platform owned; restoring owner auth");
                        let existing_cert = self.take_existing_ak_cert()?;
                        if let AkCertType::PlatformOwned(cert) = existing_cert {
                            self.nv_define_space(
                                TPM20_RH_OWNER,
                                0,
                                TPM_NV_INDEX_AIK_CERT,
                                cert.len() as u16,
                            )
                            .map_err(|error| {
                                Error::TpmCommandError {
                                    command_debug_info: CommandDebugInfo {
                                        command_code: CommandCodeEnum::NV_DefineSpace,
                                        auth_handle: Some(TPM20_RH_OWNER),
                                        nv_index: Some(TPM_NV_INDEX_AIK_CERT),
                                    },
                                    error,
                                }
                            })?;

                            let start_time = std::time::SystemTime::now();
                            if let Err(error) =
                                self.nv_write(TPM20_RH_OWNER, None, TPM_NV_INDEX_AIK_CERT, &cert)
                            {
                                tracing::error!(
                                    CVM_ALLOWED,
                                    op_type = ?LogOpType::NvWrite,
                                    nv_index = TPM_NV_INDEX_AIK_CERT,
                                    data_size = cert.len(),
                                    success = false,
                                    err = &error as &dyn std::error::Error,
                                    latency = std::time::SystemTime::now()
                                        .duration_since(start_time)
                                        .map_or(0, |d| d.as_millis()),
                                    "Error writing AKCert TPM NVRAM index"
                                );

                                return Err(Error::TpmCommandError {
                                    command_debug_info: CommandDebugInfo {
                                        command_code: CommandCodeEnum::NV_Write,
                                        auth_handle: Some(TPM20_RH_OWNER),
                                        nv_index: Some(TPM_NV_INDEX_AIK_CERT),
                                    },
                                    error,
                                });
                            } else {
                                tracing::info!(
                                    CVM_ALLOWED,
                                    op_type = ?LogOpType::NvWrite,
                                    nv_index = TPM_NV_INDEX_AIK_CERT,
                                    data_size = cert.len(),
                                    success = true,
                                    latency = std::time::SystemTime::now()
                                        .duration_since(start_time)
                                        .map_or(0, |d| d.as_millis()),
                                    "Wrote AKCert TPM NVRAM index"
                                );
                            }
                        }
                    }
                }
                Ok(NvIndexState::Uninitialized) => {
                    tracing::warn!("AKCert index uninitialized with mitigation marker")
                }
                Ok(NvIndexState::Unallocated) => {
                    tracing::warn!("AKCert index unallocated with mitigation marker")
                }
            }

            return Ok(());
        } else {
            tracing::info!(
                "No small-vTPM mitigation marker; proceeding to resize AKCert index if needed"
            );
        }

        let previous_ak_cert = self.take_existing_ak_cert()?;

        match previous_ak_cert {
            AkCertType::None => {
                if params.create_if_missing {
                    let size = TPM_DEFAULT_AKCERT_SIZE as u16;

                    tracing::info!(
                        nv_index = format!("{:x}", TPM_NV_INDEX_AIK_CERT),
                        size,
                        "Allocate nv index for AK cert"
                    );

                    self.nv_define_space(
                        TPM20_RH_PLATFORM,
                        auth_value,
                        TPM_NV_INDEX_AIK_CERT,
                        size,
                    )
                    .map_err(|error| Error::TpmCommandError {
                        command_debug_info: CommandDebugInfo {
                            command_code: CommandCodeEnum::NV_DefineSpace,
                            auth_handle: Some(TPM20_RH_PLATFORM),
                            nv_index: Some(TPM_NV_INDEX_AIK_CERT),
                        },
                        error,
                    })?;
                }
            }
            AkCertType::PlatformOwned(mut cert) => {
                let will_mitigate_cert =
                    params.mitigate_legacy_akcert && cert.len() == TPM_DEFAULT_AKCERT_SIZE;

                if will_mitigate_cert {
                    self.write_mitigation_marker(auth_value);
                }

                let size = if will_mitigate_cert {
                    // To save space in the NVRAM, if the AKCert index contents
                    // look like a DER-encoded X.509 certificate, use its actual
                    // size (plus 4 bytes for the DER header).
                    if let &[0x30, 0x82, len0, len1, ..] = cert.as_slice() {
                        let len = u16::from_be_bytes([len0, len1]);
                        let parsed_size = len.saturating_add(4).min(TPM_DEFAULT_AKCERT_SIZE as u16);
                        tracing::warn!(parsed_size, "redefining AKCert index with limited size");
                        assert!(parsed_size as usize <= cert.len());
                        cert.resize(parsed_size as usize, 0);
                        parsed_size
                    } else {
                        TPM_DEFAULT_AKCERT_SIZE as u16
                    }
                } else {
                    TPM_DEFAULT_AKCERT_SIZE as u16
                };

                tracing::info!(
                    nv_index = format!("{:x}", TPM_NV_INDEX_AIK_CERT),
                    size,
                    "allocate nv index for previous platform AK cert"
                );

                let (handle, auth, write_auth_handle) = if will_mitigate_cert {
                    (TPM20_RH_OWNER, None, TPM20_RH_OWNER)
                } else {
                    (
                        TPM20_RH_PLATFORM,
                        Some(auth_value),
                        ReservedHandle(TPM_NV_INDEX_AIK_CERT.into()),
                    )
                };

                let result = self
                    .nv_define_space(handle, auth.unwrap_or(0), TPM_NV_INDEX_AIK_CERT, size)
                    .map_err(|error| Error::TpmCommandError {
                        command_debug_info: CommandDebugInfo {
                            command_code: CommandCodeEnum::NV_DefineSpace,
                            auth_handle: Some(handle),
                            nv_index: Some(TPM_NV_INDEX_AIK_CERT),
                        },
                        error,
                    });

                match result {
                    Err(e) => {
                        tracing::error!(
                            error = &e as &dyn std::error::Error,
                            "Failed to allocate AK cert nv index"
                        );

                        // Unless this VM was mitigated, bubble this error up to
                        // the caller.
                        if !will_mitigate_cert {
                            return Err(e);
                        }
                    }
                    Ok(_) => {
                        tracing::info!("Successfully allocated AK cert nv index");

                        if params.preserve_ak_cert {
                            // For resiliency, write the previous AK cert to the
                            // newly created nv index in case the following
                            // boot-time AK cert request fails.
                            tracing::info!("Preserve previous AK cert across boot");

                            let start_time = std::time::SystemTime::now();
                            if let Err(error) =
                                self.nv_write(write_auth_handle, auth, TPM_NV_INDEX_AIK_CERT, &cert)
                            {
                                tracing::error!(
                                    CVM_ALLOWED,
                                    op_type = ?LogOpType::NvWrite,
                                    nv_index = TPM_NV_INDEX_AIK_CERT,
                                    data_size = cert.len(),
                                    success = false,
                                    err = &error as &dyn std::error::Error,
                                    latency = std::time::SystemTime::now()
                                        .duration_since(start_time)
                                        .map_or(0, |d| d.as_millis()),
                                    "Error rewriting existing AKCert TPM NVRAM index"
                                );
                                return Err(Error::TpmCommandError {
                                    command_debug_info: CommandDebugInfo {
                                        command_code: CommandCodeEnum::NV_Write,
                                        auth_handle: Some(ReservedHandle(
                                            TPM_NV_INDEX_AIK_CERT.into(),
                                        )),
                                        nv_index: Some(TPM_NV_INDEX_AIK_CERT),
                                    },
                                    error,
                                });
                            } else {
                                tracing::info!(
                                    CVM_ALLOWED,
                                    op_type = ?LogOpType::NvWrite,
                                    nv_index = TPM_NV_INDEX_AIK_CERT,
                                    data_size = cert.len(),
                                    success = true,
                                    latency = std::time::SystemTime::now()
                                        .duration_since(start_time)
                                        .map_or(0, |d| d.as_millis()),
                                    "Rewrote existing AKCert TPM NVRAM index"
                                );
                            }
                        }
                    }
                }
            }
            AkCertType::OwnerOwned => {
                // Owner owned AK certs are left as-is.
            }
        }

        // Allocate `TPM_NV_INDEX_ATTESTATION_REPORT` if `support_attestation_report` is true
        if params.support_attestation_report {
            // Attempt to remove previous `TPM_NV_INDEX_ATTESTATION_REPORT` allocation before the allocation
            if self
                .find_nv_index(TPM_NV_INDEX_ATTESTATION_REPORT)?
                .is_some()
            {
                self.nv_undefine_space(TPM20_RH_PLATFORM, TPM_NV_INDEX_ATTESTATION_REPORT)
                    .map_err(|error| Error::TpmCommandError {
                        command_debug_info: CommandDebugInfo {
                            command_code: CommandCodeEnum::NV_UndefineSpace,
                            auth_handle: Some(TPM20_RH_PLATFORM),
                            nv_index: Some(TPM_NV_INDEX_ATTESTATION_REPORT),
                        },
                        error,
                    })?;
            }

            tracing::info!(
                nv_index = format!("{:x}", TPM_NV_INDEX_ATTESTATION_REPORT),
                size = MAX_ATTESTATION_INDEX_SIZE,
                "Allocate nv index for attestation report",
            );

            self.nv_define_space(
                TPM20_RH_PLATFORM,
                auth_value,
                TPM_NV_INDEX_ATTESTATION_REPORT,
                MAX_ATTESTATION_INDEX_SIZE,
            )
            .map_err(|error| Error::TpmCommandError {
                command_debug_info: CommandDebugInfo {
                    command_code: CommandCodeEnum::NV_DefineSpace,
                    auth_handle: Some(TPM20_RH_PLATFORM),
                    nv_index: Some(TPM_NV_INDEX_ATTESTATION_REPORT),
                },
                error,
            })?;
        }

        Ok(())
    }

    fn has_mitigation_marker(&mut self) -> bool {
        self.find_nv_index(TPM_NV_INDEX_MITIGATED)
            .is_ok_and(|v| v.is_some())
    }

    fn write_mitigation_marker(&mut self, auth_value: u64) {
        match self.nv_define_space(TPM20_RH_PLATFORM, auth_value, TPM_NV_INDEX_MITIGATED, 1) {
            Ok(_) => {
                tracing::warn!(TPM_NV_INDEX_MITIGATED, "wrote tpm mitigation marker");
            }
            Err(e) => {
                tracing::error!(
                    error = &e as &dyn std::error::Error,
                    "failed to write mitigation marker"
                );
            }
        }
    }

    /// Check if the AKCert NV index exists and has the platform_create attribute.
    pub fn has_platform_akcert_index(&mut self) -> bool {
        self.find_nv_index(TPM_NV_INDEX_AIK_CERT).is_ok_and(|res| {
            res.is_some_and(|reply| {
                TpmaNvBits::from(reply.nv_public.nv_public.attributes.0.get()).nv_platformcreate()
            })
        })
    }

    /// Check if the nv index is present using NV_ReadPublic command.
    ///
    /// Returns Ok(Some(NvReadPublicReply)) if nv index is present.
    /// Returns Ok(None) if nv index is not present.
    pub fn find_nv_index(&mut self, nv_index: u32) -> Result<Option<NvReadPublicReply>, Error> {
        match self.nv_read_public(nv_index) {
            Err(error) => {
                if let TpmCommandError::TpmCommandFailed { response_code } = error {
                    if response_code == (ResponseCode::Handle as u32 | ResponseCode::Rc1 as u32) {
                        // nv index not found
                        Ok(None)
                    } else {
                        // Unexpected response code
                        Err(Error::TpmCommandError {
                            command_debug_info: CommandDebugInfo {
                                command_code: CommandCodeEnum::NV_ReadPublic,
                                auth_handle: None,
                                nv_index: Some(nv_index),
                            },
                            error,
                        })?
                    }
                } else {
                    // Unexpected failure
                    Err(Error::TpmCommandError {
                        command_debug_info: CommandDebugInfo {
                            command_code: CommandCodeEnum::NV_ReadPublic,
                            auth_handle: None,
                            nv_index: Some(nv_index),
                        },
                        error,
                    })?
                }
            }
            Ok(res) => Ok(Some(res)),
        }
    }

    /// Write data to a NV index that is password-based and platform-created.
    /// If the data size is less than the size of the index, the function applies
    /// zero padding and ensure the entire NV space is filled.
    ///
    /// # Arguments
    /// * `auth_value` - The authorization value for the password-based index.
    /// * `nv_index` - The target NV index.
    /// * `data` - The data to write.
    ///
    pub fn write_to_nv_index(
        &mut self,
        auth_value: u64,
        nv_index: u32,
        data: &[u8],
    ) -> Result<(), Error> {
        let res = self
            .nv_read_public(nv_index)
            .map_err(|error| Error::TpmCommandError {
                command_debug_info: CommandDebugInfo {
                    command_code: CommandCodeEnum::NV_ReadPublic,
                    auth_handle: None,
                    nv_index: Some(nv_index),
                },
                error,
            })?;

        let nv_bits = TpmaNvBits::from(res.nv_public.nv_public.attributes.0.get());
        let nv_index_size = res.nv_public.nv_public.data_size.get();

        // Validate the input size against the nv index size
        let data = match data.len().cmp(&nv_index_size.into()) {
            std::cmp::Ordering::Greater => Err(Error::NvWriteInputTooLarge {
                nv_index,
                input_size: data.len(),
                allocated_size: nv_index_size.into(),
            })?,
            std::cmp::Ordering::Less => {
                // Ensure the nv index is filled by padding 0's.
                let mut data = data.to_vec();
                data.resize(nv_index_size.into(), 0);
                data
            }
            std::cmp::Ordering::Equal => data.to_vec(),
        };

        // Always expect nv index to be password-based and platform-created given that
        // the index is always created or re-created at boot-time.
        if !nv_bits.nv_authwrite() || !nv_bits.nv_platformcreate() {
            return Err(Error::InvalidPermission {
                nv_index,
                auth_write: nv_bits.nv_authwrite(),
                platform_created: nv_bits.nv_platformcreate(),
            });
        }

        let start_time = std::time::SystemTime::now();
        if let Err(error) = self.nv_write(
            ReservedHandle(nv_index.into()),
            Some(auth_value),
            nv_index,
            &data,
        ) {
            tracing::error!(
                CVM_ALLOWED,
                op_type = ?LogOpType::NvWrite,
                nv_index,
                data_size = data.len(),
                success = false,
                err = &error as &dyn std::error::Error,
                latency = std::time::SystemTime::now()
                    .duration_since(start_time)
                    .map_or(0, |d| d.as_millis()),
                "Error writing TPM NVRAM index"
            );
            return Err(Error::TpmCommandError {
                command_debug_info: CommandDebugInfo {
                    command_code: CommandCodeEnum::NV_Write,
                    auth_handle: Some(ReservedHandle(nv_index.into())),
                    nv_index: Some(nv_index),
                },
                error,
            });
        } else {
            tracing::info!(
                CVM_ALLOWED,
                op_type = ?LogOpType::NvWrite,
                nv_index,
                data_size = data.len(),
                success = true,
                latency = std::time::SystemTime::now()
                    .duration_since(start_time)
                    .map_or(0, |d| d.as_millis()),
                "Wrote TPM NVRAM index"
            );
        }

        Ok(())
    }

    /// Read data from a owner-defined NV Index if the index is present.
    ///
    /// # Arguments
    /// * `nv_index` - The target NV index.
    /// * `data` - The data to write.
    ///
    /// Returns Ok(NvIndexState::Available) if the index is present and read succeeds.
    /// Returns Ok(NvIndexState::Unallocated) if the index is not present.
    /// Returns Ok(NvIndexState::Uninitialized) if the index is present but uninitialized.
    pub fn read_from_nv_index(
        &mut self,
        nv_index: u32,
        data: &mut [u8],
    ) -> Result<NvIndexState, Error> {
        let Some(res) = self.find_nv_index(nv_index)? else {
            // nv index may not exist before guest makes a request
            return Ok(NvIndexState::Unallocated);
        };

        let nv_bits = TpmaNvBits::from(res.nv_public.nv_public.attributes.0.get());
        if !nv_bits.nv_ownerread() {
            Err(Error::NoOwnerReadFlag(nv_index))?
        }

        let nv_index_size = res.nv_public.nv_public.data_size.get();
        let start_time = std::time::SystemTime::now();
        tracing::info!(
            CVM_ALLOWED,
            op_type = ?LogOpType::BeginNvRead,
            nv_index,
            data_size = nv_index_size,
            "Reading TPM NVRAM index"
        );

        if let Err(error) = self.nv_read(TPM20_RH_OWNER, nv_index, nv_index_size, data) {
            tracing::error!(
                CVM_ALLOWED,
                op_type = ?LogOpType::NvRead,
                nv_index,
                data_size = nv_index_size,
                success = false,
                err = &error as &dyn std::error::Error,
                latency = std::time::SystemTime::now()
                    .duration_since(start_time)
                    .map_or(0, |d| d.as_millis()),
                "Error reading TPM NVRAM index"
            );

            if let TpmCommandError::TpmCommandFailed { response_code } = error {
                if response_code == ResponseCode::NvUninitialized as u32 {
                    Ok(NvIndexState::Uninitialized)
                } else {
                    // Unexpected response code
                    Err(Error::TpmCommandError {
                        command_debug_info: CommandDebugInfo {
                            command_code: CommandCodeEnum::NV_Read,
                            auth_handle: Some(TPM20_RH_OWNER),
                            nv_index: Some(nv_index),
                        },
                        error,
                    })?
                }
            } else {
                // Unexpected failure
                Err(Error::TpmCommandError {
                    command_debug_info: CommandDebugInfo {
                        command_code: CommandCodeEnum::NV_Read,
                        auth_handle: Some(TPM20_RH_OWNER),
                        nv_index: Some(nv_index),
                    },
                    error,
                })?
            }
        } else {
            tracing::info!(
                CVM_ALLOWED,
                op_type = ?LogOpType::NvRead,
                nv_index,
                data_size = nv_index_size,
                success = true,
                latency = std::time::SystemTime::now()
                    .duration_since(start_time)
                    .map_or(0, |d| d.as_millis()),
                "Read TPM NVRAM index"
            );
            Ok(NvIndexState::Available)
        }
    }

    /// Check if the object is present using ReadPublic command.
    ///
    /// Returns Ok(Some(ReadPublicReply)) if the object is present.
    /// Returns Ok(None) if nv index is not present.
    pub fn find_object(
        &mut self,
        object_handle: ReservedHandle,
    ) -> Result<Option<ReadPublicReply>, Error> {
        match self.read_public(object_handle) {
            Err(error) => {
                if let TpmCommandError::TpmCommandFailed { response_code } = error {
                    if response_code == (ResponseCode::Handle as u32 | ResponseCode::Rc1 as u32) {
                        // nv index not found
                        Ok(None)
                    } else {
                        // Unexpected response code
                        Err(Error::TpmCommandError {
                            command_debug_info: CommandDebugInfo {
                                command_code: CommandCodeEnum::ReadPublic,
                                auth_handle: None,
                                nv_index: Some(object_handle.0.get()),
                            },
                            error,
                        })?
                    }
                } else {
                    // Unexpected failure
                    Err(Error::TpmCommandError {
                        command_debug_info: CommandDebugInfo {
                            command_code: CommandCodeEnum::ReadPublic,
                            auth_handle: None,
                            nv_index: Some(object_handle.0.get()),
                        },
                        error,
                    })?
                }
            }
            Ok(res) => Ok(Some(res)),
        }
    }

    /// Initialize the guest secret key with the given data
    /// blob using Import, Load, and EvictControl commands.
    ///
    /// # Arguments
    /// * `guest_secret_key`: The guest secret key data blob.
    ///   The format of the data blob is expected to be:
    ///   (TPM2B_PUBLIC || TPM2B_PRIVATE || TPM2B_ENCRYPTED_SECRET)
    ///
    pub fn initialize_guest_secret_key(&mut self, guest_secret_key: &[u8]) -> Result<(), Error> {
        use tpm_protocol::tpm20proto::protocol::ImportCmd;

        if self.find_object(TPM_GUEST_SECRET_HANDLE)?.is_some() {
            // ECC key found, early return.
            return Ok(());
        };

        if self.find_object(TPM_RSA_SRK_HANDLE)?.is_none() {
            // SRK not found, return an error.
            return Err(Error::SrkNotFound(TPM_RSA_SRK_HANDLE.0.get()));
        };

        // Deserialize the guest secret key data blob
        let import_command = ImportCmd::deserialize_no_wrapping_key(guest_secret_key)
            .ok_or(Error::DeserializeGuestSecretKey)?;

        // Import the key under `TPM_RSA_SRK_HANDLE`
        let import_reply = self
            .import(
                TPM_RSA_SRK_HANDLE,
                &import_command.object_public,
                &import_command.duplicate,
                &import_command.in_sym_seed,
            )
            .map_err(|error| Error::TpmCommandError {
                command_debug_info: CommandDebugInfo {
                    command_code: CommandCodeEnum::Import,
                    auth_handle: None,
                    nv_index: None,
                },
                error,
            })?;

        // Load the imported key
        let load_reply = self
            .load(
                TPM_RSA_SRK_HANDLE,
                &import_reply.out_private,
                &import_command.object_public,
            )
            .map_err(|error| Error::TpmCommandError {
                command_debug_info: CommandDebugInfo {
                    command_code: CommandCodeEnum::Load,
                    auth_handle: None,
                    nv_index: None,
                },
                error,
            })?;

        // Persist the imported key into TPM
        self.evict_or_persist_handle(EvictOrPersist::Persist {
            from: load_reply.object_handle,
            to: TPM_GUEST_SECRET_HANDLE,
        })?;

        Ok(())
    }

    // === TPM commands === //

    /// Helper function to send Startup command.
    ///
    /// # Arguments
    /// * `startup_type`: The requested type to the command.
    ///
    pub fn startup(&mut self, startup_type: StartupType) -> Result<(), TpmCommandError> {
        use tpm20proto::protocol::StartupCmd;

        let session_tag = SessionTagEnum::NoSessions;
        let mut cmd = StartupCmd::new(session_tag.into(), startup_type);

        self.tpm_engine
            .execute_command(cmd.as_mut_bytes(), &mut self.reply_buffer)
            .map_err(TpmCommandError::TpmExecuteCommand)?;

        match StartupCmd::base_validate_reply(&self.reply_buffer, session_tag) {
            Err(error) => Err(TpmCommandError::InvalidResponse(error))?,
            Ok((res, false)) => Err(TpmCommandError::TpmCommandFailed {
                response_code: res.header.response_code.get(),
            })?,
            Ok((_res, true)) => Ok(()),
        }
    }

    /// Helper function to send SelfTest command.
    ///
    /// # Arguments
    /// * `full_test`*: Perform full test or not.
    ///
    pub fn self_test(&mut self, full_test: bool) -> Result<(), TpmCommandError> {
        use tpm20proto::protocol::SelfTestCmd;

        let session_tag = SessionTagEnum::NoSessions;

        // Perform full test by default
        let mut cmd = SelfTestCmd::new(session_tag.into(), full_test);

        self.tpm_engine
            .execute_command(cmd.as_mut_bytes(), &mut self.reply_buffer)
            .map_err(TpmCommandError::TpmExecuteCommand)?;

        match SelfTestCmd::base_validate_reply(&self.reply_buffer, session_tag) {
            Err(error) => Err(TpmCommandError::InvalidResponse(error))?,
            Ok((res, false)) => Err(TpmCommandError::TpmCommandFailed {
                response_code: res.header.response_code.get(),
            })?,
            Ok((_res, true)) => Ok(()),
        }
    }

    /// Helper function to send ClearControl command.
    ///
    /// # Arguments
    /// * `auth_handle`: The authorization handle used in the command.
    /// * `disable`: Disable the execution of the Control command or not.
    ///
    pub fn clear_control(
        &mut self,
        auth_handle: ReservedHandle,
        disable: bool,
    ) -> Result<(), TpmCommandError> {
        use tpm20proto::protocol::ClearControlCmd;

        let session_tag = SessionTagEnum::Sessions;
        let mut cmd = ClearControlCmd::new(
            session_tag.into(),
            auth_handle,
            CmdAuth::new(TPM20_RS_PW, 0, 0, 0),
            disable,
        );

        self.tpm_engine
            .execute_command(cmd.as_mut_bytes(), &mut self.reply_buffer)
            .map_err(TpmCommandError::TpmExecuteCommand)?;

        match ClearControlCmd::base_validate_reply(&self.reply_buffer, session_tag) {
            Err(error) => Err(TpmCommandError::InvalidResponse(error))?,
            Ok((res, false)) => Err(TpmCommandError::TpmCommandFailed {
                response_code: res.header.response_code.get(),
            })?,
            Ok((_res, true)) => Ok(()),
        }
    }

    /// Helper function to send Clear command.
    ///
    /// # Arguments
    /// * `auth_handle`: The authorization handle used in the command.
    ///
    /// Returns the response code of the command (write back into `last_ppi_state`).
    pub fn clear(&mut self, auth_handle: ReservedHandle) -> Result<u32, TpmCommandError> {
        use tpm20proto::protocol::ClearCmd;

        let session_tag = SessionTagEnum::Sessions;
        let mut cmd = ClearCmd::new(
            session_tag.into(),
            auth_handle,
            CmdAuth::new(TPM20_RS_PW, 0, 0, 0),
        );

        self.tpm_engine
            .execute_command(cmd.as_mut_bytes(), &mut self.reply_buffer)
            .map_err(TpmCommandError::TpmExecuteCommand)?;

        match ClearCmd::base_validate_reply(&self.reply_buffer, session_tag) {
            Err(error) => Err(TpmCommandError::InvalidResponse(error))?,
            Ok((res, false)) => Err(TpmCommandError::TpmCommandFailed {
                response_code: res.header.response_code.get(),
            })?,
            Ok((res, true)) => Ok(res.header.response_code.get()),
        }
    }

    /// Helper function to send PcrAllocate command.
    ///
    /// # Arguments
    /// * `supported_pcr_banks` - 5-bit bitmap for supported PCR banks.
    /// * `pcr_banks_to_allocate` - 5-bit bitmap for PCR banks to be allocate.
    ///
    /// Returns the response code of the command (write back into `last_ppi_state`).
    pub fn pcr_allocate(
        &mut self,
        auth_handle: ReservedHandle,
        supported_pcr_banks: u32,
        pcr_banks_to_allocate: u32,
    ) -> Result<u32, TpmCommandError> {
        use tpm20proto::protocol::PcrAllocateCmd;

        let mut pcr_selections = Vec::new(); // TODO: replace with smallvec<5>?
        for (alg_hash, alg_id) in PcrAllocateCmd::HASH_ALG_TO_ID {
            if (alg_hash & supported_pcr_banks) != 0 {
                pcr_selections.push(PcrSelection {
                    hash: alg_id,
                    size_of_select: 3,
                    bitmap: if (alg_hash & pcr_banks_to_allocate) != 0 {
                        [0xff, 0xff, 0xff]
                    } else {
                        [0x00, 0x00, 0x00]
                    },
                })
            }
        }

        let session_tag = SessionTagEnum::Sessions;
        let cmd = PcrAllocateCmd::new(
            session_tag.into(),
            auth_handle,
            CmdAuth::new(TPM20_RS_PW, 0, 0, 0),
            &pcr_selections,
        )
        .map_err(TpmCommandError::TpmCommandCreationFailed)?;

        self.tpm_engine
            .execute_command(&mut cmd.serialize(), &mut self.reply_buffer)
            .map_err(TpmCommandError::TpmExecuteCommand)?;

        match PcrAllocateCmd::base_validate_reply(&self.reply_buffer, session_tag) {
            Err(error) => Err(TpmCommandError::InvalidResponse(error))?,
            Ok((res, false)) => Err(TpmCommandError::TpmCommandFailed {
                response_code: res.header.response_code.get(),
            })?,
            Ok((res, true)) => Ok(res.header.response_code.get()),
        }
    }

    /// Helper function to send ChangeEPS and ChangePPS commands.
    ///
    /// # Arguments
    /// * `auth_handle`: The authorization handle used in the command.
    /// * `command_code`: The command corresponding to the seed to refresh (ChangeEPS or ChangePPS).
    ///
    pub fn change_seed(
        &mut self,
        auth_handle: ReservedHandle,
        command_code: CommandCodeEnum,
    ) -> Result<(), TpmCommandError> {
        use tpm_protocol::tpm20proto::protocol::ChangeSeedCmd;

        assert!(matches!(
            command_code,
            CommandCodeEnum::ChangeEPS | CommandCodeEnum::ChangePPS
        ));

        let session_tag = SessionTagEnum::Sessions;
        let mut cmd = ChangeSeedCmd::new(
            session_tag.into(),
            auth_handle,
            CmdAuth::new(TPM20_RS_PW, 0, 0, 0),
            command_code,
        );

        self.tpm_engine
            .execute_command(cmd.as_mut_bytes(), &mut self.reply_buffer)
            .map_err(TpmCommandError::TpmExecuteCommand)?;

        match ChangeSeedCmd::base_validate_reply(&self.reply_buffer, session_tag) {
            Err(error) => Err(TpmCommandError::InvalidResponse(error))?,
            Ok((res, false)) => Err(TpmCommandError::TpmCommandFailed {
                response_code: res.header.response_code.get(),
            })?,
            Ok((_res, true)) => Ok(()),
        }
    }

    /// Helper function to send ReadPublic command.
    ///
    /// # Arguments
    /// * `object_handle` - The handle to read.
    ///
    /// Returns Ok(ReadPublicReply) if the command succeeds. Returns
    /// Err(TpmCommandError) otherwise.
    pub fn read_public(
        &mut self,
        object_handle: ReservedHandle,
    ) -> Result<ReadPublicReply, TpmCommandError> {
        use tpm20proto::protocol::ReadPublicCmd;

        let session_tag = SessionTagEnum::NoSessions;
        let mut cmd = ReadPublicCmd::new(session_tag.into(), object_handle);

        self.tpm_engine
            .execute_command(cmd.as_mut_bytes(), &mut self.reply_buffer)
            .map_err(TpmCommandError::TpmExecuteCommand)?;

        match ReadPublicCmd::base_validate_reply(&self.reply_buffer, session_tag) {
            Err(error) => Err(TpmCommandError::InvalidResponse(error))?,
            Ok((res, false)) => Err(TpmCommandError::TpmCommandFailed {
                response_code: res.header.response_code.get(),
            })?,
            Ok((res, true)) => Ok(res),
        }
    }

    /// Helper function to send FlushContext command.
    ///
    /// # Arguments
    /// * `flush_handle` - The handle to flush.
    ///
    pub fn flush_context(&mut self, flush_handle: ReservedHandle) -> Result<(), TpmCommandError> {
        use tpm20proto::protocol::FlushContextCmd;

        let mut cmd = FlushContextCmd::new(flush_handle);

        self.tpm_engine
            .execute_command(cmd.as_mut_bytes(), &mut self.reply_buffer)
            .map_err(TpmCommandError::TpmExecuteCommand)?;

        match FlushContextCmd::base_validate_reply(&self.reply_buffer, cmd.header.session_tag) {
            Err(error) => Err(TpmCommandError::InvalidResponse(error))?,
            Ok((res, false)) => Err(TpmCommandError::TpmCommandFailed {
                response_code: res.header.response_code.get(),
            })?,
            Ok((_res, true)) => Ok(()),
        }
    }

    /// Helper function to send EvictControl command.
    ///
    /// # Arguments
    /// * `auth_handle`: The authorization handle used in the command.
    /// * `object_handle` - Transient object handle.
    /// * `persistent_handle` - Handle for persisted object.
    ///
    pub fn evict_control(
        &mut self,
        auth_handle: ReservedHandle,
        object_handle: ReservedHandle,
        persistent_handle: ReservedHandle,
    ) -> Result<(), TpmCommandError> {
        use tpm20proto::protocol::EvictControlCmd;

        let session_tag = SessionTagEnum::Sessions;
        let mut cmd = EvictControlCmd::new(
            session_tag.into(),
            auth_handle,
            object_handle,
            CmdAuth::new(TPM20_RS_PW, 0, 0, 0),
            persistent_handle,
        );

        self.tpm_engine
            .execute_command(cmd.as_mut_bytes(), &mut self.reply_buffer)
            .map_err(TpmCommandError::TpmExecuteCommand)?;

        match EvictControlCmd::base_validate_reply(&self.reply_buffer, session_tag) {
            Err(error) => Err(TpmCommandError::InvalidResponse(error))?,
            Ok((res, false)) => Err(TpmCommandError::TpmCommandFailed {
                response_code: res.header.response_code.get(),
            })?,
            Ok((_res, true)) => Ok(()),
        }
    }

    /// Helper function to send NV_ReadPublic command.
    ///
    /// # Arguments
    /// * `nv_index` - The NV index to read.
    ///
    /// Returns Ok(NvReadPublicReply) if the command succeeds. Returns
    /// Err(TpmCommandError) otherwise.
    pub fn nv_read_public(&mut self, nv_index: u32) -> Result<NvReadPublicReply, TpmCommandError> {
        use tpm20proto::protocol::NvReadPublicCmd;

        let session_tag = SessionTagEnum::NoSessions;
        let mut cmd = NvReadPublicCmd::new(session_tag.into(), nv_index);

        self.tpm_engine
            .execute_command(cmd.as_mut_bytes(), &mut self.reply_buffer)
            .map_err(TpmCommandError::TpmExecuteCommand)?;

        match NvReadPublicCmd::base_validate_reply(&self.reply_buffer, session_tag) {
            Err(error) => Err(TpmCommandError::InvalidResponse(error))?,
            Ok((res, false)) => Err(TpmCommandError::TpmCommandFailed {
                response_code: res.header.response_code.get(),
            })?,
            Ok((res, true)) => Ok(res),
        }
    }

    /// Helper function to send NV_UndefineSpace command.
    ///
    /// # Arguments
    /// * `auth_handle`: The authorization handle used in the command.
    /// * `nv_index` - The NV Index to undefine.
    ///
    pub fn nv_undefine_space(
        &mut self,
        auth_handle: ReservedHandle,
        nv_index: u32,
    ) -> Result<(), TpmCommandError> {
        use tpm20proto::protocol::NvUndefineSpaceCmd;

        let session_tag = SessionTagEnum::Sessions;
        let mut cmd = NvUndefineSpaceCmd::new(
            session_tag.into(),
            auth_handle,
            CmdAuth::new(TPM20_RS_PW, 0, 0, 0),
            nv_index,
        );

        self.tpm_engine
            .execute_command(cmd.as_mut_bytes(), &mut self.reply_buffer)
            .map_err(TpmCommandError::TpmExecuteCommand)?;

        match NvUndefineSpaceCmd::base_validate_reply(&self.reply_buffer, session_tag) {
            Err(error) => Err(TpmCommandError::InvalidResponse(error))?,
            Ok((res, false)) => Err(TpmCommandError::TpmCommandFailed {
                response_code: res.header.response_code.get(),
            })?,
            Ok((_res, true)) => Ok(()),
        }
    }

    /// Helper function to send NV_DefineSpace command, which defines the attributes
    /// of an NV Index and causes the TPM to reserve space to hold the data associated
    /// with the index.
    ///
    /// # Arguments
    /// * `auth_handle`: The authorization handle used in the command.
    /// * `auth_value` - The password associated with the allocated NV index.
    /// * `nv_index` - The NV index to allocate.
    /// * `nv_index_size` - Size of NV index to allocate.
    ///
    pub fn nv_define_space(
        &mut self,
        auth_handle: ReservedHandle,
        auth_value: u64,
        nv_index: u32,
        nv_index_size: u16,
    ) -> Result<(), TpmCommandError> {
        use tpm20proto::protocol::NvDefineSpaceCmd;

        let session_tag = SessionTagEnum::Sessions;

        // Use password-based authorization and allow owner to read
        let attributes = if auth_handle == TPM20_RH_PLATFORM {
            platform_akcert_attributes()
        } else {
            TpmaNvBits::new()
                .with_nv_ownerread(true)
                .with_nv_ownerwrite(true)
                .with_nv_authread(true)
                .with_nv_authwrite(true)
        };

        let public_info = TpmsNvPublic::new(
            nv_index,
            AlgIdEnum::SHA256.into(),
            attributes,
            &[],
            nv_index_size,
        )
        .map_err(TpmCommandError::InvalidInputParameter)?;

        let cmd = NvDefineSpaceCmd::new(
            session_tag.into(),
            auth_handle,
            CmdAuth::new(TPM20_RS_PW, 0, 0, 0),
            auth_value,
            public_info,
        )
        .map_err(TpmCommandError::TpmCommandCreationFailed)?;

        self.tpm_engine
            .execute_command(&mut cmd.serialize(), &mut self.reply_buffer)
            .map_err(TpmCommandError::TpmExecuteCommand)?;

        match NvDefineSpaceCmd::base_validate_reply(&self.reply_buffer, session_tag) {
            Err(error) => Err(TpmCommandError::InvalidResponse(error))?,
            Ok((res, false)) => Err(TpmCommandError::TpmCommandFailed {
                response_code: res.header.response_code.get(),
            })?,
            Ok((_res, true)) => Ok(()),
        }
    }

    /// Helper function to send CreatePrimary command.
    ///
    /// # Arguments
    /// * `auth_handle`: The authorization handle used in the command.
    /// * `in_public` - The public template used to create the primary.
    ///
    pub fn create_primary(
        &mut self,
        auth_handle: ReservedHandle,
        in_public: TpmtPublic,
    ) -> Result<CreatePrimaryReply, TpmCommandError> {
        use tpm20proto::protocol::CreatePrimaryCmd;

        let session_tag = SessionTagEnum::Sessions;
        let cmd = CreatePrimaryCmd::new(
            session_tag.into(),
            auth_handle,
            CmdAuth::new(TPM20_RS_PW, 0, 0, 0),
            &[],
            &[],
            in_public,
            &[],
            &[],
        )
        .map_err(TpmCommandError::TpmCommandCreationFailed)?;

        self.tpm_engine
            .execute_command(&mut cmd.serialize(), &mut self.reply_buffer)
            .map_err(TpmCommandError::TpmExecuteCommand)?;

        match CreatePrimaryCmd::base_validate_reply(&self.reply_buffer, session_tag) {
            Err(error) => Err(TpmCommandError::InvalidResponse(error))?,
            Ok((res, false)) => Err(TpmCommandError::TpmCommandFailed {
                response_code: res.header.response_code.get(),
            })?,
            Ok((res, true)) => Ok(res),
        }
    }

    /// Helper function to send NV_Write command.
    ///
    /// # Arguments
    /// * `auth_handle`: The authorization handle used in the command.
    /// * `auth_value` - The optional password associated with the NV index.
    /// * `nv_index` - The NV index to write.
    /// * `data` - The data to be written to the NV index.
    ///
    pub fn nv_write(
        &mut self,
        auth_handle: ReservedHandle,
        auth_value: Option<u64>,
        nv_index: u32,
        data: &[u8],
    ) -> Result<(), TpmCommandError> {
        use tpm20proto::protocol::NvWriteCmd;

        tracing::info!(
            CVM_ALLOWED,
            op_type = ?LogOpType::BeginNvWrite,
            nv_index,
            data_size = data.len(),
            "Writing TPM NVRAM index"
        );

        let session_tag = SessionTagEnum::Sessions;

        let mut cmd = if let Some(auth_value) = auth_value {
            // Password-based authorization (the NV index was created at boot-time)
            NvWriteCmd::new(
                session_tag.into(),
                auth_handle,
                CmdAuth::new(TPM20_RS_PW, 0, 0, size_of_val(&auth_value) as u16),
                auth_value,
                nv_index,
                &[],
                0,
            )
        } else {
            // Owner write (the NV index was pre-provisioned)
            NvWriteCmd::new(
                session_tag.into(),
                auth_handle,
                CmdAuth::new(TPM20_RS_PW, 0, 0, 0),
                0,
                nv_index,
                &[],
                0,
            )
        }
        .map_err(TpmCommandError::TpmCommandCreationFailed)?;

        let mut transferred_bytes = 0;
        while transferred_bytes < data.len() {
            let bytes_remaining = data.len() - transferred_bytes;
            let bytes_to_transfer = std::cmp::min(bytes_remaining, MAX_NV_BUFFER_SIZE);
            let data_to_transfer = &data[transferred_bytes..transferred_bytes + bytes_to_transfer];

            cmd.update_write_data(data_to_transfer, transferred_bytes as u16)
                .map_err(TpmCommandError::InvalidInputParameter)?;

            self.tpm_engine
                .execute_command(&mut cmd.serialize(), &mut self.reply_buffer)
                .map_err(TpmCommandError::TpmExecuteCommand)?;

            match NvWriteCmd::base_validate_reply(&self.reply_buffer, session_tag) {
                Err(error) => Err(TpmCommandError::InvalidResponse(error))?,
                Ok((res, false)) => Err(TpmCommandError::TpmCommandFailed {
                    response_code: res.header.response_code.get(),
                })?,
                Ok((_res, true)) => {}
            }

            transferred_bytes += bytes_to_transfer;
        }

        Ok(())
    }

    /// Helper function to send NV_Read command.
    ///
    /// # Arguments
    /// * `auth_handle`: The authorization handle used in the command.
    /// * `nv_index` - The NV index to read.
    /// * `nv_index_size` - Size of NV index.
    /// * `data` - The output buffer to hold the data read from the NV index.
    ///
    pub fn nv_read(
        &mut self,
        auth_handle: ReservedHandle,
        nv_index: u32,
        nv_index_size: u16,
        data: &mut [u8],
    ) -> Result<(), TpmCommandError> {
        use tpm20proto::protocol::NvReadCmd;

        let session_tag = SessionTagEnum::Sessions;
        let mut nv_read = NvReadCmd::new(
            session_tag.into(),
            auth_handle,
            nv_index,
            CmdAuth::new(TPM20_RS_PW, 0, 0, 0),
            0,
            0,
        );

        let mut transferred_bytes = 0;
        let total_bytes = std::cmp::min(nv_index_size, data.len() as u16);

        while transferred_bytes < total_bytes {
            let bytes_remaining = total_bytes - transferred_bytes;
            let bytes_to_transfer = std::cmp::min(bytes_remaining, MAX_NV_BUFFER_SIZE as u16);

            nv_read.update_read_parameters(bytes_to_transfer, transferred_bytes);

            self.tpm_engine
                .execute_command(nv_read.as_mut_bytes(), &mut self.reply_buffer)
                .map_err(TpmCommandError::TpmExecuteCommand)?;

            let res = match NvReadCmd::base_validate_reply(&self.reply_buffer, session_tag) {
                Err(error) => Err(TpmCommandError::InvalidResponse(error))?,
                Ok((res, false)) => Err(TpmCommandError::TpmCommandFailed {
                    response_code: res.header.response_code.get(),
                })?,
                Ok((res, true)) => res,
            };

            data[transferred_bytes as usize..(transferred_bytes + bytes_to_transfer) as usize]
                .copy_from_slice(&res.data.buffer[..bytes_to_transfer as usize]);
            transferred_bytes += bytes_to_transfer;
        }

        Ok(())
    }

    /// Helper function to send Import command.
    ///
    /// # Arguments
    /// * `auth_handle`: The authorization handle used in the command.
    /// * `object_public` - The public part of the key to be imported.
    /// * `duplicate` - The private part of the key to be imported.
    /// * `in_sym_seed` - The value associated with `duplicate`.
    ///
    pub(crate) fn import(
        &mut self,
        auth_handle: ReservedHandle,
        object_public: &Tpm2bPublic,
        duplicate: &Tpm2bBuffer,
        in_sym_seed: &Tpm2bBuffer,
    ) -> Result<ImportReply, TpmCommandError> {
        use tpm20proto::protocol::ImportCmd;

        // Assuming there is no inner wrapper
        let encryption_key = Tpm2bBuffer::new_zeroed();
        let symmetric_alg = TpmtSymDefObject::new(AlgIdEnum::NULL.into(), None, None);

        let session_tag = SessionTagEnum::Sessions;
        let cmd = ImportCmd::new(
            session_tag.into(),
            auth_handle,
            CmdAuth::new(TPM20_RS_PW, 0, 0, 0),
            &encryption_key,
            object_public,
            duplicate,
            in_sym_seed,
            &symmetric_alg,
        );

        self.tpm_engine
            .execute_command(&mut cmd.serialize(), &mut self.reply_buffer)
            .map_err(TpmCommandError::TpmExecuteCommand)?;

        match ImportCmd::base_validate_reply(&self.reply_buffer, session_tag) {
            Err(error) => Err(TpmCommandError::InvalidResponse(error))?,
            Ok((res, false)) => Err(TpmCommandError::TpmCommandFailed {
                response_code: res.header.response_code.get(),
            })?,
            Ok((res, true)) => Ok(res),
        }
    }

    /// Helper function to send Load command.
    ///
    /// # Arguments
    /// * `auth_handle`: The authorization handle used in the command.
    /// * `in_private` - The private part of the key to be loaded.
    /// * `in_public` - The public part of the key to be loaded.
    ///
    pub(crate) fn load(
        &mut self,
        auth_handle: ReservedHandle,
        in_private: &Tpm2bBuffer,
        in_public: &Tpm2bPublic,
    ) -> Result<LoadReply, TpmCommandError> {
        use tpm20proto::protocol::LoadCmd;

        let session_tag = SessionTagEnum::Sessions;
        let cmd = LoadCmd::new(
            session_tag.into(),
            auth_handle,
            CmdAuth::new(TPM20_RS_PW, 0, 0, 0),
            in_private,
            in_public,
        );

        self.tpm_engine
            .execute_command(&mut cmd.serialize(), &mut self.reply_buffer)
            .map_err(TpmCommandError::TpmExecuteCommand)?;

        match LoadCmd::base_validate_reply(&self.reply_buffer, session_tag) {
            Err(error) => Err(TpmCommandError::InvalidResponse(error))?,
            Ok((res, false)) => Err(TpmCommandError::TpmCommandFailed {
                response_code: res.header.response_code.get(),
            })?,
            Ok((res, true)) => Ok(res),
        }
    }
}

/// Returns the public template for AK.
pub fn ak_pub_template() -> Result<TpmtPublic, TpmHelperUtilityError> {
    let symmetric = TpmtSymDefObject::new(AlgIdEnum::NULL.into(), None, None);
    let scheme = TpmtRsaScheme::new(AlgIdEnum::RSASSA.into(), Some(AlgIdEnum::SHA256.into()));
    let rsa_params = TpmsRsaParams::new(symmetric, scheme, RSA_2K_MODULUS_BITS, 0);

    let object_attributes = TpmaObjectBits::new()
        .with_fixed_tpm(true)
        .with_fixed_parent(true)
        .with_sensitive_data_origin(true)
        .with_user_with_auth(true)
        .with_no_da(true)
        .with_restricted(true)
        .with_sign_encrypt(true);

    let in_public = TpmtPublic::new(
        AlgIdEnum::RSA.into(),
        AlgIdEnum::SHA256.into(),
        object_attributes,
        &[],
        rsa_params,
        &[0u8; RSA_2K_MODULUS_SIZE],
    )
    .map_err(TpmHelperUtilityError::InvalidInputParameter)?;

    Ok(in_public)
}

/// Returns the public template for the EK.
pub fn ek_pub_template() -> Result<TpmtPublic, TpmHelperUtilityError> {
    // Create Windows-style EK.
    // The following parameters are based on low-range RSA 2048 EK Template.
    // See B 3.3 & 6.2, "TCG EK Credential Profile", version 2.5.
    const AUTH_POLICY_A_SHA_256: [u8; 32] = [
        0x83, 0x71, 0x97, 0x67, 0x44, 0x84, 0xB3, 0xF8, 0x1A, 0x90, 0xCC, 0x8D, 0x46, 0xA5, 0xD7,
        0x24, 0xFD, 0x52, 0xD7, 0x6E, 0x06, 0x52, 0x0B, 0x64, 0xF2, 0xA1, 0xDA, 0x1B, 0x33, 0x14,
        0x69, 0xAA,
    ];
    let symmetric = TpmtSymDefObject::new(
        AlgIdEnum::AES.into(),
        Some(128),
        Some(AlgIdEnum::CFB.into()),
    );
    let scheme = TpmtRsaScheme::new(AlgIdEnum::NULL.into(), None);
    let rsa_params = TpmsRsaParams::new(symmetric, scheme, RSA_2K_MODULUS_BITS, 0);

    let object_attributes = TpmaObjectBits::new()
        .with_fixed_tpm(true)
        .with_fixed_parent(true)
        .with_sensitive_data_origin(true)
        .with_admin_with_policy(true)
        .with_restricted(true)
        .with_decrypt(true);

    let in_public = TpmtPublic::new(
        AlgIdEnum::RSA.into(),
        AlgIdEnum::SHA256.into(),
        object_attributes,
        &AUTH_POLICY_A_SHA_256,
        rsa_params,
        &[0u8; RSA_2K_MODULUS_SIZE],
    )
    .map_err(TpmHelperUtilityError::InvalidInputParameter)?;

    Ok(in_public)
}

/// Helper function for converting `Tpm2bPublic` to `TpmRsa2kPublic`.
fn export_rsa_public(public: &Tpm2bPublic) -> Result<TpmRsa2kPublic, TpmHelperUtilityError> {
    if public.public_area.parameters.exponent.get() != 0 {
        Err(TpmHelperUtilityError::UnexpectedRsaExponent)?
    }

    // Use the default value (2^16 + 1) when exponent is 0.
    // See Table 186, Section 12.2.3.5, "Trusted Platform Module Library Part 2: Structures", revision 1.38.
    const DEFAULT_EXPONENT: [u8; RSA_2K_EXPONENT_SIZE] = [0x01, 0x00, 0x01];
    let mut modulus = [0u8; RSA_2K_MODULUS_SIZE];
    let output = public.public_area.unique.serialize();
    let buffer_offset = size_of_val(&public.public_area.unique.size);

    if output.len() != buffer_offset + RSA_2K_MODULUS_SIZE {
        Err(TpmHelperUtilityError::UnexpectedRsaModulusSize)?
    }

    modulus.copy_from_slice(&output[buffer_offset..buffer_offset + RSA_2K_MODULUS_SIZE]);

    Ok(TpmRsa2kPublic {
        exponent: DEFAULT_EXPONENT,
        modulus,
    })
}
