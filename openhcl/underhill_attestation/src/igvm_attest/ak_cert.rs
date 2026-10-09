// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! The module for `AK_CERT_REQUEST` request type that supports parsing the
//! response.
use crate::igvm_attest::Error as CommonError;
use crate::igvm_attest::parse_response_header;
use der::Decode;
use openhcl_attestation_protocol::igvm_attest::get::runtime_claims::AkCertMetadata;
use thiserror::Error;
use x509_cert::Certificate;
use x509_cert::ext::pkix::AuthorityKeyIdentifier;
use x509_cert::time::Time;
use zerocopy::FromBytes;

/// The certificate or renewal decision returned by the host.
#[derive(Debug, PartialEq, Eq)]
pub enum AkCertResponse {
    /// Certificate bytes, possibly empty if the service is unavailable.
    Certificate(Vec<u8>),
    /// Keep the existing certificate.
    NoRenewal,
}

/// An error extracting renewal metadata from a certificate.
#[derive(Debug, Error)]
pub enum AkCertMetadataError {
    #[error("invalid AK certificate metadata")]
    Decode(#[from] der::Error),
    #[error("AK certificate has no {0} common name")]
    MissingCommonName(&'static str),
}

/// Extract renewal hints, not a certificate trust decision.
///
/// TPM NV storage can contain padding or stale bytes after the DER certificate.
pub fn parse_metadata(data: &[u8]) -> Result<AkCertMetadata, AkCertMetadataError> {
    let (cert, _) = Certificate::from_der_partial(data)?;
    let tbs = cert.tbs_certificate();
    let issuer = tbs
        .issuer()
        .common_name()?
        .ok_or(AkCertMetadataError::MissingCommonName("issuer"))?
        .value()
        .into_owned();
    let subject = tbs
        .subject()
        .common_name()?
        .ok_or(AkCertMetadataError::MissingCommonName("subject"))?
        .value()
        .into_owned();
    let authority = tbs
        .get_extension::<AuthorityKeyIdentifier>()?
        .and_then(|(_, aki)| aki.key_identifier)
        .map(|key| hex::encode_upper(key.as_bytes()))
        .unwrap_or_default();
    Ok(AkCertMetadata {
        issuer,
        subject,
        not_before: time_content(tbs.validity().not_before),
        not_after: time_content(tbs.validity().not_after),
        authority,
    })
}

fn time_content(time: Time) -> String {
    let date = time.to_date_time();
    let year = match time {
        Time::UtcTime(_) => format!("{:02}", date.year() % 100),
        Time::GeneralTime(_) => format!("{:04}", date.year()),
    };
    format!(
        "{year}{:02}{:02}{:02}{:02}{:02}Z",
        date.month(),
        date.day(),
        date.hour(),
        date.minutes(),
        date.seconds()
    )
}

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

/// Parse an `AK_CERT_REQUEST` response, including V2 renewal decisions.
pub fn parse_response(response: &[u8]) -> Result<AkCertResponse, AkCertError> {
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
    let data_size = header.data_size as usize;

    if data_size < header_size {
        return Err(AkCertError::SizeTooSmall {
            size: data_size,
            minimum_size: header_size,
        });
    }

    if header.version == IgvmAttestResponseVersion::VERSION_2 {
        let (header, _) = IgvmAttestAkCertResponseHeader::read_from_prefix(&response[..data_size])
            .map_err(|_| AkCertError::SizeTooSmall {
                size: data_size,
                minimum_size: header_size,
            })?;
        if header.error_info.igvm_signal.no_renew_cert() {
            return Ok(AkCertResponse::NoRenewal);
        }
    }

    Ok(AkCertResponse::Certificate(
        response[header_size..data_size].to_vec(),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use openhcl_attestation_protocol::igvm_attest::get::IgvmAttestAkCertResponseHeader;
    use openhcl_attestation_protocol::igvm_attest::get::IgvmAttestCommonResponseHeader;
    use openhcl_attestation_protocol::igvm_attest::get::IgvmAttestResponseVersion;
    use openhcl_attestation_protocol::igvm_attest::get::IgvmSignal;
    use test_with_tracing::test;
    use zerocopy::FromBytes;
    use zerocopy::IntoBytes;

    // Self-signed synthetic certificate. The private key was discarded.
    // Includes a quoted CN, UTCTime notBefore, GeneralizedTime notAfter, and AKI.
    const TEST_CERT: &[u8] = include_bytes!("../../test_data/test_ak_cert.der");

    #[test]
    fn test_certificate_metadata() {
        let metadata = parse_metadata(TEST_CERT).unwrap();
        assert_eq!(metadata.issuer, "Test \"AK\" issuer");
        assert_eq!(metadata.subject, "Test \"AK\" issuer");
        assert_eq!(metadata.not_before, "261009180232Z");
        assert_eq!(metadata.not_after, "20561120180232Z");
        assert_eq!(
            metadata.authority,
            "9FF3935C2C83D84F1DB5242A0E7D3C9B1D191D3B"
        );

        let mut padded = TEST_CERT.to_vec();
        padded.resize(4096, 0);
        assert_eq!(parse_metadata(&padded).unwrap(), metadata);
        padded[TEST_CERT.len()..].fill(0xa5);
        assert_eq!(parse_metadata(&padded).unwrap(), metadata);
    }

    #[test]
    fn test_certificate_without_authority_key_identifier() {
        // Changing the extension OID leaves valid DER, but no AKI extension.
        let mut cert = TEST_CERT.to_vec();
        let offset = cert
            .windows(3)
            .position(|w| w == [0x55, 0x1d, 0x23])
            .unwrap();
        cert[offset + 2] = 0x24;
        assert_eq!(parse_metadata(&cert).unwrap().authority, "");
    }

    #[test]
    fn test_certificate_missing_common_name() {
        for name_index in [0, 1] {
            let mut cert = TEST_CERT.to_vec();
            let offset = cert
                .windows(3)
                .enumerate()
                .filter(|(_, w)| *w == [0x55, 0x04, 0x03])
                .nth(name_index)
                .unwrap()
                .0;
            cert[offset + 2] = 0x0a;
            assert!(matches!(
                parse_metadata(&cert),
                Err(AkCertMetadataError::MissingCommonName(_))
            ));
        }
    }

    #[test]
    fn test_invalid_certificate_metadata() {
        for size in 0..TEST_CERT.len() {
            assert!(parse_metadata(&TEST_CERT[..size]).is_err());
        }
        assert!(parse_metadata(&[0; 4096]).is_err());
        let mut cert = TEST_CERT.to_vec();
        let offset = cert
            .windows(4)
            .position(|w| w == [0x30, 0x16, 0x80, 0x14])
            .unwrap();
        cert[offset + 3] = 0x15;
        assert!(parse_metadata(&cert).is_err());
    }

    fn v2_response(signal: IgvmSignal, payload: &[u8]) -> Vec<u8> {
        let header = IgvmAttestAkCertResponseHeader {
            data_size: (size_of::<IgvmAttestAkCertResponseHeader>() + payload.len()) as u32,
            version: IgvmAttestResponseVersion::VERSION_2,
            error_info: openhcl_attestation_protocol::igvm_attest::get::IgvmErrorInfo {
                igvm_signal: signal,
                ..Default::default()
            },
        };
        let mut response = header.as_bytes().to_vec();
        response.extend_from_slice(payload);
        response
    }

    #[test]
    fn test_v2_certificate_and_no_renewal() {
        for payload in [b"".as_slice(), b"certificate"] {
            assert_eq!(
                parse_response(&v2_response(IgvmSignal::new(), payload)).unwrap(),
                AkCertResponse::Certificate(payload.to_vec())
            );
            // The signal takes precedence over any payload, as in the legacy agent ABI.
            let signal = IgvmSignal::new().with_no_renew_cert(true);
            assert_eq!(signal.as_bytes(), &32u32.to_le_bytes());
            assert_eq!(
                parse_response(&v2_response(signal, payload)).unwrap(),
                AkCertResponse::NoRenewal
            );
        }
        for bit in [0, 1, 2, 3, 4, 6, 31] {
            let signal = IgvmSignal::from(1u32 << bit);
            assert_eq!(
                parse_response(&v2_response(signal, b"cert")).unwrap(),
                AkCertResponse::Certificate(b"cert".to_vec())
            );
        }
    }

    #[test]
    fn test_no_renewal_does_not_override_errors_or_bad_sizes() {
        let response = v2_response(IgvmSignal::new().with_no_renew_cert(true), &[]);
        for size in 0..response.len() {
            assert!(parse_response(&response[..size]).is_err());
            // Bytes present outside the declared size must not supply the signal.
            let mut invalid = response.clone();
            invalid[..4].copy_from_slice(&(size as u32).to_le_bytes());
            assert!(parse_response(&invalid).is_err());
        }
        let mut invalid = response.clone();
        invalid[..4].copy_from_slice(&((response.len() + 1) as u32).to_le_bytes());
        assert!(parse_response(&invalid).is_err());

        let mut error = response.clone();
        error[8..12].copy_from_slice(&1u32.to_le_bytes());
        assert!(matches!(
            parse_response(&error),
            Err(AkCertError::ParseHeader(CommonError::Attestation { .. }))
        ));
        for version in [0u32, 3, u32::MAX] {
            let mut invalid = response.clone();
            invalid[4..8].copy_from_slice(&version.to_le_bytes());
            assert!(parse_response(&invalid).is_err());
        }
    }

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

        let AkCertResponse::Certificate(payload) = result.unwrap() else {
            panic!("expected a certificate");
        };
        let data_size = parse_response_header(&VALID_RESPONSE).unwrap().data_size as usize;
        assert_eq!(payload.len(), data_size - HEADER_SIZE);
        assert_eq!(payload, &VALID_RESPONSE[HEADER_SIZE..data_size]);
    }

    #[test]
    fn test_parse_response_small_size() {
        let mut response = [0u8; 8];
        // data_size = 4 (little-endian u32)
        response[0..4].copy_from_slice(&4u32.to_le_bytes());
        // version = VERSION_1 = 1 (little-endian u32)
        response[4..8].copy_from_slice(&1u32.to_le_bytes());

        assert!(parse_response(&response).is_err());
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

        let AkCertResponse::Certificate(payload) = result.unwrap() else {
            panic!("expected a certificate");
        };
        let data_size = parse_response_header(&VALID_RESPONSE).unwrap().data_size as usize;
        assert_eq!(payload.len(), data_size - HEADER_SIZE);
        assert_eq!(payload, &VALID_RESPONSE[HEADER_SIZE..data_size]);
    }
}
