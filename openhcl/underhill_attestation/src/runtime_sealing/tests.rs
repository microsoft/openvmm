// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use super::*;
use openhcl_attestation_protocol::igvm_attest::get::runtime_claims::AttestationTpmVersion;
use parking_lot::Mutex;
use tee_call::GetAttestationReportResult;
use tee_call::HW_DERIVED_KEY_LENGTH;
use test_with_tracing::test;
use zerocopy::FromBytes;

const DEK: [u8; 32] = [0xab; 32];
const SNP_SVN: KeyDerivationSvn = KeyDerivationSvn::Snp { tcb_version: 7 };
const TDX_SVN: KeyDerivationSvn = KeyDerivationSvn::Tdx {
    tee_tcb_svn: [3; 16],
    cpu_svn: [5; 16],
};

// Deliberately not Debug: this state includes a mock hardware secret.
struct HardwareState {
    secret: [u8; 32],
    report_svn: Option<KeyDerivationSvn>,
    unavailable_svn: Option<KeyDerivationSvn>,
    fail_report: bool,
    fail_derivation: bool,
    report_calls: usize,
    derivations: Vec<KeyDerivationPolicy>,
}

struct MutableTee {
    tee_type: TeeType,
    supports_derivation: bool,
    state: Mutex<HardwareState>,
}

impl MutableTee {
    fn new(svn: KeyDerivationSvn) -> Self {
        Self {
            tee_type: match svn {
                KeyDerivationSvn::Snp { .. } => TeeType::Snp,
                KeyDerivationSvn::Tdx { .. } => TeeType::Tdx,
            },
            supports_derivation: true,
            state: Mutex::new(HardwareState {
                secret: [0x42; 32],
                report_svn: Some(svn),
                unavailable_svn: None,
                fail_report: false,
                fail_derivation: false,
                report_calls: 0,
                derivations: Vec::new(),
            }),
        }
    }
}

fn svn_bytes(svn: KeyDerivationSvn) -> [u8; 33] {
    let mut bytes = [0; 33];
    match svn {
        KeyDerivationSvn::Snp { tcb_version } => {
            bytes[1..9].copy_from_slice(&tcb_version.to_le_bytes());
        }
        KeyDerivationSvn::Tdx {
            tee_tcb_svn,
            cpu_svn,
        } => {
            bytes[0] = 1;
            bytes[1..17].copy_from_slice(&tee_tcb_svn);
            bytes[17..].copy_from_slice(&cpu_svn);
        }
    }
    bytes
}

impl TeeCall for MutableTee {
    fn get_attestation_report(
        &self,
        report_data: &[u8; REPORT_DATA_SIZE],
    ) -> Result<GetAttestationReportResult, tee_call::Error> {
        assert_eq!(report_data, &[0; REPORT_DATA_SIZE]);
        let mut state = self.state.lock();
        state.report_calls += 1;
        if state.fail_report {
            return Err(tee_call::Error::AllZeroKey);
        }
        Ok(GetAttestationReportResult {
            report: Vec::new(),
            key_derivation_svn: state.report_svn,
        })
    }

    fn supports_get_derived_key(&self) -> Option<&dyn TeeCallGetDerivedKey> {
        self.supports_derivation
            .then_some(self as &dyn TeeCallGetDerivedKey)
    }

    fn tee_type(&self) -> TeeType {
        match self.tee_type {
            TeeType::Snp => TeeType::Snp,
            TeeType::Tdx => TeeType::Tdx,
            TeeType::Cca => TeeType::Cca,
            TeeType::Vbs => TeeType::Vbs,
        }
    }
}

