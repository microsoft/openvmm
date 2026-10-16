// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Stateful AK certificate test policies with deterministic bit-5 decisions.
//! These validate JSON renewal hints only, not expiry, measured reports,
//! certificate trust, or the relationship to the TPM AK.

use crate::Error;
use der::Encode;
use der::Tag;
use der::TagNumber;
use der::asn1::AnyRef;
use der::asn1::BitStringRef;
use der::asn1::ObjectIdentifier;
use get_resources::ged::IgvmAttestTestConfig;
use openhcl_attestation_protocol::igvm_attest::get::IgvmAttestAkCertResponseHeader;
use openhcl_attestation_protocol::igvm_attest::get::IgvmAttestResponseVersion;
use openhcl_attestation_protocol::igvm_attest::get::IgvmErrorInfo;
use openhcl_attestation_protocol::igvm_attest::get::IgvmSignal;
use serde_json::Value;
use serde_json::json;
use zerocopy::IntoBytes;

// Public dummy identifier retained solely for the expected metadata contract.
// It is not a secret or derived from a key used by these fixtures.
const DUMMY_AUTHORITY: [u8; 20] = [
    0x9f, 0xf3, 0x93, 0x5c, 0x2c, 0x83, 0xd8, 0x4f, 0x1d, 0xb5, 0x24, 0x2a, 0x0e, 0x7d, 0x3c, 0x9b,
    0x1d, 0x19, 0x1d, 0x3b,
];

/// Construct certificate-shaped DER at runtime for parser-only E2E tests.
///
/// This is not a cryptographic certificate: the public key and signature are
/// each a dummy zero byte. It does not certify the VM's AK or establish trust.
pub fn certificate() -> Vec<u8> {
    encode_certificate("Test \"AK\" issuer").expect("fixed parser fixture must encode")
}

/// Construct replacement certificate-shaped DER for parser-only tests.
///
/// Only the issuer and subject names differ. Like [`certificate`], this has no
/// usable key or signature. Replacement is scripted, not based on expiry.
pub fn replacement_certificate() -> Vec<u8> {
    encode_certificate("Next \"AK\" issuer").expect("fixed parser fixture must encode")
}

fn der_value(tag: Tag, value: &[u8]) -> der::Result<Vec<u8>> {
    AnyRef::new(tag, value)?.to_der()
}

// Encode only the X.509 envelope needed by the metadata parser. All TLV lengths
// come from der, including when a different name changes enclosing lengths.
fn encode_certificate(name: &str) -> der::Result<Vec<u8>> {
    let signature_algorithm = der_value(
        Tag::Sequence,
        &ObjectIdentifier::new_unwrap("1.2.840.10045.4.3.2").to_der()?, // ecdsaWithSHA256
    )?;
    let name = der_value(
        Tag::Sequence,
        &der_value(
            Tag::Set,
            &der_value(
                Tag::Sequence,
                &[
                    ObjectIdentifier::new_unwrap("2.5.4.3").to_der()?, // commonName
                    der_value(Tag::Utf8String, name.as_bytes())?,
                ]
                .concat(),
            )?,
        )?,
    )?;
    let validity = der_value(
        Tag::Sequence,
        &[
            der_value(Tag::UtcTime, b"261009180232Z")?,
            der_value(Tag::GeneralizedTime, b"20561120180232Z")?,
        ]
        .concat(),
    )?;
    let dummy_bits = BitStringRef::from_bytes(&[0])?.to_der()?;
    let spki = der_value(
        Tag::Sequence,
        &[
            der_value(
                Tag::Sequence,
                &ObjectIdentifier::new_unwrap("1.2.840.10045.2.1").to_der()?, // id-ecPublicKey
            )?,
            dummy_bits.clone(),
        ]
        .concat(),
    )?;
    let authority = der_value(
        Tag::Sequence,
        &der_value(TagNumber(0).context_specific(false), &DUMMY_AUTHORITY)?,
    )?;
    let extension = der_value(
        Tag::Sequence,
        &[
            ObjectIdentifier::new_unwrap("2.5.29.35").to_der()?, // authorityKeyIdentifier
            der_value(Tag::OctetString, &authority)?,
        ]
        .concat(),
    )?;
    let tbs = der_value(
        Tag::Sequence,
        &[
            der_value(TagNumber(0).context_specific(true), &2u8.to_der()?)?,
            1u8.to_der()?,
            signature_algorithm.clone(),
            name.clone(),
            validity,
            name,
            spki,
            der_value(
                TagNumber(3).context_specific(true),
                &der_value(Tag::Sequence, &extension)?,
            )?,
        ]
        .concat(),
    )?;
    der_value(
        Tag::Sequence,
        &[tbs, signature_algorithm, dummy_bits].concat(),
    )
}

