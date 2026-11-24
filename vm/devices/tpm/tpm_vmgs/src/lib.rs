// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Integration between TPM devices and VMGS-backed storage.

#![forbid(unsafe_code)]

use tpm_resources::TpmVersion;
use vmgs_format::FileId;

/// Returns the VMGS file ID used for the TPM version's NVRAM.
pub const fn tpm_nvram_file_id(version: TpmVersion) -> FileId {
    match version {
        TpmVersion::V138 => FileId::TPM_NVRAM,
        TpmVersion::V185 => FileId::TPM_185_NVRAM,
    }
}
