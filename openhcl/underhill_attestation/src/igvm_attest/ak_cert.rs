// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! The module for `AK_CERT_REQUEST` request type that supports parsing the
//! response.

use openhcl_attestation_protocol::igvm_attest::get::IgvmAttestAkCertResponseHeader;
use openhcl_attestation_protocol::igvm_attest::get::AK_CERT_RESPONSE_HEADER_VERSION;
use thiserror::Error;
use zerocopy::FromBytes;

/// AkCertError is returned by parse_ak_cert_response() in emuplat/tpm.rs
#[derive(Debug, Error)]
pub enum AkCertError {
    #[error("AK cert response is too small to parse. Found {size} bytes but expected at least {minimum_size}")]
    SizeTooSmall { size: usize, minimum_size: usize },
    #[error(
        "AK cert response size {specified_size} specified in the header is larger then the actual size {size}"
    )]
    SizeMismatch { size: usize, specified_size: usize },
    #[error(
        "AK cert response header version {version} does match the expected version {expected_version}"
    )]
    HeaderVersionMismatch { version: u32, expected_version: u32 },
}

/// Parse a `AK_CERT_REQUEST` response and return the payload (i.e., the AK cert).
///
/// Returns `Ok(Vec<u8>)` on successfully validating the response, otherwise returns an error.
pub fn parse_response(response: &[u8]) -> Result<Vec<u8>, AkCertError> {
    const HEADER_SIZE: usize = size_of::<IgvmAttestAkCertResponseHeader>();

    let header = IgvmAttestAkCertResponseHeader::read_from_prefix(response)
        .map_err(|_| AkCertError::SizeTooSmall {
            size: response.len(),
            minimum_size: HEADER_SIZE,
        })? // TODO: zerocopy: map_err (https://github.com/microsoft/openvmm/issues/759)
        .0;

    let size = header.data_size as usize;
    if size > response.len() {
        Err(AkCertError::SizeMismatch {
            size: response.len(),
            specified_size: size,
        })?
    }

    if header.version != AK_CERT_RESPONSE_HEADER_VERSION {
        Err(AkCertError::HeaderVersionMismatch {
            version: header.version,
            expected_version: AK_CERT_RESPONSE_HEADER_VERSION,
        })?
    }

    Ok(response[HEADER_SIZE..size].to_vec())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_undersized_response() {
        const HEADER_SIZE: usize = size_of::<IgvmAttestAkCertResponseHeader>();
        let properly_sized_response: [u8; HEADER_SIZE] = [1; HEADER_SIZE];
        let undersized_response = &properly_sized_response[..HEADER_SIZE - 1];

        // Empty response counts as an undersized response
        let result = parse_response(&[]);
        assert!(result.is_err());
        assert_eq!(
            result.unwrap_err().to_string(),
            "AK cert response is too small to parse. Found 0 bytes but expected at least 8"
        );

        // Response has to be at least `HEADER_SIZE` bytes long, so `HEADER_SIZE - 1` bytes is too small.
        let undersized_parse_ = parse_response(undersized_response);
        assert!(undersized_parse_.is_err());
        assert_eq!(
            undersized_parse_.unwrap_err().to_string(),
            format!(
                "AK cert response is too small to parse. Found {} bytes but expected at least {}",
                HEADER_SIZE - 1,
                HEADER_SIZE
            )
        );

        // When we finally have `HEADER_SIZE` bytes, we no longer see the failure as `AkCertError::SizeTooSmall`,
        // but we still see a different error since the response is not valid.
        let properly_sized_parse = parse_response(&properly_sized_response);
        assert!(!properly_sized_parse
            .unwrap_err()
            .to_string()
            .starts_with("AK cert response is too small to parse"),);
    }

    #[test]
    fn test_valid_response_size_match() {
        const VALID_RESPONSE: [u8; 56] = [
            0x38, 0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x30, 0x82, 0x03, 0xeb, 0x30, 0x82,
            0x02, 0xd3, 0xa0, 0x03, 0x02, 0x01, 0x02, 0x02, 0x10, 0x3b, 0xa3, 0x33, 0x97, 0xef,
            0x2f, 0x9e, 0xef, 0xbd, 0x35, 0x5e, 0xda, 0xdd, 0x27, 0x38, 0x42, 0x30, 0x0d, 0x06,
            0x09, 0x2a, 0x86, 0x48, 0x86, 0xf7, 0x0d, 0x01, 0x01, 0x0b, 0x05, 0x00, 0x30, 0x25,
        ];

        const HEADER_SIZE: usize = size_of::<IgvmAttestAkCertResponseHeader>();

        let result = IgvmAttestAkCertResponseHeader::read_from_prefix(&VALID_RESPONSE);
        assert!(result.is_ok());
        let header = result.unwrap().0;

        let result = parse_response(&VALID_RESPONSE);
        assert!(result.is_ok());

        let payload = result.unwrap();
        assert_eq!(payload.len(), header.data_size as usize - HEADER_SIZE);
        assert_eq!(
            payload,
            &VALID_RESPONSE[HEADER_SIZE..header.data_size as usize]
        );
    }

    #[test]
    fn test_valid_response_size_smaller_than_specified() {
        const VALID_RESPONSE: [u8; 56] = [
            0x37, 0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x30, 0x82, 0x03, 0xeb, 0x30, 0x82,
            0x02, 0xd3, 0xa0, 0x03, 0x02, 0x01, 0x02, 0x02, 0x10, 0x3b, 0xa3, 0x33, 0x97, 0xef,
            0x2f, 0x9e, 0xef, 0xbd, 0x35, 0x5e, 0xda, 0xdd, 0x27, 0x38, 0x42, 0x30, 0x0d, 0x06,
            0x09, 0x2a, 0x86, 0x48, 0x86, 0xf7, 0x0d, 0x01, 0x01, 0x0b, 0x05, 0x00, 0x30, 0x25,
        ];

        const HEADER_SIZE: usize = size_of::<IgvmAttestAkCertResponseHeader>();

        let result = IgvmAttestAkCertResponseHeader::read_from_prefix(&VALID_RESPONSE);
        assert!(result.is_ok());
        let header = result.unwrap().0;

        let result = parse_response(&VALID_RESPONSE);
        assert!(result.is_ok());

        let payload = result.unwrap();
        assert_eq!(payload.len(), header.data_size as usize - HEADER_SIZE);
        assert_eq!(
            payload,
            &VALID_RESPONSE[HEADER_SIZE..header.data_size as usize]
        );
    }

    #[test]
    fn test_invalid_header_version() {
        const INVALID_RESPONSE: [u8; 56] = [
            0x38, 0x00, 0x00, 0x00, 0x02, 0x00, 0x00, 0x00, 0x30, 0x82, 0x03, 0xeb, 0x30, 0x82,
            0x02, 0xd3, 0xa0, 0x03, 0x02, 0x01, 0x02, 0x02, 0x10, 0x3b, 0xa3, 0x33, 0x97, 0xef,
            0x2f, 0x9e, 0xef, 0xbd, 0x35, 0x5e, 0xda, 0xdd, 0x27, 0x38, 0x42, 0x30, 0x0d, 0x06,
            0x09, 0x2a, 0x86, 0x48, 0x86, 0xf7, 0x0d, 0x01, 0x01, 0x0b, 0x05, 0x00, 0x30, 0x25,
        ];

        let result = parse_response(&INVALID_RESPONSE);
        assert!(result.is_err());
    }

    #[test]
    fn test_invalid_response_size() {
        const INVALID_RESPONSE: [u8; 56] = [
            0x39, 0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x30, 0x82, 0x03, 0xeb, 0x30, 0x82,
            0x02, 0xd3, 0xa0, 0x03, 0x02, 0x01, 0x02, 0x02, 0x10, 0x3b, 0xa3, 0x33, 0x97, 0xef,
            0x2f, 0x9e, 0xef, 0xbd, 0x35, 0x5e, 0xda, 0xdd, 0x27, 0x38, 0x42, 0x30, 0x0d, 0x06,
            0x09, 0x2a, 0x86, 0x48, 0x86, 0xf7, 0x0d, 0x01, 0x01, 0x0b, 0x05, 0x00, 0x30, 0x25,
        ];

        let result = parse_response(&INVALID_RESPONSE);
        assert!(result.is_err());
    }
}
