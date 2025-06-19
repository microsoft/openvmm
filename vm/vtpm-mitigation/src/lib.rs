// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! The crate is a thin wrapper around the functions exposed by the tpm crate
//! for vTPM state corruption detection and recovery. The crates exposes two FFI interfaces
//! IsValidVtpmBlob and RecoverVtpmBlob. IsValidVtpmBlob is used to check if the given blob is a valid
//! TPM blob. RecoverVtpmBlob is used to recover the given blob and return the recovered blob.

// SAFETY: We need to use unsafe code to create Error types with Send and Sync markers
#![expect(unsafe_code)]

mod errors;

use ms_tpm_20_ref::DynResult;
use ms_tpm_20_ref::MsTpm20RefPlatform;
use parking_lot::Mutex;
use std::borrow::Cow;
use std::sync::Arc;
use std::time::Instant;
use tpm::recover::check_blob;
use tpm::tpm_helper::TpmEngineHelper;

use crate::errors::PlatformError;
use crate::errors::TpmStateRecoveryError;
use crate::errors::TpmStateValidationError;

/// STATUS_SUCCESS: The operation completed successfully.
pub const STATUS_SUCCESS: u64 = 0x0;

/// LEGACY_VTPM_SIZE: The size of the legacy vTPM blob
pub const LEGACY_VTPM_SIZE: usize = 16 * 1024;

// Mutex to protect the global TPM state while concurrently recover calls are made
static RECOVER_MUTEX: Mutex<()> = Mutex::new(());

struct TpmPlatformCallbacks {
    pending_nvram: Arc<Mutex<Vec<u8>>>,
    time: Instant,
}

impl ms_tpm_20_ref::PlatformCallbacks for TpmPlatformCallbacks {
    fn commit_nv_state(&mut self, state: &[u8]) -> DynResult<()> {
        *self.pending_nvram.lock() = state.to_vec();
        tracing::info!("new commit made to nvram with size: {} bytes", state.len());
        Ok(())
    }

    fn get_crypt_random(&mut self, buf: &mut [u8]) -> DynResult<usize> {
        match getrandom::fill(buf) {
            Ok(()) => Ok(buf.len()),
            Err(_) => Err(Box::new(PlatformError::ErrorRngGenerator)),
        }
    }

    fn monotonic_timer(&mut self) -> std::time::Duration {
        self.time.elapsed()
    }

    fn get_unique_value(&self) -> &'static [u8] {
        b"hvlite vtpm"
    }
}

/// FFI to check if the given blob is a valid TPM blob
/// # Safety
/// The caller should ensure that the input blob pointer is valid and size is within the bounds
#[allow(non_snake_case)]
#[unsafe(no_mangle)]
// SAFETY: The caller should ensure that the input blob pointer is valid and size is within the bounds
pub unsafe extern "C" fn IsValidVtpmBlob(
    blob: *const u8,
    size: usize,
    err_offset: *mut u64,
) -> u64 {
    if blob.is_null() || err_offset.is_null() || size == 0 {
        tracing::error!("Input blob is null");
        return TpmStateValidationError::INVALID_PARAMETER_ERROR;
    }
    // SAFETY: The caller should ensure that the input pointer is valid and size is within the bounds
    let blob = unsafe { std::slice::from_raw_parts(blob, size) };
    // SAFETY: The caller should ensure that the output pointer is valid
    unsafe { *err_offset = 0 };

    let result = is_valid_tpm_blob(blob);
    match result {
        Ok(_) => STATUS_SUCCESS,
        Err(offset) => {
            // SAFETY: The caller should ensure that the output pointer is valid
            unsafe { *err_offset = offset as u64 };
            TpmStateValidationError::INVALID_TPM_STATE
        }
    }
}

/// FFI to Recover the given blob and return the recovered blob
/// # Safety
/// The caller should ensure that the input and output blob pointers are valid and size is within the bounds
#[allow(non_snake_case)]
#[unsafe(no_mangle)]
pub unsafe extern "C" fn RecoverVtpmBlob(
    blob: *mut u8,
    size: usize,
    out_blob: *mut u8,
    out_blob_size: usize,
) -> u64 {
    if blob.is_null() || out_blob.is_null() || size == 0 || out_blob_size == 0 {
        tracing::error!("Input or output blob is null");
        return TpmStateRecoveryError::INVALID_PARAMETER_ERROR;
    }

    if size != out_blob_size {
        tracing::error!("Input and output blob size should be same");
        return TpmStateRecoveryError::INPUT_OUTPUT_BLOB_SIZE_MISMATCH;
    }

    // SAFETY: The caller should ensure that the input and output blob pointers are valid and size is within the bounds
    let blob = unsafe { std::slice::from_raw_parts(blob, size) };
    // SAFETY: The caller should ensure that the output blob pointer is valid and size is within the bounds
    let out_blob = unsafe { std::slice::from_raw_parts_mut(out_blob, out_blob_size) };

    out_blob.fill(0);

    match recover_vtpm_blob(blob) {
        Ok(data) => {
            out_blob.copy_from_slice(&data);
            STATUS_SUCCESS
        }
        Err(err) => err.into(),
    }
}