impl TeeCallGetDerivedKey for MutableTee {
    fn get_derived_key(
        &self,
        policy: KeyDerivationPolicy,
    ) -> Result<[u8; HW_DERIVED_KEY_LENGTH], tee_call::Error> {
        let mut state = self.state.lock();
        state.derivations.push(policy);
        if !svn_matches_tee(policy.svn, self.tee_type()) {
            return Err(tee_call::Error::KeyDerivationSvnMismatch);
        }
        if state.fail_derivation
            || state
                .unavailable_svn
                .is_some_and(|svn| svn_bytes(svn) == svn_bytes(policy.svn))
        {
            return Err(tee_call::Error::AllZeroKey);
        }
        let mut context = svn_bytes(policy.svn).to_vec();
        context.push(u8::from(policy.mix_measurement));
        // Model independently changing hardware identity with unchanged SVN and
        // measurement. Do not derive the secret solely from the requested SVN.
        Ok(crypto::hmac_sha_256::hmac_sha_256(&state.secret, &context).unwrap())
    }
}

fn config(hardware_sealing_policy: HardwareSealingPolicy) -> AttestationVmConfig {
    AttestationVmConfig {
        current_time: None,
        root_cert_thumbprint: String::new(),
        console_enabled: false,
        interactive_console_enabled: false,
        secure_boot: false,
        tpm_enabled: false,
        tpm_version: AttestationTpmVersion::V138,
        tpm_persisted: false,
        hardware_sealing_policy,
        filtered_vpci_devices_allowed: true,
        vm_unique_id: String::new(),
        vmgs_provisioner: None,
    }
}

fn migration_reseals_unchanged_dek(svn: KeyDerivationSvn, policy: HardwareSealingPolicy) {
    let tee = MutableTee::new(svn);
    let config = config(policy);
    let source = create_protector(&tee, &config, &DEK).unwrap();
    assert!(protector_matches(&tee, &config, &source, &DEK).unwrap());
    assert!(!protector_matches(&tee, &config, &source, &[0xcd; 32]).unwrap());

    // Same object, same SVN, same policy, but a new destination hardware secret.
    tee.state.lock().secret = [0x73; 32];
    assert!(!protector_matches(&tee, &config, &source, &DEK).unwrap());
    let destination = create_protector(&tee, &config, &DEK).unwrap();
    assert!(protector_matches(&tee, &config, &destination, &DEK).unwrap());
    assert!(!protector_matches(&tee, &config, &destination, &[0xcd; 32]).unwrap());
    let protector = parse_hardware_key_protector(&destination).unwrap();
    assert_eq!(protector.version(), vmgs::HW_KEY_PROTECTOR_VERSION_3);
    assert_eq!(destination.len(), vmgs::HW_KEY_PROTECTOR_V3_SIZE);
    assert_eq!(
        svn_bytes(validated_policy(&protector).unwrap().svn),
        svn_bytes(svn)
    );

    tee.state.lock().secret = [0x42; 32];
    assert!(!protector_matches(&tee, &config, &destination, &DEK).unwrap());
    assert!(protector_matches(&tee, &config, &source, &DEK).unwrap());
}

#[test]
fn snp_hash_migration_reseals_unchanged_dek() {
    migration_reseals_unchanged_dek(SNP_SVN, HardwareSealingPolicy::Hash);
}

#[test]
fn snp_signer_migration_reseals_unchanged_dek() {
    migration_reseals_unchanged_dek(SNP_SVN, HardwareSealingPolicy::Signer);
}

#[test]
fn tdx_hash_migration_reseals_unchanged_dek() {
    migration_reseals_unchanged_dek(TDX_SVN, HardwareSealingPolicy::Hash);
}

