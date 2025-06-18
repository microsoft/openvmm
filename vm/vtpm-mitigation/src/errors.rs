
use std::fmt::{Display, Formatter};
use std::error::Error;


#[derive(Debug)]
pub enum PlatformError {
    ErrorRngGenerator,
}

impl Error for PlatformError {
    fn description(&self) -> &str {
        match self {
            PlatformError::ErrorRngGenerator => "Error in Rng Generator",
        }
    }
}

impl Display for PlatformError {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.to_string())
    }
}
unsafe impl Send for PlatformError {}
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

impl Error for TpmStateRecoveryError {
    fn description(&self) -> &str {
        match self.0 {
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
        }
    }
}

impl Display for TpmStateRecoveryError {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.to_string())
    }
}

impl From<TpmStateRecoveryError> for u64 {
    fn from(err: TpmStateRecoveryError) -> u64 {
        err.0 as u64
    }
}

impl From<u64> for TpmStateRecoveryError {
    fn from(err: u64) -> TpmStateRecoveryError {
        TpmStateRecoveryError(err as u64)
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
        err.0 as u64
    }
}

impl From<u64> for TpmStateValidationError {
    fn from(err: u64) -> TpmStateValidationError {
        TpmStateValidationError(err as u64)
    }
}

impl Error for TpmStateValidationError {
    fn description(&self) -> &str {
        match self.0 {
            TpmStateValidationError::INVALID_TPM_STATE => {
                "The input blob is not a valid TPM state blob with the error offset"
            }
            TpmStateValidationError::INVALID_PARAMETER_ERROR => "Invalid pointer error",
            _ => "Unknown error",
        }
    }
}

impl Display for TpmStateValidationError {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.to_string())
    }
}