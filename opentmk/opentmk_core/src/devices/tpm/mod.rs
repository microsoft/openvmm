// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! TPM device interface and utility functions.

use zerocopy::IntoBytes;

pub mod tpm_protocol;
use tpm_protocol::SessionTagEnum;
use tpm_protocol::TpmCommandError;
use tpm_protocol::protocol::SelfTestCmd;
use tpm_protocol::protocol::TpmCommand;

/// Trait representing a TPM device.
pub trait TpmDevice<'a> {
    /// Sets the command buffer.
    fn set_command_buffer(&mut self, buffer: &'a mut [u8]);
    /// Sets the response buffer.
    fn set_response_buffer(&mut self, buffer: &'a mut [u8]);
    /// Copies data from the response buffer to the provided buffer.
    fn copy_from_response_buffer(&self, buffer: &mut [u8]);
    /// Copies data to the command buffer from the provided buffer.
    fn copy_to_command_buffer(&mut self, buffer: &[u8]);
    /// Gets the TPM protocol version.
    fn get_tcg_protocol_version() -> u32
    where
        Self: Sized;
    /// Gets the mapped shared memory address.
    fn get_mapped_shared_memory() -> u32
    where
        Self: Sized;
    /// Maps the shared memory to the given GPA.
    fn map_shared_memory(gpa: u32) -> u32
    where
        Self: Sized;
    /// Submits a command to the TPM and returns the response.
    fn submit_command(&mut self, buffer: &[u8]) -> [u8; 4096];
    /// Executes the command without checking the command buffer.
    /// Used for finer control.
    fn execute_command_no_check()
    where
        Self: Sized;
}

/// Utility functions for TPM operations.
pub struct TpmUtil;
impl TpmUtil {
    /// Returns a TPM self-test command buffer.
    pub fn get_self_test_cmd() -> [u8; 4096] {
        let session_tag = SessionTagEnum::NoSessions;
        let cmd = SelfTestCmd::new(session_tag.into(), true);
        let mut buffer = [0; 4096];
        buffer[..cmd.as_bytes().len()].copy_from_slice(cmd.as_bytes());
        buffer
    }

    /// Executes a TPM self-test using the provided TPM device.
    pub fn exec_self_test(tpm_device: &mut dyn TpmDevice<'_>) -> Result<(), TpmCommandError> {
        let session_tag = SessionTagEnum::NoSessions;
        let cmd = SelfTestCmd::new(session_tag.into(), true);
        let response = tpm_device.submit_command(cmd.as_bytes());

        match SelfTestCmd::base_validate_reply(&response, session_tag) {
            Err(error) => Err(TpmCommandError::InvalidResponse(error)),
            Ok((res, false)) => Err(TpmCommandError::TpmCommandFailed {
                response_code: res.header.response_code.get(),
            })?,
            Ok((_res, true)) => Ok(()),
        }
    }
}