#[test]
fn verification_rederives_stored_svn_and_creation_fetches_current_svn() {
    for (source_svn, destination_svn) in [
        (SNP_SVN, KeyDerivationSvn::Snp { tcb_version: 9 }),
        (
            TDX_SVN,
            KeyDerivationSvn::Tdx {
                tee_tcb_svn: [4; 16],
                cpu_svn: [6; 16],
            },
        ),
    ] {
        let tee = MutableTee::new(source_svn);
        let config = config(HardwareSealingPolicy::Hash);
        let source = create_protector(&tee, &config, &DEK).unwrap();
        {
            let mut state = tee.state.lock();
            state.report_svn = Some(destination_svn);
            state.fail_report = true;
        }
        for _ in 0..3 {
            assert!(protector_matches(&tee, &config, &source, &DEK).unwrap());
        }
        {
            let state = tee.state.lock();
            assert_eq!(state.report_calls, 1);
            assert_eq!(state.derivations.len(), 4);
            assert!(state.derivations.iter().all(|policy| {
                svn_bytes(policy.svn) == svn_bytes(source_svn) && policy.mix_measurement
            }));
        }
        assert!(matches!(
            create_protector(&tee, &config, &DEK),
            Err(Error(ErrorInner::Report(_)))
        ));
        tee.state.lock().fail_report = false;
        let destination = create_protector(&tee, &config, &DEK).unwrap();
        let protector = parse_hardware_key_protector(&destination).unwrap();
        assert_eq!(
            svn_bytes(validated_policy(&protector).unwrap().svn),
            svn_bytes(destination_svn)
        );
        assert!(protector_matches(&tee, &config, &destination, &DEK).unwrap());
        assert_eq!(tee.state.lock().report_calls, 3);
    }
}

#[test]
fn unavailable_source_svn_is_error_but_current_svn_can_reseal() {
    for (source_svn, destination_svn) in [
        (SNP_SVN, KeyDerivationSvn::Snp { tcb_version: 6 }),
        (
            TDX_SVN,
            KeyDerivationSvn::Tdx {
                tee_tcb_svn: [2; 16],
                cpu_svn: [4; 16],
            },
        ),
    ] {
        let tee = MutableTee::new(source_svn);
        let config = config(HardwareSealingPolicy::Hash);
        let source = create_protector(&tee, &config, &DEK).unwrap();
        {
            let mut state = tee.state.lock();
            state.secret = [0x73; 32];
            state.report_svn = Some(destination_svn);
            state.unavailable_svn = Some(source_svn);
        }
        assert!(matches!(
            protector_matches(&tee, &config, &source, &DEK),
            Err(Error(ErrorInner::Derive(
                HardwareDerivedKeysError::InitializeHardwareSecret(_)
            )))
        ));
        let destination = create_protector(&tee, &config, &DEK).unwrap();
        assert!(protector_matches(&tee, &config, &destination, &DEK).unwrap());
    }
}

#[test]
fn missing_or_incompatible_report_svn_is_error() {
    for svn in [SNP_SVN, TDX_SVN] {
        let tee = MutableTee::new(svn);
        let config = config(HardwareSealingPolicy::Hash);
        tee.state.lock().report_svn = None;
        assert!(matches!(
            create_protector(&tee, &config, &DEK),
            Err(Error(ErrorInner::MissingKeyDerivationSvn))
        ));
        tee.state.lock().report_svn = Some(match svn {
            KeyDerivationSvn::Snp { .. } => TDX_SVN,
            KeyDerivationSvn::Tdx { .. } => SNP_SVN,
        });
        assert!(matches!(
            create_protector(&tee, &config, &DEK),
            Err(Error(ErrorInner::ReportSvnMismatch))
        ));
        assert!(tee.state.lock().derivations.is_empty());
    }
}

#[test]
fn transient_derivation_failure_is_error_on_both_paths() {
    let tee = MutableTee::new(SNP_SVN);
    let config = config(HardwareSealingPolicy::Hash);
    let protector = create_protector(&tee, &config, &DEK).unwrap();
    tee.state.lock().fail_derivation = true;
    assert!(matches!(
        protector_matches(&tee, &config, &protector, &DEK),
        Err(Error(ErrorInner::Derive(_)))
    ));
    assert!(matches!(
        create_protector(&tee, &config, &DEK),
        Err(Error(ErrorInner::Derive(_)))
    ));
    tee.state.lock().fail_derivation = false;
    assert!(protector_matches(&tee, &config, &protector, &DEK).unwrap());
}

