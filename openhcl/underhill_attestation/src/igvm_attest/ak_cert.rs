// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! The module for `AK_CERT_REQUEST` request type that supports parsing the
//! response.
use crate::igvm_attest::parse_response_header;
use crate::igvm_attest::Error as CommonError;
use thiserror::Error;

/// AkCertError is returned by parse_ak_cert_response() in emuplat/tpm.rs
#[derive(Debug, Error)]
pub enum AkCertError {
    #[error("AK cert response size is too small to parse")]
    SizeTooSmall,
    #[error(
        "AK cert response size {specified_size} specified in the header is larger then the actual size {size}"
    )]
    SizeMismatch { size: usize, specified_size: usize },
    #[error(
        "AK cert response header version {version} does match the expected version {expected_version}"
    )]
    HeaderVersionMismatch { version: u32, expected_version: u32 },
    #[error("error in response header")]
    ParseHeader(#[source] CommonError),
}

/// Parse a `AK_CERT_REQUEST` response and return the payload (i.e., the AK cert).
///
/// Returns `Ok(Vec<u8>)` on successfully validating the response, otherwise returns an error.
pub fn parse_response(response: &[u8]) -> Result<Vec<u8>, AkCertError> {
    use openhcl_attestation_protocol::igvm_attest::get::IGVM_ATTEST_RESPONSE_VERSION_1;
    use openhcl_attestation_protocol::igvm_attest::get::IgvmAttestAkCertResponseHeader;
    use openhcl_attestation_protocol::igvm_attest::get::IgvmAttestCommonResponseHeader;

    let header = parse_response_header(response).map_err(AkCertError::ParseHeader)?;

    // Extract payload as per header version
    let header_size = match header.version {
        IGVM_ATTEST_RESPONSE_VERSION_1 => size_of::<IgvmAttestCommonResponseHeader>(),
        _ => size_of::<IgvmAttestAkCertResponseHeader>(),
    };
    Ok(response[header_size..header.data_size as usize].to_vec())
}

#[cfg(test)]
mod tests {
    use super::*;
    use openhcl_attestation_protocol::igvm_attest::get::IgvmAttestCommonResponseHeader;
    use openhcl_attestation_protocol::igvm_attest::get::IgvmAttestAkCertResponseHeader;
    use zerocopy::FromBytes;

    #[test]
    fn test_empty_response() {
        let result = parse_response(&[]);
        assert!(result.is_err());
        assert_eq!(
            result.unwrap_err().to_string(),
            AkCertError::ParseHeader(CommonError::ResponseSizeTooSmall { response_size: 0 })
                .to_string()
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
        assert!(result.is_some());

        let result = parse_response(&VALID_RESPONSE);
        assert!(result.is_ok());

        let payload = result.unwrap();
        let data_size = parse_response_header(&VALID_RESPONSE)
            .unwrap()
            .data_size as usize;
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
        assert!(result.is_some());

        let result = parse_response(&VALID_RESPONSE);
        assert!(result.is_ok());

        let payload = result.unwrap();
        let data_size = parse_response_header(&VALID_RESPONSE)
            .unwrap()
            .data_size as usize;
        assert_eq!(payload.len(), data_size - HEADER_SIZE);
        assert_eq!(payload, &VALID_RESPONSE[HEADER_SIZE..data_size]);
    }
}
