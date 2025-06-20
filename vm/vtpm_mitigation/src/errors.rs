// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use std::error::Error;
use std::fmt::{Display, Formatter};

#[derive(Debug)]
pub enum PlatformError {
    ErrorRngGenerator,
}

impl Error for PlatformError {}

impl Display for PlatformError {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        let description = match self {
            PlatformError::ErrorRngGenerator => "Error in Rng Generator",
        };
        write!(f, "{}", description)
    }
}
// SAFETY: The enum is stateless and should be safe to send
unsafe impl Send for PlatformError {}
// SAFETY: The enum is stateless and should be safe to Sync
unsafe impl Sync for PlatformError {}

#[derive(Debug)]
pub struct TpmStateRecoveryError(pub u64);

impl TpmStateRecoveryError {
    pub const RECOVERY_FAILED: u64 = 0x1001;
    pub const INPUT_OUTPUT_BLOB_SIZE_MISMATCH: u64 = 0x1002;
    pub const ALREADY_VALID: u64 = 0x1003;
    pub const TPM_ENGINE_INIT_FAILED: u64 = 0x1004;
    pub const TPM_COMMAND_FAILED: u64 = 0x1005;
    pub const NVRAM_SIZE_MISMATCH: u64 = 0x1006;
    pub const INVALID_BLOB: u64 = 0x1007;
    pub const INVALID_PARAMETER_ERROR: u64 = 0x4001;
}

impl Error for TpmStateRecoveryError {}

impl Display for TpmStateRecoveryError {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        let description = match self.0 {
            TpmStateRecoveryError::RECOVERY_FAILED => "vTPM NVRAM Recovery failed",
            TpmStateRecoveryError::INPUT_OUTPUT_BLOB_SIZE_MISMATCH => {
                "The input and output blob size should be same"
            }
            TpmStateRecoveryError::INVALID_PARAMETER_ERROR => "Invalid parameter error",
            TpmStateRecoveryError::ALREADY_VALID => "The input blob is a valid TPM state blob",
            TpmStateRecoveryError::TPM_ENGINE_INIT_FAILED => "TPM Engine initialization failed",
            TpmStateRecoveryError::TPM_COMMAND_FAILED => "TPM Command failed",
            TpmStateRecoveryError::NVRAM_SIZE_MISMATCH => "NVRAM size mismatch",
            TpmStateRecoveryError::INVALID_BLOB => "The input blob is not a valid TPM state blob",
            _ => "Unknown error",
        };
        write!(f, "{}", description)
    }
}

impl From<TpmStateRecoveryError> for u64 {
    fn from(err: TpmStateRecoveryError) -> u64 {
        err.0
    }
}

impl From<u64> for TpmStateRecoveryError {
    fn from(err: u64) -> TpmStateRecoveryError {
        TpmStateRecoveryError(err)
    }
}

#[derive(Debug)]
pub struct TpmStateValidationError(u64);

impl TpmStateValidationError {
    pub const INVALID_TPM_STATE: u64 = 0x2001;
    pub const INVALID_PARAMETER_ERROR: u64 = 0x4001;

    pub fn new(err: u64) -> TpmStateValidationError {
        TpmStateValidationError(err)
    }
}

impl From<TpmStateValidationError> for u64 {
    fn from(err: TpmStateValidationError) -> u64 {
        err.0
    }
}

impl From<u64> for TpmStateValidationError {
    fn from(err: u64) -> TpmStateValidationError {
        TpmStateValidationError(err)
    }
}

impl Error for TpmStateValidationError {}

impl Display for TpmStateValidationError {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        let description = match self.0 {
            TpmStateValidationError::INVALID_TPM_STATE => {
                "The input blob is not a valid TPM state blob with the error offset"
            }
            TpmStateValidationError::INVALID_PARAMETER_ERROR => "Invalid pointer error",
            _ => "Unknown error",
        };
        write!(f, "{}", description)
    }
}