#[test]
fn unsupported_policy_and_tee_are_rejected_without_hardware_calls() {
    let mut cases = Vec::new();
    for svn in [SNP_SVN, TDX_SVN] {
        cases.push((MutableTee::new(svn), HardwareSealingPolicy::None));
        let mut tee = MutableTee::new(svn);
        tee.supports_derivation = false;
        cases.push((tee, HardwareSealingPolicy::Hash));
    }
    cases.push((MutableTee::new(TDX_SVN), HardwareSealingPolicy::Signer));
    for tee_type in [TeeType::Cca, TeeType::Vbs] {
        let mut tee = MutableTee::new(SNP_SVN);
        tee.tee_type = tee_type;
        cases.push((tee, HardwareSealingPolicy::Hash));
    }
    for (tee, policy) in cases {
        let config = config(policy);
        let check_error = |err: Error| {
            assert!(matches!(
                err,
                Error(
                    ErrorInner::DisabledPolicy
                        | ErrorInner::TdxSignerPolicyUnsupported
                        | ErrorInner::UnsupportedTee
                )
            ));
        };
        check_error(create_protector(&tee, &config, &DEK).unwrap_err());
        check_error(protector_matches(&tee, &config, &[], &DEK).unwrap_err());
        let state = tee.state.lock();
        assert_eq!(state.report_calls, 0);
        assert!(state.derivations.is_empty());
    }
}

#[test]
fn malformed_and_unknown_v3_formats_do_not_reach_hardware() {
    for svn in [SNP_SVN, TDX_SVN] {
        let tee = MutableTee::new(svn);
        let config = config(HardwareSealingPolicy::Hash);
        let valid = create_protector(&tee, &config, &DEK).unwrap();
        let mut invalid = vec![Vec::new(), vec![0; vmgs::HW_KEY_PROTECTOR_SIZE]];
        for size in 1..valid.len() {
            invalid.push(valid[..size].to_vec());
        }
        let mut oversized = valid.clone();
        oversized.push(0);
        invalid.push(oversized);
        for version in [0, 1, 2, 4, u32::MAX] {
            let mut p = vmgs::HardwareKeyProtectorV3::read_from_bytes(&valid).unwrap();
            p.header.version = version;
            invalid.push(p.as_bytes().to_vec());
        }
        for length in [0, 1, vmgs::HW_KEY_PROTECTOR_SIZE as u32, u32::MAX] {
            let mut p = vmgs::HardwareKeyProtectorV3::read_from_bytes(&valid).unwrap();
            p.header.length = length;
            invalid.push(p.as_bytes().to_vec());
        }
        let mut p = vmgs::HardwareKeyProtectorV3::read_from_bytes(&valid).unwrap();
        p.header.tee_type = u32::MAX;
        invalid.push(p.as_bytes().to_vec());
        let mut p = vmgs::HardwareKeyProtectorV3::read_from_bytes(&valid).unwrap();
        p.header.mix_measurement = 2;
        invalid.push(p.as_bytes().to_vec());
        let mut p = vmgs::HardwareKeyProtectorV3::read_from_bytes(&valid).unwrap();
        p.header._reserved[0] = 1;
        invalid.push(p.as_bytes().to_vec());
        if matches!(svn, KeyDerivationSvn::Snp { .. }) {
            let mut p = vmgs::HardwareKeyProtectorV3::read_from_bytes(&valid).unwrap();
            p.header.svn[8] = 1;
            invalid.push(p.as_bytes().to_vec());
        }
        for bytes in invalid {
            assert!(!protector_matches(&tee, &config, &bytes, &DEK).unwrap());
        }
        assert_eq!(tee.state.lock().derivations.len(), 1);
    }
}

