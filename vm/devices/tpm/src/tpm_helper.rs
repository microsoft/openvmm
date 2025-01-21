// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! The module includes the helper functions for sending TPM commands.

use crate::tpm20proto;
use crate::tpm20proto::protocol::common::CmdAuth;
use crate::tpm20proto::protocol::CreatePrimaryReply;
use crate::tpm20proto::protocol::ImportReply;
use crate::tpm20proto::protocol::LoadReply;
use crate::tpm20proto::protocol::NvReadPublicReply;
use crate::tpm20proto::protocol::PcrSelection;
use crate::tpm20proto::protocol::ReadPublicReply;
use crate::tpm20proto::protocol::StartupType;
use crate::tpm20proto::protocol::Tpm2bBuffer;
use crate::tpm20proto::protocol::Tpm2bPublic;
use crate::tpm20proto::protocol::TpmCommand;
use crate::tpm20proto::protocol::TpmsNvPublic;
use crate::tpm20proto::protocol::TpmsRsaParams;
use crate::tpm20proto::protocol::TpmtPublic;
use crate::tpm20proto::protocol::TpmtRsaScheme;
use crate::tpm20proto::protocol::TpmtSymDefObject;
use crate::tpm20proto::AlgIdEnum;
use crate::tpm20proto::CommandCodeEnum;
use crate::tpm20proto::ReservedHandle;
use crate::tpm20proto::ResponseCode;
use crate::tpm20proto::ResponseValidationError;
use crate::tpm20proto::SessionTagEnum;
use crate::tpm20proto::TpmProtoError;
use crate::tpm20proto::TpmaNvBits;
use crate::tpm20proto::TpmaObjectBits;
use crate::tpm20proto::MAX_DIGEST_BUFFER_SIZE;
use crate::tpm20proto::TPM20_RH_ENDORSEMENT;
use crate::tpm20proto::TPM20_RH_OWNER;
use crate::tpm20proto::TPM20_RH_PLATFORM;
use crate::tpm20proto::TPM20_RS_PW;
use crate::TpmRsa2kPublic;
use crate::TPM_AZURE_AIK_HANDLE;
use crate::TPM_GUEST_SECRET_HANDLE;
use crate::TPM_NV_INDEX_AIK_CERT;
use crate::TPM_NV_INDEX_ATTESTATION_REPORT;
use crate::TPM_RSA_SRK_HANDLE;
use inspect::InspectMut;
use ms_tpm_20_ref::MsTpm20RefPlatform;
use thiserror::Error;
use zerocopy::FromZeros;
use zerocopy::IntoBytes;

// The size of command and response buffers.
// DEVNOTE: The specification only requires the size to be large
// enough for the command and response fit into the buffer. We
// would need to scale this value up in case it is not sufficient.
const TPM_PAGE_SIZE: usize = 4096;
const MAX_NV_BUFFER_SIZE: usize = MAX_DIGEST_BUFFER_SIZE;
const MAX_NV_INDEX_SIZE: u16 = 4096;
// Scale this with maximum attestation payload
const MAX_ATTESTATION_INDEX_SIZE: u16 = 2600;

const RSA_2K_MODULUS_BITS: u16 = 2048;
const RSA_2K_MODULUS_SIZE: usize = (RSA_2K_MODULUS_BITS / 8) as usize;
const RSA_2K_EXPONENT_SIZE: usize = 3;

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

