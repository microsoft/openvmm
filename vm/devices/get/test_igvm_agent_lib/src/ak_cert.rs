// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Stateful AK certificate test policies with deterministic bit-5 decisions.
//! These validate JSON renewal hints only, not expiry, measured reports,
//! certificate trust, or the relationship to the TPM AK.

use crate::Error;
use crypto::rsa::RsaKeyPair;
use crypto::x509::X509Certificate;
use crypto::x509::X509SelfSignedOptions;
use get_resources::ged::IgvmAttestTestConfig;
use openhcl_attestation_protocol::igvm_attest::get::IgvmAttestAkCertResponseHeader;
use openhcl_attestation_protocol::igvm_attest::get::IgvmAttestResponseVersion;
use openhcl_attestation_protocol::igvm_attest::get::IgvmErrorInfo;
use openhcl_attestation_protocol::igvm_attest::get::IgvmSignal;
use serde_json::Value;
use serde_json::json;
use std::sync::LazyLock;
use zerocopy::IntoBytes;

// Public dummy identifier retained solely for the expected metadata contract.
// It is not a secret or derived from a key used by these fixtures.
const DUMMY_AUTHORITY: [u8; 20] = [
    0x9f, 0xf3, 0x93, 0x5c, 0x2c, 0x83, 0xd8, 0x4f, 0x1d, 0xb5, 0x24, 0x2a, 0x0e, 0x7d, 0x3c, 0x9b,
    0x1d, 0x19, 0x1d, 0x3b,
];

static SIGNING_KEY: LazyLock<RsaKeyPair> =
    LazyLock::new(|| RsaKeyPair::generate(2048).expect("test certificate key generation"));
static CERTIFICATE: LazyLock<Vec<u8>> = LazyLock::new(|| {
    encode_certificate("Test \"AK\" issuer").expect("fixed test certificate must encode")
});
static REPLACEMENT_CERTIFICATE: LazyLock<Vec<u8>> = LazyLock::new(|| {
    encode_certificate("Next \"AK\" issuer").expect("fixed replacement certificate must encode")
});

/// Construct a self-signed certificate for renewal metadata E2E tests.
///
/// The test signing key is unrelated to the VM's AK and does not establish trust.
pub fn certificate() -> Vec<u8> {
    CERTIFICATE.clone()
}

/// Construct a replacement self-signed certificate for renewal tests.
///
/// Uses the same key, validity and AKI as [`certificate`], but a different name.
/// Replacement is scripted, not based on expiry.
pub fn replacement_certificate() -> Vec<u8> {
    REPLACEMENT_CERTIFICATE.clone()
}

fn encode_certificate(name: &str) -> Result<Vec<u8>, crypto::x509::X509Error> {
    // Both names use the same test key. Cache the certificates above because
    // generating keys and signatures is unnecessary on each attestation request.
    X509Certificate::build_self_signed(
        (&*SIGNING_KEY).into(),
        &X509SelfSignedOptions {
            // Metadata mutation tests require the CN to be the only RDN.
            country: "",
            state: "",
            locality: "",
            organization: "",
            common_name: name,
            not_before: "261009180232Z",
            not_after: "20561120180232Z",
            authority_key_identifier: Some(&DUMMY_AUTHORITY),
        },
    )
    .expect("fixed test certificate parameters must encode")
    .to_der()
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

    #[test]
    fn certificate_fixture() {
        let cert = certificate();
        let replacement = replacement_certificate();
        assert_eq!(cert, certificate());
        assert_eq!(replacement, replacement_certificate());
        assert_ne!(cert, replacement);

        let original = X509Certificate::from_der(&cert).unwrap();
        let replaced = X509Certificate::from_der(&replacement).unwrap();
        assert!(replaced.verify(&original.public_key().unwrap()).unwrap());
        assert_eq!(
            original
                .public_key()
                .unwrap()
                .rsa()
                .unwrap()
                .to_components()
                .unwrap(),
            replaced
                .public_key()
                .unwrap()
                .rsa()
                .unwrap()
                .to_components()
                .unwrap(),
        );
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
