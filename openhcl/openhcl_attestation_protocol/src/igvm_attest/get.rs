// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! The module helps preparing requests and parsing responses that are
//! sent to and received from the IGVm agent runs on the host via GET
//! `IGVM_ATTEST` host request.

use bitfield_struct::bitfield;
use open_enum::open_enum;
use zerocopy::FromBytes;
use zerocopy::Immutable;
use zerocopy::IntoBytes;
use zerocopy::KnownLayout;

const ATTESTATION_VERSION: u32 = 2;
const ATTESTATION_SIGNATURE: u32 = 0x414c4348; // 'HCLA'
/// The value is based on the maximum report size of the supported isolated VM
/// Currently it's the size of a SNP report.
const ATTESTATION_REPORT_SIZE_MAX: usize = SNP_VM_REPORT_SIZE;

pub const VBS_VM_REPORT_SIZE: usize = hvdef::vbs::VBS_REPORT_SIZE;
pub const SNP_VM_REPORT_SIZE: usize = x86defs::snp::SNP_REPORT_SIZE;
pub const TDX_VM_REPORT_SIZE: usize = x86defs::tdx::TDX_REPORT_SIZE;
/// No TEE attestation report for TVM
pub const TVM_REPORT_SIZE: usize = 0;

const PAGE_SIZE: usize = 4096;

/// Number of pages required by the response buffer of WRAPPED_KEY request
/// Currently the number matches the maximum value defined by `get_protocol`
pub const WRAPPED_KEY_RESPONSE_BUFFER_SIZE: usize = 16 * PAGE_SIZE;
/// Number of pages required by the response buffer of KEY_RELEASE request
/// Currently the number matches the maximum value defined by `get_protocol`
pub const KEY_RELEASE_RESPONSE_BUFFER_SIZE: usize = 16 * PAGE_SIZE;
/// Number of pages required by the response buffer of AK_CERT request
/// Currently the AK cert request only requires 1 page.
pub const AK_CERT_RESPONSE_BUFFER_SIZE: usize = PAGE_SIZE;

/// Current IGVM Attest response header version.
pub const IGVM_ATTEST_RESPONSE_CURRENT_VERSION: IgvmAttestResponseVersion =
    IgvmAttestResponseVersion::VERSION_2;

open_enum! {
    /// IGVM Attest response header versions.
    #[derive(Default, IntoBytes, Immutable, KnownLayout, FromBytes)]
    pub enum IgvmAttestResponseVersion: u32 {
        /// Version 1
        VERSION_1 = 1,
        /// Version 2
        VERSION_2 = 2,
    }
}

/// Current IGVM Attest request header version.
pub const IGVM_ATTEST_REQUEST_CURRENT_VERSION: IgvmAttestRequestVersion =
    IgvmAttestRequestVersion::VERSION_2;

open_enum! {
    /// IGVM Attest request header versions.
    #[derive(IntoBytes, Immutable, KnownLayout, FromBytes)]
    pub enum IgvmAttestRequestVersion: u32 {
        /// Version 1
        VERSION_1 = 1,
        /// Version 2
        VERSION_2 = 2,
    }
}

/// Request base structure (C-style)
/// The struct (includes the appended [`runtime_claims::RuntimeClaims`]) also serves as the
/// attestation report in vTPM guest attestation.
#[repr(C)]
#[derive(Debug, IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct IgvmAttestRequestBase {
    /// Header (unmeasured)
    pub header: IgvmAttestRequestHeader,
    /// TEE attestation report
    pub attestation_report: [u8; ATTESTATION_REPORT_SIZE_MAX],
    /// Request data (unmeasured)
    pub request_data: IgvmAttestRequestData,
    // Data to be appended at the end of the struct:
    // - Optional Extended request data [`IgvmAttestRequestDataExt`] (request version 2+).
    // - Variable-length [`runtime_claims::RuntimeClaims`] (JSON string)
    //   The hash of [`runtime_claims::RuntimeClaims`] in [`IgvmAttestHashType`] will be captured
    //   in the `report_data` or equivalent field of the TEE attestation report.
}

