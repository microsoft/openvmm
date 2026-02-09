// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Error-types associated with various GET client methods.

use mesh::MeshPayload;
use thiserror::Error;

/// Error while issuing VMGS IO over the GET
#[derive(Debug, Error)]
#[error("vmgs io error: {0:?}")]
pub struct VmgsIoError(pub(crate) get_protocol::VmgsIoStatus);

/// Error while fetching Device Platform Settings
#[expect(missing_docs)] // self-explanatory fields
#[derive(Debug, Error)]
pub enum DevicePlatformSettingsError {
    #[error("unknown secure boot template type: {0:?}")]
    UnknownSecureBootTemplateType(get_protocol::SecureBootTemplateType),
    #[error("invalid console mode (must be 0b00..=0b11): {}", 0.0)]
    InvalidConsoleMode(get_protocol::UefiConsoleMode),
    #[error("invalid memory protection mode (must be 0b00..=0b11): {0}")]
    InvalidMemoryProtectionMode(u8),
    #[error("could not parse DPSv2 JSON")]
    BadJson(#[source] serde_json::Error),
    #[error("could not parse embedded VTL2 settings data")]
    BadVtl2Settings(#[source] underhill_config::schema::ParseError),
    #[error("invalid legacy bool representation ({0:?})")]
    InvalidProtocolBool(#[from] InvalidProtocolBool),
}

/// Encountered GET Protocol bool with an invalid representation (not 0 or 1)
#[derive(Debug, Error)]
#[error("expected 0 or 1, found {0}")]
pub struct InvalidProtocolBool(pub(crate) u8);

/// Error while mapping framebuffer
#[derive(Debug, Error)]
#[error("map framebuffer error: {0:?}")]
pub struct MapFramebufferError(pub(crate) get_protocol::MapFramebufferStatus);

/// Error while unmapping framebuffer
#[derive(Debug, Error)]
#[error("unmap framebuffer error: {0:?}")]
pub struct UnmapFramebufferError(pub(crate) get_protocol::UnmapFramebufferStatus);

/// Error while performing a VPCI operation
#[derive(Debug, Error)]
#[error("vpci operation error: {0:?}")]
pub struct VpciControlError(pub(crate) get_protocol::VpciDeviceControlStatus);

/// Error while invoking a CreateRamGpaRangeRequest
#[derive(Debug, Error)]
#[error("create ram GPA range error: {0:?}")]
pub struct CreateRamGpaRangeError(pub(crate) get_protocol::CreateRamGpaRangeStatus);

/// Error while invoking a GuestStateProtectionByIdRequest
#[derive(Debug, Error)]
#[error("malformed response - reported len > actual len: {0} > {1}")]
pub struct GuestStateProtectionByIdError(pub(crate) u32, pub(crate) u32);

/// Error while performing save/restore operation
#[derive(Debug, Error)]
#[error("host rejected save/restore operation")]
#[non_exhaustive]
pub struct SaveRestoreOperationFailure {}

/// Error while invoking an IgvmAttestRequest
#[expect(missing_docs)] // self-explanatory fields
#[derive(Debug, Error, MeshPayload)]
pub enum IgvmAttestError {
    #[error("`agent_data` size {input_size} was larger than expected {expected_size}")]
    InvalidAgentDataSize {
        input_size: usize,
        expected_size: usize,
    },
    #[error("`report` size {input_size} was larger than expected {expected_size}")]
    InvalidReportSize {
        input_size: usize,
        expected_size: usize,
    },
    #[error("IGVM agent returned an error")]
    IgvmAgentGenericError,
}

pub(crate) trait TryIntoProtocolBool {
    fn into_bool(self) -> Result<bool, InvalidProtocolBool>;
}

impl TryIntoProtocolBool for get_protocol::ProtocolBool {
    fn into_bool(self) -> Result<bool, InvalidProtocolBool> {
        match self.0 {
            0 => Ok(false),
            1 => Ok(true),
            _ => Err(InvalidProtocolBool(self.0)),
        }
    }
}
