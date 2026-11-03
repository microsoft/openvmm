// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! The module for `AK_CERT_REQUEST` request type that supports parsing the
//! response.
use crate::igvm_attest::Error as CommonError;
use crate::igvm_attest::parse_response_header;
use der::Decode;
use der::Reader;
use der::SliceReader;
use der::Tag;
use der::TagNumber;
use der::Tagged;
use der::asn1::AnyRef;
use der::asn1::BitStringRef;
use der::asn1::BmpString;
use der::asn1::GeneralizedTime;
use der::asn1::IntRef;
use der::asn1::ObjectIdentifier;
use der::asn1::OctetStringRef;
use der::asn1::PrintableStringRef;
use der::asn1::TeletexStringRef;
use der::asn1::UtcTime;
use der::asn1::Utf8StringRef;
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
    #[error("invalid AK certificate metadata")]
    Decode(#[from] der::Error),
    #[error("AK certificate has no {0} common name")]
    MissingCommonName(&'static str),
}

/// Extract renewal hints, not a certificate trust decision.
///
/// TPM NV storage can contain padding or stale bytes after the DER certificate.
/// Only metadata and its enclosing structure are decoded. Algorithm parameters,
/// public key/signature contents, non-CN attributes, non-AKI extension values,
/// and individual AKI GeneralName contents are opaque, bounded DER values.
/// Do not replace this with the full X.509 decoder: its owned certificate and
/// GeneralNames machinery significantly increases the OpenHCL binary size.
pub fn parse_metadata(data: &[u8]) -> Result<AkCertMetadata, AkCertMetadataError> {
    // Deliberately do not finish this reader: only bytes outside the certificate
    // may be ignored. Every nested reader below must be consumed completely.
    SliceReader::new(data)?.sequence(|certificate| {
        let metadata = certificate.sequence(|tbs| {
            let version = match optional_field(tbs, TagNumber(0).context_specific(true))? {
                Some(field) => {
                    let version = u8::from_der(field.value())?;
                    // v1 is DEFAULT and must be omitted in DER.
                    if !(1..=2).contains(&version) {
                        return Err(der::Error::from(Tag::Integer.value_error()).into());
                    }
                    version
                }
                None => 0,
            };
            check_serial(tbs.decode()?)?;
            skip_algorithm(tbs)?;
            let issuer = common_name(tbs, "issuer")?;
            let (not_before, not_after) = tbs.sequence(|validity| {
                Ok::<_, der::Error>((
                    time_content(validity.decode()?)?,
                    time_content(validity.decode()?)?,
                ))
            })?;
            let subject = common_name(tbs, "subject")?;
            // SPKI: validate the envelope without decoding the key itself.
            tbs.sequence(|spki| {
                skip_algorithm(spki)?;
                spki.decode::<AnyRef<'_>>()?
                    .decode_as::<BitStringRef<'_>>()?;
                Ok::<_, der::Error>(())
            })?;
            for number in [1, 2] {
                if let Some(uid) = optional_field(tbs, TagNumber(number).context_specific(false))? {
                    if version == 0 {
                        return Err(der::Error::from(uid.tag().value_error()).into());
                    }
                    AnyRef::new(Tag::BitString, uid.value())?.decode_as::<BitStringRef<'_>>()?;
                }
            }
            let authority = match optional_field(tbs, TagNumber(3).context_specific(true))? {
                Some(field) => {
                    if version != 2 {
                        return Err(der::Error::from(field.tag().value_error()).into());
                    }
                    authority_key_identifier(field.value())?
                }
                None => None,
            };
            Ok::<_, AkCertMetadataError>(AkCertMetadata {
                issuer,
                subject,
                not_before,
                not_after,
                authority: authority.map(hex::encode_upper).unwrap_or_default(),
            })
        })?;
        skip_algorithm(certificate)?;
        certificate
            .decode::<AnyRef<'_>>()?
            .decode_as::<BitStringRef<'_>>()?;
        Ok(metadata)
    })
}

/// Unlike the general context-specific decoder, never skip unknown fields.
/// Reading optional fields in schema order and finishing the enclosing reader
/// rejects duplicates, out-of-order fields, and incorrect constructed bits.
fn optional_field<'a>(reader: &mut SliceReader<'a>, tag: Tag) -> der::Result<Option<AnyRef<'a>>> {
    if !reader.is_finished() && Tag::peek(reader)? == tag {
        reader.decode().map(Some)
    } else {
        Ok(None)
    }
}