open_enum! {
    /// TEE attestation report type (C-style enum)
    #[derive(IntoBytes, Immutable, KnownLayout, FromBytes)]
    pub enum IgvmAttestReportType: u32 {
        /// Invalid report
        INVALID_REPORT = 0,
        /// VBS report
        VBS_VM_REPORT = 1,
        /// SNP report
        SNP_VM_REPORT = 2,
        /// Trusted VM report
        TVM_REPORT = 3,
        /// TDX report
        TDX_VM_REPORT = 4,
    }
}

open_enum! {
    /// Request type (C-style enum)
    #[derive(IntoBytes, Immutable, KnownLayout, FromBytes)]
    pub enum IgvmAttestRequestType: u32 {
        /// Invalid request
        INVALID_REQUEST = 0,
        /// Request for getting wrapped key from AKV.
        KEY_RELEASE_REQUEST = 1,
        /// Request to getting attestation key certificate.
        AK_CERT_REQUEST = 2,
        /// Request for getting VMMD blob from CPS.
        WRAPPED_KEY_REQUEST = 3,
    }
}

open_enum! {
    /// Hash algorithm used for content of report data (C-style enum)
    #[derive(IntoBytes, Immutable, KnownLayout, FromBytes)]
    pub enum IgvmAttestHashType: u32 {
        /// Invalid hash
        INVALID_HASH = 0,
        /// SHA-256
        SHA_256 = 1,
        /// SHA-384
        SHA_384 = 2,
        /// SHA-512
        SHA_512 = 3,
    }
}

/// Unmeasured data used to provide transport sanity and versioning (C-style struct)
#[repr(C)]
#[derive(Debug, IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct IgvmAttestRequestHeader {
    /// Signature
    pub signature: u32,
    /// Version
    pub version: u32,
    /// Report size
    pub report_size: u32,
    /// Request type
    pub request_type: IgvmAttestRequestType,
    /// Status
    pub status: u32,
    /// Reserved
    pub reserved: [u32; 3],
}

impl IgvmAttestRequestHeader {
    /// Create an `HardwareKeyProtectorHeader` instance.
    pub fn new(report_size: u32, request_type: IgvmAttestRequestType, status: u32) -> Self {
        Self {
            signature: ATTESTATION_SIGNATURE,
            version: ATTESTATION_VERSION,
            report_size,
            request_type,
            status,
            reserved: [0u32; 3],
        }
    }
}

/// Bitmap of additional Igvm request attributes.
/// 0 - error_code: Requesting IGVM Agent Error code
/// 1 - retry: Retry preference
#[bitfield(u32)]
#[derive(IntoBytes, FromBytes, Immutable, KnownLayout)]
pub struct IgvmCapabilityBitMap {
    pub error_code: bool,
    pub retry: bool,
    #[bits(30)]
    _reserved: u32,
}

/// Unmeasured user data, used for host attestation requests (C-style struct)
#[repr(C)]
#[derive(Debug, IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct IgvmAttestRequestData {
    /// Data size
    pub data_size: u32,
    /// Version
    pub version: IgvmAttestRequestVersion,
    /// Report type
    pub report_type: IgvmAttestReportType,
    /// Report data hash type
    pub report_data_hash_type: IgvmAttestHashType,
    /// Size of the appended raw runtime claims
    pub variable_data_size: u32,
}

impl IgvmAttestRequestData {
    /// Create an `IgvmAttestRequestData` instance.
    pub fn new(
        version: IgvmAttestRequestVersion,
        data_size: u32,
        report_type: IgvmAttestReportType,
        report_data_hash_type: IgvmAttestHashType,
        variable_data_size: u32,
    ) -> Self {
        Self {
            data_size,
            version,
            report_type,
            report_data_hash_type,
            variable_data_size,
        }
    }
}

/// Unmeasured user data appended to `IgvmAttestRequestData` for version 2+,
/// used for host attestation requests (C-style struct).
#[repr(C)]
#[derive(Debug, IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct IgvmAttestRequestDataExt {
    /// Bitmap of additional requested attributes
    pub capability_bitmap: IgvmCapabilityBitMap,
}

