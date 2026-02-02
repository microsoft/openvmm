// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! The module helps preparing requests and parsing responses that are
//! sent to and received from the IGVm agent runs on the host via GET
//! `IGVM_ATTEST` host request.

use base64_serde::base64_serde_type;
use openhcl_attestation_protocol::igvm_attest::get::IGVM_ATTEST_RESPONSE_CURRENT_VERSION;
use openhcl_attestation_protocol::igvm_attest::get::IgvmAttestCommonResponseHeader;
use openhcl_attestation_protocol::igvm_attest::get::IgvmAttestHashType;
use openhcl_attestation_protocol::igvm_attest::get::IgvmAttestReportType;
use openhcl_attestation_protocol::igvm_attest::get::IgvmAttestRequestType;
use openhcl_attestation_protocol::igvm_attest::get::IgvmAttestRequestVersion;
use openhcl_attestation_protocol::igvm_attest::get::IgvmAttestResponseVersion;
use openhcl_attestation_protocol::igvm_attest::get::IgvmCapabilityBitMap;
use openhcl_attestation_protocol::igvm_attest::get::IgvmErrorInfo;
use openhcl_attestation_protocol::igvm_attest::get::runtime_claims::AttestationVmConfig;
use tee_call::TeeType;
use thiserror::Error;
use zerocopy::FromBytes;
use zerocopy::FromZeros;
use zerocopy::IntoBytes;

pub mod ak_cert;
pub mod key_release;
pub mod wrapped_key;

base64_serde_type!(Base64Url, base64::engine::general_purpose::URL_SAFE_NO_PAD);

#[expect(missing_docs)] // self-explanatory fields
#[derive(Debug, Error)]
pub enum Error {
    #[error(
        "the size of the attestation report {report_size} is invalid, expected {expected_size}"
    )]
    InvalidAttestationReportSize {
        report_size: usize,
        expected_size: usize,
    },
    #[error("the size of the attestation response {response_size} is too small to parse")]
    ResponseSizeTooSmall { response_size: usize },
    #[error(
        "the header of the attestation response (size {response_size}) is not in correct format"
    )]
    ResponseHeaderInvalidFormat { response_size: usize },
    #[error(
        "response size {specified_size} specified in the header not match the actual size {size}"
    )]
    ResponseSizeMismatch { size: usize, specified_size: usize },
    #[error("response header version {version:?} larger than current version {latest_version:?}")]
    InvalidResponseHeaderVersion {
        version: IgvmAttestResponseVersion,
        latest_version: IgvmAttestResponseVersion,
    },
    #[error(
        "attest failed ({igvm_error_code}-{http_status_code}), retry recommendation ({retry_signal})"
    )]
    Attestation {
        igvm_error_code: u32,
        http_status_code: u32,
        retry_signal: bool,
    },
}

/// Rust-style enum for `IgvmAttestReportType`
pub enum ReportType {
    /// VBS report
    Vbs,
    /// SNP report
    Snp,
    /// TDX report
    Tdx,
    /// Trusted VM report
    Tvm,
}

impl ReportType {
    /// Map the value to `IgvmAttestReportType`
    fn to_external_type(&self) -> IgvmAttestReportType {
        match self {
            Self::Vbs => IgvmAttestReportType::VBS_VM_REPORT,
            Self::Snp => IgvmAttestReportType::SNP_VM_REPORT,
            Self::Tdx => IgvmAttestReportType::TDX_VM_REPORT,
            Self::Tvm => IgvmAttestReportType::TVM_REPORT,
        }
    }
}

/// Helper struct to create `IgvmAttestRequest` in raw bytes.
pub struct IgvmAttestRequestHelper {
    /// The request type.
    request_type: IgvmAttestRequestType,
    /// The report type.
    report_type: ReportType,
    /// Raw bytes of `RuntimeClaims`.
    runtime_claims: Vec<u8>,
    /// The hash of the `runtime_claims` to be included in the
    /// `report_data` field of the attestation report.
    runtime_claims_hash: [u8; tee_call::REPORT_DATA_SIZE],
    /// THe hash type of the `runtime_claims_hash`.
    hash_type: IgvmAttestHashType,
}

