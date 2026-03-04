// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! The module for `AK_CERT_REQUEST` request type that supports parsing the
//! response.
use crate::igvm_attest::Error as CommonError;
use crate::igvm_attest::parse_response_header;

use thiserror::Error;

/// AkCertError is returned by parse_ak_cert_response() in emuplat/tpm.rs
#[derive(Debug, Error)]
pub enum AkCertError {
    #[error(
        "AK cert response is too small to parse. Found {size} bytes but expected at least {minimum_size}"
    )]
    SizeTooSmall { size: usize, minimum_size: usize },
    #[error(
        "AK cert response size {specified_size} specified in the header is larger then the actual size {size}"
    )]
    SizeMismatch { size: usize, specified_size: usize },
    #[error(
        "AK cert response header version {version} does match the expected version {expected_version}"
    )]
    HeaderVersionMismatch { version: u32, expected_version: u32 },
    #[error("error in parsing response header")]
    ParseHeader(#[source] CommonError),
    #[error("invalid response header version: {0}")]
    InvalidResponseVersion(u32),
}

/// Parse a `AK_CERT_REQUEST` response and return the payload (i.e., the AK cert).
///
/// Returns `Ok(Vec<u8>)` on successfully validating the response, otherwise returns an error.
pub fn parse_response(response: &[u8]) -> Result<Vec<u8>, AkCertError> {
    use openhcl_attestation_protocol::igvm_attest::get::IgvmAttestAkCertResponseHeader;
    use openhcl_attestation_protocol::igvm_attest::get::IgvmAttestCommonResponseHeader;
    use openhcl_attestation_protocol::igvm_attest::get::IgvmAttestResponseVersion;

    let header = parse_response_header(response).map_err(AkCertError::ParseHeader)?;

    // Extract payload as per header version
    let header_size = match header.version {
        IgvmAttestResponseVersion::VERSION_1 => size_of::<IgvmAttestCommonResponseHeader>(),
        IgvmAttestResponseVersion::VERSION_2 => size_of::<IgvmAttestAkCertResponseHeader>(),
        invalid_version => return Err(AkCertError::InvalidResponseVersion(invalid_version.0)),
    };

    Ok(response[header_size..header.data_size as usize].to_vec())
}

#[cfg(test)]
mod tests {
    use super::*;
    use openhcl_attestation_protocol::igvm_attest::get::IgvmAttestAkCertResponseHeader;
    use openhcl_attestation_protocol::igvm_attest::get::IgvmAttestCommonResponseHeader;
    use zerocopy::FromBytes;

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
            AkCertError::ParseHeader(CommonError::ResponseSizeTooSmall { response_size: 0 })
                .to_string()
        );

        // Response has to be at least `HEADER_SIZE` bytes long, so `HEADER_SIZE - 1` bytes is too small.
        let undersized_parse_ = parse_response(undersized_response);
        assert!(undersized_parse_.is_err());
        assert_eq!(
            undersized_parse_.unwrap_err().to_string(),
            AkCertError::ParseHeader(CommonError::ResponseSizeTooSmall {
                response_size: HEADER_SIZE - 1
            })
            .to_string()
        );

        // When we finally have `HEADER_SIZE` bytes, we no longer see the failure as `AkCertError::SizeTooSmall`,
        // but we still see a different error since the response is not valid.
        let properly_sized_parse = parse_response(&properly_sized_response);
        assert!(
            !properly_sized_parse
                .unwrap_err()
                .to_string()
                .starts_with("AK cert response is too small to parse"),
        );
    }

    #[test]
    fn test_valid_response_size_match() {
        const VALID_RESPONSE: [u8; 56] = [
            0x38, 0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x30, 0x82, 0x03, 0xeb, 0x30, 0x82,
            0x02, 0xd3, 0xa0, 0x03, 0x02, 0x01, 0x02, 0x02, 0x10, 0x3b, 0xa3, 0x33, 0x97, 0xef,
            0x2f, 0x9e, 0xef, 0xbd, 0x35, 0x5e, 0xda, 0xdd, 0x27, 0x38, 0x42, 0x30, 0x0d, 0x06,
            0x09, 0x2a, 0x86, 0x48, 0x86, 0xf7, 0x0d, 0x01, 0x01, 0x0b, 0x05, 0x00, 0x30, 0x25,
        ];

        const HEADER_SIZE: usize = size_of::<IgvmAttestCommonResponseHeader>();
        let result = IgvmAttestAkCertResponseHeader::read_from_prefix(&VALID_RESPONSE);
        assert!(result.is_ok());

        let result = parse_response(&VALID_RESPONSE);
        assert!(result.is_ok());

        let payload = result.unwrap();
        let data_size = parse_response_header(&VALID_RESPONSE).unwrap().data_size as usize;
        assert_eq!(payload.len(), data_size - HEADER_SIZE);
        assert_eq!(payload, &VALID_RESPONSE[HEADER_SIZE..data_size]);
    }

    #[test]
    fn test_valid_response_size_smaller_than_specified() {
        const VALID_RESPONSE: [u8; 56] = [
            0x37, 0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x30, 0x82, 0x03, 0xeb, 0x30, 0x82,
            0x02, 0xd3, 0xa0, 0x03, 0x02, 0x01, 0x02, 0x02, 0x10, 0x3b, 0xa3, 0x33, 0x97, 0xef,
            0x2f, 0x9e, 0xef, 0xbd, 0x35, 0x5e, 0xda, 0xdd, 0x27, 0x38, 0x42, 0x30, 0x0d, 0x06,
            0x09, 0x2a, 0x86, 0x48, 0x86, 0xf7, 0x0d, 0x01, 0x01, 0x0b, 0x05, 0x00, 0x30, 0x25,
        ];

        const HEADER_SIZE: usize = size_of::<IgvmAttestCommonResponseHeader>();

        let result = IgvmAttestAkCertResponseHeader::read_from_prefix(&VALID_RESPONSE);
        assert!(result.is_ok());

        let result = parse_response(&VALID_RESPONSE);
        assert!(result.is_ok());

        let payload = result.unwrap();
        let data_size = parse_response_header(&VALID_RESPONSE).unwrap().data_size as usize;
        assert_eq!(payload.len(), data_size - HEADER_SIZE);
        assert_eq!(payload, &VALID_RESPONSE[HEADER_SIZE..data_size]);
    }
}