// Deliberately independent of production metadata extraction and serialization.
fn expected_metadata(replacement: bool) -> Value {
    let name = if replacement {
        "Next \"AK\" issuer"
    } else {
        "Test \"AK\" issuer"
    };
    json!({
        "issuer": name,
        "subject": name,
        "not_before": "261009180232Z",
        "not_after": "20561120180232Z",
        "authority": "9FF3935C2C83D84F1DB5242A0E7D3C9B1D191D3B",
    })
}

// Nonempty, non-DER data makes an erroneous NV write observable to the guest.
const POISON: &[u8] = b"AK NO-RENEWAL POISON: MUST NOT BE WRITTEN TO NV";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Policy {
    NoRenewal,
    KeepThenUnavailable,
    ReplaceThenUnavailable,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Stage {
    // No certificate issued; akcert must be absent.
    Initial,
    // A issued; require exact A metadata before deciding whether to replace it.
    Original,
    // A retained once with bit 5 set; require A and remain unavailable.
    Retained,
    // B issued with bit 5 clear; require B and remain unavailable.
    Replaced,
}

pub(crate) struct State {
    policy: Policy,
    stage: Stage,
    last_error: Option<String>,
}

impl State {
    pub(crate) fn from_config(config: &IgvmAttestTestConfig) -> Option<Self> {
        let policy = match config {
            IgvmAttestTestConfig::AkCertNoRenewal => Policy::NoRenewal,
            IgvmAttestTestConfig::AkCertPersistentAcrossBoot => Policy::KeepThenUnavailable,
            IgvmAttestTestConfig::AkCertRenewal
            | IgvmAttestTestConfig::AkCertPersistentAcrossBootExtended => {
                Policy::ReplaceThenUnavailable
            }
            _ => return None,
        };
        Some(Self {
            policy,
            stage: Stage::Initial,
            last_error: None,
        })
    }

    fn validate(&self, claims: &[u8]) -> Result<(), String> {
        let value: Value = serde_json::from_slice(claims)
            .map_err(|err| format!("invalid runtime claims JSON: {err}"))?;
        let object = value
            .as_object()
            .ok_or("runtime claims must be a JSON object")?;
        if self.stage != Stage::Initial {
            let expected = expected_metadata(self.stage == Stage::Replaced);
            if object.get("akcert") != Some(&expected) {
                return Err(format!("akcert must exactly match {expected}"));
            }
        } else if object.contains_key("akcert") {
            return Err("akcert must be absent, not null or metadata".into());
        }
        Ok(())
    }

    pub(crate) fn handle_request(&mut self, claims: &[u8]) -> Result<(Vec<u8>, u32), Error> {
        // Once validation fails, even a later valid request must not hide it.
        if let Some(error) = &self.last_error {
            return Err(Error::AkCertValidation(error.clone()));
        }
        if let Err(error) = self.validate(claims) {
            self.last_error = Some(error.clone());
            return Err(Error::AkCertValidation(error));
        }

        let (data, no_renewal) = match self.stage {
            Stage::Initial => {
                self.stage = Stage::Original;
                (certificate(), false)
            }
            Stage::Original => match self.policy {
                Policy::NoRenewal => (POISON.to_vec(), true),
                Policy::KeepThenUnavailable => {
                    self.stage = Stage::Retained;
                    (POISON.to_vec(), true)
                }
                Policy::ReplaceThenUnavailable => {
                    self.stage = Stage::Replaced;
                    (replacement_certificate(), false)
                }
            },
            // Still validate the retained/replaced metadata on every request.
            // Unavailable means no transport response, not an empty payload.
            Stage::Retained | Stage::Replaced => return Ok((vec![], 0)),
        };
        let header = IgvmAttestAkCertResponseHeader {
            data_size: (size_of::<IgvmAttestAkCertResponseHeader>() + data.len()) as u32,
            version: IgvmAttestResponseVersion::VERSION_2,
            error_info: IgvmErrorInfo {
                igvm_signal: IgvmSignal::new().with_no_renew_cert(no_renewal),
                ..Default::default()
            },
        };
        Ok(([header.as_bytes(), &data].concat(), header.data_size))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::IgvmAgentAction;
    use crate::IgvmAgentTestSetting;
    use crate::TestIgvmAgent;
    use der::Decode;
    use der::Reader;
    use der::SliceReader;
    use der::Tagged;
    use der::asn1::GeneralizedTime;
    use der::asn1::OctetStringRef;
    use der::asn1::UtcTime;
    use der::asn1::Utf8StringRef;
    use openhcl_attestation_protocol::igvm_attest::get::IGVM_ATTEST_REQUEST_CURRENT_VERSION;
    use openhcl_attestation_protocol::igvm_attest::get::IgvmAttestRequestBase;
    use openhcl_attestation_protocol::igvm_attest::get::IgvmAttestRequestDataExt;
    use openhcl_attestation_protocol::igvm_attest::get::IgvmAttestRequestType;
    use test_with_tracing::test;
    use zerocopy::FromBytes;
    use zerocopy::FromZeros;

    const CONFIGS: [IgvmAttestTestConfig; 4] = [
        IgvmAttestTestConfig::AkCertNoRenewal,
        IgvmAttestTestConfig::AkCertPersistentAcrossBoot,
        IgvmAttestTestConfig::AkCertRenewal,
        IgvmAttestTestConfig::AkCertPersistentAcrossBootExtended,
    ];

    fn state() -> State {
        State::from_config(&IgvmAttestTestConfig::AkCertNoRenewal).unwrap()
    }

    fn claims(replacement: bool) -> Vec<u8> {
        serde_json::to_vec(&json!({"akcert": expected_metadata(replacement)})).unwrap()
    }

    fn check_response(response: (Vec<u8>, u32), payload: &[u8], no_renewal: bool) {
        let (bytes, len) = response;
        let (header, data) = IgvmAttestAkCertResponseHeader::read_from_prefix(&bytes).unwrap();
        assert_eq!(len as usize, bytes.len());
        assert_eq!(header.data_size, len);
        assert_eq!(header.version, IgvmAttestResponseVersion::VERSION_2);
        assert_eq!(header.error_info.error_code, 0);
        assert_eq!(header.error_info.http_status_code, 0);
        assert_eq!(header.error_info.reserved, [0; 3]);
        // Check the actual ABI bit, not just the production bitfield accessor.
        assert_eq!(
            header.error_info.igvm_signal.as_bytes(),
            &(if no_renewal { 1u32 << 5 } else { 0 }).to_le_bytes()
        );
        assert_eq!(data, payload);
        assert!(!data.is_empty());
    }

    fn request(state: &mut State, claims: &[u8], payload: &[u8], no_renewal: bool) {
        check_response(state.handle_request(claims).unwrap(), payload, no_renewal);
    }

    #[test]
    fn no_renewal_policy() {
        let mut state = state();
        assert_eq!(state.stage, Stage::Initial);
        // Other runtime claims are permitted, but not interpreted as HW evidence.
        request(&mut state, br#"{"keys":[]}"#, &certificate(), false);
        for _ in 0..3 {
            request(&mut state, &claims(false), POISON, true);
        }
        assert_eq!(state.stage, Stage::Original);
        assert!(state.last_error.is_none());
    }

    #[test]
    fn keep_then_unavailable() {
        let mut agent = TestIgvmAgent::new("ak-keep-test");
        let setting =
            IgvmAgentTestSetting::TestConfig(IgvmAttestTestConfig::AkCertPersistentAcrossBoot);
        agent.install_plan_from_setting(&setting);
        check_response(
            agent.handle_request(&wire_request(b"{}")).unwrap(),
            &certificate(),
            false,
        );
        assert_eq!(agent.ak_cert.as_ref().unwrap().stage, Stage::Original);
        check_response(
            agent.handle_request(&wire_request(&claims(false))).unwrap(),
            POISON,
            true,
        );
        for _ in 0..3 {
            agent.install_plan_from_setting(&setting);
            assert_eq!(
                agent.handle_request(&wire_request(&claims(false))).unwrap(),
                (vec![], 0)
            );
            assert_eq!(agent.ak_cert.as_ref().unwrap().stage, Stage::Retained);
        }
        assert!(agent.plan.as_ref().unwrap().is_empty());
        assert!(agent.ak_cert.as_ref().unwrap().last_error.is_none());
    }

    #[test]
    fn replacement_config_aliases_then_unavailable() {
        for config in [
            IgvmAttestTestConfig::AkCertRenewal,
            IgvmAttestTestConfig::AkCertPersistentAcrossBootExtended,
        ] {
            let mut agent = TestIgvmAgent::new("ak-replacement-test");
            let setting = IgvmAgentTestSetting::TestConfig(config);
            agent.install_plan_from_setting(&setting);
            assert_eq!(
                agent.ak_cert.as_ref().unwrap().policy,
                Policy::ReplaceThenUnavailable
            );
            check_response(
                agent.handle_request(&wire_request(b"{}")).unwrap(),
                &certificate(),
                false,
            );
            assert_eq!(agent.ak_cert.as_ref().unwrap().stage, Stage::Original);
            check_response(
                agent.handle_request(&wire_request(&claims(false))).unwrap(),
                &replacement_certificate(),
                false,
            );
            for _ in 0..3 {
                agent.install_plan_from_setting(&setting);
                assert_eq!(
                    agent.handle_request(&wire_request(&claims(true))).unwrap(),
                    (vec![], 0)
                );
                assert_eq!(agent.ak_cert.as_ref().unwrap().stage, Stage::Replaced);
            }
            assert!(agent.plan.as_ref().unwrap().is_empty());
            assert!(agent.ak_cert.as_ref().unwrap().last_error.is_none());
        }
    }

    fn check_sticky_failure(state: &mut State, invalid: &[u8], valid: &[u8]) {
        let stage = state.stage;
        let Err(Error::AkCertValidation(first)) = state.handle_request(invalid) else {
            panic!("invalid request must produce a typed validation error");
        };
        assert!(!first.is_empty());
        for input in [valid, b"not JSON".as_slice()] {
            let Err(Error::AkCertValidation(error)) = state.handle_request(input) else {
                panic!("validation failure must be sticky");
            };
            assert_eq!(error, first);
        }
        assert_eq!(state.last_error.as_ref(), Some(&first));
        assert_eq!(state.stage, stage);
    }

    #[test]
    fn absent_is_not_null_or_present_metadata() {
        for config in CONFIGS {
            for input in [
                b"{\"akcert\":null}".to_vec(),
                claims(false),
                claims(true),
                b"{\"akcert\":{}}".to_vec(),
                b"null".to_vec(),
                b"[]".to_vec(),
                b"not JSON".to_vec(),
            ] {
                let mut state = State::from_config(&config).unwrap();
                check_sticky_failure(&mut state, &input, b"{}");
            }
        }
    }

    fn invalid_metadata_claims(replacement: bool) -> Vec<Vec<u8>> {
        let mut invalid = vec![
            b"{}".to_vec(),
            b"{\"akcert\":null}".to_vec(),
            b"{\"akcert\":[]}".to_vec(),
            b"null".to_vec(),
            b"[]".to_vec(),
            b"not JSON".to_vec(),
            claims(!replacement),
        ];
        for field in ["issuer", "subject", "not_before", "not_after", "authority"] {
            for value in [json!("incorrect"), Value::Null, json!(42)] {
                let mut metadata = expected_metadata(replacement);
                metadata[field] = value;
                invalid.push(serde_json::to_vec(&json!({"akcert": metadata})).unwrap());
            }
            let mut metadata = expected_metadata(replacement);
            metadata.as_object_mut().unwrap().remove(field);
            invalid.push(serde_json::to_vec(&json!({"akcert": metadata})).unwrap());
        }
        let mut extra = expected_metadata(replacement);
        extra["extra"] = json!(true);
        invalid.push(serde_json::to_vec(&json!({"akcert": extra})).unwrap());
        invalid
    }

    #[test]
    fn exact_metadata_required_and_failures_are_sticky() {
        for config in CONFIGS {
            // Validate both the original-certificate stage and the stage after
            // the policy decision (including after multiple empty responses).
            for subsequent_requests in [0, 1, 4] {
                let replacement = subsequent_requests > 0
                    && matches!(
                        config,
                        IgvmAttestTestConfig::AkCertRenewal
                            | IgvmAttestTestConfig::AkCertPersistentAcrossBootExtended
                    );
                for input in invalid_metadata_claims(replacement) {
                    let mut state = State::from_config(&config).unwrap();
                    state.handle_request(b"{}").unwrap();
                    for index in 0..subsequent_requests {
                        state
                            .handle_request(&claims(replacement && index > 0))
                            .unwrap();
                    }
                    check_sticky_failure(&mut state, &input, &claims(replacement));
                }
            }
        }
    }

    fn der_children(value: AnyRef<'_>, tag: Tag) -> Vec<AnyRef<'_>> {
        assert_eq!(value.tag(), tag);
        let mut reader = SliceReader::new(value.value()).unwrap();
        let mut children = Vec::new();
        while !reader.is_finished() {
            children.push(reader.decode().unwrap());
        }
        reader.finish().unwrap();
        children
    }

    fn check_algorithm(value: AnyRef<'_>, oid: &str) {
        let fields = der_children(value, Tag::Sequence);
        assert_eq!(fields.len(), 1);
        assert_eq!(
            fields[0].decode_as::<ObjectIdentifier>().unwrap(),
            ObjectIdentifier::new_unwrap(oid)
        );
    }

    fn check_dummy_bits(value: AnyRef<'_>) {
        let bits = value.decode_as::<BitStringRef<'_>>().unwrap();
        assert_eq!(bits.unused_bits(), 0);
        assert_eq!(bits.as_bytes(), Some([0].as_slice()));
    }

    fn check_certificate(data: &[u8], name: &str) {
        // This validates runtime encoding for a metadata parser, not a
        // cryptographic certificate. No key or signature verification occurs.
        let cert = der_children(AnyRef::from_der(data).unwrap(), Tag::Sequence);
        assert_eq!(cert.len(), 3);
        check_algorithm(cert[1], "1.2.840.10045.4.3.2");
        check_dummy_bits(cert[2]);

        let tbs = der_children(cert[0], Tag::Sequence);
        assert_eq!(tbs.len(), 8);
        let version = der_children(tbs[0], TagNumber(0).context_specific(true));
        assert_eq!(version.len(), 1);
        assert_eq!(version[0].decode_as::<u8>().unwrap(), 2);
        assert_eq!(tbs[1].decode_as::<u8>().unwrap(), 1);
        assert_eq!(tbs[2], cert[1]);
        for index in [3, 5] {
            let rdns = der_children(tbs[index], Tag::Sequence);
            assert_eq!(rdns.len(), 1);
            let attributes = der_children(rdns[0], Tag::Set);
            assert_eq!(attributes.len(), 1);
            let attribute = der_children(attributes[0], Tag::Sequence);
            assert_eq!(attribute.len(), 2);
            assert_eq!(
                attribute[0].decode_as::<ObjectIdentifier>().unwrap(),
                ObjectIdentifier::new_unwrap("2.5.4.3")
            );
            assert_eq!(
                attribute[1]
                    .decode_as::<Utf8StringRef<'_>>()
                    .unwrap()
                    .as_str(),
                name
            );
        }
        let validity = der_children(tbs[4], Tag::Sequence);
        assert_eq!(validity.len(), 2);
        validity[0].decode_as::<UtcTime>().unwrap();
        validity[1].decode_as::<GeneralizedTime>().unwrap();
        assert_eq!(validity[0].value(), b"261009180232Z");
        assert_eq!(validity[1].value(), b"20561120180232Z");

        let spki = der_children(tbs[6], Tag::Sequence);
        assert_eq!(spki.len(), 2);
        check_algorithm(spki[0], "1.2.840.10045.2.1");
        check_dummy_bits(spki[1]);

        let explicit = der_children(tbs[7], TagNumber(3).context_specific(true));
        assert_eq!(explicit.len(), 1);
        let extensions = der_children(explicit[0], Tag::Sequence);
        assert_eq!(extensions.len(), 1);
        let extension = der_children(extensions[0], Tag::Sequence);
        assert_eq!(extension.len(), 2);
        assert_eq!(
            extension[0].decode_as::<ObjectIdentifier>().unwrap(),
            ObjectIdentifier::new_unwrap("2.5.29.35")
        );
        let value = extension[1].decode_as::<&OctetStringRef>().unwrap();
        let aki = der_children(AnyRef::from_der(value.as_bytes()).unwrap(), Tag::Sequence);
        assert_eq!(aki.len(), 1);
        assert_eq!(aki[0].tag(), TagNumber(0).context_specific(false));
        assert_eq!(aki[0].value(), DUMMY_AUTHORITY);
    }

    #[test]
    fn certificate_fixture() {
        let cert = certificate();
        let replacement = replacement_certificate();
        assert_eq!(cert, certificate());
        assert_eq!(replacement, replacement_certificate());
        assert_ne!(cert, replacement);
        check_certificate(&cert, "Test \"AK\" issuer");
        check_certificate(&replacement, "Next \"AK\" issuer");

        let original = der_children(AnyRef::from_der(&cert).unwrap(), Tag::Sequence);
        let replaced = der_children(AnyRef::from_der(&replacement).unwrap(), Tag::Sequence);
        assert_eq!(original[1..], replaced[1..]);
        let original_tbs = der_children(original[0], Tag::Sequence);
        let replaced_tbs = der_children(replaced[0], Tag::Sequence);
        // Only the issuer and subject differ; compare decoded fields rather
        // than relying on byte offsets or a particular total DER length.
        for (index, (a, b)) in original_tbs.iter().zip(&replaced_tbs).enumerate() {
            assert_eq!(a != b, matches!(index, 3 | 5));
        }
    }

    #[test]
    fn certificate_name_controls_encoding_lengths() {
        // Cross DER's short/long length boundary without patching any bytes.
        let name = "Parser-only AK name ".repeat(16);
        check_certificate(&encode_certificate(&name).unwrap(), &name);
    }

    // Exercise dispatch without a VM or a hardware attestation report.
    fn wire_request(claims: &[u8]) -> Vec<u8> {
        let mut base = IgvmAttestRequestBase::new_zeroed();
        base.header.request_type = IgvmAttestRequestType::AK_CERT_REQUEST;
        base.request_data.version = IGVM_ATTEST_REQUEST_CURRENT_VERSION;
        base.request_data.variable_data_size = claims.len() as u32;
        let ext = IgvmAttestRequestDataExt::new_zeroed();
        [base.as_bytes(), ext.as_bytes(), claims].concat()
    }

    #[test]
    fn agent_opt_in_dispatch_and_install_once() {
        let mut agent = TestIgvmAgent::new("ak-renewal-test");
        check_response(
            agent.handle_request(&wire_request(b"{}")).unwrap(),
            &[0xab; 2500],
            false,
        );
        assert!(agent.ak_cert.is_none());
        agent.install_plan_from_setting(&IgvmAgentTestSetting::TestConfig(
            IgvmAttestTestConfig::AkCertNoRenewal,
        ));
        check_response(
            agent.handle_request(&wire_request(b"{}")).unwrap(),
            &certificate(),
            false,
        );
        agent.install_plan_from_setting(&IgvmAgentTestSetting::TestConfig(
            IgvmAttestTestConfig::AkCertNoRenewal,
        ));
        agent.install_plan_from_setting(&IgvmAgentTestSetting::TestConfig(
            IgvmAttestTestConfig::AkCertRequestFailureAndRetry,
        ));
        check_response(
            agent.handle_request(&wire_request(&claims(false))).unwrap(),
            POISON,
            true,
        );
        assert_eq!(agent.ak_cert.as_ref().unwrap().stage, Stage::Original);
        assert!(agent.ak_cert.as_ref().unwrap().last_error.is_none());
        assert!(
            agent
                .take_next_action(IgvmAttestRequestType::AK_CERT_REQUEST)
                .is_none()
        );

        let mut legacy = TestIgvmAgent::new("legacy-test");
        legacy.install_plan_from_setting(&IgvmAgentTestSetting::TestConfig(
            IgvmAttestTestConfig::AkCertRequestFailureAndRetry,
        ));
        legacy.install_plan_from_setting(&IgvmAgentTestSetting::TestConfig(
            IgvmAttestTestConfig::AkCertNoRenewal,
        ));
        assert!(legacy.ak_cert.is_none());
        for _ in 0..2 {
            assert!(matches!(
                legacy.take_next_action(IgvmAttestRequestType::AK_CERT_REQUEST),
                Some(IgvmAgentAction::RespondFailure)
            ));
        }
        assert!(matches!(
            legacy.take_next_action(IgvmAttestRequestType::AK_CERT_REQUEST),
            Some(IgvmAgentAction::RespondSuccess)
        ));
    }

    #[test]
    fn legacy_retry_and_state_refresh_plans_unchanged() {
        for (config, failures, successes, unavailable) in [
            (
                IgvmAttestTestConfig::AkCertRequestFailureAndRetry,
                2,
                1,
                false,
            ),
            (
                IgvmAttestTestConfig::AkCertRequestFailureAndRetryExtended,
                6,
                1,
                false,
            ),
            (IgvmAttestTestConfig::StateRefresh, 0, 2, true),
        ] {
            let mut agent = TestIgvmAgent::new("legacy-plan-test");
            agent.install_plan_from_setting(&IgvmAgentTestSetting::TestConfig(config));
            assert!(agent.ak_cert.is_none());
            for _ in 0..failures {
                assert!(matches!(
                    agent.take_next_action(IgvmAttestRequestType::AK_CERT_REQUEST),
                    Some(IgvmAgentAction::RespondFailure)
                ));
            }
            for _ in 0..successes {
                check_response(
                    agent.handle_request(&wire_request(b"{}")).unwrap(),
                    &[0xab; 2500],
                    false,
                );
            }
            for _ in 0..3 {
                if unavailable {
                    assert_eq!(
                        agent.handle_request(&wire_request(b"{}")).unwrap(),
                        (vec![], 0)
                    );
                } else {
                    assert!(
                        agent
                            .take_next_action(IgvmAttestRequestType::AK_CERT_REQUEST)
                            .is_none()
                    );
                }
            }
        }
    }
}
