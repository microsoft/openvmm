// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! The module for `AK_CERT_REQUEST` request type that supports parsing the
//! response.
use crate::igvm_attest::Error as CommonError;
use crate::igvm_attest::parse_response_header;

use thiserror::Error;
use zerocopy::FromBytes;

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
    #[error("invalid TVM host-certification response")]
    InvalidHostCertificationResponse,
    #[error("invalid TVM host-certification evidence")]
    InvalidHostCertificationEvidence,
}

/// Parsed AK certificate response.
#[derive(Debug, PartialEq, Eq)]
pub struct AkCertResponse {
    pub ak_cert: Vec<u8>,
    pub host_certification_evidence: Option<Vec<u8>>,
}

/// Parse an `AK_CERT_REQUEST` response.
///
/// Legacy responses contain only the AK certificate. Capability-gated TVM
/// responses contain a versioned wrapper with the AK certificate and one
/// complete host-certification evidence payload.
pub fn parse_response(response: &[u8]) -> Result<AkCertResponse, AkCertError> {
    use openhcl_attestation_protocol::igvm_attest::get::IgvmAttestAkCertResponseHeader;
    use openhcl_attestation_protocol::igvm_attest::get::IgvmAttestCommonResponseHeader;
    use openhcl_attestation_protocol::igvm_attest::get::IgvmAttestResponseVersion;
    use openhcl_attestation_protocol::igvm_attest::get::TVM_HOST_CERTIFICATION_AK_CERT_MAX_SIZE;
    use openhcl_attestation_protocol::igvm_attest::get::TVM_HOST_CERTIFICATION_BINDING_HASH_ALG_SHA256;
    use openhcl_attestation_protocol::igvm_attest::get::TVM_HOST_CERTIFICATION_BINDING_VERSION;
    use openhcl_attestation_protocol::igvm_attest::get::TVM_HOST_CERTIFICATION_EVIDENCE_FLAG_HOST_CERTIFIED;
    use openhcl_attestation_protocol::igvm_attest::get::TVM_HOST_CERTIFICATION_EVIDENCE_HEADER_SIZE;
    use openhcl_attestation_protocol::igvm_attest::get::TVM_HOST_CERTIFICATION_EVIDENCE_MAGIC;
    use openhcl_attestation_protocol::igvm_attest::get::TVM_HOST_CERTIFICATION_EVIDENCE_MAX_SIZE;
    use openhcl_attestation_protocol::igvm_attest::get::TVM_HOST_CERTIFICATION_EVIDENCE_VERSION;
    use openhcl_attestation_protocol::igvm_attest::get::TVM_HOST_CERTIFICATION_IDKS_SIGNATURE_SIZE;
    use openhcl_attestation_protocol::igvm_attest::get::TVM_HOST_CERTIFICATION_RESPONSE_HEADER_SIZE;
    use openhcl_attestation_protocol::igvm_attest::get::TVM_HOST_CERTIFICATION_RESPONSE_MAGIC;
    use openhcl_attestation_protocol::igvm_attest::get::TVM_HOST_CERTIFICATION_RESPONSE_MAX_SIZE;
    use openhcl_attestation_protocol::igvm_attest::get::TVM_HOST_CERTIFICATION_RESPONSE_VERSION;
    use openhcl_attestation_protocol::igvm_attest::get::TvmHostCertificationEvidenceHeader;
    use openhcl_attestation_protocol::igvm_attest::get::TvmHostCertificationResponseHeader;

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

    let payload = &response[header_size..data_size];
    if payload.len() < size_of::<u32>()
        || u32::from_le_bytes(
            payload[..size_of::<u32>()]
                .try_into()
                .expect("fixed-size slice"),
        ) != TVM_HOST_CERTIFICATION_RESPONSE_MAGIC
    {
        return Ok(AkCertResponse {
            ak_cert: payload.to_vec(),
            host_certification_evidence: None,
        });
    }

    if payload.len() < TVM_HOST_CERTIFICATION_RESPONSE_HEADER_SIZE
        || payload.len() > TVM_HOST_CERTIFICATION_RESPONSE_MAX_SIZE
    {
        return Err(AkCertError::InvalidHostCertificationResponse);
    }

    let (response_header, _) = TvmHostCertificationResponseHeader::read_from_prefix(payload)
        .map_err(|_| AkCertError::InvalidHostCertificationResponse)?;
    let response_total_size = usize::try_from(response_header.total_size)
        .map_err(|_| AkCertError::InvalidHostCertificationResponse)?;
    let ak_cert_size = usize::try_from(response_header.ak_cert_size)
        .map_err(|_| AkCertError::InvalidHostCertificationResponse)?;
    let evidence_size = usize::try_from(response_header.evidence_size)
        .map_err(|_| AkCertError::InvalidHostCertificationResponse)?;
    let expected_response_size = TVM_HOST_CERTIFICATION_RESPONSE_HEADER_SIZE
        .checked_add(ak_cert_size)
        .and_then(|size| size.checked_add(evidence_size))
        .ok_or(AkCertError::InvalidHostCertificationResponse)?;
    if response_header.magic != TVM_HOST_CERTIFICATION_RESPONSE_MAGIC
        || response_header.version != TVM_HOST_CERTIFICATION_RESPONSE_VERSION
        || response_header.header_size as usize != TVM_HOST_CERTIFICATION_RESPONSE_HEADER_SIZE
        || response_total_size != payload.len()
        || expected_response_size != payload.len()
        || ak_cert_size == 0
        || ak_cert_size > TVM_HOST_CERTIFICATION_AK_CERT_MAX_SIZE
        || evidence_size < TVM_HOST_CERTIFICATION_EVIDENCE_HEADER_SIZE
        || evidence_size > TVM_HOST_CERTIFICATION_EVIDENCE_MAX_SIZE
        || response_header.reserved != [0; 2]
    {
        return Err(AkCertError::InvalidHostCertificationResponse);
    }

    let ak_cert_offset = TVM_HOST_CERTIFICATION_RESPONSE_HEADER_SIZE;
    let evidence_offset = ak_cert_offset + ak_cert_size;
    let evidence = &payload[evidence_offset..];
    let (evidence_header, _) = TvmHostCertificationEvidenceHeader::read_from_prefix(evidence)
        .map_err(|_| AkCertError::InvalidHostCertificationEvidence)?;
    let evidence_total_size = usize::try_from(evidence_header.total_size)
        .map_err(|_| AkCertError::InvalidHostCertificationEvidence)?;
    let report_size = usize::try_from(evidence_header.report_size)
        .map_err(|_| AkCertError::InvalidHostCertificationEvidence)?;
    let report_signature_size = usize::try_from(evidence_header.report_signature_size)
        .map_err(|_| AkCertError::InvalidHostCertificationEvidence)?;
    let runtime_data_size = usize::try_from(evidence_header.runtime_data_size)
        .map_err(|_| AkCertError::InvalidHostCertificationEvidence)?;
    let expected_evidence_size = TVM_HOST_CERTIFICATION_EVIDENCE_HEADER_SIZE
        .checked_add(report_size)
        .and_then(|size| size.checked_add(report_signature_size))
        .and_then(|size| size.checked_add(runtime_data_size))
        .ok_or(AkCertError::InvalidHostCertificationEvidence)?;
    if evidence_header.magic != TVM_HOST_CERTIFICATION_EVIDENCE_MAGIC
        || evidence_header.version != TVM_HOST_CERTIFICATION_EVIDENCE_VERSION
        || evidence_header.header_size as usize != TVM_HOST_CERTIFICATION_EVIDENCE_HEADER_SIZE
        || evidence_total_size != evidence.len()
        || expected_evidence_size != evidence.len()
        || evidence_header.flags != TVM_HOST_CERTIFICATION_EVIDENCE_FLAG_HOST_CERTIFIED
        || evidence_header.binding_version != TVM_HOST_CERTIFICATION_BINDING_VERSION
        || evidence_header.binding_hash_algorithm != TVM_HOST_CERTIFICATION_BINDING_HASH_ALG_SHA256
        || report_size == 0
        || report_signature_size != TVM_HOST_CERTIFICATION_IDKS_SIGNATURE_SIZE
        || runtime_data_size == 0
        || evidence_header.reserved != [0; 2]
    {
        return Err(AkCertError::InvalidHostCertificationEvidence);
    }

    Ok(AkCertResponse {
        ak_cert: payload[ak_cert_offset..evidence_offset].to_vec(),
        host_certification_evidence: Some(evidence.to_vec()),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use openhcl_attestation_protocol::igvm_attest::get::IgvmAttestAkCertResponseHeader;
    use openhcl_attestation_protocol::igvm_attest::get::IgvmAttestCommonResponseHeader;
    use openhcl_attestation_protocol::igvm_attest::get::IgvmAttestResponseVersion;
    use openhcl_attestation_protocol::igvm_attest::get::TVM_HOST_CERTIFICATION_AK_CERT_MAX_SIZE;
    use openhcl_attestation_protocol::igvm_attest::get::TVM_HOST_CERTIFICATION_BINDING_HASH_ALG_SHA256;
    use openhcl_attestation_protocol::igvm_attest::get::TVM_HOST_CERTIFICATION_BINDING_VERSION;
    use openhcl_attestation_protocol::igvm_attest::get::TVM_HOST_CERTIFICATION_EVIDENCE_FLAG_HOST_CERTIFIED;
    use openhcl_attestation_protocol::igvm_attest::get::TVM_HOST_CERTIFICATION_EVIDENCE_HEADER_SIZE;
    use openhcl_attestation_protocol::igvm_attest::get::TVM_HOST_CERTIFICATION_EVIDENCE_MAGIC;
    use openhcl_attestation_protocol::igvm_attest::get::TVM_HOST_CERTIFICATION_EVIDENCE_MAX_SIZE;
    use openhcl_attestation_protocol::igvm_attest::get::TVM_HOST_CERTIFICATION_EVIDENCE_VERSION;
    use openhcl_attestation_protocol::igvm_attest::get::TVM_HOST_CERTIFICATION_IDKS_SIGNATURE_SIZE;
    use openhcl_attestation_protocol::igvm_attest::get::TVM_HOST_CERTIFICATION_RESPONSE_HEADER_SIZE;
    use openhcl_attestation_protocol::igvm_attest::get::TVM_HOST_CERTIFICATION_RESPONSE_MAGIC;
    use openhcl_attestation_protocol::igvm_attest::get::TVM_HOST_CERTIFICATION_RESPONSE_VERSION;
    use openhcl_attestation_protocol::igvm_attest::get::TvmHostCertificationEvidenceHeader;
    use openhcl_attestation_protocol::igvm_attest::get::TvmHostCertificationResponseHeader;
    use zerocopy::FromBytes;
    use zerocopy::IntoBytes;

    fn host_certification_response(
        response_version: IgvmAttestResponseVersion,
        ak_cert: &[u8],
        report: &[u8],
        runtime_data: &[u8],
    ) -> Vec<u8> {
        let signature = vec![0xa5; TVM_HOST_CERTIFICATION_IDKS_SIGNATURE_SIZE];
        let evidence_size = TVM_HOST_CERTIFICATION_EVIDENCE_HEADER_SIZE
            + report.len()
            + signature.len()
            + runtime_data.len();
        let evidence_header = TvmHostCertificationEvidenceHeader {
            magic: TVM_HOST_CERTIFICATION_EVIDENCE_MAGIC,
            version: TVM_HOST_CERTIFICATION_EVIDENCE_VERSION,
            header_size: TVM_HOST_CERTIFICATION_EVIDENCE_HEADER_SIZE as u32,
            total_size: evidence_size as u32,
            flags: TVM_HOST_CERTIFICATION_EVIDENCE_FLAG_HOST_CERTIFIED,
            binding_version: TVM_HOST_CERTIFICATION_BINDING_VERSION,
            binding_hash_algorithm: TVM_HOST_CERTIFICATION_BINDING_HASH_ALG_SHA256,
            report_size: report.len() as u32,
            report_signature_size: signature.len() as u32,
            runtime_data_size: runtime_data.len() as u32,
            reserved: [0; 2],
        };
        let mut evidence = Vec::with_capacity(evidence_size);
        evidence.extend_from_slice(evidence_header.as_bytes());
        evidence.extend_from_slice(report);
        evidence.extend_from_slice(&signature);
        evidence.extend_from_slice(runtime_data);

        let payload_size =
            TVM_HOST_CERTIFICATION_RESPONSE_HEADER_SIZE + ak_cert.len() + evidence.len();
        let response_header = TvmHostCertificationResponseHeader {
            magic: TVM_HOST_CERTIFICATION_RESPONSE_MAGIC,
            version: TVM_HOST_CERTIFICATION_RESPONSE_VERSION,
            header_size: TVM_HOST_CERTIFICATION_RESPONSE_HEADER_SIZE as u32,
            total_size: payload_size as u32,
            ak_cert_size: ak_cert.len() as u32,
            evidence_size: evidence.len() as u32,
            reserved: [0; 2],
        };
        let outer_header_size = match response_version {
            IgvmAttestResponseVersion::VERSION_1 => size_of::<IgvmAttestCommonResponseHeader>(),
            IgvmAttestResponseVersion::VERSION_2 => size_of::<IgvmAttestAkCertResponseHeader>(),
            _ => unreachable!("unsupported test response version"),
        };
        let data_size = (outer_header_size + payload_size) as u32;

        let mut response = Vec::with_capacity(data_size as usize);
        match response_version {
            IgvmAttestResponseVersion::VERSION_1 => {
                response.extend_from_slice(
                    IgvmAttestCommonResponseHeader {
                        data_size,
                        version: response_version,
                    }
                    .as_bytes(),
                );
            }
            IgvmAttestResponseVersion::VERSION_2 => {
                response.extend_from_slice(
                    IgvmAttestAkCertResponseHeader {
                        data_size,
                        version: response_version,
                        error_info: Default::default(),
                    }
                    .as_bytes(),
                );
            }
            _ => unreachable!("unsupported test response version"),
        }
        response.extend_from_slice(response_header.as_bytes());
        response.extend_from_slice(ak_cert);
        response.extend_from_slice(&evidence);
        response
    }

    fn standard_host_certification_response(
        response_version: IgvmAttestResponseVersion,
    ) -> Vec<u8> {
        host_certification_response(
            response_version,
            &[0x30, 0x82, 0x01, 0x00],
            &[1, 2, 3, 4],
            br#"{"keys":[]}"#,
        )
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

        let payload = result.unwrap();
        let data_size = parse_response_header(&VALID_RESPONSE).unwrap().data_size as usize;
        assert_eq!(payload.ak_cert.len(), data_size - HEADER_SIZE);
        assert_eq!(payload.ak_cert, &VALID_RESPONSE[HEADER_SIZE..data_size]);
        assert!(payload.host_certification_evidence.is_none());
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

        let payload = result.unwrap();
        let data_size = parse_response_header(&VALID_RESPONSE).unwrap().data_size as usize;
        assert_eq!(payload.ak_cert.len(), data_size - HEADER_SIZE);
        assert_eq!(payload.ak_cert, &VALID_RESPONSE[HEADER_SIZE..data_size]);
        assert!(payload.host_certification_evidence.is_none());
    }

    #[test]
    fn test_valid_host_certification_response() {
        for response_version in [
            IgvmAttestResponseVersion::VERSION_1,
            IgvmAttestResponseVersion::VERSION_2,
        ] {
            let response = standard_host_certification_response(response_version);
            let parsed = parse_response(&response).expect("valid response");
            assert_eq!(parsed.ak_cert, [0x30, 0x82, 0x01, 0x00]);
            let evidence = parsed
                .host_certification_evidence
                .expect("host-certification evidence");
            assert!(evidence.len() > TVM_HOST_CERTIFICATION_EVIDENCE_HEADER_SIZE);
        }
    }

    #[test]
    fn test_host_certification_response_rejects_invalid_wrapper() {
        let mutations: [fn(&mut TvmHostCertificationResponseHeader); 6] = [
            |header| header.version += 1,
            |header| header.header_size -= 1,
            |header| header.total_size -= 1,
            |header| header.ak_cert_size = 0,
            |header| header.evidence_size = 0,
            |header| header.reserved[0] = 1,
        ];
        for mutate in mutations {
            let mut response =
                standard_host_certification_response(IgvmAttestResponseVersion::VERSION_2);
            let offset = size_of::<IgvmAttestAkCertResponseHeader>();
            let (header, _) =
                TvmHostCertificationResponseHeader::mut_from_prefix(&mut response[offset..])
                    .expect("response header");
            mutate(header);
            assert!(matches!(
                parse_response(&response),
                Err(AkCertError::InvalidHostCertificationResponse)
            ));
        }
    }

    #[test]
    fn test_host_certification_response_rejects_invalid_evidence() {
        let mutations: [fn(&mut TvmHostCertificationEvidenceHeader); 12] = [
            |header| header.magic ^= 1,
            |header| header.version += 1,
            |header| header.header_size -= 1,
            |header| header.total_size -= 1,
            |header| header.flags = 0,
            |header| header.binding_version += 1,
            |header| header.binding_hash_algorithm += 1,
            |header| header.report_size = 0,
            |header| header.report_signature_size -= 1,
            |header| header.runtime_data_size = 0,
            |header| header.reserved[0] = 1,
            |header| header.reserved[1] = 1,
        ];
        for mutate in mutations {
            let mut response =
                standard_host_certification_response(IgvmAttestResponseVersion::VERSION_2);
            let response_offset = size_of::<IgvmAttestAkCertResponseHeader>();
            let (response_header, _) =
                TvmHostCertificationResponseHeader::read_from_prefix(&response[response_offset..])
                    .expect("response header");
            let evidence_offset = response_offset
                + TVM_HOST_CERTIFICATION_RESPONSE_HEADER_SIZE
                + response_header.ak_cert_size as usize;
            let (evidence_header, _) = TvmHostCertificationEvidenceHeader::mut_from_prefix(
                &mut response[evidence_offset..],
            )
            .expect("evidence header");
            mutate(evidence_header);
            assert!(matches!(
                parse_response(&response),
                Err(AkCertError::InvalidHostCertificationEvidence)
            ));
        }
    }

    #[test]
    fn test_host_certification_response_size_boundaries() {
        let ak_cert = vec![0x30; TVM_HOST_CERTIFICATION_AK_CERT_MAX_SIZE];
        let report = [1];
        let runtime_data = vec![
            2;
            TVM_HOST_CERTIFICATION_EVIDENCE_MAX_SIZE
                - TVM_HOST_CERTIFICATION_EVIDENCE_HEADER_SIZE
                - TVM_HOST_CERTIFICATION_IDKS_SIGNATURE_SIZE
                - report.len()
        ];
        let response = host_certification_response(
            IgvmAttestResponseVersion::VERSION_2,
            &ak_cert,
            &report,
            &runtime_data,
        );
        let parsed = parse_response(&response).expect("maximum-sized response");
        assert_eq!(
            parsed.ak_cert.len(),
            TVM_HOST_CERTIFICATION_AK_CERT_MAX_SIZE
        );
        assert_eq!(
            parsed
                .host_certification_evidence
                .expect("host-certification evidence")
                .len(),
            TVM_HOST_CERTIFICATION_EVIDENCE_MAX_SIZE
        );

        let oversized_response = host_certification_response(
            IgvmAttestResponseVersion::VERSION_2,
            &ak_cert,
            &report,
            &[runtime_data, vec![3]].concat(),
        );
        assert!(matches!(
            parse_response(&oversized_response),
            Err(AkCertError::InvalidHostCertificationResponse)
        ));
    }
}
