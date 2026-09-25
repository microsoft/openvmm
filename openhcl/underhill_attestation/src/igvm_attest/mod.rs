// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! The module helps preparing requests and parsing responses that are
//! sent to and received from the IGVm agent runs on the host via GET
//! `IGVM_ATTEST` host request.

use base64_serde::base64_serde_type;
use openhcl_attestation_protocol::igvm_attest::get::IGVM_ATTEST_RESPONSE_CURRENT_VERSION;
use openhcl_attestation_protocol::igvm_attest::get::IGVM_ATTEST_RESPONSE_SCHEMA_VERSION;
use openhcl_attestation_protocol::igvm_attest::get::IgvmAttestCommonResponseHeader;
use openhcl_attestation_protocol::igvm_attest::get::IgvmAttestHashType;
use openhcl_attestation_protocol::igvm_attest::get::IgvmAttestReportType;
use openhcl_attestation_protocol::igvm_attest::get::IgvmAttestRequestType;
use openhcl_attestation_protocol::igvm_attest::get::IgvmAttestRequestVersion;
use openhcl_attestation_protocol::igvm_attest::get::IgvmAttestResponseEnvelope;
use openhcl_attestation_protocol::igvm_attest::get::IgvmAttestResponseRequestType;
use openhcl_attestation_protocol::igvm_attest::get::IgvmAttestResponseVersion;
use openhcl_attestation_protocol::igvm_attest::get::IgvmCapabilityBitMap;
use openhcl_attestation_protocol::igvm_attest::get::IgvmErrorInfo;
use openhcl_attestation_protocol::igvm_attest::get::InvalidKeyReleaseContextHash;
use openhcl_attestation_protocol::igvm_attest::get::KEY_RELEASE_RESPONSE_BUFFER_SIZE;
use openhcl_attestation_protocol::igvm_attest::get::runtime_claims::AttestationVmConfig;
use serde::de::DeserializeOwned;
use std::borrow::Cow;
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
    #[error("unsupported request version {version:?} for {request_type:?}")]
    InvalidRequestVersion {
        version: IgvmAttestRequestVersion,
        request_type: IgvmAttestRequestType,
    },
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
    #[error("unsupported response header version {version:?}, latest version {latest_version:?}")]
    InvalidResponseHeaderVersion {
        version: IgvmAttestResponseVersion,
        latest_version: IgvmAttestResponseVersion,
    },
    #[error("response size {response_size} exceeds maximum {max_size}")]
    ResponseSizeTooLarge {
        response_size: usize,
        max_size: usize,
    },
    #[error("response payload is not UTF-8")]
    InvalidResponseUtf8(#[source] std::str::Utf8Error),
    #[error("invalid version 3 response envelope")]
    InvalidResponseEnvelope(#[source] serde_json::Error),
    #[error("unsupported response envelope schema version {0}")]
    InvalidResponseSchemaVersion(u32),
    #[error("response envelope request type {actual:?} does not match {expected:?}")]
    ResponseRequestTypeMismatch {
        actual: IgvmAttestResponseRequestType,
        expected: IgvmAttestResponseRequestType,
    },
    #[error("invalid key-release context hash")]
    InvalidContextHash(#[source] InvalidKeyReleaseContextHash),
    #[error("key-release response is missing the required context hash")]
    MissingRequiredResponseContext,
    #[error(
        "attest failed ({igvm_error_code}-{http_status_code}), retry recommendation ({retry_signal}), skip hw unsealing recommendation ({skip_hw_unsealing_signal})"
    )]
    Attestation {
        igvm_error_code: u32,
        http_status_code: u32,
        retry_signal: bool,
        skip_hw_unsealing_signal: bool,
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
    /// CCA report
    Cca,
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
            Self::Cca => IgvmAttestReportType::CCA_VM_REPORT,
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
            TeeType::Cca => ReportType::Cca,
            TeeType::Vbs => ReportType::Vbs,
        };

        let attestation_vm_config =
            attestation_vm_config_with_time(attestation_vm_config, host_time);
        let runtime_claims =
            openhcl_attestation_protocol::igvm_attest::get::runtime_claims::RuntimeClaims::key_release_request_runtime_claims(rsa_exponent, rsa_modulus, &attestation_vm_config);
        let runtime_claims = runtime_claims_to_bytes(&runtime_claims);

        let hash_type = IgvmAttestHashType::SHA_256;
        let hash = crypto::sha_256::sha_256(runtime_claims.as_bytes());
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
            Some(TeeType::Cca) => ReportType::Cca,
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
        let hash = crypto::sha_256::sha_256(runtime_claims.as_bytes());
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
        // AK certificate provisioning has no V3 protocol. Keep legacy V1
        // callers unchanged and pin callers using the current V3 default to V2.
        let version = if self.request_type == IgvmAttestRequestType::AK_CERT_REQUEST
            && version == IgvmAttestRequestVersion::VERSION_3
        {
            IgvmAttestRequestVersion::VERSION_2
        } else {
            version
        };
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
    if !matches!(
        header.version,
        IgvmAttestResponseVersion::VERSION_1
            | IgvmAttestResponseVersion::VERSION_2
            | IgvmAttestResponseVersion::VERSION_3
    ) {
        Err(Error::InvalidResponseHeaderVersion {
            version: header.version,
            latest_version: IGVM_ATTEST_RESPONSE_CURRENT_VERSION,
        })?
    }

    let header_size = response_header_size(header.version);
    if (header.data_size as usize) < header_size {
        return Err(Error::ResponseHeaderInvalidFormat {
            response_size: header.data_size as usize,
        });
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
                skip_hw_unsealing_signal: igvm_error_info.igvm_signal.skip_hw_unsealing(),
            })?
        }
    }
    Ok(IgvmAttestCommonResponseHeader {
        data_size: header.data_size,
        version: header.version,
    })
}