impl IgvmAttestRequestHelper {
    /// Prepare the data necessary for creating the `KEY_RELEASE` request.
    pub fn prepare_key_release_request(
        tee_type: TeeType,
        rsa_exponent: &[u8],
        rsa_modulus: &[u8],
        host_time: i64,
        attestation_vm_config: &AttestationVmConfig,
    ) -> Self {
        let report_type = match tee_type {
            TeeType::Snp => ReportType::Snp,
            TeeType::Tdx => ReportType::Tdx,
            TeeType::Vbs => ReportType::Vbs,
        };

        let attestation_vm_config =
            attestation_vm_config_with_time(attestation_vm_config, host_time);
        let runtime_claims =
            openhcl_attestation_protocol::igvm_attest::get::runtime_claims::RuntimeClaims::key_release_request_runtime_claims(rsa_exponent, rsa_modulus, &attestation_vm_config);
        let runtime_claims = runtime_claims_to_bytes(&runtime_claims);

        let hash_type = IgvmAttestHashType::SHA_256;
        let hash = crate::crypto::sha_256(runtime_claims.as_bytes());
        let mut runtime_claims_hash = [0u8; tee_call::REPORT_DATA_SIZE];
        runtime_claims_hash[0..hash.len()].copy_from_slice(&hash);

        Self {
            request_type: IgvmAttestRequestType::KEY_RELEASE_REQUEST,
            report_type,
            runtime_claims,
            runtime_claims_hash,
            hash_type,
        }
    }

    /// Prepare the data necessary for creating the `AK_CERT` request.
    pub fn prepare_ak_cert_request(
        tee_type: Option<TeeType>,
        ak_pub_exponent: &[u8],
        ak_pub_modulus: &[u8],
        ek_pub_exponent: &[u8],
        ek_pub_modulus: &[u8],
        attestation_vm_config: &AttestationVmConfig,
        guest_input: &[u8],
    ) -> Self {
        let report_type = match tee_type {
            Some(TeeType::Snp) => ReportType::Snp,
            Some(TeeType::Tdx) => ReportType::Tdx,
            Some(TeeType::Vbs) => ReportType::Vbs,
            None => ReportType::Tvm,
        };

        let runtime_claims =
            openhcl_attestation_protocol::igvm_attest::get::runtime_claims::RuntimeClaims::ak_cert_runtime_claims(
                ak_pub_exponent,
                ak_pub_modulus,
                ek_pub_exponent,
                ek_pub_modulus,
                attestation_vm_config,
                guest_input,
            );

        let runtime_claims = runtime_claims_to_bytes(&runtime_claims);

        let hash_type = IgvmAttestHashType::SHA_256;
        let hash = crate::crypto::sha_256(runtime_claims.as_bytes());
        let mut runtime_claims_hash = [0u8; tee_call::REPORT_DATA_SIZE];
        runtime_claims_hash[0..hash.len()].copy_from_slice(&hash);

        Self {
            request_type: IgvmAttestRequestType::AK_CERT_REQUEST,
            report_type,
            runtime_claims,
            runtime_claims_hash,
            hash_type,
        }
    }

    /// Return the `runtime_claims_hash`.
    pub fn get_runtime_claims_hash(&self) -> &[u8; tee_call::REPORT_DATA_SIZE] {
        &self.runtime_claims_hash
    }

    /// Set the `request_type`.
    pub fn set_request_type(&mut self, request_type: IgvmAttestRequestType) {
        self.request_type = request_type
    }

    /// Create the request in raw bytes.
    pub fn create_request(
        &self,
        version: IgvmAttestRequestVersion,
        attestation_report: &[u8],
    ) -> Result<Vec<u8>, Error> {
        create_request(
            version,
            self.request_type,
            &self.runtime_claims,
            attestation_report,
            &self.report_type,
            self.hash_type,
        )
    }
}