#[test]
fn corrupted_authenticated_bytes_are_mismatches() {
    for svn in [SNP_SVN, TDX_SVN] {
        let tee = MutableTee::new(svn);
        let config = config(HardwareSealingPolicy::Hash);
        let valid = create_protector(&tee, &config, &DEK).unwrap();
        for offset in [
            std::mem::offset_of!(vmgs::HardwareKeyProtectorHeaderV3, svn),
            std::mem::offset_of!(vmgs::HardwareKeyProtectorV3, iv),
            std::mem::offset_of!(vmgs::HardwareKeyProtectorV3, ciphertext),
            std::mem::offset_of!(vmgs::HardwareKeyProtectorV3, hmac),
        ] {
            let mut corrupted = valid.clone();
            corrupted[offset] ^= 1;
            assert!(!protector_matches(&tee, &config, &corrupted, &DEK).unwrap());
        }
        assert_eq!(tee.state.lock().derivations.len(), 5);
    }
}

#[test]
fn stored_tee_or_policy_mismatch_does_not_derive() {
    let snp = MutableTee::new(SNP_SVN);
    let tdx = MutableTee::new(TDX_SVN);
    let hash = config(HardwareSealingPolicy::Hash);
    let signer = config(HardwareSealingPolicy::Signer);
    let snp_hash = create_protector(&snp, &hash, &DEK).unwrap();
    let snp_signer = create_protector(&snp, &signer, &DEK).unwrap();
    let tdx_hash = create_protector(&tdx, &hash, &DEK).unwrap();
    assert!(!protector_matches(&snp, &hash, &tdx_hash, &DEK).unwrap());
    assert!(!protector_matches(&tdx, &hash, &snp_hash, &DEK).unwrap());
    assert!(!protector_matches(&snp, &hash, &snp_signer, &DEK).unwrap());
    assert!(!protector_matches(&snp, &signer, &snp_hash, &DEK).unwrap());
    assert_eq!(snp.state.lock().derivations.len(), 2);
    assert_eq!(tdx.state.lock().derivations.len(), 1);
}

#[test]
fn changed_vm_configuration_is_mismatch() {
    let tee = MutableTee::new(SNP_SVN);
    let mut config = config(HardwareSealingPolicy::Hash);
    let protector = create_protector(&tee, &config, &DEK).unwrap();
    config.secure_boot = !config.secure_boot;
    assert!(!protector_matches(&tee, &config, &protector, &DEK).unwrap());
}

/// Build a legacy fixture using the same keys and ciphertext as v3, but an
/// independently authenticated v2 header. Production only creates v3 blobs.
fn legacy_protector(tee: &MutableTee, config: &AttestationVmConfig) -> vmgs::HardwareKeyProtector {
    let bytes = create_protector(tee, config, &DEK).unwrap();
    let v3 = vmgs::HardwareKeyProtectorV3::read_from_bytes(&bytes).unwrap();
    let policy = validated_policy(&parse_hardware_key_protector(&bytes).unwrap()).unwrap();
    let KeyDerivationSvn::Snp { tcb_version } = policy.svn else {
        panic!("legacy protectors are SNP-only");
    };
    let secret = tee.get_derived_key(policy).unwrap();
    let context = serde_json::to_string(config).unwrap();
    let keys = crypto::kbkdf::kbkdf_hmac_sha256(
        &secret,
        context.as_bytes(),
        b"ISOHWKEY",
        vmgs::AES_CBC_KEY_LENGTH + vmgs::HMAC_SHA_256_KEY_LENGTH,
    )
    .unwrap();
    let mut legacy = vmgs::HardwareKeyProtector {
        header: vmgs::HardwareKeyProtectorHeader::new(
            vmgs::HW_KEY_PROTECTOR_VERSION_2,
            vmgs::HW_KEY_PROTECTOR_SIZE as u32,
            tcb_version,
            u8::from(policy.mix_measurement),
        ),
        iv: v3.iv,
        ciphertext: v3.ciphertext,
        hmac: [0; vmgs::HMAC_SHA_256_KEY_LENGTH],
    };
    legacy.hmac = crypto::hmac_sha_256::hmac_sha_256(
        &keys[vmgs::AES_CBC_KEY_LENGTH..],
        &legacy.as_bytes()[..std::mem::offset_of!(vmgs::HardwareKeyProtector, hmac)],
    )
    .unwrap();
    legacy
}