fn response_header_size(version: IgvmAttestResponseVersion) -> usize {
    size_of::<IgvmAttestCommonResponseHeader>()
        + if version == IgvmAttestResponseVersion::VERSION_1 {
            0
        } else {
            size_of::<IgvmErrorInfo>()
        }
}

/// Original service payload and metadata after validating the binary framing
/// and, for V3 only, exactly one JSON envelope.
pub(crate) struct ParsedResponsePayload<'a, E> {
    pub header: IgvmAttestCommonResponseHeader,
    pub payload: Cow<'a, str>,
    pub extensions: E,
}

/// Validate size limits before allocating or parsing JSON. Legacy responses
/// borrow their UTF-8 payload; V3 responses own the unescaped service payload.
/// Header errors deliberately remain `Error::Attestation` for caller retry and
/// hardware-unsealing decisions, even when no envelope accompanies an error.
pub(crate) fn parse_response_payload<E: DeserializeOwned + Default>(
    response: &[u8],
    expected_type: IgvmAttestResponseRequestType,
    max_size: usize,
) -> Result<ParsedResponsePayload<'_, E>, Error> {
    let max_size = max_size.min(KEY_RELEASE_RESPONSE_BUFFER_SIZE);
    if response.len() > max_size {
        return Err(Error::ResponseSizeTooLarge {
            response_size: response.len(),
            max_size,
        });
    }
    let header = parse_response_header(response)?;
    let payload = &response[response_header_size(header.version)..header.data_size as usize];
    let payload = std::str::from_utf8(payload).map_err(Error::InvalidResponseUtf8)?;
    if header.version != IgvmAttestResponseVersion::VERSION_3 {
        return Ok(ParsedResponsePayload {
            header,
            payload: Cow::Borrowed(payload),
            extensions: E::default(),
        });
    }

    // Deserialize directly into typed structs, not Value: serde rejects
    // duplicate recognized fields rather than silently overwriting them.
    let envelope: IgvmAttestResponseEnvelope<E> =
        serde_json::from_str(payload).map_err(Error::InvalidResponseEnvelope)?;
    if envelope.schema_version != IGVM_ATTEST_RESPONSE_SCHEMA_VERSION {
        return Err(Error::InvalidResponseSchemaVersion(envelope.schema_version));
    }
    if envelope.request_type != expected_type {
        return Err(Error::ResponseRequestTypeMismatch {
            actual: envelope.request_type,
            expected: expected_type,
        });
    }
    Ok(ParsedResponsePayload {
        header,
        payload: Cow::Owned(envelope.payload),
        extensions: envelope.extensions,
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

    if !matches!(
        version,
        IgvmAttestRequestVersion::VERSION_1
            | IgvmAttestRequestVersion::VERSION_2
            | IgvmAttestRequestVersion::VERSION_3
    ) || (request_type == IgvmAttestRequestType::AK_CERT_REQUEST
        && version == IgvmAttestRequestVersion::VERSION_3)
    {
        return Err(Error::InvalidRequestVersion {
            version,
            request_type,
        });
    }

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
            .with_retry(true)
            .with_skip_hw_unsealing(true)
            .with_use_rsa_aes_key_wrap_384(true)
            // Signal the IGVM Agent to fetch the CoRIM launch endorsement.
            // TDX only for now.
            .with_corim_endorsement(matches!(report_type, &ReportType::Tdx));
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
        ReportType::Cca => todo!(),
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
pub(crate) mod tests {
    use super::*;
    use openhcl_attestation_protocol::igvm_attest::get::IgvmAttestResponseExtensions;
    use openhcl_attestation_protocol::igvm_attest::get::IgvmAttestWrappedKeyResponseExtensions;
    use openhcl_attestation_protocol::igvm_attest::get::decode_key_release_context_hash;
    use openhcl_attestation_protocol::igvm_attest::get::runtime_claims::AttestationTpmVersion;
    use openhcl_attestation_protocol::igvm_attest::get::runtime_claims::HardwareSealingPolicy;
    use test_with_tracing::test;

    const VM_CONFIG_WITHOUT_CONTEXT_HASH: &str = r#"{"root-cert-thumbprint":"","console-enabled":false,"interactive-console-enabled":false,"ipmi-enabled":true,"secure-boot":false,"tpm-enabled":false,"tpm-version":"1.38","tpm-persisted":false,"filtered-vpci-devices-allowed":true,"vmUniqueId":"","hardware-sealing-policy":"signer"}"#;
    const CONTEXT_HASH_HEX: &str =
        "000102030405060708090a0b0c0d0e0f101112131415161718191a1b1c1d1e1f";

    pub(super) fn frame_response(
        version: IgvmAttestResponseVersion,
        payload: &[u8],
        error_info: IgvmErrorInfo,
    ) -> Vec<u8> {
        use openhcl_attestation_protocol::igvm_attest::get::IgvmAttestKeyReleaseResponseHeader;

        let data_size = (response_header_size(version) + payload.len()) as u32;
        let header = if version == IgvmAttestResponseVersion::VERSION_1 {
            IgvmAttestCommonResponseHeader { data_size, version }
                .as_bytes()
                .to_vec()
        } else {
            IgvmAttestKeyReleaseResponseHeader {
                data_size,
                version,
                error_info,
            }
            .as_bytes()
            .to_vec()
        };
        [header.as_slice(), payload].concat()
    }

    pub(crate) fn v3_response(
        request_type: IgvmAttestResponseRequestType,
        payload: &str,
        context_hash: Option<[u8; 32]>,
        error_info: IgvmErrorInfo,
    ) -> Vec<u8> {
        use openhcl_attestation_protocol::igvm_attest::get::IgvmAttestResponseExtensions;
        use openhcl_attestation_protocol::igvm_attest::get::encode_key_release_context_hash;

        let envelope = IgvmAttestResponseEnvelope {
            schema_version: IGVM_ATTEST_RESPONSE_SCHEMA_VERSION,
            request_type,
            payload: payload.to_owned(),
            extensions: IgvmAttestResponseExtensions {
                key_release_context_hash: context_hash
                    .as_ref()
                    .map(encode_key_release_context_hash),
            },
        };
        frame_response(
            IgvmAttestResponseVersion::VERSION_3,
            &serde_json::to_vec(&envelope).unwrap(),
            error_info,
        )
    }

    fn parse_key_payload(
        response: &[u8],
    ) -> Result<ParsedResponsePayload<'_, IgvmAttestResponseExtensions>, Error> {
        parse_response_payload(
            response,
            IgvmAttestResponseRequestType::KeyRelease,
            KEY_RELEASE_RESPONSE_BUFFER_SIZE,
        )
    }

    #[test]
    fn context_hash_canonical_encoding() {
        use openhcl_attestation_protocol::igvm_attest::get::encode_key_release_context_hash;

        let hash = std::array::from_fn(|i| i as u8);
        let encoded = encode_key_release_context_hash(&hash);
        assert_eq!(encoded, CONTEXT_HASH_HEX);
        for hash in [hash, [0; 32], [255; 32]] {
            let encoded = encode_key_release_context_hash(&hash);
            assert_eq!(encoded, hex::encode(hash));
            assert_eq!(encoded.len(), 64);
            assert_eq!(decode_key_release_context_hash(&encoded), Ok(hash));
        }
        for encoded in [
            encoded.clone(),
            encoded.to_ascii_uppercase(),
            encoded.replace('a', "A"),
        ] {
            let decoded = decode_key_release_context_hash(&encoded).unwrap();
            assert_eq!(decoded, hash);
            assert_eq!(encode_key_release_context_hash(&decoded), CONTEXT_HASH_HEX);
        }
        for invalid in invalid_context_hashes() {
            assert!(
                decode_key_release_context_hash(&invalid).is_err(),
                "{invalid:?}"
            );
        }
    }

    fn invalid_context_hashes() -> Vec<String> {
        let mut invalid: Vec<_> = [0, 1, 31, 32, 33, 62, 63, 65, 66, 128]
            .into_iter()
            .map(|len| "0".repeat(len))
            .collect();
        invalid.extend([
            format!("0x{CONTEXT_HASH_HEX}"),
            format!("0X{CONTEXT_HASH_HEX}"),
            format!("0x{}", &CONTEXT_HASH_HEX[2..]), // exactly 64 bytes, but not hex
            format!(" {CONTEXT_HASH_HEX}"),
            format!("{CONTEXT_HASH_HEX}\n"),
            format!("{} ", &CONTEXT_HASH_HEX[..63]),
            CONTEXT_HASH_HEX.replacen('a', "\t", 1),
            CONTEXT_HASH_HEX.replacen('a', "\n", 1),
            CONTEXT_HASH_HEX.replacen('a', "\0", 1),
            CONTEXT_HASH_HEX.replacen('a', "g", 1),
            CONTEXT_HASH_HEX.replacen('a', "G", 1),
            "!".repeat(64),
            "é".repeat(32),  // 64 bytes, but not ASCII
            "０".repeat(64), // 64 characters, but not ASCII
            // Previously valid standard padded base64 must no longer be accepted.
            "AAECAwQFBgcICQoLDA0ODxAREhMUFRYXGBkaGxwdHh8=".to_owned(),
            "AAECAwQFBgcICQoLDA0ODxAREhMUFRYXGBkaGxwdHh8".to_owned(),
            format!("{}8=", "_".repeat(42)), // URL-safe base64
        ]);
        invalid
    }

    #[test]
    fn v3_accepts_context_hash_hex_casing() {
        let payload = serde_json::to_string(
            &openhcl_attestation_protocol::igvm_attest::akv::AkvKeyReleaseKeyBlob {
                ciphertext: vec![42; 520],
            },
        )
        .unwrap();
        for hash in [
            CONTEXT_HASH_HEX.to_owned(),
            CONTEXT_HASH_HEX.to_ascii_uppercase(),
            CONTEXT_HASH_HEX.replace('a', "A"),
        ] {
            let envelope = serde_json::json!({
                "schema_version": 1, "request_type": "key_release", "payload": payload,
                "extensions": {"key_release_context_hash": hash}
            });
            let response = frame_response(
                IgvmAttestResponseVersion::VERSION_3,
                &serde_json::to_vec(&envelope).unwrap(),
                IgvmErrorInfo::default(),
            );
            let expected = std::array::from_fn(|i| i as u8);
            let parsed =
                key_release::parse_response_requiring_context(&response, 256, true).unwrap();
            assert_eq!(parsed.key_release_context_hash, Some(expected));
            assert_eq!(parsed.wrapped_key, vec![42; 520]);
        }
    }

    #[test]
    fn v3_payload_roundtrip_and_legacy_borrowing() {
        for request_type in [
            IgvmAttestResponseRequestType::KeyRelease,
            IgvmAttestResponseRequestType::WrappedKey,
        ] {
            for hash in [None, Some([42; 32])] {
                let response = v3_response(
                    request_type,
                    "{\"original\":\"payload\"}",
                    hash,
                    IgvmErrorInfo::default(),
                );
                let payload = match request_type {
                    IgvmAttestResponseRequestType::KeyRelease => {
                        let parsed = parse_key_payload(&response).unwrap();
                        assert_eq!(
                            parsed.extensions.key_release_context_hash,
                            hash.map(hex::encode)
                        );
                        parsed.payload
                    }
                    IgvmAttestResponseRequestType::WrappedKey => {
                        let parsed =
                            parse_response_payload::<IgvmAttestWrappedKeyResponseExtensions>(
                                &response,
                                request_type,
                                65536,
                            )
                            .unwrap();
                        // Wrapped-key metadata exposes no context hash.
                        assert_eq!(
                            parsed.extensions,
                            IgvmAttestWrappedKeyResponseExtensions::default()
                        );
                        assert_eq!(serde_json::to_string(&parsed.extensions).unwrap(), "{}");
                        parsed.payload
                    }
                };
                assert!(matches!(payload, Cow::Owned(_)));
                assert_eq!(payload, "{\"original\":\"payload\"}");
            }
        }
        for version in [
            IgvmAttestResponseVersion::VERSION_1,
            IgvmAttestResponseVersion::VERSION_2,
        ] {
            let response = frame_response(version, b"legacy", IgvmErrorInfo::default());
            let parsed = parse_key_payload(&response).unwrap();
            assert!(matches!(parsed.payload, Cow::Borrowed("legacy")));
            assert_eq!(parsed.extensions.key_release_context_hash, None);
            let wrapped = parse_response_payload::<IgvmAttestWrappedKeyResponseExtensions>(
                &response,
                IgvmAttestResponseRequestType::WrappedKey,
                65536,
            )
            .unwrap();
            assert!(matches!(wrapped.payload, Cow::Borrowed("legacy")));
            assert_eq!(
                wrapped.extensions,
                IgvmAttestWrappedKeyResponseExtensions::default()
            );
        }
    }

    #[test]
    fn v3_rejects_invalid_envelopes_without_downgrade() {
        // Use raw JSON so duplicate fields are preserved in the test input.
        for invalid in [
            r#"{"schema_version":2,"request_type":"key_release","payload":"x","extensions":{}}"#,
            r#"{"schema_version":0,"request_type":"key_release","payload":"x","extensions":{}}"#,
            r#"{"schema_version":1,"request_type":"wrapped_key","payload":"x","extensions":{}}"#,
            r#"{"schema_version":1,"request_type":"ak_cert","payload":"x","extensions":{}}"#,
            r#"{"schema_version":1,"schema_version":1,"request_type":"key_release","payload":"x","extensions":{}}"#,
            r#"{"schema_version":1,"request_type":"key_release","request_type":"key_release","payload":"x","extensions":{}}"#,
            r#"{"schema_version":1,"request_type":"key_release","payload":"x","payload":"x","extensions":{}}"#,
            r#"{"schema_version":1,"request_type":"key_release","payload":"x","extensions":{},"extensions":{}}"#,
            r#"{"schema_version":1,"request_type":"key_release","payload":"x","extensions":{"key_release_context_hash":null}}"#,
            r#"{"schema_version":1,"request_type":"key_release","payload":"x","extensions":{"key_release_context_hash":5}}"#,
            r#"{"schema_version":1,"request_type":"key_release","payload":"x","extensions":{"key_release_context_hash":false}}"#,
            r#"{"schema_version":1,"request_type":"key_release","payload":"x","extensions":{"key_release_context_hash":[]}}"#,
            r#"{"schema_version":1,"request_type":"key_release","payload":"x","extensions":{"key_release_context_hash":{}}}"#,
            r#"{"schema_version":1,"request_type":"key_release","payload":"x","extensions":null}"#,
            r#"{"schema_version":1,"request_type":"key_release","payload":"x","extensions":[]}"#,
            r#"{"schema_version":1,"request_type":"key_release","payload":"x","extensions":[null]}"#,
            r#"{"schema_version":1,"request_type":"key_release","payload":"x","extensions":42}"#,
            r#"{"schema_version":1,"request_type":"key_release","payload":"x","extensions":"ignored"}"#,
            r#"[1,"key_release","x",{}]"#,
            r#"{"schema_version":1,"request_type":"key_release","payload":{},"extensions":{}}"#,
            r#"{"schema_version":1,"request_type":"key_release","payload":"x"}"#,
            r#"{"request_type":"key_release","payload":"x","extensions":{}}"#,
            r#"{"schema_version":1,"payload":"x","extensions":{}}"#,
            r#"{"schema_version":1,"request_type":"key_release","extensions":{}}"#,
            r#"{"schema_version":1,"request_type":"key_release","payload":"\ud800","extensions":{}}"#,
            r#"{"ciphertext":"legacy JSON cannot downgrade V3"}"#,
            "a.b.c",
        ] {
            let response = frame_response(
                IgvmAttestResponseVersion::VERSION_3,
                invalid.as_bytes(),
                IgvmErrorInfo::default(),
            );
            assert!(parse_key_payload(&response).is_err(), "accepted {invalid}");
            assert!(
                key_release::parse_response(&response, 256).is_err(),
                "accepted {invalid}"
            );
        }

        let hash = CONTEXT_HASH_HEX;
        let duplicate = format!(
            r#"{{"schema_version":1,"request_type":"key_release","payload":"x","extensions":{{"key_release_context_hash":"{hash}","key_release_context_hash":"{hash}"}}}}"#
        );
        let response = frame_response(
            IgvmAttestResponseVersion::VERSION_3,
            duplicate.as_bytes(),
            IgvmErrorInfo::default(),
        );
        assert!(matches!(
            parse_key_payload(&response),
            Err(Error::InvalidResponseEnvelope(_))
        ));
    }

    #[test]
    fn v3_rejects_invalid_context_hashes() {
        for hash in invalid_context_hashes() {
            let envelope = serde_json::json!({
                "schema_version": 1, "request_type": "key_release", "payload": "x",
                "extensions": {"key_release_context_hash": hash}
            });
            let response = frame_response(
                IgvmAttestResponseVersion::VERSION_3,
                &serde_json::to_vec(&envelope).unwrap(),
                IgvmErrorInfo::default(),
            );
            assert!(matches!(
                key_release::parse_response(&response, 256),
                Err(key_release::KeyReleaseError::ParseHeader(
                    Error::InvalidContextHash(_)
                ))
            ));
        }
    }

    #[test]
    fn response_bounds_and_utf8() {
        for version in [
            IgvmAttestResponseVersion::VERSION_1,
            IgvmAttestResponseVersion::VERSION_2,
            IgvmAttestResponseVersion::VERSION_3,
        ] {
            let response = frame_response(version, &[0xff], IgvmErrorInfo::default());
            assert!(matches!(
                parse_key_payload(&response),
                Err(Error::InvalidResponseUtf8(_))
            ));
            for end in 0..response.len() {
                assert!(parse_key_payload(&response[..end]).is_err());
            }
            for declared_size in 0..response_header_size(version) {
                let mut response = response.clone();
                response[..4].copy_from_slice(&(declared_size as u32).to_le_bytes());
                assert!(parse_key_payload(&response).is_err());
            }
        }
        let envelope = br#"{"schema_version":1,"request_type":"key_release","payload":"x","extensions":{"future_extension":{"opaque":[1,2,3]}}}"#;
        for version in [
            IgvmAttestResponseVersion::VERSION_1,
            IgvmAttestResponseVersion::VERSION_2,
            IgvmAttestResponseVersion::VERSION_3,
        ] {
            let mut payload = envelope.to_vec();
            payload.resize(65536 - response_header_size(version), b' ');
            let mut response = frame_response(version, &payload, IgvmErrorInfo::default());
            assert_eq!(response.len(), 65536);
            assert!(parse_key_payload(&response).is_ok());
            assert!(matches!(
                parse_response_payload::<IgvmAttestResponseExtensions>(
                    &response,
                    IgvmAttestResponseRequestType::KeyRelease,
                    65535,
                ),
                Err(Error::ResponseSizeTooLarge { .. })
            ));
            // Actual buffer size counts too, even if data_size is unchanged.
            response.push(b' ');
            assert!(matches!(
                parse_key_payload(&response),
                Err(Error::ResponseSizeTooLarge { .. })
            ));
            response[..4].copy_from_slice(&65537u32.to_le_bytes());
            assert!(parse_key_payload(&response).is_err());
        }
        // A valid shorter response may be returned in a padded transport buffer.
        let mut response = v3_response(
            IgvmAttestResponseRequestType::KeyRelease,
            "x",
            None,
            IgvmErrorInfo::default(),
        );
        response.extend_from_slice(&[0; 16]);
        assert_eq!(parse_key_payload(&response).unwrap().payload, "x");
        for version in [0, 4, u32::MAX] {
            let response = frame_response(
                IgvmAttestResponseVersion(version),
                b"",
                IgvmErrorInfo::default(),
            );
            assert!(matches!(
                parse_key_payload(&response),
                Err(Error::InvalidResponseHeaderVersion { .. })
            ));
        }
    }

    #[test]
    fn v3_request_layout_ak_pin_and_v2_response() {
        use openhcl_attestation_protocol::igvm_attest::get::IGVM_ATTEST_REQUEST_CURRENT_VERSION;
        use openhcl_attestation_protocol::igvm_attest::get::IgvmAttestRequestBase;
        use openhcl_attestation_protocol::igvm_attest::get::IgvmAttestRequestDataExt;

        assert_eq!(
            IGVM_ATTEST_REQUEST_CURRENT_VERSION,
            IgvmAttestRequestVersion::VERSION_3
        );
        assert_eq!(
            IGVM_ATTEST_RESPONSE_CURRENT_VERSION,
            IgvmAttestResponseVersion::VERSION_3
        );
        assert_eq!(
            response_header_size(IgvmAttestResponseVersion::VERSION_3),
            32
        );
        for request_type in [
            IgvmAttestRequestType::KEY_RELEASE_REQUEST,
            IgvmAttestRequestType::WRAPPED_KEY_REQUEST,
            IgvmAttestRequestType::AK_CERT_REQUEST,
        ] {
            let helper = IgvmAttestRequestHelper {
                request_type,
                report_type: ReportType::Tvm,
                runtime_claims: b"{}".to_vec(),
                runtime_claims_hash: [0; tee_call::REPORT_DATA_SIZE],
                hash_type: IgvmAttestHashType::SHA_256,
            };
            let request = helper
                .create_request(IGVM_ATTEST_REQUEST_CURRENT_VERSION, &[])
                .unwrap();
            let (base, remaining) = IgvmAttestRequestBase::read_from_prefix(&request).unwrap();
            assert_eq!(base.header.version, 2);
            assert_eq!(base.header.report_size as usize, request.len());
            assert_eq!(
                base.request_data.version,
                if request_type == IgvmAttestRequestType::AK_CERT_REQUEST {
                    IgvmAttestRequestVersion::VERSION_2
                } else {
                    IgvmAttestRequestVersion::VERSION_3
                }
            );
            let (extension, claims) =
                IgvmAttestRequestDataExt::read_from_prefix(remaining).unwrap();
            assert!(extension.capability_bitmap.error_code());
            assert!(extension.capability_bitmap.retry());
            assert!(extension.capability_bitmap.skip_hw_unsealing());
            assert!(extension.capability_bitmap.use_rsa_aes_key_wrap_384());
            assert_eq!(claims, b"{}");
            for version in [0, 4, u32::MAX] {
                assert!(matches!(
                    helper.create_request(IgvmAttestRequestVersion(version), &[]),
                    Err(Error::InvalidRequestVersion { .. })
                ));
            }
        }
        assert!(matches!(
            create_request(
                IgvmAttestRequestVersion::VERSION_3,
                IgvmAttestRequestType::AK_CERT_REQUEST,
                &[],
                &[],
                &ReportType::Tvm,
                IgvmAttestHashType::SHA_256
            ),
            Err(Error::InvalidRequestVersion { .. })
        ));
        // The response parser does not assume the request's V3 version: old
        // agents may continue responding with V2 without any context metadata.
        let response = frame_response(
            IgvmAttestResponseVersion::VERSION_2,
            b"original service payload",
            IgvmErrorInfo::default(),
        );
        let parsed = parse_key_payload(&response).unwrap();
        assert_eq!(parsed.header.version, IgvmAttestResponseVersion::VERSION_2);
        assert_eq!(parsed.extensions.key_release_context_hash, None);
    }

    #[test]
    fn test_create_request() {
        let result = create_request(
            IgvmAttestRequestVersion::VERSION_2,
            IgvmAttestRequestType::AK_CERT_REQUEST,
            &[],
            &[0u8; openhcl_attestation_protocol::igvm_attest::get::SNP_VM_REPORT_SIZE],
            &ReportType::Snp,
            IgvmAttestHashType::SHA_256,
        );
        assert!(result.is_ok());

        let result = create_request(
            IgvmAttestRequestVersion::VERSION_2,
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
        assert!(ext.capability_bitmap.skip_hw_unsealing());
        // CoRIM endorsement is requested for TDX only; an SNP request must not
        // set the bit.
        assert!(!ext.capability_bitmap.corim_endorsement());

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
    fn test_create_request_version2_tdx_requests_corim() {
        use openhcl_attestation_protocol::igvm_attest::get::IgvmAttestRequestBase;
        use openhcl_attestation_protocol::igvm_attest::get::IgvmAttestRequestDataExt;
        use openhcl_attestation_protocol::igvm_attest::get::IgvmAttestRequestVersion;

        let runtime_claims = vec![4u8, 5, 6, 7];
        let attestation_report =
            vec![0u8; openhcl_attestation_protocol::igvm_attest::get::TDX_VM_REPORT_SIZE];

        let buffer = create_request(
            IgvmAttestRequestVersion::VERSION_2,
            IgvmAttestRequestType::KEY_RELEASE_REQUEST,
            &runtime_claims,
            &attestation_report,
            &ReportType::Tdx,
            IgvmAttestHashType::SHA_256,
        )
        .expect("request generation");

        let header_size = size_of::<IgvmAttestRequestBase>();
        let (ext, _) = IgvmAttestRequestDataExt::read_from_prefix(&buffer[header_size..])
            .expect("parse IgvmAttestRequestDataExt");
        // TDX requests must signal the IGVM Agent to fetch the CoRIM endorsement.
        assert!(ext.capability_bitmap.corim_endorsement());
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
        let attestation_vm_config = AttestationVmConfig {
            current_time: None,
            root_cert_thumbprint: String::new(),
            console_enabled: false,
            interactive_console_enabled: false,
            ipmi_enabled: true,
            secure_boot: false,
            tpm_enabled: false,
            tpm_version: AttestationTpmVersion::V138,
            tpm_persisted: false,
            hardware_sealing_policy: HardwareSealingPolicy::Signer,
            filtered_vpci_devices_allowed: true,
            vm_unique_id: String::new(),
            vmgs_provisioner: None,
            key_release_context_hash: None,
        };
        let result = serde_json::to_string(&attestation_vm_config);
        assert!(result.is_ok());

        let vm_config = result.unwrap();
        assert_eq!(vm_config, VM_CONFIG_WITHOUT_CONTEXT_HASH);
        let decoded: AttestationVmConfig = serde_json::from_str(&vm_config).unwrap();
        assert!(decoded.key_release_context_hash.is_none());
        assert_eq!(serde_json::to_string(&decoded).unwrap(), vm_config);
    }

    #[test]
    fn test_vm_configuration_context_hash_roundtrip() {
        let context_hash = CONTEXT_HASH_HEX;
        assert_eq!(
            hex::decode(context_hash).unwrap(),
            (0u8..32).collect::<Vec<_>>()
        );
        let mut config: AttestationVmConfig =
            serde_json::from_str(VM_CONFIG_WITHOUT_CONTEXT_HASH).unwrap();
        config.key_release_context_hash = Some(context_hash.to_owned());
        let config = attestation_vm_config_with_time(&config, 1691103220);
        let serialized = serde_json::to_value(&config).unwrap();
        assert_eq!(serialized["key-release-context-hash"], context_hash);
        assert!(serialized.get("key_release_context_hash").is_none());
        let decoded: AttestationVmConfig = serde_json::from_value(serialized).unwrap();
        assert_eq!(
            decoded.key_release_context_hash.as_deref(),
            Some(context_hash)
        );
        assert_eq!(decoded.current_time, Some(1691103220));
    }

    #[test]
    fn test_vm_configuration_null_context_hash_is_omitted() {
        let mut value: serde_json::Value =
            serde_json::from_str(VM_CONFIG_WITHOUT_CONTEXT_HASH).unwrap();
        value["key-release-context-hash"] = serde_json::Value::Null;
        let config: AttestationVmConfig = serde_json::from_value(value).unwrap();
        assert!(config.key_release_context_hash.is_none());
        assert_eq!(
            serde_json::to_string(&config).unwrap(),
            VM_CONFIG_WITHOUT_CONTEXT_HASH
        );
    }

    #[test]
    fn test_vm_configuration_with_time() {
        const EXPECTED_JWK: &str = r#"{"current-time":1691103220,"root-cert-thumbprint":"","console-enabled":false,"interactive-console-enabled":false,"ipmi-enabled":false,"secure-boot":false,"tpm-enabled":false,"tpm-version":"185","tpm-persisted":false,"filtered-vpci-devices-allowed":true,"vmUniqueId":"","hardware-sealing-policy":"hash"}"#;

        let attestation_vm_config = AttestationVmConfig {
            current_time: None,
            root_cert_thumbprint: String::new(),
            console_enabled: false,
            interactive_console_enabled: false,
            ipmi_enabled: false,
            secure_boot: false,
            tpm_enabled: false,
            tpm_version: AttestationTpmVersion::V185,
            tpm_persisted: false,
            hardware_sealing_policy: HardwareSealingPolicy::Hash,
            filtered_vpci_devices_allowed: true,
            vm_unique_id: String::new(),
            vmgs_provisioner: None,
            key_release_context_hash: None,
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
            0x2a, 0x00, 0x00, 0x00, 0x04, 0x00, 0x00, 0x00, 0x30, 0x82, 0x03, 0xeb, 0x30, 0x82,
            0x02, 0xd3, 0xa0, 0x03, 0x02, 0x01, 0x02, 0x02, 0x10, 0x3b, 0xa3, 0x33, 0x97, 0xef,
            0x2f, 0x9e, 0xef, 0xbd, 0x35, 0x5e, 0xda, 0xdd, 0x27, 0x38, 0x42, 0x30, 0x0d, 0x06,
        ];

        let result = parse_response_header(&INVALID_RESPONSE);
        assert!(result.is_err());
        assert_eq!(
            result.unwrap_err().to_string(),
            Error::InvalidResponseHeaderVersion {
                version: IgvmAttestResponseVersion(4),
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
                retry_signal: true,
                skip_hw_unsealing_signal: false
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
                retry_signal: false,
                skip_hw_unsealing_signal: false
            }
            .to_string()
        );
    }

    #[test]
    fn test_failed_response_with_skip_hw_unsealing_signal() {
        // error_code: 1103 (0x44f), http_status_code: 400 (0x190),
        // igvm_signal: retry=true, skip_hw_unsealing=true (0x03 = bits 0 and 1 set)
        const INVALID_RESPONSE: [u8; 42] = [
            0x2a, 0x00, 0x00, 0x00, 0x02, 0x00, 0x00, 0x00, 0x4f, 0x04, 0x00, 0x00, 0x90, 0x01,
            0x00, 0x00, 0x03, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
            0x00, 0x00, 0x00, 0x00, 0x35, 0x5e, 0xda, 0xdd, 0x27, 0x38, 0x42, 0x30, 0x0d, 0x06,
        ];

        let result = parse_response_header(&INVALID_RESPONSE);
        assert!(result.is_err());
        assert_eq!(
            result.unwrap_err().to_string(),
            Error::Attestation {
                igvm_error_code: 1103,
                http_status_code: 400,
                retry_signal: true,
                skip_hw_unsealing_signal: true
            }
            .to_string()
        );
    }

    #[test]
    fn test_failed_response_with_skip_hw_unsealing_only() {
        // error_code: 1103 (0x44f), http_status_code: 400 (0x190),
        // igvm_signal: retry=false, skip_hw_unsealing=true (0x02 = bit 1 set)
        const INVALID_RESPONSE: [u8; 42] = [
            0x2a, 0x00, 0x00, 0x00, 0x02, 0x00, 0x00, 0x00, 0x4f, 0x04, 0x00, 0x00, 0x90, 0x01,
            0x00, 0x00, 0x02, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
            0x00, 0x00, 0x00, 0x00, 0x35, 0x5e, 0xda, 0xdd, 0x27, 0x38, 0x42, 0x30, 0x0d, 0x06,
        ];

        let result = parse_response_header(&INVALID_RESPONSE);
        assert!(result.is_err());
        assert_eq!(
            result.unwrap_err().to_string(),
            Error::Attestation {
                igvm_error_code: 1103,
                http_status_code: 400,
                retry_signal: false,
                skip_hw_unsealing_signal: true
            }
            .to_string()
        );
    }
}