/// Check if the given blob is a valid TPM blob
fn is_valid_tpm_blob(blob: &[u8]) -> Result<(), usize> {
    let result = check_blob(blob);
    if let Err(size) = result {
        Err(size)
    } else {
        Ok(())
    }
}

/// Recover the given blob and return the recovered blob
fn recover_vtpm_blob(original_blob: &[u8]) -> Result<Vec<u8>, TpmStateRecoveryError> {
    let _guard = RECOVER_MUTEX.lock();
    if original_blob.len() > LEGACY_VTPM_SIZE {
        tracing::error!("Blob size is greater than the legacy vtpm size, skipping recovery");
        return Err(TpmStateRecoveryError::INVALID_PARAMETER_ERROR.into());
    }

    if check_blob(original_blob).is_ok() {
        tracing::info!("TPM NVRAM is already good, skipping recovery");
        return Err(TpmStateRecoveryError::ALREADY_VALID.into());
    }

    let mut tpm_state_blob = Vec::from(original_blob);

    tpm::recover::recover_blob(tpm_state_blob.as_mut());
    let pending_nvram = Arc::new(Mutex::new(Vec::new()));
    let plat = TpmPlatformCallbacks {
        pending_nvram: pending_nvram.clone(),
        time: Instant::now(),
    };

    let state_cow: Cow<'_, [u8]> = Cow::Borrowed(&tpm_state_blob);

    let tpm_engine = MsTpm20RefPlatform::initialize(
        Box::new(plat),
        ms_tpm_20_ref::InitKind::ColdInitWithPersistentState {
            nvmem_blob: state_cow,
        },
    );

    if tpm_engine.is_err() {
        tracing::error!("Failed to recover the blob, error: {:?}", tpm_engine);
        return Err(TpmStateRecoveryError::TPM_ENGINE_INIT_FAILED.into());
    }

    let mut tpm_engine_helper = TpmEngineHelper {
        tpm_engine: tpm_engine.unwrap(),
        reply_buffer: [0u8; 4096],
    };

    let result = tpm_engine_helper.initialize_tpm_engine();
    if result.is_err() {
        tracing::error!("Failed to initialize the tpm engine, error: {:?}", result);
        return Err(TpmStateRecoveryError::TPM_ENGINE_INIT_FAILED.into());
    }

    // TODO: line 658 doesn't propagate the error, change signature to accept a value to return the error.
    let result = tpm_engine_helper.allocate_guest_attestation_nv_indices(0, true, false, true);
    if result.is_err() {
        tracing::error!(
            "Failed to allocate the guest attestation nv indices, error: {:?}",
            result
        );
        return Err(TpmStateRecoveryError::TPM_COMMAND_FAILED.into());
    }

    let recovered_state = {
        let mut pending_nvram = pending_nvram.lock();
        std::mem::take(&mut *pending_nvram)
    };

    drop(tpm_engine_helper);

    if recovered_state.len() != tpm_state_blob.len() {
        tracing::error!("Recovered blob size is not same as the original blob size");
        return Err(TpmStateRecoveryError::NVRAM_SIZE_MISMATCH.into());
    }
    Ok(recovered_state)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tracing_subscriber::EnvFilter;

    pub fn setup_logging() {
        tracing_subscriber::fmt()
            .with_env_filter(
                EnvFilter::from_default_env().add_directive(tracing::Level::DEBUG.into()),
            )
            .init();
    }

    #[test]
    fn test_corrupted_tpm_state_is_recovered() {
        setup_logging();

        let corrupt_state = include_bytes!("../test-data/corrupted_blob.bin");

        let is_ok = is_valid_tpm_blob(corrupt_state.as_slice());

        assert!(is_ok.is_err());

        if let Err(size) = is_ok {
            tracing::error!("Blob is corrupted at index: {}", size);
            let recovered_state = recover_vtpm_blob(corrupt_state.as_slice());

            assert!(recovered_state.is_ok());

            tracing::info!("Blob is recovered");

            let result = check_blob(recovered_state.unwrap().as_slice());
            assert!(result.is_ok());
        }
    }
}
