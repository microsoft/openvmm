// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Shared TPM protocol constants and helpers used across the vTPM stack.

pub mod tpm20proto;

use tpm20proto::NV_INDEX_RANGE_BASE_PLATFORM_MANUFACTURER;
use tpm20proto::NV_INDEX_RANGE_BASE_TCG_ASSIGNED;
use tpm20proto::ReservedHandle;
use tpm20proto::TpmaObject;
use tpm20proto::TpmaObjectBits;

/// Reserved handle for the Storage Root Key (SRK).
pub const TPM_RSA_SRK_HANDLE: ReservedHandle = ReservedHandle::new(TPM20_HT_PERSISTENT, 0x01);

/// Reserved handle for the Azure-provisioned Attestation Key (AK).
pub const TPM_AZURE_AIK_HANDLE: ReservedHandle = ReservedHandle::new(TPM20_HT_PERSISTENT, 0x03);

/// Reserved handle for the persisted guest secret key.
pub const TPM_GUEST_SECRET_HANDLE: ReservedHandle = ReservedHandle::new(TPM20_HT_PERSISTENT, 0x04);

/// NV index used to store the attestation key certificate payload.
pub const TPM_NV_INDEX_AIK_CERT: u32 = NV_INDEX_RANGE_BASE_TCG_ASSIGNED + 0x0001_01d0;

/// NV index used to mark that legacy vTPM mitigation has been applied.
pub const TPM_NV_INDEX_MITIGATED: u32 = NV_INDEX_RANGE_BASE_TCG_ASSIGNED + 0x0001_01d2;

/// NV index used to persist the most recent attestation report.
pub const TPM_NV_INDEX_ATTESTATION_REPORT: u32 =
    NV_INDEX_RANGE_BASE_PLATFORM_MANUFACTURER + 0x0000_0001;

/// NV index used to persist the latest guest attestation input blob.
pub const TPM_NV_INDEX_GUEST_ATTESTATION_INPUT: u32 =
    NV_INDEX_RANGE_BASE_PLATFORM_MANUFACTURER + 0x0000_0002;

/// Expected object attributes for a correctly provisioned Attestation Key.
pub fn expected_ak_attributes() -> TpmaObject {
    TpmaObjectBits::new()
        .with_fixed_tpm(true)
        .with_fixed_parent(true)
        .with_sensitive_data_origin(true)
        .with_user_with_auth(true)
        .with_no_da(true)
        .with_restricted(true)
        .with_sign_encrypt(true)
        .into()
}

pub use tpm20proto::AlgId;
pub use tpm20proto::ResponseCode;
pub use tpm20proto::TPM20_HT_PERSISTENT;
pub use tpm20proto::TPM20_RH_ENDORSEMENT;
pub use tpm20proto::TPM20_RH_OWNER;
pub use tpm20proto::TPM20_RH_PLATFORM;
pub use tpm20proto::TPM20_RS_PW;
