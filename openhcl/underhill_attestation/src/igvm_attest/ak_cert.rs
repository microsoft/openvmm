// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! The module for `AK_CERT_REQUEST` request type that supports parsing the
//! response.
use crate::igvm_attest::Error as CommonError;
use crate::igvm_attest::parse_response_header;
use crypto::x509::X509Certificate;
use openhcl_attestation_protocol::igvm_attest::get::runtime_claims::AkCertMetadata;
use thiserror::Error;
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
    #[error("invalid AK certificate")]
    Certificate(#[from] crypto::x509::X509Error),
    #[error("invalid AK certificate validity")]
    Validity(#[from] crypto::x509::X509ValidityError),
    #[error("AK certificate has no {0} common name")]
    MissingCommonName(&'static str),
}

/// Extract renewal hints, not a certificate trust decision.
///
/// TPM NV storage can contain padding or stale bytes after the DER certificate.
pub fn parse_metadata(data: &[u8]) -> Result<AkCertMetadata, AkCertMetadataError> {
    let certificate = X509Certificate::from_der(data)?;
    let (not_before, not_after) = certificate.validity()?;
    Ok(AkCertMetadata {
        issuer: certificate
            .issuer_common_name()?
            .ok_or(AkCertMetadataError::MissingCommonName("issuer"))?,
        subject: certificate
            .subject_common_name()?
            .ok_or(AkCertMetadataError::MissingCommonName("subject"))?,
        not_before: format_validity_time(not_before),
        not_after: format_validity_time(not_after),
        authority: certificate
            .authority_key_identifier()?
            .map(hex::encode_upper)
            .unwrap_or_default(),
    })
}

fn format_validity_time(time: jiff::civil::DateTime) -> String {
    let year = time.year();
    // Jiff's %y supports 1969-2068, not X.509's 1950-2049 range.
    let year = if (1950..=2049).contains(&year) {
        format!("{:02}", year % 100)
    } else {
        format!("{year:04}")
    };
    format!(
        "{year}{:02}{:02}{:02}{:02}{:02}Z",
        time.month(),
        time.day(),
        time.hour(),
        time.minute(),
        time.second(),
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
    use crypto::rsa::RsaKeyPair;
    use crypto::x509::X509SelfSignedOptions;
    use openhcl_attestation_protocol::igvm_attest::get::IgvmAttestAkCertResponseHeader;
    use openhcl_attestation_protocol::igvm_attest::get::IgvmAttestCommonResponseHeader;
    use openhcl_attestation_protocol::igvm_attest::get::IgvmAttestResponseVersion;
    use openhcl_attestation_protocol::igvm_attest::get::IgvmSignal;
    use test_igvm_agent_lib::ak_cert::certificate;
    use test_with_tracing::test;
    use zerocopy::IntoBytes;

    #[test]
    fn test_certificate_metadata() {
        let cert = certificate();
        let metadata = parse_metadata(&cert).unwrap();
        assert_eq!(metadata.issuer, "Test \"AK\" issuer");
        assert_eq!(metadata.subject, "Test \"AK\" issuer");
        assert_eq!(metadata.not_before, "261009180232Z");
        assert_eq!(metadata.not_after, "20561120180232Z");
        assert_eq!(
            metadata.authority,
            "9FF3935C2C83D84F1DB5242A0E7D3C9B1D191D3B"
        );

        let mut padded = cert.clone();
        padded.resize(4096, 0);
        assert_eq!(parse_metadata(&padded).unwrap(), metadata);
        padded[cert.len()..].fill(0xa5);
        assert_eq!(parse_metadata(&padded).unwrap(), metadata);
    }

    #[test]
    fn test_certificate_without_authority_key_identifier() {
        let key = RsaKeyPair::generate(2048).unwrap();
        let cert = X509Certificate::build_self_signed((&key).into(), &Default::default())
            .unwrap()
            .to_der()
            .unwrap();
        assert_eq!(parse_metadata(&cert).unwrap().authority, "");
    }

    #[test]
    fn test_certificate_no_expiration_metadata() {
        let key = RsaKeyPair::generate(2048).unwrap();
        let cert = X509Certificate::build_self_signed(
            (&key).into(),
            &X509SelfSignedOptions {
                common_name: "No expiration",
                not_before: "261009180232Z",
                not_after: "99991231235959Z",
                authority_key_identifier: Some(&[0x01, 0xab, 0x00, 0xff]),
                ..Default::default()
            },
        )
        .unwrap()
        .to_der()
        .unwrap();
        let metadata = parse_metadata(&cert).unwrap();
        assert_eq!(metadata.issuer, "No expiration");
        assert_eq!(metadata.subject, "No expiration");
        assert_eq!(metadata.not_before, "261009180232Z");
        assert_eq!(metadata.not_after, "99991231235959Z");
        assert_eq!(metadata.authority, "01AB00FF");
    }

    #[test]
    fn test_certificate_missing_common_name() {
        let key = RsaKeyPair::generate(2048).unwrap();
        let cert = X509Certificate::build_self_signed(
            (&key).into(),
            &X509SelfSignedOptions {
                common_name: "",
                ..Default::default()
            },
        )
        .unwrap()
        .to_der()
        .unwrap();
        assert!(matches!(
            parse_metadata(&cert),
            Err(AkCertMetadataError::MissingCommonName("issuer"))
        ));
    }

    #[test]
    fn test_metadata_invalid_storage_buffer() {
        let cert = certificate();
        assert!(parse_metadata(&[]).is_err());
        assert!(parse_metadata(&cert[..cert.len() - 1]).is_err());
        assert!(parse_metadata(&[0; 4096]).is_err());
    }

    #[test]
    fn test_metadata_time_formatting() {
        for (time, expected) in [
            ("1949-12-31T23:59:59", "19491231235959Z"),
            ("1950-01-01T00:00:00", "500101000000Z"),
            ("1970-01-01T00:00:00", "700101000000Z"),
            ("2000-02-29T00:00:00", "000229000000Z"),
            ("2049-12-31T23:59:59", "491231235959Z"),
            ("2050-01-01T00:00:00", "20500101000000Z"),
            ("2056-11-20T18:02:32", "20561120180232Z"),
            ("9999-12-30T00:00:00", "99991230000000Z"),
            ("9999-12-31T23:59:59", "99991231235959Z"),
        ] {
            assert_eq!(format_validity_time(time.parse().unwrap()), expected);
        }
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
    fn ak_responses_remain_v1_or_v2_only() {
        use crate::igvm_attest::tests::frame_response;
        use openhcl_attestation_protocol::igvm_attest::get::IgvmAttestResponseVersion;
        use openhcl_attestation_protocol::igvm_attest::get::IgvmErrorInfo;

        for version in [
            IgvmAttestResponseVersion::VERSION_1,
            IgvmAttestResponseVersion::VERSION_2,
        ] {
            // AK certificates are binary, not subject to service payload UTF-8 checks.
            let response = frame_response(version, &[0xff, 0x30], IgvmErrorInfo::default());
            assert_eq!(parse_response(&response).unwrap(), [0xff, 0x30]);
        }
        let response = frame_response(
            IgvmAttestResponseVersion::VERSION_3,
            b"{}",
            IgvmErrorInfo::default(),
        );
        assert!(matches!(
            parse_response(&response),
            Err(AkCertError::InvalidResponseVersion(3))
        ));
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
            AkCertError::ParseHeader(CommonError::ResponseSizeMismatch {
                size: HEADER_SIZE - 1,
                specified_size: 0x01010101,
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

    fn v1_response(payload: &[u8]) -> Vec<u8> {
        let data_size = (size_of::<IgvmAttestCommonResponseHeader>() + payload.len()) as u32;
        let mut response = data_size.to_le_bytes().to_vec();
        response.extend_from_slice(&IgvmAttestResponseVersion::VERSION_1.0.to_le_bytes());
        response.extend_from_slice(payload);
        response
    }

    #[test]
    fn test_valid_response_size_match() {
        let cert = certificate();
        assert_eq!(
            parse_response(&v1_response(&cert)).unwrap(),
            AkCertResponse::Certificate(cert)
        );
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
        let cert = certificate();
        let mut response = v1_response(&cert);
        // Bytes beyond the declared response size must not become payload.
        response.extend_from_slice(b"trailing bytes");
        assert_eq!(
            parse_response(&response).unwrap(),
            AkCertResponse::Certificate(cert)
        );
    }
}
