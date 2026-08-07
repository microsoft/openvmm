// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Tests the TPM helpers against the TPM 1.85 backend.

mod test_tpm_backend {
    pub(crate) use ms_tcg_tpm_sys::DynResult;
    pub(crate) use ms_tcg_tpm_sys::InitKind;
    pub(crate) use ms_tcg_tpm_sys::PlatformCallbacks;

    pub(crate) type TestTpmPlatform = ms_tcg_tpm_sys::MsTpm185Platform;
    // TODO: Create a pre-provisioned state for TPM 1.85 and replace this bool with a path.
    pub(crate) const USE_LEGACY_PREPROVISIONED_STATE: bool = false;
}

/// TPM helper implementation and shared tests.
#[path = "../src/lib.rs"]
pub mod tpm_lib;

#[path = "common/mod.rs"]
mod tests;
