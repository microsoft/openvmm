// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Structures and constants for the user-facing attestation report that
//! is accessible via TPM NV Index 0x01400001. The definition is based
//! on `openhcl/openhcl_attestation_protocol/src/igvm_agemt/get.rs` with
//! IgvmAttestRequestVersion as VERSION_1.

use std::mem::size_of;

use zerocopy::FromBytes;
use zerocopy::Immutable;
use zerocopy::IntoBytes;

pub const IGVM_ATTESTATION_SIGNATURE: u32 = 0x414c_4348;
pub const IGVM_ATTESTATION_VERSION: u32 = 2;
pub const IGVM_ATTESTATION_REPORT_SIZE_MAX: usize = 0x4a0;
pub const IGVM_ATTEST_REQUEST_VERSION_1: u32 = 1;
pub const IGVM_REQUEST_TYPE_AK_CERT: u32 = 2;

#[repr(C)]
#[derive(Clone, Copy, IntoBytes, FromBytes, Immutable, Default)]
pub struct IgvmAttestRequestHeader {
    pub signature: u32,
    pub version: u32,
    pub report_size: u32,
    pub request_type: u32,
    pub status: u32,
    pub reserved: [u32; 3],
}

#[repr(C)]
#[derive(Clone, Copy, IntoBytes, FromBytes, Immutable, Default)]
pub struct IgvmAttestRequestData {
    pub data_size: u32,
    pub version: u32,
    pub report_type: u32,
    pub report_data_hash_type: u32,
    pub variable_data_size: u32,
}

#[repr(C)]
#[derive(Clone, Copy, IntoBytes, FromBytes, Immutable)]
pub struct IgvmAttestRequestBase {
    pub header: IgvmAttestRequestHeader,
    pub attestation_report: [u8; IGVM_ATTESTATION_REPORT_SIZE_MAX],
    pub request_data: IgvmAttestRequestData,
}

pub const IGVM_REQUEST_DATA_SIZE: usize = size_of::<IgvmAttestRequestData>();
pub const IGVM_REQUEST_BASE_SIZE: usize = size_of::<IgvmAttestRequestBase>();
pub const IGVM_REQUEST_DATA_OFFSET: usize = IGVM_REQUEST_BASE_SIZE - IGVM_REQUEST_DATA_SIZE;

impl Default for IgvmAttestRequestBase {
    fn default() -> Self {
        Self {
            header: IgvmAttestRequestHeader::default(),
            attestation_report: [0u8; IGVM_ATTESTATION_REPORT_SIZE_MAX],
            request_data: IgvmAttestRequestData::default(),
        }
    }
}
