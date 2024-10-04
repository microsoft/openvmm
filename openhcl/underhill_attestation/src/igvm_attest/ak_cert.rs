// Copyright (C) Microsoft Corporation. All rights reserved.

//! The module for `AK_CERT_REQUEST` request type that supports parsing the
//! response.

use crate::igvm_attest::Error;
use crate::protocol::igvm_attest::get::IgvmAttestAkCertResponseHeader;
use crate::protocol::igvm_attest::get::AK_CERT_RESPONSE_HEADER_VERSION;
use zerocopy::FromBytes;

/// Parse a `AK_CERT_REQUEST` response and return the payload (i.e., the AK cert).
///
/// Returns `Ok(Vec<u8>)` on successfully validating the response, otherwise returns an error.
pub fn parse_response(response: &[u8]) -> Result<Vec<u8>, Error> {
    const HEADER_SIZE: usize = size_of::<IgvmAttestAkCertResponseHeader>();

    let Some(header) = IgvmAttestAkCertResponseHeader::read_from_prefix(response) else {
        Err(Error::AkCertResponseSizeTooSmall)?
    };

    let size = header.data_size as usize;
    if size > response.len() {
        Err(Error::AkCertResponseSizeMismatch {
            size: response.len(),
            specified_size: size,
        })?
    }

    if header.version != AK_CERT_RESPONSE_HEADER_VERSION {
        Err(Error::AkCertResponseHeaderVersionMismatch {
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
    fn test_empty_response() {
        let result = parse_response(&[]);
        assert!(result.is_err());
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
        assert!(result.is_some());
        let header = result.unwrap();

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
        assert!(result.is_some());
        let header = result.unwrap();

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