#[derive(Error, Debug)]
pub enum TpmHelperError {
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
    #[error("nv index {0:#x} with invalid write permission")]
    InvalidWritePermission(u32),
    #[error("input size {input_size} to nv write exceeds the allocated size {allocated_size} of nv index {nv_index:#x}")]
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

#[derive(Error, Debug)]
pub enum TpmCommandError {
    #[error("failed to execute the TPM command")]
    TpmExecuteCommand(#[source] ms_tpm_20_ref::Error),
    #[error("invalid response from the TPM command")]
    InvalidResponse(#[source] ResponseValidationError),
    #[error("invalid input parameter for the TPM command")]
    InvalidInputParameter(#[source] TpmProtoError),
    #[error("TPM command failed, response code: {response_code:#x}")]
    TpmCommandFailed { response_code: u32 },
    #[error("failed to create the TPM command struct")]
    TpmCommandCreationFailed(#[source] TpmProtoError),
}

#[derive(Error, Debug)]
pub enum TpmHelperUtilityError {
    #[error("the RSA exponent returned by TPM is unexpected")]
    UnexpectedRsaExponent,
    #[error("the size of RSA modulus returned by TPM is unexpected")]
    UnexpectedRsaModulusSize,
    #[error("invalid input parameter")]
    InvalidInputParameter(#[source] TpmProtoError),
}

#[derive(InspectMut)]
pub struct TpmEngineHelper {
    /// An TPM engine instance.
    #[inspect(skip)]
    pub tpm_engine: MsTpm20RefPlatform,
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

impl TpmEngineHelper {
    // === Helper functions built on top of TPM commands === //

    /// Initialize the TPM instance and perform self-tests using Startup and SelfTest commands.
    /// This function should only be invoked after an TPM reset.
    pub fn initialize_tpm_engine(&mut self) -> Result<(), TpmHelperError> {
        // Set TPM to the default state.
        self.startup(StartupType::Clear)
            .map_err(|error| TpmHelperError::TpmCommandError {
                command_debug_info: CommandDebugInfo {
                    command_code: CommandCodeEnum::Startup,
                    auth_handle: None,
                    nv_index: None,
                },
                error,
            })?;

        // Perform capabilities test
        self.self_test(true)
            .map_err(|error| TpmHelperError::TpmCommandError {
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
    pub fn clear_tpm_platform_context(&mut self) -> Result<u32, TpmHelperError> {
        // Use clear control to enable the execution of clear
        if let Err(error) = self.clear_control(TPM20_RH_PLATFORM, false) {
            if let TpmCommandError::TpmCommandFailed { response_code } = error {
                tracelimit::error_ratelimited!(
                    err = &error as &dyn std::error::Error,
                    "tpm ClearControlCmd failed"
                );

                // Return the error code to be written to `last_ppi_state`
                return Ok(response_code);
            } else {
                // Unexpected failure
                return Err(TpmHelperError::TpmCommandError {
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
                        err = &error as &dyn std::error::Error,
                        "tpm ClearCmd failed"
                    );

                    // Return the error code to be written to `last_ppi_state`
                    Ok(response_code)
                } else {
                    // Unexpected failure
                    Err(TpmHelperError::TpmCommandError {
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
    pub fn refresh_tpm_seeds(&mut self) -> Result<(), TpmHelperError> {
        // Refresh endorsement primary seed (EPS)
        self.change_seed(TPM20_RH_PLATFORM, CommandCodeEnum::ChangeEPS)
            .map_err(|error| TpmHelperError::TpmCommandError {
                command_debug_info: CommandDebugInfo {
                    command_code: CommandCodeEnum::ChangeEPS,
                    auth_handle: Some(TPM20_RH_PLATFORM),
                    nv_index: None,
                },
                error,
            })?;

        // Refresh platform primary seed (PPS)
        self.change_seed(TPM20_RH_PLATFORM, CommandCodeEnum::ChangePPS)
            .map_err(|error| TpmHelperError::TpmCommandError {
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
    /// Returns the AK public in `TpmRsa2kPublic`.
    pub fn create_ak_pub(&mut self, force_create: bool) -> Result<TpmRsa2kPublic, TpmHelperError> {
        if let Some(res) = self.find_object(TPM_AZURE_AIK_HANDLE)? {
            if force_create {
                // Remove existing key before creating a new one
                self.evict_or_persist_handle(EvictOrPersist::Evict(TPM_AZURE_AIK_HANDLE))?;
            } else {
                // Use existing key
                return export_rsa_public(&res.out_public).map_err(|error| {
                    TpmHelperError::ExportRsaPublicFromAkHandle {
                        ak_handle: TPM_AZURE_AIK_HANDLE.0.get(),
                        error,
                    }
                });
            }
        }

        let in_public = ak_pub_template().map_err(TpmHelperError::CreateAkPubTemplateFailed)?;

        self.create_key_object(in_public, Some(TPM_AZURE_AIK_HANDLE))
    }

    /// Create Windows-style Endorsement key (EK) based on the template from the TPM specification. Note that
    /// this function does not persist the EK in the tpm platform. Instead, EK will be created and persisted
    /// using the same template by other software component during guest OS boot.
    ///
    /// Returns the EK public in `TpmRsa2kPublic`.
    pub fn create_ek_pub(&mut self) -> Result<TpmRsa2kPublic, TpmHelperError> {
        let in_public = ek_pub_template().map_err(TpmHelperError::CreateEkPubTemplateFailed)?;

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
    ) -> Result<TpmRsa2kPublic, TpmHelperError> {
        let res = match self.create_primary(TPM20_RH_ENDORSEMENT, in_public) {
            Err(error) => {
                if let TpmCommandError::TpmCommandFailed { response_code: _ } = error {
                    // Guest might cause the command to fail (e.g., taking the ownership of a hierarchy).
                    // Making this failure as non-fatal.
                    tracelimit::error_ratelimited!(
                        err = &error as &dyn std::error::Error,
                        "tpm CreatePrimaryCmd failed"
                    );

                    return Ok(TpmRsa2kPublic {
                        modulus: [0u8; RSA_2K_MODULUS_SIZE],
                        exponent: [0u8; RSA_2K_EXPONENT_SIZE],
                    });
                } else {
                    // Unexpected failure
                    return Err(TpmHelperError::TpmCommandError {
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
            tracelimit::error_ratelimited!("No public data in CreatePrimaryCmd response");

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
        .map_err(TpmHelperError::ExportRsaPublicFromPrimaryObject)?;

        if let Err(error) = self.flush_context(res.object_handle) {
            if let TpmCommandError::TpmCommandFailed { response_code: _ } = error {
                // Guest might cause the command to fail (e.g., taking the ownership of a hierarchy).
                // Making this failure as non-fatal.
                tracelimit::error_ratelimited!(
                    err = &error as &dyn std::error::Error,
                    "tpm FlushContextCmd failed"
                );
            } else {
                // Unexpected failure
                return Err(TpmHelperError::TpmCommandError {
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
    fn evict_or_persist_handle(&mut self, action: EvictOrPersist) -> Result<(), TpmHelperError> {
        let (object_handle, persistent_handle) = match action {
            EvictOrPersist::Evict(handle) => (handle, handle),
            EvictOrPersist::Persist { from, to } => (from, to),
        };

        if let Err(error) = self.evict_control(TPM20_RH_OWNER, object_handle, persistent_handle) {
            if let TpmCommandError::TpmCommandFailed { response_code: _ } = error {
                // Guest might cause the command to fail (e.g., taking the ownership of a hierarchy).
                // Making this failure as non-fatal.
                tracelimit::error_ratelimited!(
                    err = &error as &dyn std::error::Error,
                    "tpm EvictControlCmd failed"
                );
            } else {
                // Unexpected failure
                return Err(TpmHelperError::TpmCommandError {
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

    /// Allocate NV indices under platform hierarchy that are necessary for guest
    /// attestation.
    ///
    /// # Arguments
    /// * `auth_value`: The password used during the NV indices allocation.
    /// * `force_create_ak_cert`: Whether to remove the existing AK and re-create one.
    /// * `support_attestation_report`: Whether to allocate NV index for attestation report.
    ///
    pub fn allocate_guest_attestation_nv_indices(
        &mut self,
        auth_value: u64,
        force_create_ak_cert: bool,
        support_attestation_report: bool,
    ) -> Result<(), TpmHelperError> {
        let ak_cert_nv_index_present =
            if let Some(res) = self.find_nv_index(TPM_NV_INDEX_AIK_CERT)? {
                let nv_bits = TpmaNvBits::from(res.nv_public.nv_public.attributes.0.get());
                let platform_created = nv_bits.nv_platformcreate();

                // Remove previous `TPM_NV_INDEX_AIK_CERT` allocation if `force_create_ak_cert`
                // is true or the nv index is platform created. The latter condition means that
                // the nv index is created by previous boot (not pre-provisioned). Clearing the
                // nv index ensures that we will recreate the nv index with newly-created auth_value
                // (which does not persist across boots) for each boot.
                if force_create_ak_cert || platform_created {
                    self.nv_undefine_space(TPM20_RH_PLATFORM, TPM_NV_INDEX_AIK_CERT)
                        .map_err(|error| TpmHelperError::TpmCommandError {
                            command_debug_info: CommandDebugInfo {
                                command_code: CommandCodeEnum::NV_UndefineSpace,
                                auth_handle: Some(TPM20_RH_PLATFORM),
                                nv_index: Some(TPM_NV_INDEX_AIK_CERT),
                            },
                            error,
                        })?;

                    // NV index has been undefined
                    false
                } else {
                    // NV index already exists
                    true
                }
            } else {
                // NV index does not exist
                false
            };

        // Only allocate the `TPM_NV_INDEX_AIK_CERT` if the nv index is not present
        if !ak_cert_nv_index_present {
            tracing::info!("Allocate nv index {:x} for AK cert", TPM_NV_INDEX_AIK_CERT);

            self.nv_define_space(
                TPM20_RH_PLATFORM,
                auth_value,
                TPM_NV_INDEX_AIK_CERT,
                MAX_NV_INDEX_SIZE,
            )
            .map_err(|error| TpmHelperError::TpmCommandError {
                command_debug_info: CommandDebugInfo {
                    command_code: CommandCodeEnum::NV_DefineSpace,
                    auth_handle: Some(TPM20_RH_PLATFORM),
                    nv_index: Some(TPM_NV_INDEX_AIK_CERT),
                },
                error,
            })?;
        }

        // Allocate `TPM_NV_INDEX_ATTESTATION_REPORT` if `support_attestation_report` is true
        if support_attestation_report {
            // Attempt to remove previous `TPM_NV_INDEX_ATTESTATION_REPORT` allocation before the allocation
            if self
                .find_nv_index(TPM_NV_INDEX_ATTESTATION_REPORT)?
                .is_some()
            {
                self.nv_undefine_space(TPM20_RH_PLATFORM, TPM_NV_INDEX_ATTESTATION_REPORT)
                    .map_err(|error| TpmHelperError::TpmCommandError {
                        command_debug_info: CommandDebugInfo {
                            command_code: CommandCodeEnum::NV_UndefineSpace,
                            auth_handle: Some(TPM20_RH_PLATFORM),
                            nv_index: Some(TPM_NV_INDEX_ATTESTATION_REPORT),
                        },
                        error,
                    })?;
            }

            tracing::info!(
                "Allocate nv index {:x} for attestation report",
                TPM_NV_INDEX_ATTESTATION_REPORT
            );

            self.nv_define_space(
                TPM20_RH_PLATFORM,
                auth_value,
                TPM_NV_INDEX_ATTESTATION_REPORT,
                MAX_ATTESTATION_INDEX_SIZE,
            )
            .map_err(|error| TpmHelperError::TpmCommandError {
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

    /// Check if the nv index is present using NV_ReadPublic command.
    ///
    /// Returns Ok(Some(NvReadPublicReply)) if nv index is present.
    /// Returns Ok(None) if nv index is not present.
    fn find_nv_index(
        &mut self,
        nv_index: u32,
    ) -> Result<Option<NvReadPublicReply>, TpmHelperError> {
        match self.nv_read_public(nv_index) {
            Err(error) => {
                if let TpmCommandError::TpmCommandFailed { response_code } = error {
                    if response_code == (ResponseCode::Handle as u32 | ResponseCode::Rc1 as u32) {
                        // nv index not found
                        Ok(None)
                    } else {
                        // Unexpected response code
                        Err(TpmHelperError::TpmCommandError {
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
                    Err(TpmHelperError::TpmCommandError {
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

    /// Write data to a NV index that can be either owner- or platform-created.
    /// If the data size is less than the size of the index, the function applies
    /// zero padding and ensure the entire NV space is filled.
    ///
    /// # Arguments
    /// * `auth_value` - The authorization value for the platform-created index.
    /// * `nv_index` - The target NV index.
    /// * `data` - The data to write.
    ///
    pub fn write_to_nv_index(
        &mut self,
        auth_value: u64,
        nv_index: u32,
        data: &[u8],
    ) -> Result<(), TpmHelperError> {
        let res =
            self.nv_read_public(nv_index)
                .map_err(|error| TpmHelperError::TpmCommandError {
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
            std::cmp::Ordering::Greater => Err(TpmHelperError::NvWriteInputTooLarge {
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

        // Determine the authorization type based on the nv bits.
        // ownerwrite and authwrite might both be set, let ownerwrite take precedence.
        if nv_bits.nv_ownerwrite() {
            // Owner write (the NV index was pre-provisioned)
            self.nv_write(TPM20_RH_OWNER, None, nv_index, &data)
                .map_err(|error| TpmHelperError::TpmCommandError {
                    command_debug_info: CommandDebugInfo {
                        command_code: CommandCodeEnum::NV_Write,
                        auth_handle: Some(TPM20_RH_OWNER),
                        nv_index: Some(nv_index),
                    },
                    error,
                })?;
        } else if nv_bits.nv_authwrite() {
            // Password-based authorization (the NV index was created at boot-time)
            self.nv_write(
                ReservedHandle(nv_index.into()),
                Some(auth_value),
                nv_index,
                &data,
            )
            .map_err(|error| TpmHelperError::TpmCommandError {
                command_debug_info: CommandDebugInfo {
                    command_code: CommandCodeEnum::NV_Write,
                    auth_handle: Some(ReservedHandle(nv_index.into())),
                    nv_index: Some(nv_index),
                },
                error,
            })?;
        } else {
            // Unexpected scenario
            Err(TpmHelperError::InvalidWritePermission(nv_index))?
        };

        Ok(())
    }

    /// Read data from a owner-defined NV Index if the index is present.
    ///
    /// # Arguments
    /// * `nv_index` - The target NV index.
    /// * `data` - The data to write.
    ///
    /// Returns Ok(true) if the index is present and read succeeds.
    /// Returns Ok(false) if the index is not present.
    pub fn read_from_nv_index(
        &mut self,
        nv_index: u32,
        data: &mut [u8],
    ) -> Result<bool, TpmHelperError> {
        let Some(res) = self.find_nv_index(nv_index)? else {
            // nv index may not exist before guest makes a request
            return Ok(false);
        };

        let nv_bits = TpmaNvBits::from(res.nv_public.nv_public.attributes.0.get());
        if !nv_bits.nv_ownerread() {
            Err(TpmHelperError::NoOwnerReadFlag(nv_index))?
        }

        let nv_index_size = res.nv_public.nv_public.data_size.get();
        self.nv_read(TPM20_RH_OWNER, nv_index, nv_index_size, data)
            .map_err(|error| TpmHelperError::TpmCommandError {
                command_debug_info: CommandDebugInfo {
                    command_code: CommandCodeEnum::NV_Read,
                    auth_handle: Some(TPM20_RH_OWNER),
                    nv_index: Some(nv_index),
                },
                error,
            })?;

        Ok(true)
    }

    /// Check if the object is present using ReadPublic command.
    ///
    /// Returns Ok(Some(ReadPublicReply)) if the object is present.
    /// Returns Ok(None) if nv index is not present.
    fn find_object(
        &mut self,
        object_handle: ReservedHandle,
    ) -> Result<Option<ReadPublicReply>, TpmHelperError> {
        match self.read_public(object_handle) {
            Err(error) => {
                if let TpmCommandError::TpmCommandFailed { response_code } = error {
                    if response_code == (ResponseCode::Handle as u32 | ResponseCode::Rc1 as u32) {
                        // nv index not found
                        Ok(None)
                    } else {
                        // Unexpected response code
                        Err(TpmHelperError::TpmCommandError {
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
                    Err(TpmHelperError::TpmCommandError {
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
    ///    The format of the data blob is expected to be:
    ///    (TPM2B_PUBLIC || TPM2B_PRIVATE || TPM2B_ENCRYPTED_SECRET)
    ///
    pub fn initialize_guest_secret_key(
        &mut self,
        guest_secret_key: &[u8],
    ) -> Result<(), TpmHelperError> {
        use crate::tpm20proto::protocol::ImportCmd;

        if self.find_object(TPM_GUEST_SECRET_HANDLE)?.is_some() {
            // ECC key found, early return.
            return Ok(());
        };

        if self.find_object(TPM_RSA_SRK_HANDLE)?.is_none() {
            // SRK not found, return an error.
            return Err(TpmHelperError::SrkNotFound(TPM_RSA_SRK_HANDLE.0.get()));
        };

        // Deserialize the guest secret key data blob
        let import_command = ImportCmd::deserialize_no_wrapping_key(guest_secret_key)
            .ok_or(TpmHelperError::DeserializeGuestSecretKey)?;

        // Import the key under `TPM_RSA_SRK_HANDLE`
        let import_reply = self
            .import(
                TPM_RSA_SRK_HANDLE,
                &import_command.object_public,
                &import_command.duplicate,
                &import_command.in_sym_seed,
            )
            .map_err(|error| TpmHelperError::TpmCommandError {
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
            .map_err(|error| TpmHelperError::TpmCommandError {
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

    /// Helper function to send HierarchyControl command.
    ///
    /// # Arguments
    /// * `auth_handle`: The authorization handle used in the command.
    /// * `hierarchy`: The hierarchy to control.
    /// * `state`: Enable the target hierarchy or not.
    ///
    pub fn hierarchy_control(
        &mut self,
        auth_handle: ReservedHandle,
        hierarchy: ReservedHandle,
        state: bool,
    ) -> Result<(), TpmCommandError> {
        use tpm20proto::protocol::HierarchyControlCmd;

        let session_tag = SessionTagEnum::Sessions;
        let mut cmd = HierarchyControlCmd::new(
            session_tag.into(),
            auth_handle,
            CmdAuth::new(TPM20_RS_PW, 0, 0, 0),
            hierarchy,
            state,
        );

        self.tpm_engine
            .execute_command(cmd.as_mut_bytes(), &mut self.reply_buffer)
            .map_err(TpmCommandError::TpmExecuteCommand)?;

        match HierarchyControlCmd::base_validate_reply(&self.reply_buffer, session_tag) {
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
        use crate::tpm20proto::protocol::ChangeSeedCmd;

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
        let attributes = TpmaNvBits::new()
            .with_nv_authread(true)
            .with_nv_authwrite(true)
            .with_nv_ownerread(true)
            .with_nv_platformcreate(true)
            .with_nv_no_da(true);

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
    fn import(
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
    fn load(
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
    let rsa_params = TpmsRsaParams::new(symmetric, scheme, crate::RSA_2K_MODULUS_BITS, 0);

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
        &[0u8; crate::RSA_2K_MODULUS_SIZE],
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
    let rsa_params = TpmsRsaParams::new(symmetric, scheme, crate::RSA_2K_MODULUS_BITS, 0);

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
        &[0u8; crate::RSA_2K_MODULUS_SIZE],
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tpm20proto::ResponseCode;
    use crate::tpm20proto::TPM20_HT_PERSISTENT;
    use crate::tpm20proto::TPM20_RH_ENDORSEMENT;
    use crate::tpm20proto::TPM20_RH_OWNER;
    use crate::tpm20proto::TPM20_RH_PLATFORM;
    use crate::TPM_AZURE_AIK_HANDLE;
    use crate::TPM_NV_INDEX_AIK_CERT;
    use crate::TPM_NV_INDEX_ATTESTATION_REPORT;
    use ms_tpm_20_ref::DynResult;
    use std::time::Instant;
    use tpm20proto::AlgId;

    const TPM_AZURE_EK_HANDLE: ReservedHandle = ReservedHandle::new(TPM20_HT_PERSISTENT, 0x010001);
    const AUTH_VALUE: u64 = 0x7766554433221100;

    /// Sample platform callback implementation for testing purposes.
    struct TestPlatformCallbacks {
        blob: Vec<u8>,
        time: Instant,
    }

    impl ms_tpm_20_ref::PlatformCallbacks for TestPlatformCallbacks {
        fn commit_nv_state(&mut self, state: &[u8]) -> DynResult<()> {
            self.blob = state.to_vec();

            Ok(())
        }

        fn get_crypt_random(&mut self, buf: &mut [u8]) -> DynResult<usize> {
            getrandom::getrandom(buf).expect("rng failure");

            Ok(buf.len())
        }

        fn monotonic_timer(&mut self) -> std::time::Duration {
            self.time.elapsed()
        }

        fn get_unique_value(&self) -> &'static [u8] {
            b"vtpm test"
        }
    }

    fn create_tpm_engine_helper() -> TpmEngineHelper {
        let result = MsTpm20RefPlatform::initialize(
            Box::new(TestPlatformCallbacks {
                blob: vec![],
                time: Instant::now(),
            }),
            ms_tpm_20_ref::InitKind::ColdInit,
        );
        assert!(result.is_ok());

        let tpm_engine = result.unwrap();

        TpmEngineHelper {
            tpm_engine,
            reply_buffer: [0u8; 4096],
        }
    }

    fn restart_tpm_engine(
        tpm_engine_helper: &mut TpmEngineHelper,
        clear_context: bool,
        initialize: bool,
    ) {
        if clear_context {
            let result = tpm_engine_helper.clear_tpm_platform_context();
            assert!(result.is_ok());
        }

        let result = tpm_engine_helper.tpm_engine.reset(None);
        assert!(result.is_ok());

        if initialize {
            let result = tpm_engine_helper.initialize_tpm_engine();
            assert!(result.is_ok());
        }
    }

    #[test]
    fn test_create_ak_ek_pub() {
        let mut tpm_engine_helper = create_tpm_engine_helper();
        restart_tpm_engine(&mut tpm_engine_helper, false, true);

        // Test creating AK and EK

        // Ensure nothing present
        assert!(tpm_engine_helper
            .find_object(TPM_AZURE_AIK_HANDLE)
            .unwrap()
            .is_none());
        assert!(tpm_engine_helper
            .find_object(TPM_AZURE_EK_HANDLE)
            .unwrap()
            .is_none());

        let (ak_pub_first, ek_pub_first) = create_ak_ek_pub(&mut tpm_engine_helper);

        // Test creating AK and EK with clearing context

        restart_tpm_engine(&mut tpm_engine_helper, true, true);

        // Ensure nothing present after context is cleared and tpm reset
        assert!(tpm_engine_helper
            .find_object(TPM_AZURE_AIK_HANDLE)
            .unwrap()
            .is_none());
        assert!(tpm_engine_helper
            .find_object(TPM_AZURE_EK_HANDLE)
            .unwrap()
            .is_none());

        let (ak_pub_second, ek_pub_second) = create_ak_ek_pub(&mut tpm_engine_helper);

        // Ensure AK and EK match across reset if seeds do not change
        assert_eq!(ak_pub_first, ak_pub_second);
        assert_eq!(ek_pub_first, ek_pub_second);

        // Test creating AK and EK without clearing context and force_create = false

        restart_tpm_engine(&mut tpm_engine_helper, false, true);

        // Ensure that AK is persisted across reset without clearing context
        assert!(tpm_engine_helper
            .find_object(TPM_AZURE_AIK_HANDLE)
            .unwrap()
            .is_some());
        assert!(tpm_engine_helper
            .find_object(TPM_AZURE_EK_HANDLE)
            .unwrap()
            .is_none());

        let (ak_pub_third, ek_pub_third) = create_ak_ek_pub(&mut tpm_engine_helper);

        // Ensure AK and EK match across reset if seeds do not change
        assert_eq!(ak_pub_second, ak_pub_third);
        assert_eq!(ek_pub_second, ek_pub_third);

        // Test creating AK and EK without clearing context and force_create = true

        restart_tpm_engine(&mut tpm_engine_helper, false, true);

        // Ensure that AK is persisted across reset without clearing context
        assert!(tpm_engine_helper
            .find_object(TPM_AZURE_AIK_HANDLE)
            .unwrap()
            .is_some());
        assert!(tpm_engine_helper
            .find_object(TPM_AZURE_EK_HANDLE)
            .unwrap()
            .is_none());

        let (ak_pub_fourth, ek_pub_fourth) = create_ak_ek_pub(&mut tpm_engine_helper);

        // Ensure AK and EK match across reset if seeds do not change
        assert_eq!(ak_pub_third, ak_pub_fourth);
        assert_eq!(ek_pub_third, ek_pub_fourth);

        // Test creating AK and EK after refreshing TPM seeds

        restart_tpm_engine(&mut tpm_engine_helper, false, true);

        let result = tpm_engine_helper.refresh_tpm_seeds();
        assert!(result.is_ok());

        // Ensure nothing present after seeds refreshment
        assert!(tpm_engine_helper
            .find_object(TPM_AZURE_AIK_HANDLE)
            .unwrap()
            .is_none());
        assert!(tpm_engine_helper
            .find_object(TPM_AZURE_EK_HANDLE)
            .unwrap()
            .is_none());

        let (ak_pub_fifth, ek_pub_fifth) = create_ak_ek_pub(&mut tpm_engine_helper);

        // Ensure AK and EK mismatch across reset if seeds do change
        assert_ne!(ak_pub_fourth, ak_pub_fifth);
        assert_ne!(ek_pub_fourth, ek_pub_fifth);
    }

    fn create_ak_ek_pub(
        tpm_engine_helper: &mut TpmEngineHelper,
    ) -> (TpmRsa2kPublic, TpmRsa2kPublic) {
        let result = tpm_engine_helper.create_ak_pub(false);
        assert!(result.is_ok());
        let ak_pub = result.unwrap();

        // Ensure `create_ak_pub` persists AK
        assert!(tpm_engine_helper
            .find_object(TPM_AZURE_AIK_HANDLE)
            .unwrap()
            .is_some());
        assert!(tpm_engine_helper
            .find_object(TPM_AZURE_EK_HANDLE)
            .unwrap()
            .is_none());

        let result = tpm_engine_helper.create_ek_pub();
        assert!(result.is_ok());
        let ek_pub = result.unwrap();

        // Ensure `create_ek_pub` does not persist anything
        assert!(tpm_engine_helper
            .find_object(TPM_AZURE_AIK_HANDLE)
            .unwrap()
            .is_some());
        assert!(tpm_engine_helper
            .find_object(TPM_AZURE_EK_HANDLE)
            .unwrap()
            .is_none());

        (ak_pub, ek_pub)
    }

    #[test]
    fn test_allocate_guest_attestation_nv_indices() {
        const AK_CERT_INPUT: [u8; 512] = [7u8; 512];
        const ATTESTATION_REPORT_INPUT: [u8; 256] = [6u8; 256];

        let mut tpm_engine_helper = create_tpm_engine_helper();
        restart_tpm_engine(&mut tpm_engine_helper, false, true);

        // Test allocation without initial states and with with force_create_ak = true, support_attestation_report = false
        // Expect only the ak cert nv index to be created
        {
            // Ensure both nv indices are not present
            let result = tpm_engine_helper.find_nv_index(TPM_NV_INDEX_AIK_CERT);
            assert!(result.is_ok());
            assert!(result.unwrap().is_none());

            let result = tpm_engine_helper.find_nv_index(TPM_NV_INDEX_ATTESTATION_REPORT);
            assert!(result.is_ok());
            assert!(result.unwrap().is_none());

            restart_tpm_engine(&mut tpm_engine_helper, true, true);

            let result =
                tpm_engine_helper.allocate_guest_attestation_nv_indices(AUTH_VALUE, true, false);
            assert!(result.is_ok());

            let result = tpm_engine_helper.find_nv_index(TPM_NV_INDEX_AIK_CERT);
            assert!(result.is_ok());
            assert!(result.unwrap().is_some());

            let result = tpm_engine_helper.find_nv_index(TPM_NV_INDEX_ATTESTATION_REPORT);
            assert!(result.is_ok());
            assert!(result.unwrap().is_none());

            // Expect read to fail given that no data has been written
            let mut output = [0u8; MAX_NV_INDEX_SIZE as usize];
            let result = tpm_engine_helper.read_from_nv_index(TPM_NV_INDEX_AIK_CERT, &mut output);
            assert!(result.is_err());

            // Write to ak cert nv
            let result = tpm_engine_helper.write_to_nv_index(
                AUTH_VALUE,
                TPM_NV_INDEX_AIK_CERT,
                &AK_CERT_INPUT,
            );
            assert!(result.is_ok());

            // Read the data and ensure it is zero-padded
            let result = tpm_engine_helper.read_from_nv_index(TPM_NV_INDEX_AIK_CERT, &mut output);
            assert!(result.is_ok());
            let input_with_padding = {
                let mut input = AK_CERT_INPUT.to_vec();
                input.resize(MAX_NV_INDEX_SIZE.into(), 0);
                input
            };
            assert_eq!(&output, input_with_padding.as_slice());
        }

        // Test allocation after a restart with force_create_ak = false, support_attestation_report = false
        // Expect the content of ak cert nv index to be re-created given that it's platform-created
        {
            restart_tpm_engine(&mut tpm_engine_helper, true, true);

            let result =
                tpm_engine_helper.allocate_guest_attestation_nv_indices(AUTH_VALUE, false, false);
            assert!(result.is_ok());

            let result = tpm_engine_helper.find_nv_index(TPM_NV_INDEX_AIK_CERT);
            assert!(result.is_ok());
            assert!(result.unwrap().is_some());

            let result = tpm_engine_helper.find_nv_index(TPM_NV_INDEX_ATTESTATION_REPORT);
            assert!(result.is_ok());
            assert!(result.unwrap().is_none());

            // Expect read to fail given that the nv index is re-created and no data has been written
            let mut output = [0u8; MAX_NV_INDEX_SIZE as usize];
            let result = tpm_engine_helper.read_from_nv_index(TPM_NV_INDEX_AIK_CERT, &mut output);
            assert!(result.is_err());

            // Write to ak cert nv
            let result = tpm_engine_helper.write_to_nv_index(
                AUTH_VALUE,
                TPM_NV_INDEX_AIK_CERT,
                &AK_CERT_INPUT,
            );
            assert!(result.is_ok());

            // Read the data and ensure it is zero-padded
            let result = tpm_engine_helper.read_from_nv_index(TPM_NV_INDEX_AIK_CERT, &mut output);
            assert!(result.is_ok());
            let input_with_padding = {
                let mut input = AK_CERT_INPUT.to_vec();
                input.resize(MAX_NV_INDEX_SIZE.into(), 0);
                input
            };
            assert_eq!(&output, input_with_padding.as_slice());
        }

        // Test allocation after a restart with force_create_ak = true, support_attestation_report = false
        // Expect ak cert nv index to be re-created
        {
            restart_tpm_engine(&mut tpm_engine_helper, true, true);

            let result =
                tpm_engine_helper.allocate_guest_attestation_nv_indices(AUTH_VALUE, true, false);
            assert!(result.is_ok());

            let result = tpm_engine_helper.find_nv_index(TPM_NV_INDEX_AIK_CERT);
            assert!(result.is_ok());
            assert!(result.unwrap().is_some());

            let result = tpm_engine_helper.find_nv_index(TPM_NV_INDEX_ATTESTATION_REPORT);
            assert!(result.is_ok());
            assert!(result.unwrap().is_none());

            // Expect read to fail given that the nv index is re-created and no data has been written
            let mut output = [0u8; MAX_NV_INDEX_SIZE as usize];
            let result = tpm_engine_helper.read_from_nv_index(TPM_NV_INDEX_AIK_CERT, &mut output);
            assert!(result.is_err());

            // Write to ak cert nv
            let result = tpm_engine_helper.write_to_nv_index(
                AUTH_VALUE,
                TPM_NV_INDEX_AIK_CERT,
                &AK_CERT_INPUT,
            );
            assert!(result.is_ok());

            // Read the data and ensure it is zero-padded
            let result = tpm_engine_helper.read_from_nv_index(TPM_NV_INDEX_AIK_CERT, &mut output);
            assert!(result.is_ok());
            let input_with_padding = {
                let mut input = AK_CERT_INPUT.to_vec();
                input.resize(MAX_NV_INDEX_SIZE.into(), 0);
                input
            };
            assert_eq!(&output, input_with_padding.as_slice());
        }

        // Test allocation after a restart force_create_ak = true, support_attestation_report = true
        // Expect ak cert nv index to be re-created and attestation report nv index to be created
        {
            restart_tpm_engine(&mut tpm_engine_helper, true, true);

            let result =
                tpm_engine_helper.allocate_guest_attestation_nv_indices(AUTH_VALUE, true, true);
            assert!(result.is_ok());

            let result = tpm_engine_helper.find_nv_index(TPM_NV_INDEX_AIK_CERT);
            assert!(result.is_ok());
            assert!(result.unwrap().is_some());

            let result = tpm_engine_helper.find_nv_index(TPM_NV_INDEX_ATTESTATION_REPORT);
            assert!(result.is_ok());
            assert!(result.unwrap().is_some());

            // Expect read to fail given that the nv index is re-created and no data has been written
            let mut ak_cert_output = [0u8; MAX_NV_INDEX_SIZE as usize];
            let result =
                tpm_engine_helper.read_from_nv_index(TPM_NV_INDEX_AIK_CERT, &mut ak_cert_output);
            assert!(result.is_err());

            // Write to ak cert nv
            let result = tpm_engine_helper.write_to_nv_index(
                AUTH_VALUE,
                TPM_NV_INDEX_AIK_CERT,
                &AK_CERT_INPUT,
            );
            assert!(result.is_ok());

            // Read the data and ensure it is zero-padded
            let result =
                tpm_engine_helper.read_from_nv_index(TPM_NV_INDEX_AIK_CERT, &mut ak_cert_output);
            assert!(result.is_ok());
            let input_with_padding = {
                let mut input = AK_CERT_INPUT.to_vec();
                input.resize(MAX_NV_INDEX_SIZE.into(), 0);
                input
            };
            assert_eq!(&ak_cert_output, input_with_padding.as_slice());

            // Expect read to fail given that the nv index is created and no data has been written
            let mut report_output = [0u8; MAX_ATTESTATION_INDEX_SIZE as usize];
            let result = tpm_engine_helper
                .read_from_nv_index(TPM_NV_INDEX_ATTESTATION_REPORT, &mut report_output);
            assert!(result.is_err());

            // Write to attestation report nv
            let result = tpm_engine_helper.write_to_nv_index(
                AUTH_VALUE,
                TPM_NV_INDEX_ATTESTATION_REPORT,
                &ATTESTATION_REPORT_INPUT,
            );
            assert!(result.is_ok());

            // Read the data and ensure it is zero-padded
            let result = tpm_engine_helper
                .read_from_nv_index(TPM_NV_INDEX_ATTESTATION_REPORT, &mut report_output);
            assert!(result.is_ok());
            let input_with_padding = {
                let mut input = ATTESTATION_REPORT_INPUT.to_vec();
                input.resize(MAX_ATTESTATION_INDEX_SIZE.into(), 0);
                input
            };
            assert_eq!(&report_output, input_with_padding.as_slice());
        }

        // Test allocation after a restart force_create_ak = true, support_attestation_report = true
        // Expect both ak cert and attestation report nv indices to be re-created
        {
            restart_tpm_engine(&mut tpm_engine_helper, true, true);

            let result =
                tpm_engine_helper.allocate_guest_attestation_nv_indices(AUTH_VALUE, true, true);
            assert!(result.is_ok());

            let result = tpm_engine_helper.find_nv_index(TPM_NV_INDEX_AIK_CERT);
            assert!(result.is_ok());
            assert!(result.unwrap().is_some());

            let result = tpm_engine_helper.find_nv_index(TPM_NV_INDEX_ATTESTATION_REPORT);
            assert!(result.is_ok());
            assert!(result.unwrap().is_some());

            // Expect read to fail given that the nv index is re-created and no data has been written
            let mut ak_cert_output = [0u8; MAX_NV_INDEX_SIZE as usize];
            let result =
                tpm_engine_helper.read_from_nv_index(TPM_NV_INDEX_AIK_CERT, &mut ak_cert_output);
            assert!(result.is_err());

            // Expect read to fail given that the nv index is re-created and no data has been written
            let mut report_output = [0u8; MAX_ATTESTATION_INDEX_SIZE as usize];
            let result = tpm_engine_helper
                .read_from_nv_index(TPM_NV_INDEX_ATTESTATION_REPORT, &mut report_output);
            assert!(result.is_err());
        }
    }

    #[test]
    fn test_read_write_guest_attestation_indices() {
        let mut tpm_engine_helper = create_tpm_engine_helper();
        restart_tpm_engine(&mut tpm_engine_helper, false, true);

        let result =
            tpm_engine_helper.allocate_guest_attestation_nv_indices(AUTH_VALUE, true, true);
        assert!(result.is_ok());

        let result = tpm_engine_helper.find_nv_index(TPM_NV_INDEX_AIK_CERT);
        assert!(result.is_ok());
        assert!(result.unwrap().is_some());

        let result = tpm_engine_helper.find_nv_index(TPM_NV_INDEX_ATTESTATION_REPORT);
        assert!(result.is_ok());
        assert!(result.unwrap().is_some());

        // Test writing to ak cert nv index with data size equal to index size
        {
            let ak_cert_input_equal = [7u8; MAX_NV_INDEX_SIZE as usize];
            let result = tpm_engine_helper.write_to_nv_index(
                AUTH_VALUE,
                TPM_NV_INDEX_AIK_CERT,
                &ak_cert_input_equal,
            );
            assert!(result.is_ok());

            let mut ak_cert_output = [0u8; MAX_NV_INDEX_SIZE as usize];
            let result =
                tpm_engine_helper.read_from_nv_index(TPM_NV_INDEX_AIK_CERT, &mut ak_cert_output);
            assert!(result.is_ok());
            assert_eq!(&ak_cert_output, &ak_cert_input_equal);
        }

        // Test writing to ak cert nv index with data size less than index size
        {
            let ak_cert_input_less = [7u8; MAX_NV_INDEX_SIZE as usize - 1024];
            let result = tpm_engine_helper.write_to_nv_index(
                AUTH_VALUE,
                TPM_NV_INDEX_AIK_CERT,
                &ak_cert_input_less,
            );
            assert!(result.is_ok());

            // Read the data and ensure it is zero-padded
            let mut ak_cert_output = [0u8; MAX_NV_INDEX_SIZE as usize];
            let result =
                tpm_engine_helper.read_from_nv_index(TPM_NV_INDEX_AIK_CERT, &mut ak_cert_output);
            assert!(result.is_ok());
            let input_with_padding = {
                let mut input = ak_cert_input_less.to_vec();
                input.resize(MAX_NV_INDEX_SIZE.into(), 0);
                input
            };
            assert_eq!(&ak_cert_output, input_with_padding.as_slice());
        }

        // Test writing to ak cert nv index with data size larger than index size
        {
            let ak_cert_input_larger = [7u8; MAX_NV_INDEX_SIZE as usize + 1024];
            let result = tpm_engine_helper.write_to_nv_index(
                AUTH_VALUE,
                TPM_NV_INDEX_AIK_CERT,
                &ak_cert_input_larger,
            );
            assert!(result.is_err());
            let err = result.unwrap_err();
            if let TpmHelperError::NvWriteInputTooLarge {
                nv_index,
                input_size,
                allocated_size,
            } = err
            {
                assert_eq!(nv_index, TPM_NV_INDEX_AIK_CERT);
                assert_eq!(input_size, ak_cert_input_larger.len());
                assert_eq!(allocated_size, MAX_NV_INDEX_SIZE.into());
            } else {
                panic!()
            }
        }

        // Test writing to ak cert nv index with wrong authorization value
        {
            let ak_cert_input_larger = [7u8; MAX_NV_INDEX_SIZE as usize];
            let result = tpm_engine_helper.write_to_nv_index(
                0,
                TPM_NV_INDEX_AIK_CERT,
                &ak_cert_input_larger,
            );
            assert!(result.is_err());
            let err = result.unwrap_err();
            if let TpmHelperError::TpmCommandError {
                command_debug_info,
                error: command_error,
            } = err
            {
                assert_eq!(command_debug_info.nv_index, Some(TPM_NV_INDEX_AIK_CERT));
                assert_eq!(
                    command_debug_info.auth_handle,
                    Some(ReservedHandle(TPM_NV_INDEX_AIK_CERT.into()))
                );
                assert_eq!(command_debug_info.command_code, CommandCodeEnum::NV_Write);
                assert!(matches!(
                    command_error,
                    TpmCommandError::TpmCommandFailed { response_code: _ }
                ));
            }
        }

        // Test writing to attestation report nv index with data size equal to index size
        {
            let report_input_equal = [7u8; MAX_ATTESTATION_INDEX_SIZE as usize];
            let result = tpm_engine_helper.write_to_nv_index(
                AUTH_VALUE,
                TPM_NV_INDEX_ATTESTATION_REPORT,
                &report_input_equal,
            );
            assert!(result.is_ok());

            let mut report_output = [0u8; MAX_ATTESTATION_INDEX_SIZE as usize];
            let result = tpm_engine_helper
                .read_from_nv_index(TPM_NV_INDEX_ATTESTATION_REPORT, &mut report_output);
            assert!(result.is_ok());
            assert_eq!(&report_output, &report_input_equal);
        }

        // Test writing to attestation report nv index with data size less than index size
        {
            let report_input_less = [7u8; MAX_ATTESTATION_INDEX_SIZE as usize - 1024];
            let result = tpm_engine_helper.write_to_nv_index(
                AUTH_VALUE,
                TPM_NV_INDEX_ATTESTATION_REPORT,
                &report_input_less,
            );
            assert!(result.is_ok());

            // Read the data and ensure it is zero-padded
            let mut report_output = [0u8; MAX_ATTESTATION_INDEX_SIZE as usize];
            let result = tpm_engine_helper
                .read_from_nv_index(TPM_NV_INDEX_ATTESTATION_REPORT, &mut report_output);
            assert!(result.is_ok());
            let input_with_padding = {
                let mut input = report_input_less.to_vec();
                input.resize(MAX_ATTESTATION_INDEX_SIZE.into(), 0);
                input
            };
            assert_eq!(&report_output, input_with_padding.as_slice());
        }

        // Test writing to attestation report nv index with data size larger than index size
        {
            let report_input_larger = [7u8; MAX_ATTESTATION_INDEX_SIZE as usize + 1024];
            let result = tpm_engine_helper.write_to_nv_index(
                AUTH_VALUE,
                TPM_NV_INDEX_ATTESTATION_REPORT,
                &report_input_larger,
            );
            assert!(result.is_err());
            let err = result.unwrap_err();
            if let TpmHelperError::NvWriteInputTooLarge {
                nv_index,
                input_size,
                allocated_size,
            } = err
            {
                assert_eq!(nv_index, TPM_NV_INDEX_ATTESTATION_REPORT);
                assert_eq!(input_size, report_input_larger.len());
                assert_eq!(allocated_size, MAX_ATTESTATION_INDEX_SIZE.into());
            }
        }

        // Test writing to attestation report nv index with wrong authorization value
        {
            let ak_cert_input_larger = [7u8; MAX_NV_INDEX_SIZE as usize];
            let result = tpm_engine_helper.write_to_nv_index(
                0,
                TPM_NV_INDEX_ATTESTATION_REPORT,
                &ak_cert_input_larger,
            );
            assert!(result.is_err());
            let err = result.unwrap_err();
            if let TpmHelperError::TpmCommandError {
                command_debug_info,
                error: command_error,
            } = err
            {
                assert_eq!(
                    command_debug_info.nv_index,
                    Some(TPM_NV_INDEX_ATTESTATION_REPORT)
                );
                assert_eq!(
                    command_debug_info.auth_handle,
                    Some(ReservedHandle(TPM_NV_INDEX_ATTESTATION_REPORT.into()))
                );
                assert_eq!(command_debug_info.command_code, CommandCodeEnum::NV_Write);
                assert!(matches!(
                    command_error,
                    TpmCommandError::TpmCommandFailed { response_code: _ }
                ));
            }
        }
    }

    #[test]
    fn test_with_pre_provisioned_state() {
        // The blob file generated by the TpmEngFWInit (internal) tool.
        let tpm_state_blob = include_bytes!("../test_data/vTpmState.blob");

        let mut tpm_engine_helper = create_tpm_engine_helper();

        let result = tpm_engine_helper.tpm_engine.reset(Some(tpm_state_blob));
        assert!(result.is_ok());

        let result = tpm_engine_helper.initialize_tpm_engine();
        assert!(result.is_ok());

        // Ensure AK cert is provisioned
        let result = tpm_engine_helper.nv_read_public(TPM_NV_INDEX_AIK_CERT);
        assert!(result.is_ok());
        let nv_read_public_reply = result.unwrap();

        // The provisioned nv size is less than the created one
        assert!(nv_read_public_reply.nv_public.nv_public.data_size.get() < MAX_NV_INDEX_SIZE);

        // Ensure AK is provisioned
        assert!(tpm_engine_helper
            .find_object(TPM_AZURE_AIK_HANDLE)
            .unwrap()
            .is_some());

        let mut provisioned_ak_cert = [0u8; MAX_NV_INDEX_SIZE as usize];
        let result =
            tpm_engine_helper.read_from_nv_index(TPM_NV_INDEX_AIK_CERT, &mut provisioned_ak_cert);
        assert!(result.is_ok());

        // Ensure allocate_guest_attestation_nv_indices with force_create_ak_cert = false does not re-create
        // the nv index.
        let result =
            tpm_engine_helper.allocate_guest_attestation_nv_indices(AUTH_VALUE, false, false);
        assert!(result.is_ok());

        let mut provisioned_ak_cert_after_call = [0u8; MAX_NV_INDEX_SIZE as usize];
        let result = tpm_engine_helper
            .read_from_nv_index(TPM_NV_INDEX_AIK_CERT, &mut provisioned_ak_cert_after_call);
        assert!(result.is_ok());
        assert_eq!(provisioned_ak_cert_after_call, provisioned_ak_cert);

        // Test updating the provisioned nv index (with ownerwrite permission)
        let ak_cert_input = [7u8; 1024];
        let result =
            tpm_engine_helper.write_to_nv_index(AUTH_VALUE, TPM_NV_INDEX_AIK_CERT, &ak_cert_input);
        assert!(result.is_ok());

        // Read the data and ensure it is zero-padded
        let mut ak_cert_output = [0u8; MAX_NV_INDEX_SIZE as usize];
        let result =
            tpm_engine_helper.read_from_nv_index(TPM_NV_INDEX_AIK_CERT, &mut ak_cert_output);
        assert!(result.is_ok());
        let input_with_padding = {
            let mut input = ak_cert_input.to_vec();
            input.resize(MAX_NV_INDEX_SIZE.into(), 0);
            input
        };
        assert_eq!(&ak_cert_output, input_with_padding.as_slice());

        // Ensure the data is overwritten
        assert_ne!(&ak_cert_output, &provisioned_ak_cert);
    }

    #[test]
    fn test_initialize_guest_secret_key() {
        const GUEST_SECRET_KEY_BLOB: [u8; 422] = [
            0x01, 0x16, 0x00, 0x01, 0x00, 0x0b, 0x00, 0x02, 0x00, 0x40, 0x00, 0x00, 0x00, 0x10,
            0x00, 0x10, 0x08, 0x00, 0x00, 0x00, 0x00, 0x00, 0x01, 0x00, 0xec, 0x0d, 0xdf, 0xf3,
            0xa2, 0x0f, 0xd4, 0x66, 0xe8, 0x53, 0x8a, 0x1c, 0x54, 0x00, 0x69, 0xbe, 0x57, 0xc4,
            0x9a, 0x7d, 0x4d, 0xd2, 0xbc, 0xd7, 0x6b, 0x93, 0xe4, 0x15, 0x3f, 0x2f, 0xbb, 0x77,
            0xf7, 0x1b, 0x19, 0x88, 0x04, 0xc7, 0x42, 0xda, 0xa2, 0x00, 0xc7, 0x8c, 0x2a, 0xfc,
            0x48, 0xa5, 0xe7, 0x3f, 0x4e, 0x06, 0x33, 0xa8, 0xb1, 0xcf, 0x09, 0x8c, 0xfe, 0x3f,
            0x91, 0x43, 0xa9, 0x4a, 0x8e, 0x05, 0xe7, 0xf0, 0x57, 0x68, 0xb5, 0x68, 0xe7, 0x7d,
            0xb3, 0x5c, 0xd5, 0x6c, 0xb9, 0x48, 0x5e, 0x0f, 0xf9, 0x0f, 0xe9, 0xf9, 0x42, 0x57,
            0x08, 0x8c, 0xff, 0x3f, 0x67, 0xd1, 0x9b, 0xb6, 0xa7, 0x7d, 0xa6, 0xa9, 0xcb, 0x00,
            0x4b, 0x1d, 0xa6, 0xf3, 0x09, 0xe0, 0x87, 0x12, 0xc6, 0x8b, 0xbe, 0x61, 0xaf, 0xc6,
            0x30, 0x35, 0xcc, 0x10, 0x68, 0x8b, 0x76, 0x36, 0x16, 0xcb, 0xce, 0x83, 0x6c, 0x7e,
            0x9e, 0x1e, 0x08, 0xc7, 0x20, 0x7d, 0x1d, 0xd4, 0xc4, 0x4f, 0x3a, 0x34, 0x06, 0xe9,
            0xae, 0xf5, 0x50, 0xd9, 0x5d, 0xb2, 0x30, 0x74, 0xed, 0x38, 0x74, 0x31, 0x3e, 0x1d,
            0xfd, 0x15, 0x26, 0x8f, 0x48, 0x5b, 0x22, 0x2f, 0xa0, 0xc3, 0xd0, 0x1c, 0x56, 0x4f,
            0xb1, 0x39, 0xe7, 0x93, 0xc1, 0x3d, 0x2d, 0x42, 0x57, 0x33, 0x4d, 0xdc, 0x90, 0x41,
            0x83, 0x6a, 0x21, 0x15, 0xbd, 0x2c, 0x5c, 0xa1, 0xc1, 0xda, 0xf9, 0x4c, 0x15, 0x89,
            0x41, 0x84, 0xad, 0xb9, 0xfc, 0xc7, 0x81, 0xa3, 0x93, 0xe9, 0xd8, 0xfc, 0xe3, 0x3f,
            0x4d, 0x6f, 0x71, 0x14, 0x9e, 0xe2, 0xe2, 0xfa, 0xa1, 0x8d, 0x3a, 0x80, 0xea, 0x5a,
            0xc9, 0x0f, 0x23, 0xb9, 0x3e, 0x36, 0xbb, 0xff, 0x4e, 0x9c, 0x40, 0x6f, 0x1d, 0x75,
            0x39, 0x96, 0x9b, 0xac, 0x54, 0xe1, 0x0b, 0x4b, 0x08, 0x3e, 0xd5, 0x94, 0x7d, 0xad,
            0x00, 0x8a, 0x00, 0x88, 0x00, 0x01, 0x00, 0x00, 0x00, 0x00, 0x00, 0x80, 0xf7, 0xca,
            0x88, 0xe3, 0x6a, 0x67, 0xbd, 0xb7, 0xfe, 0xc9, 0x49, 0x35, 0x84, 0x23, 0xf3, 0x26,
            0x7f, 0xaa, 0xf6, 0xee, 0x14, 0x86, 0x55, 0xbf, 0x26, 0xd3, 0x21, 0x9f, 0x8a, 0xb2,
            0x1f, 0x2e, 0x79, 0x69, 0x7b, 0xa0, 0xad, 0x06, 0x2e, 0x13, 0xda, 0x8a, 0x5c, 0x59,
            0x98, 0x75, 0xf5, 0xfa, 0x2e, 0x14, 0xe6, 0xef, 0xc2, 0x3c, 0xa6, 0x11, 0x90, 0xf8,
            0xc3, 0x6f, 0x7d, 0xc5, 0x4c, 0x5c, 0xe8, 0x6a, 0x7f, 0x24, 0xa0, 0xef, 0x70, 0x5e,
            0xc8, 0x92, 0xa2, 0x3c, 0xa8, 0xa4, 0x0b, 0x38, 0xb1, 0xd5, 0xeb, 0x67, 0x8f, 0x76,
            0x65, 0x73, 0xd5, 0x6b, 0xb1, 0xad, 0x85, 0xb0, 0x0b, 0x0e, 0x41, 0x6b, 0xba, 0x1c,
            0x2a, 0x02, 0x11, 0xb7, 0xb4, 0x72, 0x74, 0xe2, 0x9f, 0x8e, 0x42, 0xa1, 0x38, 0x24,
            0x25, 0xc8, 0xcf, 0x53, 0x27, 0x1b, 0x4e, 0xcc, 0x8c, 0x0b, 0x4b, 0x69, 0x3f, 0x7b,
            0x00, 0x00,
        ];

        const GUEST_SECRET_KEY_PUBLIC: [u8; 256] = [
            0xec, 0x0d, 0xdf, 0xf3, 0xa2, 0x0f, 0xd4, 0x66, 0xe8, 0x53, 0x8a, 0x1c, 0x54, 0x00,
            0x69, 0xbe, 0x57, 0xc4, 0x9a, 0x7d, 0x4d, 0xd2, 0xbc, 0xd7, 0x6b, 0x93, 0xe4, 0x15,
            0x3f, 0x2f, 0xbb, 0x77, 0xf7, 0x1b, 0x19, 0x88, 0x04, 0xc7, 0x42, 0xda, 0xa2, 0x00,
            0xc7, 0x8c, 0x2a, 0xfc, 0x48, 0xa5, 0xe7, 0x3f, 0x4e, 0x06, 0x33, 0xa8, 0xb1, 0xcf,
            0x09, 0x8c, 0xfe, 0x3f, 0x91, 0x43, 0xa9, 0x4a, 0x8e, 0x05, 0xe7, 0xf0, 0x57, 0x68,
            0xb5, 0x68, 0xe7, 0x7d, 0xb3, 0x5c, 0xd5, 0x6c, 0xb9, 0x48, 0x5e, 0x0f, 0xf9, 0x0f,
            0xe9, 0xf9, 0x42, 0x57, 0x08, 0x8c, 0xff, 0x3f, 0x67, 0xd1, 0x9b, 0xb6, 0xa7, 0x7d,
            0xa6, 0xa9, 0xcb, 0x00, 0x4b, 0x1d, 0xa6, 0xf3, 0x09, 0xe0, 0x87, 0x12, 0xc6, 0x8b,
            0xbe, 0x61, 0xaf, 0xc6, 0x30, 0x35, 0xcc, 0x10, 0x68, 0x8b, 0x76, 0x36, 0x16, 0xcb,
            0xce, 0x83, 0x6c, 0x7e, 0x9e, 0x1e, 0x08, 0xc7, 0x20, 0x7d, 0x1d, 0xd4, 0xc4, 0x4f,
            0x3a, 0x34, 0x06, 0xe9, 0xae, 0xf5, 0x50, 0xd9, 0x5d, 0xb2, 0x30, 0x74, 0xed, 0x38,
            0x74, 0x31, 0x3e, 0x1d, 0xfd, 0x15, 0x26, 0x8f, 0x48, 0x5b, 0x22, 0x2f, 0xa0, 0xc3,
            0xd0, 0x1c, 0x56, 0x4f, 0xb1, 0x39, 0xe7, 0x93, 0xc1, 0x3d, 0x2d, 0x42, 0x57, 0x33,
            0x4d, 0xdc, 0x90, 0x41, 0x83, 0x6a, 0x21, 0x15, 0xbd, 0x2c, 0x5c, 0xa1, 0xc1, 0xda,
            0xf9, 0x4c, 0x15, 0x89, 0x41, 0x84, 0xad, 0xb9, 0xfc, 0xc7, 0x81, 0xa3, 0x93, 0xe9,
            0xd8, 0xfc, 0xe3, 0x3f, 0x4d, 0x6f, 0x71, 0x14, 0x9e, 0xe2, 0xe2, 0xfa, 0xa1, 0x8d,
            0x3a, 0x80, 0xea, 0x5a, 0xc9, 0x0f, 0x23, 0xb9, 0x3e, 0x36, 0xbb, 0xff, 0x4e, 0x9c,
            0x40, 0x6f, 0x1d, 0x75, 0x39, 0x96, 0x9b, 0xac, 0x54, 0xe1, 0x0b, 0x4b, 0x08, 0x3e,
            0xd5, 0x94, 0x7d, 0xad,
        ];

        // The blob file generated by the TpmEngFWInit (internal) tool.
        let tpm_state_blob = include_bytes!("../test_data/vTpmState.blob");

        let mut tpm_engine_helper = create_tpm_engine_helper();

        let result = tpm_engine_helper.tpm_engine.reset(Some(tpm_state_blob));
        assert!(result.is_ok());

        let result = tpm_engine_helper.initialize_tpm_engine();
        assert!(result.is_ok());

        // Ensure SRK is provisioned
        assert!(tpm_engine_helper
            .find_object(TPM_RSA_SRK_HANDLE)
            .unwrap()
            .is_some());

        // Ensure guest secret key is not initialized yet
        assert!(tpm_engine_helper
            .find_object(TPM_GUEST_SECRET_HANDLE)
            .unwrap()
            .is_none());

        // Negative test: invalid data blob
        let result = tpm_engine_helper.initialize_guest_secret_key(&[]);
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(matches!(err, TpmHelperError::DeserializeGuestSecretKey));

        // Positive test

        // Apply zero paddings to `GUEST_SECRET_KEY_MAX_SIZE`
        let data_with_zero_paddings = {
            let mut data = GUEST_SECRET_KEY_BLOB.to_vec();
            data.resize(2048, 0);

            data
        };

        let result = tpm_engine_helper.initialize_guest_secret_key(&data_with_zero_paddings);
        assert!(result.is_ok());

        // Ensure guest secret key is initialized
        let result = tpm_engine_helper.find_object(TPM_GUEST_SECRET_HANDLE);
        assert!(result.is_ok());
        let result = result.unwrap();
        assert!(result.is_some());

        let read_public_reply = result.unwrap();
        let unique = read_public_reply.out_public.public_area.unique.serialize();
        let offset = size_of_val(&read_public_reply.out_public.public_area.unique.size);
        assert_eq!(
            &unique[offset..offset + RSA_2K_MODULUS_SIZE],
            GUEST_SECRET_KEY_PUBLIC
        );

        // Negative test: Test without SRK

        restart_tpm_engine(&mut tpm_engine_helper, true, true);

        // Ensure SRK is not provisioned
        assert!(tpm_engine_helper
            .find_object(TPM_RSA_SRK_HANDLE)
            .unwrap()
            .is_none());

        // Ensure guest secret key is not initialized yet
        assert!(tpm_engine_helper
            .find_object(TPM_GUEST_SECRET_HANDLE)
            .unwrap()
            .is_none());

        // Expect to fail due to SRK not found
        let result = tpm_engine_helper.initialize_guest_secret_key(&GUEST_SECRET_KEY_BLOB);
        assert!(result.is_err());
        if let TpmHelperError::SrkNotFound(srk_handle) = result.unwrap_err() {
            assert_eq!(srk_handle, TPM_RSA_SRK_HANDLE);
        } else {
            panic!()
        }
    }

    #[test]
    fn test_startup_and_self_test() {
        let mut tpm_engine_helper = create_tpm_engine_helper();
        restart_tpm_engine(&mut tpm_engine_helper, false, false);

        // Negative test for SelfTest (expect to fail before StartUp is called)
        let result = tpm_engine_helper.self_test(true);
        assert!(result.is_err());
        let err = result.unwrap_err();
        if let TpmCommandError::TpmCommandFailed { response_code } = err {
            assert_ne!(response_code, ResponseCode::Success as u32);
        } else {
            panic!()
        }

        // Positive tests
        let result = tpm_engine_helper.startup(StartupType::Clear);
        assert!(result.is_ok());

        let result = tpm_engine_helper.self_test(true);
        assert!(result.is_ok());

        // Negative test for StartUp
        let result = tpm_engine_helper.startup(StartupType::Clear);
        assert!(result.is_err());
        let err = result.unwrap_err();
        if let TpmCommandError::TpmCommandFailed { response_code } = err {
            assert_ne!(response_code, ResponseCode::Success as u32);
        } else {
            panic!()
        }
    }

    #[test]
    fn test_change_seed() {
        let mut tpm_engine_helper = create_tpm_engine_helper();
        restart_tpm_engine(&mut tpm_engine_helper, false, true);

        // Positive test
        let auth_handle = TPM20_RH_PLATFORM;
        let result = tpm_engine_helper.change_seed(auth_handle, CommandCodeEnum::ChangeEPS);
        assert!(result.is_ok());

        let result = tpm_engine_helper.change_seed(auth_handle, CommandCodeEnum::ChangePPS);
        assert!(result.is_ok());

        // Negative test
        let invalid_auth_handle = ReservedHandle(0.into());
        let result = tpm_engine_helper.change_seed(invalid_auth_handle, CommandCodeEnum::ChangeEPS);
        assert!(result.is_err());
        let err = result.unwrap_err();
        if let TpmCommandError::TpmCommandFailed { response_code } = err {
            assert_ne!(response_code, ResponseCode::Success as u32);
        } else {
            panic!()
        }

        let invalid_auth_handle = ReservedHandle(0.into());
        let result = tpm_engine_helper.change_seed(invalid_auth_handle, CommandCodeEnum::ChangePPS);
        assert!(result.is_err());
        let err = result.unwrap_err();
        if let TpmCommandError::TpmCommandFailed { response_code } = err {
            assert_ne!(response_code, ResponseCode::Success as u32);
        } else {
            panic!()
        }
    }

    #[test]
    fn test_pcr_allocate() {
        let mut tpm_engine_helper = create_tpm_engine_helper();
        restart_tpm_engine(&mut tpm_engine_helper, false, true);

        // Positive test
        let auth_handle = TPM20_RH_PLATFORM;
        let result = tpm_engine_helper.pcr_allocate(auth_handle, 0b000011, 0b00001);
        assert!(result.is_ok());
        let response_code = result.unwrap();
        assert_eq!(response_code, ResponseCode::Success as u32);

        // Negative test
        let invalid_auth_handle = ReservedHandle(0.into());
        let result = tpm_engine_helper.pcr_allocate(invalid_auth_handle, 0, 0);
        assert!(result.is_err());
        let err = result.unwrap_err();
        if let TpmCommandError::TpmCommandFailed { response_code } = err {
            assert_ne!(response_code, ResponseCode::Success as u32);
        } else {
            panic!()
        }
    }

    #[test]
    fn test_create_primary() {
        let mut tpm_engine_helper = create_tpm_engine_helper();
        restart_tpm_engine(&mut tpm_engine_helper, false, true);

        // Positive tests

        // Create EK
        let result = ek_pub_template();
        assert!(result.is_ok());
        let ek_pub_template = result.unwrap();

        let auth_handle = TPM20_RH_ENDORSEMENT;
        let result = tpm_engine_helper.create_primary(auth_handle, ek_pub_template);
        assert!(result.is_ok());
        let response = result.unwrap();
        assert_ne!(response.out_public.size.get(), 0);

        // Create AK
        let result = ak_pub_template();
        assert!(result.is_ok());
        let ak_pub_template = result.unwrap();

        let auth_handle = TPM20_RH_ENDORSEMENT;
        let result = tpm_engine_helper.create_primary(auth_handle, ak_pub_template);
        assert!(result.is_ok());
        let response = result.unwrap();
        assert_ne!(response.out_public.size.get(), 0);

        // Negative test
        let invalid_auth_handle = ReservedHandle(0.into());
        let template = TpmtPublic::new_zeroed();
        let result = tpm_engine_helper.create_primary(invalid_auth_handle, template);
        assert!(result.is_err());
        let err = result.unwrap_err();
        if let TpmCommandError::TpmCommandFailed { response_code } = err {
            assert_ne!(response_code, ResponseCode::Success as u32);
        } else {
            panic!()
        }
    }

    #[test]
    fn test_evict_control() {
        let ak_handle = TPM_AZURE_AIK_HANDLE;

        let mut tpm_engine_helper = create_tpm_engine_helper();
        restart_tpm_engine(&mut tpm_engine_helper, false, true);

        // Create AK
        let result = ak_pub_template();
        assert!(result.is_ok());
        let ak_pub_template = result.unwrap();

        let auth_handle = TPM20_RH_ENDORSEMENT;
        let result = tpm_engine_helper.create_primary(auth_handle, ak_pub_template);
        assert!(result.is_ok());
        let response = result.unwrap();
        assert_ne!(response.out_public.size.get(), 0);
        let ak_object_handle = response.object_handle;

        // Positive test
        let auth_handle = TPM20_RH_OWNER;
        let result = tpm_engine_helper.evict_control(auth_handle, ak_object_handle, ak_handle);
        assert!(result.is_ok());

        // Negative test
        let invalid_auth_handle = ReservedHandle(0.into());
        let result =
            tpm_engine_helper.evict_control(invalid_auth_handle, ak_object_handle, ak_handle);
        assert!(result.is_err());
        let err = result.unwrap_err();
        if let TpmCommandError::TpmCommandFailed { response_code } = err {
            assert_ne!(response_code, ResponseCode::Success as u32);
        } else {
            panic!()
        }
    }

    #[test]
    fn test_flush_context() {
        let mut tpm_engine_helper = create_tpm_engine_helper();
        restart_tpm_engine(&mut tpm_engine_helper, false, true);

        // Create AK
        let result = ak_pub_template();
        assert!(result.is_ok());
        let ak_pub_template = result.unwrap();

        let auth_handle = TPM20_RH_ENDORSEMENT;
        let result = tpm_engine_helper.create_primary(auth_handle, ak_pub_template);
        assert!(result.is_ok());
        let response = result.unwrap();
        assert_ne!(response.out_public.size.get(), 0);
        let ak_object_handle = response.object_handle;

        // Positive test
        let result = tpm_engine_helper.flush_context(ak_object_handle);
        assert!(result.is_ok());

        // Negative test
        let invalid_handle = ReservedHandle(0.into());
        let result = tpm_engine_helper.flush_context(invalid_handle);
        assert!(result.is_err());
        let err = result.unwrap_err();
        if let TpmCommandError::TpmCommandFailed { response_code } = err {
            assert_ne!(response_code, ResponseCode::Success as u32);
        } else {
            panic!()
        }
    }

    #[test]
    fn test_read_public() {
        let ak_handle = TPM_AZURE_AIK_HANDLE;

        let mut tpm_engine_helper = create_tpm_engine_helper();
        restart_tpm_engine(&mut tpm_engine_helper, false, true);

        // Create AK
        let result = ak_pub_template();
        assert!(result.is_ok());
        let ak_pub_template = result.unwrap();

        let auth_handle = TPM20_RH_ENDORSEMENT;
        let result = tpm_engine_helper.create_primary(auth_handle, ak_pub_template);
        assert!(result.is_ok());
        let response = result.unwrap();
        assert_ne!(response.out_public.size.get(), 0);
        let ak_object_handle = response.object_handle;

        let auth_handle = TPM20_RH_OWNER;
        let result = tpm_engine_helper.evict_control(auth_handle, ak_object_handle, ak_handle);
        assert!(result.is_ok());

        // Positive test
        let result = tpm_engine_helper.read_public(ak_handle);
        assert!(result.is_ok());

        // Negative test
        let invalid_object_handle = ReservedHandle((ak_handle.0.get() + 10).into()); // pick an unallocated handle
        let result = tpm_engine_helper.read_public(invalid_object_handle);
        assert!(result.is_err());
        let err = result.unwrap_err();
        if let TpmCommandError::TpmCommandFailed { response_code } = err {
            assert_eq!(
                response_code,
                (ResponseCode::Handle as u32 | ResponseCode::Rc1 as u32)
            );
        } else {
            panic!()
        }
    }

    #[test]
    fn test_nv_define_space() {
        let nv_index = TPM_NV_INDEX_AIK_CERT;
        let nv_index_size = MAX_NV_INDEX_SIZE;

        let mut tpm_engine_helper = create_tpm_engine_helper();
        restart_tpm_engine(&mut tpm_engine_helper, false, true);

        // Positive test
        let auth_handle = TPM20_RH_PLATFORM;
        let result =
            tpm_engine_helper.nv_define_space(auth_handle, AUTH_VALUE, nv_index, nv_index_size);
        assert!(result.is_ok());

        // Negative test
        let invalid_auth_handle = ReservedHandle(0.into());
        let result = tpm_engine_helper.nv_define_space(
            invalid_auth_handle,
            AUTH_VALUE,
            nv_index,
            nv_index_size,
        );
        assert!(result.is_err());
        let err = result.unwrap_err();
        if let TpmCommandError::TpmCommandFailed { response_code } = err {
            assert_ne!(response_code, ResponseCode::Success as u32);
        } else {
            panic!()
        }
    }

    #[test]
    fn test_nv_read_public() {
        let nv_index = TPM_NV_INDEX_AIK_CERT;
        let nv_index_size = MAX_NV_INDEX_SIZE;

        let mut tpm_engine_helper = create_tpm_engine_helper();
        restart_tpm_engine(&mut tpm_engine_helper, false, true);

        let auth_handle = TPM20_RH_PLATFORM;
        let result =
            tpm_engine_helper.nv_define_space(auth_handle, AUTH_VALUE, nv_index, nv_index_size);
        assert!(result.is_ok());

        // Positive test
        let result = tpm_engine_helper.nv_read_public(nv_index);
        assert!(result.is_ok());
        let response = result.unwrap();

        // Check the flags set by `nv_define_space`
        let nv_bits = TpmaNvBits::from(response.nv_public.nv_public.attributes.0.get());
        assert!(nv_bits.nv_authread());
        assert!(nv_bits.nv_authwrite());
        assert!(nv_bits.nv_ownerread());
        assert!(nv_bits.nv_platformcreate());
        assert!(nv_bits.nv_no_da());

        // Negative test
        let invalid_nv_index = nv_index + 10; // Pick an undefined index
        let result = tpm_engine_helper.nv_read_public(invalid_nv_index);
        assert!(result.is_err());
        let err = result.unwrap_err();
        if let TpmCommandError::TpmCommandFailed { response_code } = err {
            assert_eq!(
                response_code,
                (ResponseCode::Handle as u32 | ResponseCode::Rc1 as u32)
            );
        } else {
            panic!()
        }
    }

    #[test]
    fn test_nv_read_write() {
        let nv_index = TPM_NV_INDEX_AIK_CERT;
        let nv_index_size = MAX_NV_INDEX_SIZE;

        let mut tpm_engine_helper = create_tpm_engine_helper();
        restart_tpm_engine(&mut tpm_engine_helper, false, true);

        let auth_handle = TPM20_RH_PLATFORM;
        let result =
            tpm_engine_helper.nv_define_space(auth_handle, AUTH_VALUE, nv_index, nv_index_size);
        assert!(result.is_ok());

        // Positive tests

        // Write with data size equal to nv_index_size
        let input_data = vec![7u8; nv_index_size.into()];
        let result = tpm_engine_helper.nv_write(
            ReservedHandle(nv_index.into()),
            Some(AUTH_VALUE),
            nv_index,
            input_data.as_ref(),
        );
        assert!(result.is_ok());

        // Read the data
        let mut output_data = vec![0u8; nv_index_size.into()];
        let result = tpm_engine_helper.nv_read(
            TPM20_RH_OWNER,
            nv_index,
            nv_index_size,
            output_data.as_mut(),
        );
        assert!(result.is_ok());
        assert_eq!(input_data, output_data);

        // Write with data size smaller to nv_index_size
        let data_size = 512;
        assert!(data_size < nv_index_size.into());
        let input_data = vec![6u8; data_size];
        let result = tpm_engine_helper.nv_write(
            ReservedHandle(nv_index.into()),
            Some(AUTH_VALUE),
            nv_index,
            input_data.as_ref(),
        );
        assert!(result.is_ok());

        // Read the data
        let mut output_data = vec![0u8; nv_index_size.into()];
        let result = tpm_engine_helper.nv_read(
            TPM20_RH_OWNER,
            nv_index,
            nv_index_size,
            output_data.as_mut(),
        );
        assert!(result.is_ok());
        assert_eq!(input_data, output_data[..data_size]);

        // Negative tests

        // test nv_write with invalid auth handle
        let invalid_auth_handle = ReservedHandle(0.into());
        let input_data = vec![7u8; nv_index_size.into()];
        let result = tpm_engine_helper.nv_write(
            invalid_auth_handle,
            Some(AUTH_VALUE),
            nv_index,
            input_data.as_ref(),
        );
        assert!(result.is_err());
        let err = result.unwrap_err();
        if let TpmCommandError::TpmCommandFailed { response_code } = err {
            assert_ne!(response_code, ResponseCode::Success as u32);
        } else {
            panic!()
        }

        // test nv_read with invalid auth handle
        let invalid_auth_handle = ReservedHandle(0.into());
        let mut output_data = vec![0u8; nv_index_size.into()];
        let result = tpm_engine_helper.nv_read(
            invalid_auth_handle,
            nv_index,
            nv_index_size,
            output_data.as_mut(),
        );
        assert!(result.is_err());
        let err = result.unwrap_err();
        if let TpmCommandError::TpmCommandFailed { response_code } = err {
            assert_ne!(response_code, ResponseCode::Success as u32);
        } else {
            panic!()
        }
    }

    #[test]
    fn test_nv_undefine_space() {
        let nv_index = TPM_NV_INDEX_AIK_CERT;
        let nv_index_size = MAX_NV_INDEX_SIZE;

        let mut tpm_engine_helper = create_tpm_engine_helper();
        restart_tpm_engine(&mut tpm_engine_helper, false, true);

        let auth_handle = TPM20_RH_PLATFORM;
        let result =
            tpm_engine_helper.nv_define_space(auth_handle, AUTH_VALUE, nv_index, nv_index_size);

        assert!(result.is_ok());
        // Positive test
        let auth_handle = TPM20_RH_PLATFORM;
        let result = tpm_engine_helper.nv_undefine_space(auth_handle, nv_index);
        assert!(result.is_ok());

        // Negative test
        let invalid_auth_handle = ReservedHandle(0.into());
        let result = tpm_engine_helper.nv_undefine_space(invalid_auth_handle, nv_index);
        assert!(result.is_err());
        let err = result.unwrap_err();
        if let TpmCommandError::TpmCommandFailed { response_code } = err {
            assert_ne!(response_code, ResponseCode::Success as u32);
        } else {
            panic!()
        }
    }

    #[test]
    fn test_clear_control() {
        let mut tpm_engine_helper = create_tpm_engine_helper();
        restart_tpm_engine(&mut tpm_engine_helper, false, true);

        // Positive test
        let auth_handle = TPM20_RH_PLATFORM;
        let result = tpm_engine_helper.clear_control(auth_handle, false);
        assert!(result.is_ok());

        // Negative test
        let invalid_auth_handle = ReservedHandle(0.into());
        let result = tpm_engine_helper.clear_control(invalid_auth_handle, false);
        assert!(result.is_err());
        let err = result.unwrap_err();
        if let TpmCommandError::TpmCommandFailed { response_code } = err {
            assert_ne!(response_code, ResponseCode::Success as u32);
        } else {
            panic!()
        }
    }

    #[test]
    fn test_clear() {
        let mut tpm_engine_helper = create_tpm_engine_helper();
        restart_tpm_engine(&mut tpm_engine_helper, false, true);

        // Positive test

        // Enable the clear command
        let result = tpm_engine_helper.clear_tpm_platform_context();
        assert!(result.is_ok());
        let response_code = result.unwrap();
        assert_eq!(response_code, ResponseCode::Success as u32);

        // Negative test

        // Disable the clear command
        let auth_handle = TPM20_RH_PLATFORM;
        let result = tpm_engine_helper.clear_control(auth_handle, true);
        assert!(result.is_ok());

        let result = tpm_engine_helper.clear(auth_handle);
        assert!(result.is_err());
        let err = result.unwrap_err();
        if let TpmCommandError::TpmCommandFailed { response_code } = err {
            assert_ne!(response_code, ResponseCode::Success as u32);
        } else {
            panic!()
        }
    }

    #[test]
    fn test_hierarchy_control() {
        let mut tpm_engine_helper = create_tpm_engine_helper();
        restart_tpm_engine(&mut tpm_engine_helper, false, true);

        // Positive test
        let auth_handle = TPM20_RH_PLATFORM;
        let result = tpm_engine_helper.hierarchy_control(auth_handle, TPM20_RH_PLATFORM, false);
        assert!(result.is_ok());

        // Negative test
        let invalid_auth_handle = ReservedHandle(0.into());
        let result =
            tpm_engine_helper.hierarchy_control(invalid_auth_handle, TPM20_RH_PLATFORM, false);
        assert!(result.is_err());
        let err = result.unwrap_err();
        if let TpmCommandError::TpmCommandFailed { response_code } = err {
            assert_ne!(response_code, ResponseCode::Success as u32);
        } else {
            panic!()
        }
    }

    struct TpmtSensitive {
        /// TPMI_ALG_PUBLIC
        sensitive_type: AlgId,
        /// `TPM2B_AUTH`
        auth_value: Tpm2bBuffer,
        /// `TPM2B_DIGEST`
        seed_value: Tpm2bBuffer,
        /// `TPM2B_PRIVATE_KEY_RSA`
        sensitive: Tpm2bBuffer,
    }

    impl TpmtSensitive {
        fn serialize(&self) -> Vec<u8> {
            let mut buffer = Vec::new();

            buffer.extend_from_slice(self.sensitive_type.as_bytes());
            buffer.extend_from_slice(&self.auth_value.serialize());
            buffer.extend_from_slice(&self.seed_value.serialize());
            buffer.extend_from_slice(&self.sensitive.serialize());

            buffer
        }
    }

    fn generate_rsa() -> (TpmtPublic, Tpm2bBuffer) {
        // Using hard-coded value to avoid OpenSSL dependency
        // RSA-2k public modulus
        const N: [u8; 256] = [
            0xbc, 0x85, 0x76, 0x9b, 0x24, 0xf5, 0x55, 0x2b, 0x58, 0x77, 0xf5, 0xbd, 0x3d, 0x15,
            0x2f, 0xa4, 0x5b, 0xda, 0x17, 0x74, 0xd7, 0x97, 0x64, 0xd5, 0x64, 0x0a, 0x51, 0xb0,
            0x54, 0x98, 0xac, 0x8c, 0xf7, 0xb3, 0xf2, 0x45, 0x32, 0xf9, 0x99, 0xd2, 0x9e, 0xb4,
            0xf3, 0x49, 0xb7, 0xf2, 0x27, 0xe3, 0xe4, 0x5d, 0xa6, 0xe2, 0xc2, 0x0f, 0x58, 0x02,
            0x65, 0xf7, 0x8e, 0xe7, 0xd0, 0x41, 0x8a, 0xd4, 0xa2, 0x71, 0x7d, 0x0f, 0x27, 0x51,
            0x94, 0x9b, 0x5d, 0xd3, 0x0e, 0x05, 0xe0, 0xae, 0x2e, 0x2f, 0x3c, 0xfd, 0x46, 0x28,
            0x0a, 0x70, 0x59, 0x74, 0x5a, 0xd7, 0xac, 0x54, 0x92, 0x89, 0xb2, 0xec, 0xb8, 0x38,
            0xdf, 0x4d, 0xdb, 0x54, 0xa7, 0x9f, 0x00, 0xba, 0x9b, 0x8d, 0x2e, 0xee, 0x60, 0xd3,
            0x47, 0xea, 0x70, 0x53, 0xb9, 0x26, 0x7b, 0x1f, 0x82, 0x33, 0x22, 0x65, 0x7a, 0x60,
            0xe0, 0xba, 0xdf, 0x60, 0x55, 0xcc, 0xc2, 0x07, 0x16, 0x7f, 0x6c, 0x07, 0xf0, 0xf8,
            0xf5, 0xa6, 0xba, 0xea, 0xc0, 0x6d, 0x45, 0x38, 0x8d, 0xca, 0x0d, 0xa6, 0x98, 0x21,
            0xba, 0xdd, 0x27, 0x0f, 0x8d, 0x7e, 0x7c, 0x7a, 0xee, 0x44, 0xc7, 0xa7, 0xd4, 0x3d,
            0x39, 0x70, 0x4d, 0xde, 0xb1, 0x72, 0x56, 0x6e, 0xe9, 0x50, 0x69, 0x46, 0x56, 0xd9,
            0x83, 0x89, 0x8e, 0xe6, 0xf7, 0x7b, 0xce, 0xf0, 0x75, 0x8e, 0x18, 0xea, 0x22, 0xc5,
            0x62, 0xa7, 0x6b, 0x59, 0x80, 0xe8, 0x68, 0xb2, 0x57, 0xdc, 0xfe, 0xd1, 0xe0, 0xda,
            0xeb, 0x0f, 0x12, 0x64, 0xb2, 0x7a, 0x1f, 0x1a, 0x97, 0xa9, 0xb6, 0xdd, 0xd7, 0x78,
            0x82, 0x90, 0x07, 0xa1, 0x9d, 0x00, 0xff, 0xa9, 0x52, 0xe3, 0x0a, 0xa8, 0xa5, 0x2f,
            0xcd, 0xdf, 0x79, 0xec, 0x35, 0xb4, 0x81, 0xad, 0xa9, 0x45, 0x50, 0x30, 0x58, 0x0b,
            0xed, 0xdf, 0x10, 0x69,
        ];
        // RSA-2k private prime
        const P: [u8; 128] = [
            0xe8, 0x66, 0x31, 0x98, 0xe7, 0xab, 0xd7, 0xbe, 0x1f, 0xa9, 0x13, 0xe2, 0xd0, 0x4d,
            0xd0, 0x0a, 0xb0, 0xd1, 0x39, 0xc0, 0xc3, 0x6f, 0x4b, 0xdc, 0x4d, 0xe2, 0x03, 0xf9,
            0xd4, 0xd9, 0xb5, 0x47, 0x94, 0x97, 0x5b, 0x51, 0xe3, 0x1a, 0x25, 0x7f, 0x14, 0x50,
            0xe8, 0x12, 0x21, 0xd0, 0x0e, 0x51, 0x9a, 0xc3, 0xc5, 0x05, 0x55, 0xe8, 0x31, 0xb8,
            0x44, 0xbd, 0x71, 0xa6, 0x5b, 0x88, 0x05, 0x7b, 0x75, 0xd9, 0x75, 0xba, 0x43, 0x55,
            0x6a, 0x72, 0x15, 0x0e, 0xd4, 0x09, 0xab, 0x69, 0xee, 0xac, 0x3b, 0x68, 0x13, 0x54,
            0x43, 0x63, 0x73, 0xb7, 0x7b, 0x5d, 0x2c, 0x01, 0xb4, 0x1e, 0xfc, 0x88, 0xfe, 0xa6,
            0x04, 0x27, 0xba, 0x17, 0x0a, 0x7e, 0xc3, 0xa8, 0xea, 0xb9, 0x37, 0x6d, 0x81, 0x91,
            0x6a, 0x70, 0xfa, 0x4f, 0x18, 0xfb, 0xcf, 0x7b, 0x45, 0x12, 0xd7, 0x50, 0x64, 0xd6,
            0xc8, 0x73,
        ];

        let symmetric = TpmtSymDefObject::new(AlgIdEnum::NULL.into(), None, None);
        let scheme = TpmtRsaScheme::new(AlgIdEnum::RSASSA.into(), Some(AlgIdEnum::SHA256.into()));
        let rsa_params = TpmsRsaParams::new(symmetric, scheme, crate::RSA_2K_MODULUS_BITS, 0);

        let object_attributes = TpmaObjectBits::new()
            .with_user_with_auth(true)
            .with_sign_encrypt(true);

        let unique = {
            let mut data = [0u8; crate::RSA_2K_MODULUS_SIZE];
            data.copy_from_slice(&N);

            data
        };

        let result = TpmtPublic::new(
            AlgIdEnum::RSA.into(),
            AlgIdEnum::SHA256.into(),
            object_attributes,
            &[],
            rsa_params,
            &unique,
        );
        assert!(result.is_ok());

        let rsa_public = result.unwrap();

        let result = Tpm2bBuffer::new(&P);
        assert!(result.is_ok());
        let sensitive = result.unwrap();

        let rsa_sensitive = TpmtSensitive {
            sensitive_type: AlgIdEnum::RSA.into(),
            auth_value: Tpm2bBuffer::new_zeroed(),
            seed_value: Tpm2bBuffer::new_zeroed(),
            sensitive,
        };

        let result = Tpm2bBuffer::new(&rsa_sensitive.serialize());
        assert!(result.is_ok());
        let rsa_private = result.unwrap();

        (rsa_public, rsa_private)
    }

    fn rsa_srk_template() -> Result<TpmtPublic, TpmHelperUtilityError> {
        let symmetric = TpmtSymDefObject::new(
            AlgIdEnum::AES.into(),
            Some(128),
            Some(AlgIdEnum::CFB.into()),
        );
        let scheme = TpmtRsaScheme::new(AlgIdEnum::NULL.into(), None);
        let rsa_params = TpmsRsaParams::new(symmetric, scheme, crate::RSA_2K_MODULUS_BITS, 0);

        let object_attributes = TpmaObjectBits::new()
            .with_fixed_tpm(true)
            .with_fixed_parent(true)
            .with_sensitive_data_origin(true)
            .with_user_with_auth(true)
            .with_no_da(true)
            .with_restricted(true)
            .with_decrypt(true);

        let in_public = TpmtPublic::new(
            AlgIdEnum::RSA.into(),
            AlgIdEnum::SHA256.into(),
            object_attributes,
            &[],
            rsa_params,
            &[0u8; crate::RSA_2K_MODULUS_SIZE],
        )
        .map_err(TpmHelperUtilityError::InvalidInputParameter)?;

        Ok(in_public)
    }

    #[test]
    fn test_import_load() {
        let mut tpm_engine_helper = create_tpm_engine_helper();
        restart_tpm_engine(&mut tpm_engine_helper, false, true);

        // Create SRK
        let result = rsa_srk_template();
        assert!(result.is_ok());
        let rsa_srk_template = result.unwrap();

        // Positive tests

        let auth_handle = TPM20_RH_OWNER;
        let result = tpm_engine_helper.create_primary(auth_handle, rsa_srk_template);
        assert!(result.is_ok());
        let create_primary_reply = result.unwrap();
        assert_ne!(create_primary_reply.out_public.size.get(), 0);

        let (rsa_public, rsa_private) = generate_rsa();
        let object_public = Tpm2bPublic::new(rsa_public);
        let result = Tpm2bBuffer::new(&rsa_private.serialize());
        assert!(result.is_ok());
        let duplicate = result.unwrap();
        let in_sym_seed = Tpm2bBuffer::new_zeroed();

        let result = tpm_engine_helper.import(
            create_primary_reply.object_handle,
            &object_public,
            &duplicate,
            &in_sym_seed,
        );
        assert!(result.is_ok());
        let import_reply = result.unwrap();

        let in_public = object_public;
        let in_private = import_reply.out_private;

        let result =
            tpm_engine_helper.load(create_primary_reply.object_handle, &in_private, &in_public);
        assert!(result.is_ok());

        // Negative tests

        let invalid_auth_handle = ReservedHandle(0.into());
        let result = tpm_engine_helper.import(
            invalid_auth_handle,
            &object_public,
            &duplicate,
            &in_sym_seed,
        );
        assert!(result.is_err());
        let err = result.unwrap_err();
        if let TpmCommandError::TpmCommandFailed { response_code } = err {
            assert_ne!(response_code, ResponseCode::Success as u32);
        } else {
            panic!()
        }

        let result = tpm_engine_helper.load(invalid_auth_handle, &in_private, &in_public);
        assert!(result.is_err());
        let err = result.unwrap_err();
        if let TpmCommandError::TpmCommandFailed { response_code } = err {
            assert_ne!(response_code, ResponseCode::Success as u32);
        } else {
            panic!()
        }
    }
}