impl IgvmAttestRequestDataExt {
    /// Create an `IgvmAttestRequestDataExt` instance.
    pub fn new(capability_bitmap: IgvmCapabilityBitMap) -> Self {
        Self { capability_bitmap }
    }
}

/// Bitmap indicates a signal to requestor
/// 0 - IGVM_SIGNAL_RETRY_RCOMMENDED_BIT: Retry recommendation
#[bitfield(u32)]
#[derive(IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct IgvmSignal {
    pub retry: bool,
    #[bits(31)]
    _reserved: u32,
}

/// The common response header that comply with both V1 and V2 Igvm attest response
#[repr(C)]
#[derive(Default, Debug, IntoBytes, FromBytes)]
pub struct IgvmAttestCommonResponseHeader {
    /// Data size
    pub data_size: u32,
    /// Version
    pub version: IgvmAttestResponseVersion,
}

/// The response header for `IGVM_ERROR_INFO` (C-style struct)
#[repr(C)]
#[derive(Default, Debug, IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct IgvmErrorInfo {
    /// ErrorCode propagated from IgvmAgent
    pub error_code: u32,
    /// HttpStatusCode propagated from IgvmAgent that enhances the ErrorCode
    pub http_status_code: u32,
    /// Igvm signal from response
    pub igvm_signal: IgvmSignal,
    /// Reserved
    pub reserved: [u32; 3],
}

/// The response header for `KEY_RELEASE_REQUEST` (C-style struct)
#[repr(C)]
#[derive(Default, Debug, IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct IgvmAttestKeyReleaseResponseHeader {
    /// Data size
    pub data_size: u32,
    /// Version
    pub version: IgvmAttestResponseVersion,
    /// IgvmErrorInfo that contains RPC result and retry recommendation
    pub error_info: IgvmErrorInfo,
}

/// The response header for `WRAPPED_KEY_REQUEST` (C-style struct)
/// Currently the definition is the same as [`IgvmAttestKeyReleaseResponseHeader`].
#[repr(C)]
#[derive(Default, Debug, IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct IgvmAttestWrappedKeyResponseHeader {
    /// Data size
    pub data_size: u32,
    /// Version
    pub version: IgvmAttestResponseVersion,
    /// IgvmErrorInfo that contains RPC result and retry recommendation
    pub error_info: IgvmErrorInfo,
}

/// The response header for `AK_CERT_REQUEST` (C-style struct)
#[repr(C)]
#[derive(Default, Debug, IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct IgvmAttestAkCertResponseHeader {
    /// Data size
    pub data_size: u32,
    /// Version
    pub version: IgvmAttestResponseVersion,
    /// IgvmErrorInfo that contains RPC result and retry recommendation
    pub error_info: IgvmErrorInfo,
}

/// Definition of the runt-time claims, which will be appended to the
/// `IgvmAttestRequestBase` in raw bytes.
pub mod runtime_claims {
    use base64_serde::base64_serde_type;
    use mesh::MeshPayload;
    use serde::Deserialize;
    use serde::Serialize;

    base64_serde_type!(Base64Url, base64::engine::general_purpose::URL_SAFE_NO_PAD);

    /// Measured runtime claim in JSON format.
    /// The hash of the data is expected be put into the user_data field of
    /// the attestation report.
    #[derive(Debug, Deserialize, Serialize)]
    #[serde(rename_all = "kebab-case")]
    pub struct RuntimeClaims {
        /// An array of [`RsaJwk`]
        pub keys: Vec<RsaJwk>,
        /// VM configuration
        pub vm_configuration: AttestationVmConfig,
        /// Optional user data
        #[serde(default, skip_serializing_if = "String::is_empty")]
        pub user_data: String,
    }