#[test]
fn legacy_v2_snp_verifies_and_recreates_as_v3() {
    for policy in [HardwareSealingPolicy::Hash, HardwareSealingPolicy::Signer] {
        let tee = MutableTee::new(SNP_SVN);
        let config = config(policy);
        let mut legacy = legacy_protector(&tee, &config);
        assert!(protector_matches(&tee, &config, legacy.as_bytes(), &DEK).unwrap());
        assert!(!protector_matches(&tee, &config, legacy.as_bytes(), &[0xcd; 32]).unwrap());
        legacy.hmac[0] ^= 1;
        assert!(!protector_matches(&tee, &config, legacy.as_bytes(), &DEK).unwrap());
        legacy.hmac[0] ^= 1;
        tee.state.lock().secret = [0x73; 32];
        assert!(!protector_matches(&tee, &config, legacy.as_bytes(), &DEK).unwrap());
        let replacement = create_protector(&tee, &config, &DEK).unwrap();
        assert!(matches!(
            parse_hardware_key_protector(&replacement).unwrap(),
            HwKeyProtector::V3(_)
        ));
        assert!(protector_matches(&tee, &config, &replacement, &DEK).unwrap());
    }
}

#[test]
fn legacy_v1_unknown_malformed_or_cross_tee_protectors_do_not_derive() {
    let tee = MutableTee::new(SNP_SVN);
    let config = config(HardwareSealingPolicy::Hash);
    let mut legacy = legacy_protector(&tee, &config);
    let tdx = MutableTee::new(TDX_SVN);
    assert!(!protector_matches(&tdx, &config, legacy.as_bytes(), &DEK).unwrap());
    assert!(tdx.state.lock().derivations.is_empty());
    let calls = tee.state.lock().derivations.len();
    for version in [0, vmgs::HW_KEY_PROTECTOR_VERSION_1, 3, u32::MAX] {
        legacy.header.version = version;
        assert!(!protector_matches(&tee, &config, legacy.as_bytes(), &DEK).unwrap());
    }
    legacy.header.version = vmgs::HW_KEY_PROTECTOR_VERSION_2;
    legacy.header.length = vmgs::HW_KEY_PROTECTOR_V3_SIZE as u32;
    assert!(!protector_matches(&tee, &config, legacy.as_bytes(), &DEK).unwrap());
    legacy.header.length = vmgs::HW_KEY_PROTECTOR_SIZE as u32;
    legacy.header.mix_measurement = 2;
    assert!(!protector_matches(&tee, &config, legacy.as_bytes(), &DEK).unwrap());
    legacy.header.mix_measurement = 1;
    legacy.header._reserved[0] = 1;
    assert!(!protector_matches(&tee, &config, legacy.as_bytes(), &DEK).unwrap());
    assert_eq!(tee.state.lock().derivations.len(), calls);
}

#[test]
fn key_debug_is_redacted_and_errors_have_no_key_material() {
    let tee = MutableTee::new(SNP_SVN);
    let config = config(HardwareSealingPolicy::Hash);
    let keys = HardwareDerivedKeys::derive_key(
        &tee,
        &config,
        KeyDerivationPolicy {
            svn: SNP_SVN,
            mix_measurement: true,
        },
    )
    .unwrap();
    let debug = format!("{keys:?}");
    assert!(debug.contains("aes_key: \"[redacted]\""));
    assert!(debug.contains("hmac_key: \"[redacted]\""));
    tee.state.lock().fail_derivation = true;
    let err = create_protector(&tee, &config, &DEK).unwrap_err();
    assert_eq!(
        format!("{err:?}"),
        "Error(Derive(InitializeHardwareSecret(AllZeroKey)))"
    );
    assert_eq!(
        err.to_string(),
        "failed to freshly derive hardware sealing keys"
    );
}