/// Verify response header and try to extract IgvmErrorInfo from the header
pub fn parse_response_header(response: &[u8]) -> Result<IgvmAttestCommonResponseHeader, Error> {
    // Extract common header fields regardless of header version or request type
    // For V1 request, response buffer should be empty in case of attestation failure
    let header = IgvmAttestCommonResponseHeader::read_from_prefix(response)
        .map_err(|_| Error::ResponseSizeTooSmall {
            response_size: response.len(),
        })?
        .0; // TODO: zerocopy: err (https://github.com/microsoft/openvmm/issues/759)

    // Check header data_size and version
    if header.data_size as usize > response.len() {
        Err(Error::ResponseSizeMismatch {
            size: response.len(),
            specified_size: header.data_size as usize,
        })?
    }
    if header.version > IGVM_ATTEST_RESPONSE_CURRENT_VERSION {
        Err(Error::InvalidResponseHeaderVersion {
            version: header.version,
            latest_version: IGVM_ATTEST_RESPONSE_CURRENT_VERSION,
        })?
    }

    // IgvmErrorInfo is added in response header since version 2
    if header.version >= IgvmAttestResponseVersion::VERSION_2 {
        // Extract result info from response header
        let igvm_error_info = IgvmErrorInfo::read_from_prefix(
            &response[size_of::<IgvmAttestCommonResponseHeader>()..],
        )
        .map_err(|_| Error::ResponseHeaderInvalidFormat {
            response_size: response.len(),
        })?
        .0; // TODO: zerocopy: err (https://github.com/microsoft/openvmm/issues/759)

        if 0 != igvm_error_info.error_code {
            Err(Error::Attestation {
                igvm_error_code: igvm_error_info.error_code,
                http_status_code: igvm_error_info.http_status_code,
                retry_signal: igvm_error_info.igvm_signal.retry(),
            })?
        }
    }
    Ok(IgvmAttestCommonResponseHeader {
        data_size: header.data_size,
        version: header.version,
    })
}

/// Create a request in raw bytes.
/// A request looks like:
///   `IgvmAttestRequestBase` (raw bytes) | `IgvmAttestRequestDataExt` (raw bytes) if version >= 2 | `runtime_claims` (raw bytes)
fn create_request(
    version: IgvmAttestRequestVersion,
    request_type: IgvmAttestRequestType,
    runtime_claims: &[u8],
    attestation_report: &[u8],
    report_type: &ReportType,
    hash_type: IgvmAttestHashType,
) -> Result<Vec<u8>, Error> {
    use openhcl_attestation_protocol::igvm_attest::get::IgvmAttestRequestBase;
    use openhcl_attestation_protocol::igvm_attest::get::IgvmAttestRequestData;
    use openhcl_attestation_protocol::igvm_attest::get::IgvmAttestRequestDataExt;
    use openhcl_attestation_protocol::igvm_attest::get::IgvmAttestRequestHeader;

    let expected_report_size = get_report_size(report_type);
    if attestation_report.len() != expected_report_size {
        Err(Error::InvalidAttestationReportSize {
            report_size: attestation_report.len(),
            expected_size: expected_report_size,
        })?
    }

    let runtime_claims_len = runtime_claims.len();
    // Determine if request data extension structure is needed (introduced in version 2)
    let include_extension = version >= IgvmAttestRequestVersion::VERSION_2;
    let extension_size = if include_extension {
        size_of::<IgvmAttestRequestDataExt>()
    } else {
        0
    };
    let report_size = size_of::<IgvmAttestRequestBase>() + extension_size + runtime_claims_len;
    let user_data_size = size_of::<IgvmAttestRequestData>() + extension_size + runtime_claims_len;
    let mut request = IgvmAttestRequestBase::new_zeroed();

    request.header = IgvmAttestRequestHeader::new(report_size as u32, request_type, 0);

    request.attestation_report[..attestation_report.len()].copy_from_slice(attestation_report);

    request.request_data = IgvmAttestRequestData::new(
        version,
        user_data_size as u32,
        report_type.to_external_type(),
        hash_type,
        runtime_claims_len as u32,
    );

    let mut buffer = Vec::with_capacity(report_size);
    buffer.extend_from_slice(request.as_bytes());

    if include_extension {
        let capability_bitmap = IgvmCapabilityBitMap::new()
            .with_error_code(true)
            .with_retry(true);
        let ext = IgvmAttestRequestDataExt::new(capability_bitmap);
        buffer.extend_from_slice(ext.as_bytes());
    }

    buffer.extend_from_slice(runtime_claims);

    Ok(buffer)
}

/// Get the expected size of the given report type.
fn get_report_size(report_type: &ReportType) -> usize {
    match report_type {
        ReportType::Vbs => openhcl_attestation_protocol::igvm_attest::get::VBS_VM_REPORT_SIZE,
        ReportType::Snp => openhcl_attestation_protocol::igvm_attest::get::SNP_VM_REPORT_SIZE,
        ReportType::Tdx => openhcl_attestation_protocol::igvm_attest::get::TDX_VM_REPORT_SIZE,
        ReportType::Tvm => openhcl_attestation_protocol::igvm_attest::get::TVM_REPORT_SIZE,
    }
}

