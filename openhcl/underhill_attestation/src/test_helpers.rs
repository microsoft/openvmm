// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Helpers for unit tests.

#![cfg(test)]

use crate::jwt::JwtAlgorithm;
use crate::jwt::JwtHeader;
use base64::Engine;
use crypto::rsa::RsaKeyPair;
use crypto::x509::X509Certificate;
use guid::Guid;
use openhcl_attestation_protocol::igvm_attest::akv;
use openhcl_attestation_protocol::vmgs::DEK_BUFFER_SIZE;
use openhcl_attestation_protocol::vmgs::DekKp;
use openhcl_attestation_protocol::vmgs::GSP_BUFFER_SIZE;
use openhcl_attestation_protocol::vmgs::GspKp;
use openhcl_attestation_protocol::vmgs::KeyProtector;
use openhcl_attestation_protocol::vmgs::NUMBER_KP;
use tee_call::GetAttestationReportResult;
use tee_call::HW_DERIVED_KEY_LENGTH;
use tee_call::REPORT_DATA_SIZE;
use tee_call::TeeCall;
use tee_call::TeeCallGetDerivedKey;
use tee_call::TeeType;

pub const CIPHERTEXT: &str = "test";

/// Construct a [`KeyProtector`] populated with deterministic, easily
/// distinguishable byte patterns for each KP slot.
///
/// The caller supplies `active_kp` because different tests want different
/// starting states: the `lib.rs` orchestration tests start from a fresh
/// slot 0, while the `vmgs.rs` round-trip tests use `u32::MAX` to verify
/// that the field is preserved verbatim across serialization.
pub fn new_key_protector(active_kp: u32) -> KeyProtector {
    // Ingress and egress KPs are assumed to be the only two KPs, therefore `NUMBER_KP` should be 2
    assert_eq!(NUMBER_KP, 2);

    let ingress_dek = DekKp {
        dek_buffer: [1; DEK_BUFFER_SIZE],
    };
    let egress_dek = DekKp {
        dek_buffer: [2; DEK_BUFFER_SIZE],
    };
    let ingress_gsp = GspKp {
        gsp_length: GSP_BUFFER_SIZE as u32,
        gsp_buffer: [3; GSP_BUFFER_SIZE],
    };
    let egress_gsp = GspKp {
        gsp_length: GSP_BUFFER_SIZE as u32,
        gsp_buffer: [4; GSP_BUFFER_SIZE],
    };
    KeyProtector {
        dek: [ingress_dek, egress_dek],
        gsp: [ingress_gsp, egress_gsp],
        active_kp,
    }
}

/// Construct a [`KeyProtectorById`] for tests.
///
/// When `found_id` is true, returns [`KeyProtectorById::Found`] populated
/// with the supplied `id_guid`/`ported` (or sensible defaults). When
/// `found_id` is false, returns [`KeyProtectorById::NotFound`] regardless
/// of the other arguments — modelling a VMGS that has never had a per-VM
/// key protector entry written.
///
/// [`KeyProtectorById`]: crate::KeyProtectorById
/// [`KeyProtectorById::Found`]: crate::KeyProtectorById::Found
/// [`KeyProtectorById::NotFound`]: crate::KeyProtectorById::NotFound
pub fn new_key_protector_by_id(
    id_guid: Option<Guid>,
    ported: Option<u8>,
    found_id: bool,
) -> crate::KeyProtectorById {
    if found_id {
        crate::KeyProtectorById::Found(openhcl_attestation_protocol::vmgs::KeyProtectorById {
            id_guid: id_guid.unwrap_or_else(Guid::new_random),
            ported: ported.unwrap_or(0),
            pad: [0; 3],
        })
    } else {
        crate::KeyProtectorById::NotFound
    }
}

/// Generate a self-signed X.509 certificate for testing.
pub fn generate_x509(key_pair: &RsaKeyPair) -> X509Certificate {
    X509Certificate::build_self_signed(
        key_pair,
        "US",
        "Washington",
        "Redmond",
        "Example INC",
        "example.com",
    )
    .unwrap()
}

/// Generate an X.509 certificate chain for testing.
/// The chain consists of three certificates: cert, intermediate, and root.
/// All certs are signed by the same private key and have the same subject and issuer.
fn generate_x5c(key_pair: &RsaKeyPair) -> Vec<String> {
    let cert = generate_x509(key_pair);
    let intermediate = generate_x509(key_pair);
    let root = generate_x509(key_pair);

    let base64_cert = base64::engine::general_purpose::STANDARD.encode(cert.to_der().unwrap());
    let base64_intermediate =
        base64::engine::general_purpose::STANDARD.encode(intermediate.to_der().unwrap());
    let base64_root = base64::engine::general_purpose::STANDARD.encode(root.to_der().unwrap());

    vec![base64_cert, base64_intermediate, base64_root]
}

