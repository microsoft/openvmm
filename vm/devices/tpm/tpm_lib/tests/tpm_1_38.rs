// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Tests the TPM helpers against the TPM 1.38 backend.

mod test_tpm_backend {
    pub(crate) use ms_tpm_20_ref::DynResult;
    pub(crate) use ms_tpm_20_ref::InitKind;
    pub(crate) use ms_tpm_20_ref::PlatformCallbacks;

    pub(crate) type TestTpmPlatform = ms_tpm_20_ref::MsTpm20RefPlatform;
    pub(crate) const USE_LEGACY_PREPROVISIONED_STATE: bool = true;
}

/// TPM helper implementation and shared tests.
#[path = "../src/lib.rs"]
pub mod tpm_lib;

#[path = "common/mod.rs"]
mod tests;
