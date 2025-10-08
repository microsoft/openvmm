// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! The module for `WRAPPED_KEY_REQUEST` request type that supports parsing the
//! response in JSON format defined by Azure CVM Provisioning Service (CPS).

use crate::igvm_attest::Error as CommonError;
use crate::igvm_attest::parse_response_header;
use openhcl_attestation_protocol::igvm_attest::cps;
use thiserror::Error;

#[derive(Debug, Error)]
pub(crate) enum WrappedKeyError {
    #[error("failed to deserialize the response payload into JSON: {json_data}")]
    WrappedKeyResponsePayloadToJson {
        #[source]
        json_err: serde_json::Error,
        json_data: String,
    },
    #[error("the response payload size is too small to parse")]
    PayloadSizeTooSmall,
    #[error("error in response header)")]
    ParseHeader(#[source] CommonError),
    #[error("invalid response header version: {0}")]
    InvalidResponseVersion(u32),
}

/// Return value of the [`parse_response`].
pub struct IgvmWrappedKeyParsedResponse {
    /// Wrapped DiskEncryptionSettings key.
    pub wrapped_key: Vec<u8>,
    /// Key reference in JSON string.
    pub key_reference: Vec<u8>,
}

/// Parse a `WRAPPED_KEY_REQUEST` response and return a wrapped key blob.
///
/// Returns `Ok(IgvmWrappedKeyParsedResponse)` on successfully extracting a wrapped DiskEncryptionSettings
/// key from `response`, otherwise returns an error.
pub fn parse_response(response: &[u8]) -> Result<IgvmWrappedKeyParsedResponse, WrappedKeyError> {
    use openhcl_attestation_protocol::igvm_attest::get::IgvmAttestCommonResponseHeader;
    use openhcl_attestation_protocol::igvm_attest::get::IgvmAttestResponseVersion;
    use openhcl_attestation_protocol::igvm_attest::get::IgvmAttestWrappedKeyResponseHeader;

    // Minimum acceptable payload would look like {"ciphertext":"base64URL wrapped key"}
    const CIPHER_TEXT_KEY: &str = r#"{"ciphertext":""}"#;
    const MINIMUM_WRAPPED_KEY_SIZE: usize = 256;
    const MINIMUM_WRAPPED_KEY_BASE64_URL_SIZE: usize = MINIMUM_WRAPPED_KEY_SIZE / 3 * 4;
    const MINIMUM_PAYLOAD_SIZE: usize = CIPHER_TEXT_KEY.len() + MINIMUM_WRAPPED_KEY_BASE64_URL_SIZE;

    let header = parse_response_header(response).map_err(WrappedKeyError::ParseHeader)?;

    // Extract payload as per header version
    let header_size = match header.version {
        IgvmAttestResponseVersion::VERSION_1 => size_of::<IgvmAttestCommonResponseHeader>(),
        IgvmAttestResponseVersion::VERSION_2 => size_of::<IgvmAttestWrappedKeyResponseHeader>(),
        invalid_version => return Err(WrappedKeyError::InvalidResponseVersion(invalid_version.0)),
    };
    let payload = &response[header_size..header.data_size as usize];

    if payload.len() < MINIMUM_PAYLOAD_SIZE {
        Err(WrappedKeyError::PayloadSizeTooSmall)?
    }
    let payload = String::from_utf8_lossy(payload);
    let payload: cps::VmmdBlob = serde_json::from_str(&payload).map_err(|json_err| {
        WrappedKeyError::WrappedKeyResponsePayloadToJson {
            json_err,
            json_data: payload.to_string(),
        }
    })?;
    let wrapped_key = payload
        .disk_encryption_settings
        .encryption_info
        .aes_info
        .ciphertext;

    let key_reference = payload
        .disk_encryption_settings
        .encryption_info
        .key_reference
        .to_string()
        .as_bytes()
        .to_vec();

    Ok(IgvmWrappedKeyParsedResponse {
        wrapped_key,
        key_reference,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use openhcl_attestation_protocol::igvm_attest::get::IgvmErrorInfo;
    use zerocopy::IntoBytes;

    const KEY_REFERENCE: &str = r#"{
    "key_info": {
        "host": "name"
    },
    "attestation_info": {
        "host": "attestation_name"
    }
}"#;

    #[test]
    fn test_response() {
        const JSON_DATA: &str = r#"
{
  "version": "1.0",
  "DiskEncryptionSettings": {
    "encryption_info": {
      "aes_info": {
        "ciphertext": "Q0lQSEVSVEVYVA==",
        "algorithm": "AES_256_WRAP_PAD",
        "creation_time": "2023-11-03T22:58:59.7967119Z"
      },
      "key_reference": {
        "key_info": {
          "auth_method": "msi",
          "host": "HOST",
          "key_name": "cvmps-pmk-key",
          "key_version": "58bf696275cd4b6d8150bb3376981076",
          "aad_msi_res_id": "<identity resource id>",
          "tenant_id": "33e01921-4d64-4f8c-a055-5bdaffd5e33d"
        },
        "attestation_info": {
          "host": "HOST"
        }
      }
    },
    "recoverykey_info": {
      "wrapped_key": "WRAPPEDKEY",
      "os_type": "Windows",
      "encryption_scheme": "WindowsBitLocker",
      "algorithm_type": "RSA-OAEP-256",
      "key_id": "KEYID"
    }
  }
}"#;

        let result = serde_json::from_str(JSON_DATA);
        assert!(result.is_ok());
        let payload: cps::VmmdBlob = result.unwrap();
        assert_eq!(
            payload
                .disk_encryption_settings
                .encryption_info
                .aes_info
                .ciphertext,
            b"CIPHERTEXT"
        );
    }

    #[test]
    fn test_response_without_key_reference() {
        const JSON_DATA: &str = r#"
{
  "DiskEncryptionSettings": {
    "encryption_info": {
      "aes_info": {
        "ciphertext": "TESTKEY",
        "algorithm": "AES_256_WRAP_PAD",
        "creation_time": "2023-11-03T22:58:59.7967119Z"
      }
    }
  }
}"#;
        let result: Result<cps::VmmdBlob, _> = serde_json::from_str(JSON_DATA);
        // Expect to fail
        assert!(result.is_err());
    }

    fn mock_response() -> Vec<u8> {
        use openhcl_attestation_protocol::igvm_attest::get::IGVM_ATTEST_RESPONSE_CURRENT_VERSION;
        use openhcl_attestation_protocol::igvm_attest::get::IgvmAttestWrappedKeyResponseHeader;

        const WRAPPED_KEY: [u8; 256] = [
            0x9d, 0x72, 0x81, 0xbc, 0x6d, 0x0c, 0xeb, 0x8f, 0x32, 0xb9, 0xc3, 0xd0, 0xd2, 0x58,
            0x89, 0x2f, 0x49, 0xb4, 0x40, 0xb1, 0x3d, 0xb1, 0x2f, 0x1e, 0x9c, 0xb5, 0x46, 0x4a,
            0x4a, 0x87, 0xbe, 0x97, 0xf5, 0xa2, 0x90, 0x7a, 0xd1, 0x7d, 0x6c, 0x91, 0x8a, 0x46,
            0x9e, 0xc1, 0x87, 0x9c, 0xa9, 0xb2, 0xcd, 0xc2, 0x6e, 0x6c, 0xdc, 0xda, 0xdd, 0x79,
            0x64, 0x25, 0x7a, 0xd7, 0xb9, 0x5d, 0xd3, 0xc7, 0x82, 0x0d, 0x4a, 0xb1, 0x86, 0xe2,
            0x78, 0xc1, 0x94, 0xe4, 0x81, 0x9b, 0x48, 0xba, 0x90, 0xcb, 0x79, 0x51, 0x0c, 0xda,
            0x98, 0x69, 0xed, 0xc7, 0xc9, 0x0b, 0xde, 0xb5, 0x9a, 0xcb, 0xcc, 0x16, 0x06, 0xa7,
            0x66, 0xfe, 0xd7, 0x41, 0xe6, 0x71, 0xcb, 0x16, 0xb1, 0x16, 0xf8, 0x05, 0x41, 0x9a,
            0x6b, 0x99, 0xa3, 0xc9, 0x3c, 0x7c, 0xa3, 0x26, 0x37, 0x0c, 0xb0, 0x87, 0x6b, 0x2a,
            0xde, 0x9c, 0xce, 0x1a, 0xe8, 0x71, 0xe9, 0xce, 0xf8, 0x53, 0x75, 0xfd, 0x95, 0x47,
            0xf8, 0x60, 0x21, 0xd5, 0xce, 0x33, 0xca, 0x9b, 0x6b, 0x7c, 0xa9, 0x73, 0xe8, 0x5a,
            0x6e, 0x91, 0x57, 0x9c, 0xb1, 0xa1, 0x02, 0xce, 0x67, 0x0e, 0x8f, 0xac, 0x14, 0x0f,
            0xa7, 0x08, 0x7e, 0xa8, 0xb3, 0xb9, 0x25, 0x36, 0x41, 0xae, 0x37, 0x59, 0xf8, 0x0d,
            0x11, 0xc0, 0x81, 0xd9, 0x6f, 0x6b, 0xb1, 0xc3, 0xd1, 0xe3, 0xdd, 0xa9, 0x6d, 0x16,
            0xb2, 0x34, 0xe1, 0xf3, 0xa1, 0xa2, 0x86, 0x83, 0x65, 0x3d, 0x48, 0x9e, 0xa0, 0x50,
            0x15, 0xce, 0x0b, 0x06, 0x0a, 0x87, 0x89, 0x97, 0x42, 0x3d, 0x92, 0x1e, 0xab, 0x91,
            0x62, 0x47, 0x31, 0xfb, 0xca, 0x43, 0xa5, 0x12, 0x2a, 0x2c, 0xde, 0x4a, 0xdc, 0x7a,
            0x7f, 0x38, 0x18, 0xe0, 0x4d, 0xbe, 0xf3, 0xf2, 0xc3, 0xb9, 0x22, 0x22, 0x43, 0x19,
            0xdb, 0x0b, 0x47, 0xc7,
        ];

        let aes_info = cps::AesInfo {
            ciphertext: WRAPPED_KEY.to_vec(),
        };

        let result = serde_json::from_str(KEY_REFERENCE);
        assert!(result.is_ok());
        let key_reference = result.unwrap();

        let encryption_info = cps::EncryptionInfo {
            aes_info,
            key_reference,
        };
        let disk_encryption_settings = cps::DiskEncryptionSettings { encryption_info };
        let payload = cps::VmmdBlob {
            disk_encryption_settings,
        };

        let result = serde_json::to_string(&payload);
        assert!(result.is_ok());
        let payload = result.unwrap();

        let header = IgvmAttestWrappedKeyResponseHeader {
            data_size: (payload.len() + size_of::<IgvmAttestWrappedKeyResponseHeader>()) as u32,
            version: IGVM_ATTEST_RESPONSE_CURRENT_VERSION,
            error_info: IgvmErrorInfo::default(),
        };
        [header.as_bytes(), payload.as_bytes()].concat()
    }

    #[test]
    fn test_mock_response() {
        let response = mock_response();
        let result = parse_response(&response);
        assert!(result.is_ok());
        let igvm_wrapped_key = result.unwrap();
        assert!(!igvm_wrapped_key.wrapped_key.is_empty());

        let result = serde_json::from_str(KEY_REFERENCE);
        assert!(result.is_ok());
        let expected_key_reference: serde_json::Value = result.unwrap();
        assert_eq!(
            igvm_wrapped_key.key_reference,
            expected_key_reference.to_string().as_bytes()
        );
    }
}