/// Generate the base64 encoded components of a JWT.
pub fn generate_base64_encoded_jwt_components(key_pair: &RsaKeyPair) -> (String, String, String) {
    let header = JwtHeader {
        alg: JwtAlgorithm::RS256,
        x5c: generate_x5c(key_pair),
    };
    // Header is a base64-url encoded JSON object
    let base64_header = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .encode(serde_json::to_string(&header).unwrap());

    let key_hsm = akv::AkvKeyReleaseKeyBlob {
        ciphertext: CIPHERTEXT.as_bytes().to_vec(),
    };

    let body = akv::AkvKeyReleaseJwtBody {
        response: akv::AkvKeyReleaseResponse {
            key: akv::AkvKeyReleaseKeyObject {
                key: akv::AkvJwk {
                    key_hsm: serde_json::to_string(&key_hsm).unwrap().as_bytes().to_vec(),
                },
            },
        },
    };
    // Body is a base64-url encoded JSON object
    let base64_body = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .encode(serde_json::to_string(&body).unwrap().as_bytes());

    // The signature is generated by signing the concatenation of base64_header and base64_body
    let message = format!("{}.{}", base64_header, base64_body);
    let signature = key_pair
        .pkcs1_sign(message.as_bytes(), crypto::HashAlgorithm::Sha256)
        .unwrap();
    let base64_signature = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(&signature);

    (base64_header, base64_body, base64_signature)
}

/// Mock implementation of [`TeeCall`] with get derived key support for testing purposes
pub struct MockTeeCall {
    /// Mock TCB version to return from get_attestation_report
    pub tcb_version: u64,
}

impl MockTeeCall {
    /// Create a new instance of [`MockTeeCall`].
    pub fn new(tcb_version: u64) -> Self {
        Self { tcb_version }
    }
}

impl TeeCall for MockTeeCall {
    fn get_attestation_report(
        &self,
        report_data: &[u8; REPORT_DATA_SIZE],
    ) -> Result<GetAttestationReportResult, tee_call::Error> {
        let mut report = [0x6c; openhcl_attestation_protocol::igvm_attest::get::SNP_VM_REPORT_SIZE];
        report[..REPORT_DATA_SIZE].copy_from_slice(report_data);

        Ok(GetAttestationReportResult {
            report: report.to_vec(),
            tcb_version: Some(self.tcb_version),
        })
    }

    fn supports_get_derived_key(&self) -> Option<&dyn TeeCallGetDerivedKey> {
        Some(self)
    }

    fn tee_type(&self) -> TeeType {
        // Use Snp for testing
        TeeType::Snp
    }
}

impl TeeCallGetDerivedKey for MockTeeCall {
    fn get_derived_key(&self, tcb_version: u64) -> Result<[u8; 32], tee_call::Error> {
        // Base test key; mix in policy so different policies yield different derived secrets
        let mut key: [u8; HW_DERIVED_KEY_LENGTH] = [0xab; HW_DERIVED_KEY_LENGTH];

        // Use mutation to simulate the policy
        let tcb = tcb_version.to_le_bytes();
        for (i, b) in key.iter_mut().enumerate() {
            *b ^= tcb[i % tcb.len()];
        }

        Ok(key)
    }
}

/// Mock implementation of [`TeeCall`] without get derived key support for testing purposes
pub struct MockTeeCallNoGetDerivedKey;

impl TeeCall for MockTeeCallNoGetDerivedKey {
    fn get_attestation_report(
        &self,
        report_data: &[u8; REPORT_DATA_SIZE],
    ) -> Result<GetAttestationReportResult, tee_call::Error> {
        let mut report = [0x6c; openhcl_attestation_protocol::igvm_attest::get::SNP_VM_REPORT_SIZE];
        report[..REPORT_DATA_SIZE].copy_from_slice(report_data);

        Ok(GetAttestationReportResult {
            report: report.to_vec(),
            tcb_version: None,
        })
    }

    fn supports_get_derived_key(&self) -> Option<&dyn TeeCallGetDerivedKey> {
        None
    }

    fn tee_type(&self) -> TeeType {
        // Use Snp for testing
        TeeType::Snp
    }
}
