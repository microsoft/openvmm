// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Contains definitions used for logging certain interesting operations around
//! VM provisioning.

#![forbid(unsafe_code)]

/// Operation types for VM provisioning telemetry.
#[derive(Debug)]
pub enum LogOpType {
    /// Formatting a VMGS file
    VmgsProvision,
    /// Callback to agent to get GSP data
    GspCallback,
    /// VMGS file decryption
    DecryptVmgs,
    /// Converting VMGS file from GSP-by-ID to GSP Key encryption
    ConvertEncryptionType,
    /// VMGS decryption/encryption failed due to encryption policy
    StrictEncryptionFailure,
    /// Derivation of vTPM primary keys
    VtpmKeysProvision,
    /// Callback to obtain AK certificate
    AkCertProvision,
    /// Write to TPM NVRAM index
    NvWrite,
    /// Read from TPM NVRAM index
    NvRead,
}

/// Log a point-in-time operation. op_type is a LogOpType. The remaining
/// arguments are passed through to the underlying tracing macro.
#[macro_export]
macro_rules! log_op {
    ($op_type:expr, $($e:expr),*) => {
        tracing::info!(
            CVM_ALLOWED,
            op_type = ?$op_type,
            $($e),*
        );
    }
}

/// Log a point-in-time operation at warning level. op_type is a LogOpType. The
/// remaining arguments are passed through to the underlying tracing macro.
#[macro_export]
macro_rules! log_op_warn {
    ($op_type:expr, $($e:expr),*) => {
        tracing::warn!(
            CVM_ALLOWED,
            op_type = ?$op_type,
            $($e),*
        );
    }
}

/// Log the beginning of an operation. op_type is a LogOpType. The remaining
/// arguments are passed through to the underlying tracing macro.
#[macro_export]
macro_rules! log_op_begin {
    ($op_type:expr, $($e:tt)*) => {
        tracing::info!(
            CVM_ALLOWED,
            op_type = format!("Begin{:?}", $op_type),
            $($e)*
        );
    }
}

/// Log the end of an operation. Logs at info level if result is Ok or at error
/// level if result is Err. op_type is a LogOpType. start_time is a
/// std::time::SystemTime indicating when the operation started. The remaining
/// arguments are passed through to the underlying tracing macro.
#[macro_export]
macro_rules! log_op_end {
    ($op_type:expr, $result:expr, $start_time:expr, $($e:tt)*) => {
        if let Err(error) = $result.as_ref() {
            tracing::error!(
                CVM_ALLOWED,
                op_type = ?$op_type,
                success = false,
                err = error as &dyn std::error::Error,
                latency = std::time::SystemTime::now().duration_since($start_time).map_or(0, |d| d.as_millis()),
                $($e)*
            );
        } else {
            tracing::info!(
                CVM_ALLOWED,
                op_type = ?$op_type,
                success = true,
                latency = std::time::SystemTime::now().duration_since($start_time).map_or(0, |d| d.as_millis()),
                $($e)*
            );
        }
    }
}

/// Log the end of an operation that finished successfully. op_type is a
/// LogOpType. start_time is a std::time::SystemTime indicating when the
/// operation started. The remaining arguments are passed through to the
/// underlying tracing macro.
#[macro_export]
macro_rules! log_op_end_ok {
    ($op_type:expr, $start_time:expr, $($e:tt)*) => {
        tracing::info!(
            CVM_ALLOWED,
            op_type = ?$op_type,
            success = true,
            latency = std::time::SystemTime::now().duration_since($start_time).map_or(0, |d| d.as_millis()),
            $($e)*
        );
    }
}

/// Log the end of an operation that finished with an error. op_type is a
/// LogOpType. err is the error. start_time is a std::time::SystemTime
/// indicating when the operation started. The remaining arguments are passed
/// through to the underlying tracing macro.
#[macro_export]
macro_rules! log_op_end_err {
    ($op_type:expr, $err:expr, $start_time:expr, $($e:tt)*) => {
        tracing::error!(
            CVM_ALLOWED,
            op_type = ?$op_type,
            success = false,
            err = &$err as &dyn std::error::Error,
            latency = std::time::SystemTime::now().duration_since($start_time).map_or(0, |d| d.as_millis()),
            $($e)*
        );
    }
}