/// Helper function that returns the given config with the `current_time` set.
fn attestation_vm_config_with_time(
    vm_config: &AttestationVmConfig,
    host_epoch: i64,
) -> AttestationVmConfig {
    let mut vm_config = vm_config.clone();
    vm_config.current_time = Some(host_epoch);
    vm_config
}

/// Helper function that converts the `RuntimeClaims` to raw bytes.
fn runtime_claims_to_bytes(
    runtime_claims: &openhcl_attestation_protocol::igvm_attest::get::runtime_claims::RuntimeClaims,
) -> Vec<u8> {
    let runtime_claims = serde_json::to_string(runtime_claims).expect("JSON serialization failed");
    runtime_claims.as_bytes().to_vec()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_create_request() {
        use openhcl_attestation_protocol::igvm_attest::get::IGVM_ATTEST_REQUEST_CURRENT_VERSION;

        let result = create_request(
            IGVM_ATTEST_REQUEST_CURRENT_VERSION,
            IgvmAttestRequestType::AK_CERT_REQUEST,
            &[],
            &[0u8; openhcl_attestation_protocol::igvm_attest::get::SNP_VM_REPORT_SIZE],
            &ReportType::Snp,
            IgvmAttestHashType::SHA_256,
        );
        assert!(result.is_ok());

        let result = create_request(
            IGVM_ATTEST_REQUEST_CURRENT_VERSION,
            IgvmAttestRequestType::AK_CERT_REQUEST,
            &[],
            &[0u8; openhcl_attestation_protocol::igvm_attest::get::SNP_VM_REPORT_SIZE + 1],
            &ReportType::Snp,
            IgvmAttestHashType::SHA_256,
        );
        assert!(result.is_err());
    }

    #[test]
    fn test_create_request_version1_no_extension() {
        use openhcl_attestation_protocol::igvm_attest::get::IgvmAttestRequestBase;
        use openhcl_attestation_protocol::igvm_attest::get::IgvmAttestRequestVersion;

        let runtime_claims = vec![1u8, 2, 3];
        let attestation_report =
            vec![0u8; openhcl_attestation_protocol::igvm_attest::get::SNP_VM_REPORT_SIZE];

        let buffer = create_request(
            IgvmAttestRequestVersion::VERSION_1,
            IgvmAttestRequestType::AK_CERT_REQUEST,
            &runtime_claims,
            &attestation_report,
            &ReportType::Snp,
            IgvmAttestHashType::SHA_256,
        )
        .expect("request generation");

        let (request, _) =
            IgvmAttestRequestBase::read_from_prefix(&buffer).expect("parse IgvmAttestRequest");
        assert_eq!(
            request.request_data.version,
            IgvmAttestRequestVersion::VERSION_1
        );

        let expected_size = (size_of::<
            openhcl_attestation_protocol::igvm_attest::get::IgvmAttestRequestData,
        >() + runtime_claims.len()) as u32;
        assert_eq!(request.request_data.data_size, expected_size);

        let header_size = size_of::<IgvmAttestRequestBase>();
        assert_eq!(
            buffer.len(),
            header_size + runtime_claims.len(),
            "no extension appended for version 1"
        );
        assert_eq!(&buffer[header_size..], runtime_claims.as_slice());
    }

    #[test]
    fn test_create_request_version2_with_extension() {
        use openhcl_attestation_protocol::igvm_attest::get::IgvmAttestRequestBase;
        use openhcl_attestation_protocol::igvm_attest::get::IgvmAttestRequestDataExt;
        use openhcl_attestation_protocol::igvm_attest::get::IgvmAttestRequestVersion;

        let runtime_claims = vec![4u8, 5, 6, 7];
        let attestation_report =
            vec![0u8; openhcl_attestation_protocol::igvm_attest::get::SNP_VM_REPORT_SIZE];

        let buffer = create_request(
            IgvmAttestRequestVersion::VERSION_2,
            IgvmAttestRequestType::AK_CERT_REQUEST,
            &runtime_claims,
            &attestation_report,
            &ReportType::Snp,
            IgvmAttestHashType::SHA_256,
        )
        .expect("request generation");

        let (request, _) =
            IgvmAttestRequestBase::read_from_prefix(&buffer).expect("parse IgvmAttestRequest");
        assert_eq!(
            request.request_data.version,
            IgvmAttestRequestVersion::VERSION_2
        );

        let expected_extension_size = size_of::<IgvmAttestRequestDataExt>();
        let expected_size = (size_of::<
            openhcl_attestation_protocol::igvm_attest::get::IgvmAttestRequestData,
        >() + expected_extension_size
            + runtime_claims.len()) as u32;
        assert_eq!(request.request_data.data_size, expected_size);

        let header_size = size_of::<IgvmAttestRequestBase>();
        let ext_offset = header_size;

        let (ext, _) = IgvmAttestRequestDataExt::read_from_prefix(&buffer[ext_offset..])
            .expect("parse IgvmAttestRequestDataExt");
        assert!(ext.capability_bitmap.error_code());
        assert!(ext.capability_bitmap.retry());

        assert_eq!(
            buffer.len(),
            header_size + expected_extension_size + runtime_claims.len()
        );
        assert_eq!(
            &buffer[header_size + expected_extension_size..],
            runtime_claims.as_slice()
        );
    }

    #[test]
    fn test_transfer_key_jwk() {
        const EXPECTED_JWK: &str = r#"[{"kid":"HCLTransferKey","key_ops":["encrypt"],"kty":"RSA","e":"RVhQT05FTlQ","n":"TU9EVUxVUw"}]"#;

        let rsa_jwk = openhcl_attestation_protocol::igvm_attest::get::runtime_claims::RsaJwk::get_transfer_key_jwks(
            b"EXPONENT",
            b"MODULUS",
        );

        let result = serde_json::to_string(&rsa_jwk);
        assert!(result.is_ok());

        let transfer_key_jwk = result.unwrap();
        assert_eq!(transfer_key_jwk, EXPECTED_JWK);
    }

    #[test]
    fn test_vm_configuration_no_time() {
        const EXPECTED_JWK: &str = r#"{"root-cert-thumbprint":"","console-enabled":false,"secure-boot":false,"tpm-enabled":false,"tpm-persisted":false,"filtered-vpci-devices-allowed":true,"vmUniqueId":""}"#;

        let attestation_vm_config = AttestationVmConfig {
            current_time: None,
            root_cert_thumbprint: String::new(),
            console_enabled: false,
            interactive_console: false,
            secure_boot: false,
            tpm_enabled: false,
            tpm_persisted: false,
            filtered_vpci_devices_allowed: true,
            vm_unique_id: String::new(),
        };
        let result = serde_json::to_string(&attestation_vm_config);
        assert!(result.is_ok());

        let vm_config = result.unwrap();
        assert_eq!(vm_config, EXPECTED_JWK);
    }

    #[test]
    fn test_vm_configuration_with_time() {
        const EXPECTED_JWK: &str = r#"{"current-time":1691103220,"root-cert-thumbprint":"","console-enabled":false,"secure-boot":false,"tpm-enabled":false,"tpm-persisted":false,"filtered-vpci-devices-allowed":true,"vmUniqueId":""}"#;

        let attestation_vm_config = AttestationVmConfig {
            current_time: None,
            root_cert_thumbprint: String::new(),
            console_enabled: false,
            interactive_console: false,
            secure_boot: false,
            tpm_enabled: false,
            tpm_persisted: false,
            filtered_vpci_devices_allowed: true,
            vm_unique_id: String::new(),
        };
        let attestation_vm_config =
            attestation_vm_config_with_time(&attestation_vm_config, 1691103220);
        let result = serde_json::to_string(&attestation_vm_config);
        assert!(result.is_ok());

        let vm_config = result.unwrap();
        assert_eq!(vm_config, EXPECTED_JWK);
    }

    #[test]
    fn test_empty_response() {
        let result = parse_response_header(&[]);
        assert!(result.is_err());
        assert_eq!(
            result.unwrap_err().to_string(),
            Error::ResponseSizeTooSmall { response_size: 0 }.to_string()
        );
    }

    #[test]
    fn test_invalid_response_size_smaller_than_header_size() {
        const INVALID_RESPONSE: [u8; 4] = [0x04, 0x00, 0x00, 0x00];
        let result = parse_response_header(&INVALID_RESPONSE);
        assert!(result.is_err());
        assert_eq!(
            result.unwrap_err().to_string(),
            Error::ResponseSizeTooSmall { response_size: 4 }.to_string()
        );
    }

    #[test]
    fn test_valid_v1_response_size_match() {
        const VALID_RESPONSE: [u8; 42] = [
            0x2a, 0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x30, 0x82, 0x03, 0xeb, 0x30, 0x82,
            0x02, 0xd3, 0xa0, 0x03, 0x02, 0x01, 0x02, 0x02, 0x10, 0x3b, 0xa3, 0x33, 0x97, 0xef,
            0x2f, 0x9e, 0xef, 0xbd, 0x35, 0x5e, 0xda, 0xdd, 0x27, 0x38, 0x42, 0x30, 0x0d, 0x06,
        ];

        let result = parse_response_header(&VALID_RESPONSE);
        assert!(result.is_ok());
        let header = result.unwrap();
        assert_eq!(VALID_RESPONSE.len(), header.data_size as usize);
        assert_eq!(IgvmAttestResponseVersion::VERSION_1, header.version);
    }

    #[test]
    fn test_valid_v2_response_size_match() {
        const VALID_RESPONSE: [u8; 42] = [
            0x2a, 0x00, 0x00, 0x00, 0x02, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
            0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
            0x00, 0x00, 0x00, 0x00, 0x35, 0x5e, 0xda, 0xdd, 0x27, 0x38, 0x42, 0x30, 0x0d, 0x06,
        ];

        let result = parse_response_header(&VALID_RESPONSE);
        assert!(result.is_ok());
        let header = result.unwrap();
        assert_eq!(VALID_RESPONSE.len(), header.data_size as usize);
        assert_eq!(IgvmAttestResponseVersion::VERSION_2, header.version);
    }

    #[test]
    fn test_valid_v1_response_size_smaller_than_specified() {
        const VALID_RESPONSE: [u8; 42] = [
            0x29, 0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x30, 0x82, 0x03, 0xeb, 0x30, 0x82,
            0x02, 0xd3, 0xa0, 0x03, 0x02, 0x01, 0x02, 0x02, 0x10, 0x3b, 0xa3, 0x33, 0x97, 0xef,
            0x2f, 0x9e, 0xef, 0xbd, 0x35, 0x5e, 0xda, 0xdd, 0x27, 0x38, 0x42, 0x30, 0x0d, 0x06,
        ];

        let header = parse_response_header(&VALID_RESPONSE);
        assert!(header.is_ok());
        assert_eq!(0x29, header.unwrap().data_size as usize);
    }

    #[test]
    fn test_valid_v2_response_size_smaller_than_specified() {
        const VALID_RESPONSE: [u8; 42] = [
            0x29, 0x00, 0x00, 0x00, 0x02, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
            0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
            0x00, 0x00, 0x00, 0x00, 0x35, 0x5e, 0xda, 0xdd, 0x27, 0x38, 0x42, 0x30, 0x0d, 0x06,
        ];

        let header = parse_response_header(&VALID_RESPONSE);
        assert!(header.is_ok());
        assert_eq!(0x29, header.unwrap().data_size as usize);
    }

    #[test]
    fn test_invalid_v1_response_size() {
        const INVALID_RESPONSE: [u8; 42] = [
            0x2b, 0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x30, 0x82, 0x03, 0xeb, 0x30, 0x82,
            0x02, 0xd3, 0xa0, 0x03, 0x02, 0x01, 0x02, 0x02, 0x10, 0x3b, 0xa3, 0x33, 0x97, 0xef,
            0x2f, 0x9e, 0xef, 0xbd, 0x35, 0x5e, 0xda, 0xdd, 0x27, 0x38, 0x42, 0x30, 0x0d, 0x06,
        ];

        let result = parse_response_header(&INVALID_RESPONSE);
        assert!(result.is_err());
        assert_eq!(
            result.unwrap_err().to_string(),
            Error::ResponseSizeMismatch {
                size: INVALID_RESPONSE.len(),
                specified_size: 0x2b
            }
            .to_string()
        );
    }

    #[test]
    fn test_invalid_v2_response_size() {
        const INVALID_RESPONSE: [u8; 42] = [
            0x2b, 0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x30, 0x82, 0x03, 0xeb, 0x30, 0x82,
            0x02, 0xd3, 0xa0, 0x03, 0x02, 0x01, 0x02, 0x02, 0x10, 0x3b, 0xa3, 0x33, 0x97, 0xef,
            0x2f, 0x9e, 0xef, 0xbd, 0x35, 0x5e, 0xda, 0xdd, 0x27, 0x38, 0x42, 0x30, 0x0d, 0x06,
        ];

        let result = parse_response_header(&INVALID_RESPONSE);
        assert!(result.is_err());
        assert_eq!(
            result.unwrap_err().to_string(),
            Error::ResponseSizeMismatch {
                size: INVALID_RESPONSE.len(),
                specified_size: 0x2b
            }
            .to_string()
        );
    }

    #[test]
    fn test_invalid_header_version() {
        const INVALID_RESPONSE: [u8; 42] = [
            0x2a, 0x00, 0x00, 0x00, 0x03, 0x00, 0x00, 0x00, 0x30, 0x82, 0x03, 0xeb, 0x30, 0x82,
            0x02, 0xd3, 0xa0, 0x03, 0x02, 0x01, 0x02, 0x02, 0x10, 0x3b, 0xa3, 0x33, 0x97, 0xef,
            0x2f, 0x9e, 0xef, 0xbd, 0x35, 0x5e, 0xda, 0xdd, 0x27, 0x38, 0x42, 0x30, 0x0d, 0x06,
        ];

        let result = parse_response_header(&INVALID_RESPONSE);
        assert!(result.is_err());
        assert_eq!(
            result.unwrap_err().to_string(),
            Error::InvalidResponseHeaderVersion {
                version: IgvmAttestResponseVersion(3),
                latest_version: IGVM_ATTEST_RESPONSE_CURRENT_VERSION
            }
            .to_string()
        );
    }

    #[test]
    fn test_invalid_v2_response_size_smaller_than_specified_header_size() {
        const INVALID_RESPONSE: [u8; 28] = [
            0x1c, 0x00, 0x00, 0x00, 0x02, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
            0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
        ];

        let result = parse_response_header(&INVALID_RESPONSE);
        assert!(result.is_err());
        assert_eq!(
            result.unwrap_err().to_string(),
            Error::ResponseHeaderInvalidFormat {
                response_size: 0x1c
            }
            .to_string()
        );
    }

    #[test]
    fn test_failed_response_with_retryable_error() {
        // error_code: 1103 (0x44f), http_status_code: 403 (0x193), retryable
        const INVALID_RESPONSE: [u8; 42] = [
            0x2a, 0x00, 0x00, 0x00, 0x02, 0x00, 0x00, 0x00, 0x4f, 0x04, 0x00, 0x00, 0x93, 0x01,
            0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
            0x00, 0x00, 0x00, 0x00, 0x35, 0x5e, 0xda, 0xdd, 0x27, 0x38, 0x42, 0x30, 0x0d, 0x06,
        ];

        let result = parse_response_header(&INVALID_RESPONSE);
        assert!(result.is_err());
        assert_eq!(
            result.unwrap_err().to_string(),
            Error::Attestation {
                igvm_error_code: 1103,
                http_status_code: 403,
                retry_signal: true
            }
            .to_string()
        );
    }

    #[test]
    fn test_failed_response_with_non_retryable_error() {
        // error_code: 1103 (0x44f), http_status_code: 503 (0x1f7), not retryable
        const INVALID_RESPONSE: [u8; 42] = [
            0x2a, 0x00, 0x00, 0x00, 0x02, 0x00, 0x00, 0x00, 0x4f, 0x04, 0x00, 0x00, 0xf7, 0x01,
            0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
            0x00, 0x00, 0x00, 0x00, 0x35, 0x5e, 0xda, 0xdd, 0x27, 0x38, 0x42, 0x30, 0x0d, 0x06,
        ];

        let result = parse_response_header(&INVALID_RESPONSE);
        assert!(result.is_err());
        assert_eq!(
            result.unwrap_err().to_string(),
            Error::Attestation {
                igvm_error_code: 1103,
                http_status_code: 503,
                retry_signal: false
            }
            .to_string()
        );
    }
}