fn check_serial(serial: AnyRef<'_>) -> der::Result<()> {
    // Match the previous X.509 profile's signed INTEGER and 21-byte limit.
    if serial.decode_as::<IntRef<'_>>()?.as_bytes().len() > 21 {
        return Err(Tag::Integer.value_error().into());
    }
    Ok(())
}

fn skip_algorithm(reader: &mut SliceReader<'_>) -> der::Result<()> {
    reader.sequence(|algorithm| {
        algorithm.decode::<ObjectIdentifier>()?;
        if !algorithm.is_finished() {
            algorithm.decode::<AnyRef<'_>>()?;
        }
        Ok(())
    })
}

fn common_name(
    reader: &mut SliceReader<'_>,
    name: &'static str,
) -> Result<String, AkCertMetadataError> {
    const COMMON_NAME: ObjectIdentifier = ObjectIdentifier::new_unwrap("2.5.4.3");
    let mut cn = None;
    reader.sequence(|rdns| {
        while !rdns.is_finished() {
            let set = rdns.decode::<AnyRef<'_>>()?;
            set.tag().assert_eq(Tag::Set)?;
            let mut attributes = SliceReader::new(set.value())?;
            if attributes.is_finished() {
                return Err(Tag::Set.value_error().into());
            }
            let mut previous = None;
            while !attributes.is_finished() {
                let encoded = attributes.tlv_bytes()?;
                if previous.is_some_and(|previous| previous > encoded) {
                    return Err(Tag::Set.non_canonical_error().into());
                }
                previous = Some(encoded);
                AnyRef::from_der(encoded)?.sequence(|attribute| {
                    let oid = attribute.decode::<ObjectIdentifier>()?;
                    let value = attribute.decode::<AnyRef<'_>>()?;
                    // Preserve Name::common_name's first-CN behavior. All other
                    // attributes are borrowed, not allocated or interpreted.
                    if oid == COMMON_NAME && cn.is_none() {
                        cn = Some(value);
                    }
                    Ok::<_, der::Error>(())
                })?;
            }
            attributes.finish()?;
        }
        Ok::<_, der::Error>(())
    })?;
    Ok(directory_string(
        cn.ok_or(AkCertMetadataError::MissingCommonName(name))?,
    )?)
}