    impl RuntimeClaims {
        /// Create runtime claims for `KEY_RELEASE_REQUEST`.
        pub fn key_release_request_runtime_claims(
            exponent: &[u8],
            modulus: &[u8],
            attestation_vm_config: &AttestationVmConfig,
        ) -> Self {
            let transfer_key_jwks = RsaJwk::get_transfer_key_jwks(exponent, modulus);
            Self {
                keys: transfer_key_jwks,
                vm_configuration: attestation_vm_config.clone(),
                user_data: "".to_string(),
            }
        }

        /// Helper function for creating runtime claims of `AK_CERT_REQUEST`.
        pub fn ak_cert_runtime_claims(
            ak_pub_exponent: &[u8],
            ak_pub_modulus: &[u8],
            ek_pub_exponent: &[u8],
            ek_pub_modulus: &[u8],
            attestation_vm_config: &AttestationVmConfig,
            user_data: &[u8],
        ) -> Self {
            let tpm_jwks = RsaJwk::get_tpm_jwks(
                ak_pub_exponent,
                ak_pub_modulus,
                ek_pub_exponent,
                ek_pub_modulus,
            );
            Self {
                keys: tpm_jwks,
                vm_configuration: attestation_vm_config.clone(),
                user_data: hex::encode(user_data),
            }
        }
    }

    /// JWK for an RSA key
    #[derive(Debug, Deserialize, Serialize)]
    pub struct RsaJwk {
        /// Key id
        pub kid: String,
        /// Key operations
        pub key_ops: Vec<String>,
        /// Key type
        pub kty: String,
        /// RSA public exponent
        #[serde(with = "Base64Url")]
        pub e: Vec<u8>,
        /// RSA public modulus
        #[serde(with = "Base64Url")]
        pub n: Vec<u8>,
    }

    impl RsaJwk {
        /// Create a JWKS from inputs.
        pub fn get_transfer_key_jwks(exponent: &[u8], modulus: &[u8]) -> Vec<RsaJwk> {
            let jwk = RsaJwk {
                kid: "HCLTransferKey".to_string(),
                key_ops: vec!["encrypt".to_string()],
                kty: "RSA".to_string(),
                e: exponent.to_vec(),
                n: modulus.to_vec(),
            };

            vec![jwk]
        }

        /// Create a JWKS from inputs.
        pub fn get_tpm_jwks(
            ak_pub_exponent: &[u8],
            ak_pub_modulus: &[u8],
            ek_pub_exponent: &[u8],
            ek_pub_modulus: &[u8],
        ) -> Vec<RsaJwk> {
            let ak_pub = RsaJwk {
                kid: "HCLAkPub".to_string(),
                key_ops: vec!["sign".to_string()],
                kty: "RSA".to_string(),
                e: ak_pub_exponent.to_vec(),
                n: ak_pub_modulus.to_vec(),
            };
            let ek_pub = RsaJwk {
                kid: "HCLEkPub".to_string(),
                key_ops: vec!["encrypt".to_string()],
                kty: "RSA".to_string(),
                e: ek_pub_exponent.to_vec(),
                n: ek_pub_modulus.to_vec(),
            };

            vec![ak_pub, ek_pub]
        }
    }

    /// VM configuration to be included in the `RuntimeClaims`.
    #[derive(Clone, Debug, Deserialize, Serialize, MeshPayload)]
    #[serde(rename_all = "kebab-case")]
    pub struct AttestationVmConfig {
        /// Time stamp
        #[serde(skip_serializing_if = "Option::is_none")]
        pub current_time: Option<i64>,
        /// Base64-encoded hash of the provisioning cert
        pub root_cert_thumbprint: String,
        /// Whether the serial console is enabled
        pub console_enabled: bool,
        /// Whether the serial console, if enabled, is interactive
        pub interactive_console_enabled: bool,
        /// Whether secure boot is enabled
        pub secure_boot: bool,
        /// Whether the TPM is enabled
        pub tpm_enabled: bool,
        /// Whether the TPM states is persisted
        pub tpm_persisted: bool,
        /// Whether certain vPCI devices are allowed through the device filter
        pub filtered_vpci_devices_allowed: bool,
        /// VM id
        #[serde(rename = "vmUniqueId")]
        pub vm_unique_id: String,
    }
}
