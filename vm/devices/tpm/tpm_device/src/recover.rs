// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Code to recover a corrupt TPM NVRAM blob due to truncation.

use crate::LEGACY_VTPM_SIZE;

/// Check if the TPM blob's persistent data structures all fit inside it.
///
/// This can return false if the blob was incorrectly truncated (by a previous
/// bug that reported a 32KB blob size for a 16KB blob).
fn check_blob(blob: &[u8]) -> Result<(), usize> {
    const NV_USER_DYNAMIC: usize = 3508; // from the TPM reference implementation
    let mut i = NV_USER_DYNAMIC;
    loop {
        let size = u32::from_ne_bytes(blob[i..].get(..4).ok_or(i)?.try_into().unwrap());
        if size == 0 {
            break;
        }
        if blob[i..].get(..size as usize).is_none() {
            return Err(i);
        }
        i += size as usize;
    }
    Ok(())
}

/// Fixup an NVRAM blob that has data past the end, by zeroing out the last
/// header that refers to data past the end.
pub fn recover_blob(blob: &mut [u8]) {
    let original_size = blob.len();
    if original_size != LEGACY_VTPM_SIZE {
        tracing::debug!("TPM NVRAM size is not legacy size, skipping recovery");
        return;
    }

    match check_blob(blob) {
        Ok(()) => {
            tracing::info!("TPM NVRAM is already good, skipping recovery");
        }
        Err(bad_offset) => {
            tracing::warn!("TPM NVRAM blob is corrupt, truncating at offset {bad_offset}");
            blob[bad_offset..].fill(0);
        }
    }
}