fn directory_string(value: AnyRef<'_>) -> der::Result<String> {
    // Match the previous DirectoryString's supported encodings, including der's
    // ASCII-only Teletex support. Only the selected CN is decoded/allocated.
    match value.tag() {
        Tag::Utf8String => Ok(value.decode_as::<Utf8StringRef<'_>>()?.as_str().to_owned()),
        Tag::PrintableString => Ok(value
            .decode_as::<PrintableStringRef<'_>>()?
            .as_str()
            .to_owned()),
        Tag::TeletexString => Ok(value
            .decode_as::<TeletexStringRef<'_>>()?
            .as_str()
            .to_owned()),
        Tag::BmpString => Ok(value.decode_as::<BmpString>()?.to_string()),
        tag => Err(tag.value_error().into()),
    }
}

fn authority_key_identifier(extensions: &[u8]) -> der::Result<Option<&[u8]>> {
    const AUTHORITY_KEY_IDENTIFIER: ObjectIdentifier = ObjectIdentifier::new_unwrap("2.5.29.35");
    let mut seen = false;
    let mut key = None;
    AnyRef::from_der(extensions)?.sequence(|extensions| {
        if extensions.is_finished() {
            return Err(Tag::Sequence.value_error().into());
        }
        while !extensions.is_finished() {
            extensions.sequence(|extension| {
                let oid = extension.decode::<ObjectIdentifier>()?;
                if let Some(critical) = optional_field(extension, Tag::Boolean)? {
                    // FALSE is the default and must be omitted in DER.
                    if !critical.decode_as::<bool>()? {
                        return Err(Tag::Boolean.non_canonical_error().into());
                    }
                }
                let value = extension.decode::<&OctetStringRef>()?;
                if oid == AUTHORITY_KEY_IDENTIFIER {
                    if seen {
                        return Err(Tag::Sequence.value_error().into());
                    }
                    seen = true;
                    key = decode_authority_key_identifier(value.as_bytes())?;
                }
                Ok::<_, der::Error>(())
            })?;
        }
        Ok(key)
    })
}

fn decode_authority_key_identifier(data: &[u8]) -> der::Result<Option<&[u8]>> {
    AnyRef::from_der(data)?.sequence(|aki| {
        let key = optional_field(aki, TagNumber(0).context_specific(false))?;
        if let Some(issuer) = optional_field(aki, TagNumber(1).context_specific(true))? {
            // Check the GeneralNames envelope, but deliberately do not instantiate
            // the general X.509 name decoder (e.g. directoryName/otherName).
            let mut names = SliceReader::new(issuer.value())?;
            if names.is_finished() {
                return Err(issuer.tag().value_error().into());
            }
            while !names.is_finished() {
                let name = names.decode::<AnyRef<'_>>()?;
                match name.tag() {
                    Tag::ContextSpecific {
                        number: TagNumber(0 | 3 | 4 | 5),
                        constructed: true,
                    }
                    | Tag::ContextSpecific {
                        number: TagNumber(1 | 2 | 6 | 7 | 8),
                        constructed: false,
                    } => {}
                    tag => return Err(tag.value_error().into()),
                }
            }
            names.finish()?;
        }
        if let Some(serial) = optional_field(aki, TagNumber(2).context_specific(false))? {
            check_serial(AnyRef::new(Tag::Integer, serial.value())?)?;
        }
        Ok(key.map(|key| key.value()))
    })
}

fn time_content(time: AnyRef<'_>) -> der::Result<String> {
    match time.tag() {
        Tag::UtcTime => {
            time.decode_as::<UtcTime>()?;
        }
        Tag::GeneralizedTime => {
            time.decode_as::<GeneralizedTime>()?;
        }
        tag => return Err(tag.value_error().into()),
    }
    // The DER time decoders validate the date and require ASCII digits followed
    // by Z. Preserve that exact content, including the original year width.
    Ok(core::str::from_utf8(time.value())
        .map_err(|_| time.tag().value_error())?
        .to_owned())
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
    use der::Encode;
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
        // Changing the extension OID leaves valid DER, but no AKI extension.
        let cert = with_aki(&[der_value(TagNumber(0).context_specific(false), &[1])]);
        assert_eq!(parse_metadata(&cert).unwrap().authority, "01");
        let cert = edit_children(&cert, &[0, 7, 0, 0], |extension| {
            extension[0] = ObjectIdentifier::new_unwrap("2.5.29.36").to_der().unwrap();
        });
        assert_eq!(parse_metadata(&cert).unwrap().authority, "");
    }

    #[test]
    fn test_certificate_missing_common_name() {
        for name_index in [3, 5] {
            let cert = edit_children(&certificate(), &[0, name_index, 0, 0], |attribute| {
                attribute[0] = ObjectIdentifier::new_unwrap("2.5.4.10").to_der().unwrap();
            });
            assert!(matches!(
                parse_metadata(&cert),
                Err(AkCertMetadataError::MissingCommonName(_))
            ));
        }
    }

    #[test]
    fn test_invalid_certificate_metadata() {
        let cert = certificate();
        for size in 0..cert.len() {
            assert!(parse_metadata(&cert[..size]).is_err());
        }
        assert!(parse_metadata(&[0; 4096]).is_err());
        // Truncated key identifier inside a fully bounded AKI sequence.
        let cert = with_aki(&[vec![0x80, 2, 1]]);
        assert!(parse_metadata(&cert).is_err());
    }

    fn der_value(tag: Tag, value: &[u8]) -> Vec<u8> {
        AnyRef::new(tag, value).unwrap().to_der().unwrap()
    }

    fn der_children(data: &[u8]) -> Vec<Vec<u8>> {
        let value = AnyRef::from_der(data).unwrap();
        let mut reader = SliceReader::new(value.value()).unwrap();
        let mut children = Vec::new();
        while !reader.is_finished() {
            children.push(reader.tlv_bytes().unwrap().to_vec());
        }
        reader.finish().unwrap();
        children
    }

    // Re-encode every enclosing length with der, so mutations exercise nested
    // validation, not just the outer certificate's length check.
    fn edit_children(data: &[u8], path: &[usize], edit: impl FnOnce(&mut Vec<Vec<u8>>)) -> Vec<u8> {
        let mut children = der_children(data);
        if let Some((&index, rest)) = path.split_first() {
            children[index] = edit_children(&children[index], rest, edit);
        } else {
            edit(&mut children);
        }
        der_value(AnyRef::from_der(data).unwrap().tag(), &children.concat())
    }

    fn with_aki(fields: &[Vec<u8>]) -> Vec<u8> {
        let extension = der_value(
            Tag::Sequence,
            &[
                ObjectIdentifier::new_unwrap("2.5.29.35").to_der().unwrap(),
                der_value(
                    Tag::OctetString,
                    &der_value(Tag::Sequence, &fields.concat()),
                ),
            ]
            .concat(),
        );
        edit_children(&certificate(), &[0], |tbs| {
            tbs.truncate(7);
            tbs.push(der_value(
                TagNumber(3).context_specific(true),
                &der_value(Tag::Sequence, &extension),
            ));
        })
    }

    #[test]
    fn test_metadata_without_optional_fields() {
        let fixture = certificate();
        let expected = AkCertMetadata {
            authority: String::new(),
            ..parse_metadata(&fixture).unwrap()
        };
        // v3 without extensions, and v1 with its default version omitted.
        for omit_version in [false, true] {
            let cert = edit_children(&fixture, &[0], |tbs| {
                tbs.truncate(7);
                if omit_version {
                    tbs.remove(0);
                }
            });
            assert_eq!(parse_metadata(&cert).unwrap(), expected);
        }
        assert_eq!(parse_metadata(&with_aki(&[])).unwrap(), expected);

        let issuer = der_value(
            TagNumber(1).context_specific(true),
            &der_value(TagNumber(2).context_specific(false), b"issuer.example"),
        );
        let serial = der_value(TagNumber(2).context_specific(false), &[1]);
        // An AKI may identify the authority by issuer/serial without a key ID.
        assert_eq!(
            parse_metadata(&with_aki(&[issuer.clone(), serial.clone()])).unwrap(),
            expected
        );
        let key = der_value(TagNumber(0).context_specific(false), &[0xab, 0xcd]);
        assert_eq!(
            parse_metadata(&with_aki(&[key, issuer, serial]))
                .unwrap()
                .authority,
            "ABCD"
        );
    }

    #[test]
    fn test_metadata_duplicate_aki_extension() {
        // Duplicate detection must not depend on whether a key ID was present.
        for fields in [
            vec![],
            vec![der_value(TagNumber(0).context_specific(false), &[1])],
        ] {
            let cert = edit_children(&with_aki(&fields), &[0, 7, 0], |extensions| {
                extensions.push(extensions[0].clone());
            });
            assert!(parse_metadata(&cert).is_err());
        }
    }

    #[test]
    fn test_metadata_invalid_aki_fields() {
        let key = der_value(TagNumber(0).context_specific(false), &[1]);
        let serial = der_value(TagNumber(2).context_specific(false), &[1]);
        let malformed_fields = [
            vec![key.clone(), key.clone()],
            vec![serial.clone(), key],
            vec![serial.clone(), serial],
            vec![der_value(TagNumber(0).context_specific(true), &[1])],
            vec![der_value(TagNumber(1).context_specific(false), &[])],
            vec![der_value(TagNumber(1).context_specific(true), &[])],
            // A GeneralName truncated within the fully bounded issuer wrapper.
            vec![der_value(
                TagNumber(1).context_specific(true),
                &[0x82, 2, b'a'],
            )],
            vec![der_value(
                TagNumber(1).context_specific(true),
                &der_value(Tag::Null, &[]),
            )],
            vec![der_value(TagNumber(2).context_specific(true), &[1])],
            vec![der_value(TagNumber(2).context_specific(false), &[])],
            vec![der_value(TagNumber(2).context_specific(false), &[0, 1])],
            vec![der_value(TagNumber(2).context_specific(false), &[1; 22])],
            vec![der_value(TagNumber(3).context_specific(false), &[])],
        ];
        for fields in malformed_fields {
            assert!(parse_metadata(&with_aki(&fields)).is_err(), "{fields:?}");
        }
        // The extnValue must contain exactly one complete AKI SEQUENCE.
        for payload in [
            der_value(Tag::Null, &[]),
            vec![0x30, 1],
            vec![0x30, 0, 0x05, 0],
        ] {
            let cert = edit_children(&with_aki(&[]), &[0, 7, 0, 0], |extension| {
                extension[1] = der_value(Tag::OctetString, &payload);
            });
            assert!(parse_metadata(&cert).is_err());
        }
    }

    #[test]
    fn test_metadata_optional_field_order_and_tags() {
        let fixture = certificate();
        let issuer_uid = der_value(TagNumber(1).context_specific(false), &[0, 0x80]);
        let subject_uid = der_value(TagNumber(2).context_specific(false), &[0, 0x40]);
        for uids in [
            vec![issuer_uid.clone()],
            vec![subject_uid.clone()],
            vec![issuer_uid.clone(), subject_uid.clone()],
        ] {
            let cert = edit_children(&fixture, &[0], |tbs| {
                tbs.splice(7..7, uids);
            });
            assert_eq!(
                parse_metadata(&cert).unwrap(),
                parse_metadata(&fixture).unwrap()
            );
        }
        for uids in [
            vec![issuer_uid.clone(), issuer_uid.clone()],
            vec![subject_uid.clone(), issuer_uid],
            vec![subject_uid.clone(), subject_uid],
            vec![der_value(TagNumber(1).context_specific(true), &[0])],
            vec![der_value(TagNumber(2).context_specific(false), &[8, 0])],
            vec![der_value(TagNumber(4).context_specific(false), &[])],
        ] {
            let cert = edit_children(&fixture, &[0], |tbs| {
                tbs.splice(7..7, uids);
            });
            assert!(parse_metadata(&cert).is_err());
        }
        for index in [0, 7] {
            let cert = edit_children(&fixture, &[0], |tbs| {
                tbs.insert(index, tbs[index].clone());
            });
            assert!(parse_metadata(&cert).is_err());
        }
        for version in [0u8, 1, 3, 255] {
            let cert = edit_children(&fixture, &[0, 0], |fields| {
                fields[0] = version.to_der().unwrap();
            });
            assert!(parse_metadata(&cert).is_err());
        }
        let cert = edit_children(&fixture, &[0], |tbs| {
            tbs.push(der_value(TagNumber(1).context_specific(false), &[0]));
        });
        assert!(parse_metadata(&cert).is_err());
    }

    #[test]
    fn test_metadata_nested_sequences_are_fully_consumed() {
        let fixture = certificate();
        // Certificate, TBS, version wrapper, algorithms, Name attribute,
        // validity, SPKI, extensions wrapper, and an individual extension.
        for path in [
            vec![],
            vec![0],
            vec![0, 0],
            vec![0, 2],
            vec![1],
            vec![0, 3, 0, 0],
            vec![0, 4],
            vec![0, 5, 0, 0],
            vec![0, 6],
            vec![0, 6, 0],
            vec![0, 7],
            vec![0, 7, 0, 0],
        ] {
            let cert = edit_children(&fixture, &path, |fields| {
                // One added field could be valid optional algorithm parameters.
                fields.push(der_value(Tag::Null, &[]));
                fields.push(der_value(Tag::Null, &[]));
            });
            assert!(parse_metadata(&cert).is_err(), "extra field at {path:?}");
            let mut missing_optional = path == [0];
            let cert = edit_children(&fixture, &path, |fields| {
                fields.pop().unwrap();
                // Only the second AlgorithmIdentifier field is optional.
                if [vec![0, 2], vec![1], vec![0, 6, 0]].contains(&path) {
                    missing_optional = fields.len() == 1;
                }
            });
            if missing_optional {
                assert!(parse_metadata(&cert).is_ok(), "optional field at {path:?}");
            } else {
                assert!(parse_metadata(&cert).is_err(), "missing field at {path:?}");
            }
        }
        for index in 0..3 {
            let cert = edit_children(&fixture, &[], |fields| {
                fields[index] = der_value(Tag::Null, &[]);
            });
            assert!(parse_metadata(&cert).is_err());
        }
        for index in 0..8 {
            let cert = edit_children(&fixture, &[0], |fields| {
                fields[index] = der_value(Tag::Null, &[]);
            });
            assert!(parse_metadata(&cert).is_err());
        }
        let cert = edit_children(&fixture, &[0, 3], |rdns| {
            rdns[0] = der_value(Tag::Set, &[]);
        });
        assert!(parse_metadata(&cert).is_err());
        let cert = edit_children(&fixture, &[0, 3, 0], |attributes| {
            attributes.push(vec![0x30, 2, 0x06]);
        });
        assert!(parse_metadata(&cert).is_err());
    }

    #[test]
    fn test_metadata_directory_strings() {
        let fixture = certificate();
        for name in [3, 5] {
            for (tag, content, expected) in [
                (Tag::Utf8String, "AK é 水 😀".as_bytes(), "AK é 水 😀"),
                (
                    Tag::PrintableString,
                    b"AZaz09 '()+,-./:=?".as_slice(),
                    "AZaz09 '()+,-./:=?",
                ),
                (Tag::BmpString, b"\x00A\x00K\x00 \x00\xe9\x6c\x34", "AK é水"),
                (
                    Tag::TeletexString,
                    b"Test \"AK\" @issuer\x7f",
                    "Test \"AK\" @issuer\x7f",
                ),
                // Preserve the previous decoder's acceptance of empty strings.
                (Tag::Utf8String, b"", ""),
                (Tag::PrintableString, b"", ""),
                (Tag::BmpString, b"", ""),
                (Tag::TeletexString, b"", ""),
            ] {
                let cert = edit_children(&fixture, &[0, name, 0, 0], |attribute| {
                    attribute[1] = der_value(tag, content);
                });
                let metadata = parse_metadata(&cert).unwrap();
                assert_eq!(
                    if name == 3 {
                        metadata.issuer
                    } else {
                        metadata.subject
                    },
                    expected,
                    "{tag:?} at name {name}"
                );
            }
            for value in [
                der_value(Tag::Utf8String, &[0xff]),
                der_value(Tag::Utf8String, &[0xc0, 0xaf]),
                der_value(Tag::Utf8String, &[0xe2, 0x82]),
                der_value(Tag::PrintableString, b"invalid@CN"),
                der_value(Tag::PrintableString, "é".as_bytes()),
                der_value(Tag::PrintableString, b"invalid\0CN"),
                der_value(Tag::BmpString, &[0]),
                der_value(Tag::BmpString, &[0xd8, 0x00]),
                der_value(Tag::BmpString, &[0xdc, 0x00]),
                // A valid UTF-16 surrogate pair is not a BMP character.
                der_value(Tag::BmpString, &[0xd8, 0x3d, 0xde, 0x00]),
                der_value(Tag::BmpString, &[0xff, 0xff]),
                // der's TeletexString supports ASCII, not Latin-1 or UTF-8.
                der_value(Tag::TeletexString, &[0x80]),
                der_value(Tag::TeletexString, &[0xe9]),
                der_value(Tag::TeletexString, "é".as_bytes()),
                der_value(Tag::OctetString, b"not a DirectoryString"),
                der_value(Tag::Ia5String, b"not a DirectoryString"),
                der_value(Tag::Integer, &[1]),
                der_value(Tag::Null, &[]),
                der_value(Tag::Sequence, &[]),
                der_value(TagNumber(0).context_specific(false), b"CN"),
                // UniversalString was not supported by DirectoryString either.
                vec![0x1c, 4, 0, 0, 0, b'A'],
            ] {
                let cert = edit_children(&fixture, &[0, name, 0, 0], |attribute| {
                    attribute[1] = value.clone();
                });
                assert!(parse_metadata(&cert).is_err(), "{value:?} at name {name}");
            }
        }
    }

    #[test]
    fn test_metadata_times() {
        let fixture = certificate();
        for index in [0, 1] {
            // Both validity fields accept either time encoding. Preserve the
            // exact content, including leading zeroes and generalized years
            // that could also have been represented as UTCTime.
            for (tag, content) in [
                (Tag::UtcTime, "700101000000Z"),
                (Tag::UtcTime, "991231235959Z"),
                (Tag::UtcTime, "000229000000Z"),
                (Tag::UtcTime, "261009180232Z"),
                (Tag::UtcTime, "491231235959Z"),
                (Tag::GeneralizedTime, "19700101000000Z"),
                (Tag::GeneralizedTime, "20000229000000Z"),
                (Tag::GeneralizedTime, "20561120180232Z"),
                (Tag::GeneralizedTime, "99991231235959Z"),
            ] {
                let cert = edit_children(&fixture, &[0, 4], |validity| {
                    validity[index] = der_value(tag, content.as_bytes());
                });
                let metadata = parse_metadata(&cert).unwrap();
                assert_eq!(
                    if index == 0 {
                        metadata.not_before
                    } else {
                        metadata.not_after
                    },
                    content
                );
            }
            for (tag, content) in [
                (Tag::UtcTime, b"261309180232Z".as_slice()),
                (Tag::UtcTime, b"260009180232Z"),
                (Tag::UtcTime, b"260229180232Z"),
                (Tag::UtcTime, b"261009240232Z"),
                (Tag::UtcTime, b"261009186032Z"),
                (Tag::UtcTime, b"261009180260Z"),
                (Tag::UtcTime, b"261009180232"),
                (Tag::UtcTime, b"2610091802Z"),
                (Tag::UtcTime, b"261009180232+0000"),
                (Tag::UtcTime, b"261009180232z"),
                (Tag::UtcTime, b"261009180232.0Z"),
                (Tag::UtcTime, b"26100918023\xffZ"),
                // Retain der's existing rejection of pre-UNIX-epoch dates.
                (Tag::UtcTime, b"691231235959Z"),
                (Tag::GeneralizedTime, b"19691231235959Z"),
                (Tag::GeneralizedTime, b"20561132180232Z"),
                (Tag::GeneralizedTime, b"20561320180232Z"),
                (Tag::GeneralizedTime, b"21000229180232Z"),
                (Tag::GeneralizedTime, b"20561120240232Z"),
                (Tag::GeneralizedTime, b"20561120186032Z"),
                (Tag::GeneralizedTime, b"20561120180260Z"),
                (Tag::GeneralizedTime, b"20561120180232"),
                (Tag::GeneralizedTime, b"205611201802Z"),
                (Tag::GeneralizedTime, b"20561120180232+0000"),
                (Tag::GeneralizedTime, b"20561120180232z"),
                (Tag::GeneralizedTime, b"20561120180232.0Z"),
                (Tag::GeneralizedTime, b"2056112018023\xffZ"),
                (Tag::UtcTime, b"20561120180232Z"),
                (Tag::GeneralizedTime, b"261009180232Z"),
                (Tag::Utf8String, b"261009180232Z"),
                (Tag::OctetString, b"20561120180232Z"),
                (Tag::Null, b""),
                (Tag::Sequence, b""),
            ] {
                let cert = edit_children(&fixture, &[0, 4], |validity| {
                    validity[index] = der_value(tag, content);
                });
                assert!(
                    parse_metadata(&cert).is_err(),
                    "{tag:?} {content:?} at {index}"
                );
            }
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
