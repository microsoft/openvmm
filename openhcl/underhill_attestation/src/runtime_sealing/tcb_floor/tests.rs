// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use super::*;
use openhcl_attestation_protocol::igvm_attest::get::runtime_claims::AttestationTpmVersion;
use openhcl_attestation_protocol::igvm_attest::get::runtime_claims::HardwareSealingPolicy;
use openhcl_attestation_protocol::vmgs;
use parking_lot::Mutex;
use tee_call::GetAttestationReportResult;
use tee_call::HW_DERIVED_KEY_LENGTH;
use tee_call::KeyDerivationPolicy;
use tee_call::TeeCallGetDerivedKey;
use tee_call::TeeType;
use test_with_tracing::test;
use zerocopy::FromBytes;
use zerocopy::IntoBytes;

const DEK: [u8; 32] = [0xab; 32];
const BASE_TCB: [u8; 8] = [2, 2, 0, 0, 0, 0, 2, 2];
const TDX_SVN: KeyDerivationSvn = KeyDerivationSvn::Tdx {
    tee_tcb_svn: [3; 16],
    cpu_svn: [5; 16],
};

// Do not derive Debug: state contains a mock hardware secret.
struct State {
    report: Vec<u8>,
    svn: Option<KeyDerivationSvn>,
    secret: [u8; 32],
    fail_report: bool,
    fail_derivation: bool,
    // Change the next observation immediately after returning this report.
    after_report: Option<KeyDerivationSvn>,
    report_calls: usize,
    derivations: Vec<KeyDerivationPolicy>,
}

struct MutableTee {
    is_tdx: bool,
    supports_derivation: bool,
    state: Mutex<State>,
}

impl MutableTee {
    fn snp(version: u32, family: u8, model: u8, tcb: [u8; 8]) -> Self {
        // A full report-sized fixture with the ABI's actual field offsets.
        // No signature is needed: the mock models the trusted local interface,
        // not the reception or authentication of a remote attestation report.
        let mut report = vec![0; 1184];
        report[..4].copy_from_slice(&version.to_le_bytes());
        report[0x180..0x188].copy_from_slice(&tcb);
        report[0x188] = family;
        report[0x189] = model;
        Self {
            is_tdx: false,
            supports_derivation: true,
            state: Mutex::new(State {
                report,
                svn: Some(snp_svn(tcb)),
                secret: [0x42; 32],
                fail_report: false,
                fail_derivation: false,
                after_report: None,
                report_calls: 0,
                derivations: Vec::new(),
            }),
        }
    }

    fn tdx() -> Self {
        let mut tee = Self::snp(3, 0x19, 0, BASE_TCB);
        tee.is_tdx = true;
        tee.set_svn(TDX_SVN);
        tee
    }

    fn set_svn(&self, svn: KeyDerivationSvn) {
        self.state.lock().set_svn(svn);
    }
}

impl State {
    fn set_svn(&mut self, svn: KeyDerivationSvn) {
        self.svn = Some(svn);
        if let KeyDerivationSvn::Snp { tcb_version } = svn {
            self.report[0x180..0x188].copy_from_slice(&tcb_version.to_le_bytes());
        }
    }
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
        let result = GetAttestationReportResult {
            report: state.report.clone(),
            key_derivation_svn: state.svn,
        };
        if let Some(svn) = state.after_report.take() {
            state.set_svn(svn);
        }
        Ok(result)
    }

    fn supports_get_derived_key(&self) -> Option<&dyn TeeCallGetDerivedKey> {
        self.supports_derivation
            .then_some(self as &dyn TeeCallGetDerivedKey)
    }

    fn tee_type(&self) -> TeeType {
        if self.is_tdx {
            TeeType::Tdx
        } else {
            TeeType::Snp
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
        if state.fail_derivation {
            return Err(tee_call::Error::AllZeroKey);
        }
        let mut context = Vec::new();
        match policy.svn {
            KeyDerivationSvn::Snp { tcb_version } => {
                context.push(0);
                context.extend_from_slice(&tcb_version.to_le_bytes());
            }
            KeyDerivationSvn::Tdx {
                tee_tcb_svn,
                cpu_svn,
            } => {
                context.push(1);
                context.extend_from_slice(&tee_tcb_svn);
                context.extend_from_slice(&cpu_svn);
            }
        }
        context.push(u8::from(policy.mix_measurement));
        // Deliberately permit old SVNs: the floor, not this mock's key service,
        // must prevent rollback and reject stale candidate headers.
        Ok(crypto::hmac_sha_256::hmac_sha_256(&state.secret, &context).unwrap())
    }
}

fn snp_svn(tcb: [u8; 8]) -> KeyDerivationSvn {
    KeyDerivationSvn::Snp {
        tcb_version: u64::from_le_bytes(tcb),
    }
}

fn config() -> AttestationVmConfig {
    AttestationVmConfig {
        current_time: None,
        root_cert_thumbprint: String::new(),
        console_enabled: false,
        interactive_console_enabled: false,
        secure_boot: false,
        tpm_enabled: false,
        tpm_version: AttestationTpmVersion::V138,
        tpm_persisted: false,
        hardware_sealing_policy: HardwareSealingPolicy::Hash,
        filtered_vpci_devices_allowed: true,
        vm_unique_id: String::new(),
        vmgs_provisioner: None,
    }
}

fn header_svn(protector: &[u8]) -> KeyDerivationSvn {
    validated_policy(&parse_hardware_key_protector(protector).unwrap())
        .unwrap()
        .svn
}

fn assert_floor(floor: &RuntimeTcbFloor, svn: KeyDerivationSvn) {
    assert!(svn_equal(floor.snapshot.svn, svn));
}

#[test]
fn boot_collection_reuses_first_report_without_hardware_calls() {
    for tee in [MutableTee::snp(3, 0x19, 0, BASE_TCB), MutableTee::tdx()] {
        assert!(BootTcbFloor::new(Some(&tee), &config()).finish().is_none());
        let mut collector = BootTcbFloor::new(Some(&tee), &config());
        assert_eq!(tee.state.lock().report_calls, 0);
        let report = tee.get_attestation_report(&[0; REPORT_DATA_SIZE]).unwrap();
        // Any accidental fallback report/derivation would now fail.
        tee.state.lock().fail_report = true;
        tee.state.lock().fail_derivation = true;
        collector.observe(&tee, &report);
        let floor = collector.finish().unwrap();
        assert_floor(&floor, report.key_derivation_svn.unwrap());
        let state = tee.state.lock();
        assert_eq!(state.report_calls, 1);
        assert!(state.derivations.is_empty());
    }
}

#[test]
fn boot_collection_ratchets_across_attempts_and_equal_reports() {
    for tee in [MutableTee::snp(3, 0x19, 0, BASE_TCB), MutableTee::tdx()] {
        let mut collector = BootTcbFloor::new(Some(&tee), &config());
        for increment in [0, 1, 1, 2] {
            let svn = if tee.is_tdx {
                KeyDerivationSvn::Tdx {
                    tee_tcb_svn: [3; 16],
                    cpu_svn: [5 + increment; 16],
                }
            } else {
                let mut tcb = BASE_TCB;
                tcb[7] += increment;
                snp_svn(tcb)
            };
            tee.set_svn(svn);
            let report = tee.get_attestation_report(&[0; REPORT_DATA_SIZE]).unwrap();
            collector.observe(&tee, &report);
            assert_floor(collector.floor.as_ref().unwrap(), svn);
        }
        let floor = collector.finish().unwrap();
        let state = tee.state.lock();
        assert_floor(&floor, state.svn.unwrap());
        assert_eq!(state.report_calls, 4);
        assert!(state.derivations.is_empty());
    }
}

#[test]
fn boot_bad_observation_permanently_disables_export() {
    for first_good in [false, true] {
        // Missing SVN, wrong TEE SVN, truncation, raw TCB mismatch, domain
        // change, and lowering (even with another component increasing).
        for fault in 0..6 {
            let tee = MutableTee::snp(3, 0x19, 0, BASE_TCB);
            let good = tee.get_attestation_report(&[0; REPORT_DATA_SIZE]).unwrap();
            let mut collector = BootTcbFloor::new(Some(&tee), &config());
            if first_good {
                collector.observe(&tee, &good);
            }
            let mut bad = tee.get_attestation_report(&[0; REPORT_DATA_SIZE]).unwrap();
            match fault {
                0 => bad.key_derivation_svn = None,
                1 => bad.key_derivation_svn = Some(TDX_SVN),
                2 => bad.report.truncate(1183),
                3 => bad.report[0x180] ^= 1,
                4 => bad.report[0x189] += 1,
                5 => {
                    let mut tcb = BASE_TCB;
                    tcb[0] -= 1;
                    tcb[7] += 1;
                    bad.report[0x180..0x188].copy_from_slice(&tcb);
                    bad.key_derivation_svn = Some(snp_svn(tcb));
                }
                _ => unreachable!(),
            }
            // Domain/lowering faults require a previous observation.
            if !first_good && fault >= 4 {
                continue;
            }
            collector.observe(&tee, &bad);
            assert!(collector.invalid);
            if first_good {
                assert_floor(collector.floor.as_ref().unwrap(), snp_svn(BASE_TCB));
            }
            // A later valid report cannot undo invalidation or bootstrap anew.
            collector.observe(&tee, &good);
            assert!(collector.finish().is_none());
            let state = tee.state.lock();
            assert_eq!(state.report_calls, 2);
            assert!(state.derivations.is_empty());
        }
    }
}

#[test]
fn boot_unsupported_or_disabled_context_never_collects() {
    struct VbsTee;
    impl TeeCall for VbsTee {
        fn get_attestation_report(
            &self,
            _: &[u8; REPORT_DATA_SIZE],
        ) -> Result<GetAttestationReportResult, tee_call::Error> {
            panic!("collector must not request a report");
        }

        fn supports_get_derived_key(&self) -> Option<&dyn TeeCallGetDerivedKey> {
            None
        }

        fn tee_type(&self) -> TeeType {
            TeeType::Vbs
        }
    }

    let snp = MutableTee::snp(3, 0x19, 0, BASE_TCB);
    let tdx = MutableTee::tdx();
    let mut unavailable = MutableTee::snp(3, 0x19, 0, BASE_TCB);
    unavailable.supports_derivation = false;
    let mut disabled = config();
    disabled.hardware_sealing_policy = HardwareSealingPolicy::None;
    let mut signer = config();
    signer.hardware_sealing_policy = HardwareSealingPolicy::Signer;
    let config = config();
    let contexts: [(Option<&dyn TeeCall>, &AttestationVmConfig); 5] = [
        (None, &config),
        (Some(&VbsTee), &config),
        (Some(&unavailable), &config),
        (Some(&snp), &disabled),
        (Some(&tdx), &signer),
    ];
    for (tee, config) in contexts {
        let mut collector = BootTcbFloor::new(tee, config);
        assert!(!collector.enabled);
        if let Some(tee) = tee {
            collector.observe(
                tee,
                &GetAttestationReportResult {
                    report: Vec::new(),
                    key_derivation_svn: None,
                },
            );
        }
        assert!(!collector.invalid);
        assert!(collector.finish().is_none());
    }
    for tee in [&snp, &tdx, &unavailable] {
        let state = tee.state.lock();
        assert_eq!(state.report_calls, 0);
        assert!(state.derivations.is_empty());
    }
}

#[test]
fn initialization_fetches_one_report_without_deriving() {
    for tee in [MutableTee::snp(3, 0x19, 0, BASE_TCB), MutableTee::tdx()] {
        let floor = RuntimeTcbFloor::new(&tee, &config()).unwrap();
        let state = tee.state.lock();
        assert_eq!(state.report_calls, 1);
        assert!(state.derivations.is_empty());
        assert_floor(&floor, state.svn.unwrap());
    }
}

#[test]
fn snp_layout_selection_separates_report_support_from_cpu_encoding() {
    // Independent numeric expectations: catch gaps, endpoints, and accidental
    // widening of the named production ranges.
    for version in [0, 1, 2, 3, 4, 5, 6, u32::MAX] {
        assert_eq!(
            SnpDomain {
                version,
                cpuid: None
            }
            .tcb_layout(),
            None
        );
        for family in [0x18, 0x19, 0x1a, 0x1b] {
            for model in 0..=u8::MAX {
                let expected = match (version, family, model) {
                    (3..=5, 0x19, 0x00..=0x1f) => Some(SnpTcbLayout::Legacy),
                    (3..=5, 0x1a, 0x90..=0xaf | 0xc0..=0xcf) => Some(SnpTcbLayout::Turin),
                    _ => None,
                };
                let domain = SnpDomain {
                    version,
                    cpuid: Some([family, model]),
                };
                assert_eq!(domain.tcb_layout(), expected, "{domain:?}");
            }
        }
    }
}

#[test]
fn legacy_and_turin_equal_and_each_component_upgrade() {
    // Explicit ABI byte offsets, independent of the production layout types.
    // AMD 56860 revision 1.59 table 4 defines the two Turin model ranges.
    let layouts: &[(u8, u8, &[usize])] = &[
        (0x19, 0x00, &[0, 1, 6, 7]),
        (0x19, 0x0f, &[0, 1, 6, 7]),
        (0x19, 0x10, &[0, 1, 6, 7]),
        (0x19, 0x1f, &[0, 1, 6, 7]),
        (0x1a, 0x90, &[0, 1, 2, 3, 7]),
        (0x1a, 0xaf, &[0, 1, 2, 3, 7]),
        (0x1a, 0xc0, &[0, 1, 2, 3, 7]),
        (0x1a, 0xcf, &[0, 1, 2, 3, 7]),
    ];
    for version in [3, 4, 5] {
        for &(family, model, components) in layouts {
            let mut tcb = [0; 8];
            let tee = MutableTee::snp(version, family, model, tcb);
            let config = config();
            let mut floor = RuntimeTcbFloor::new(&tee, &config).unwrap();
            let equal = floor.create_protector(&tee, &config, &DEK).unwrap();
            assert!(floor.verify_protector(&tee, &config, &equal, &DEK).unwrap());
            for &component in components {
                tcb[component] += 1;
                tee.set_svn(snp_svn(tcb));
                let upgraded = floor.create_protector(&tee, &config, &DEK).unwrap();
                assert!(svn_equal(header_svn(&upgraded), snp_svn(tcb)));
                assert_floor(&floor, snp_svn(tcb));
                assert!(
                    floor
                        .verify_protector(&tee, &config, &upgraded, &DEK)
                        .unwrap()
                );
            }
        }
    }
}

#[test]
fn component_downgrades_rejected_even_with_larger_packed_number() {
    let layouts: &[(u8, u8, &[usize])] = &[
        (0x19, 0x00, &[0, 1, 6, 7]),
        (0x1a, 0x90, &[0, 1, 2, 3, 7]),
        (0x1a, 0xaf, &[0, 1, 2, 3, 7]),
        (0x1a, 0xc0, &[0, 1, 2, 3, 7]),
        (0x1a, 0xcf, &[0, 1, 2, 3, 7]),
    ];
    for &(family, model, components) in layouts {
        for &component in components {
            let mut base = [0; 8];
            for &index in components {
                base[index] = 2;
            }
            let tee = MutableTee::snp(3, family, model, base);
            let config = config();
            let mut floor = RuntimeTcbFloor::new(&tee, &config).unwrap();
            let mut lowered = base;
            lowered[component] -= 1;
            if component != 7 {
                lowered[7] += 1;
                assert!(u64::from_le_bytes(lowered) > u64::from_le_bytes(base));
            }
            tee.set_svn(snp_svn(lowered));
            assert!(matches!(
                floor.create_protector(&tee, &config, &DEK),
                Err(Error(ErrorInner::TcbLowered))
            ));
            assert!(matches!(
                floor.verify_protector(&tee, &config, &[], &DEK),
                Err(Error(ErrorInner::TcbLowered))
            ));
            assert_floor(&floor, snp_svn(base));
            assert!(tee.state.lock().derivations.is_empty());
        }
    }
}

#[test]
fn reserved_tcb_bytes_must_remain_equal() {
    let layouts: &[(u8, u8, &[usize])] = &[
        (0x19, 0x00, &[2, 3, 4, 5]),
        (0x1a, 0x90, &[4, 5, 6]),
        (0x1a, 0xaf, &[4, 5, 6]),
        (0x1a, 0xc0, &[4, 5, 6]),
        (0x1a, 0xcf, &[4, 5, 6]),
    ];
    for &(family, model, reserved) in layouts {
        for &index in reserved {
            // Nonzero reserved bytes are permitted only if they stay equal.
            let base = [2; 8];
            let tee = MutableTee::snp(5, family, model, base);
            let config = config();
            let mut floor = RuntimeTcbFloor::new(&tee, &config).unwrap();
            floor.create_protector(&tee, &config, &DEK).unwrap();
            for value in [1, 3] {
                let mut changed = base;
                changed[index] = value;
                changed[7] += 1;
                tee.set_svn(snp_svn(changed));
                assert!(matches!(
                    floor.create_protector(&tee, &config, &DEK),
                    Err(Error(ErrorInner::TcbIncompatible))
                ));
                assert_floor(&floor, snp_svn(base));
            }
            assert_eq!(tee.state.lock().derivations.len(), 1);
        }
    }
}

#[test]
fn same_raw_tcb_byte_is_snp_component_or_reserved_depending_on_cpu() {
    // Explicit ABI expectations: byte 6 is legacy SNP but Turin reserved;
    // byte 3 is legacy reserved but Turin SNP. Do not consult layout types.
    for version in [3, 4, 5] {
        for (family, model, index, upgrade_allowed) in [
            (0x19, 0x00, 6, true),
            (0x1a, 0x90, 6, false),
            (0x19, 0x00, 3, false),
            (0x1a, 0x90, 3, true),
            (0x1a, 0xc0, 6, false),
            (0x1a, 0xc0, 3, true),
        ] {
            let base = [2; 8];
            let tee = MutableTee::snp(version, family, model, base);
            let config = config();
            let mut floor = RuntimeTcbFloor::new(&tee, &config).unwrap();
            let mut changed = base;
            changed[index] += 1;
            tee.set_svn(snp_svn(changed));
            if upgrade_allowed {
                let protector = floor.create_protector(&tee, &config, &DEK).unwrap();
                assert!(svn_equal(header_svn(&protector), snp_svn(changed)));
                assert!(
                    floor
                        .verify_protector(&tee, &config, &protector, &DEK)
                        .unwrap()
                );
                assert_floor(&floor, snp_svn(changed));
                assert_eq!(tee.state.lock().derivations.len(), 2);
            } else {
                assert!(matches!(
                    floor.create_protector(&tee, &config, &DEK),
                    Err(Error(ErrorInner::TcbIncompatible))
                ));
                assert!(matches!(
                    floor.verify_protector(&tee, &config, &[], &DEK),
                    Err(Error(ErrorInner::TcbIncompatible))
                ));
                assert_floor(&floor, snp_svn(base));
                assert!(tee.state.lock().derivations.is_empty());
            }
        }
    }
}

#[test]
fn version_family_and_model_domains_cannot_switch_even_at_equal_svn() {
    let domains = [
        (2, 0x19, 0),
        (3, 0x19, 0),
        (4, 0x19, 0),
        (5, 0x19, 0),
        (3, 0x19, 1),
        (3, 0x19, 0x10),
        (3, 0x1a, 0),
        (3, 0x1a, 1),
        (3, 0x1a, 0x90),
        (3, 0x1a, 0xaf),
        (3, 0x1a, 0xc0),
        (3, 0x1a, 0xcf),
        (4, 0x1a, 0x90),
        (5, 0x1a, 0x90),
        (3, 0xff, 0),
        (3, 0xff, 1),
        (3, 0x19, 0x20),
        (3, 0x1a, 0x10),
        (6, 0x19, 0),
        (6, 0x19, 1),
        (6, 0x1a, 0x90),
        (6, 0x1a, 0xc0),
        (6, 0xff, 0),
        (u32::MAX, 0x19, 0),
    ];
    for source in domains {
        for destination in domains {
            if source == destination {
                continue;
            }
            let source_tee = MutableTee::snp(source.0, source.1, source.2, BASE_TCB);
            let destination_tee =
                MutableTee::snp(destination.0, destination.1, destination.2, BASE_TCB);
            let config = config();
            let mut floor = RuntimeTcbFloor::new(&source_tee, &config).unwrap();
            assert!(matches!(
                floor.create_protector(&destination_tee, &config, &DEK),
                Err(Error(ErrorInner::TcbIncompatible))
            ));
            assert_eq!(
                floor.snapshot.snp_domain.as_ref().unwrap().version,
                source.0
            );
            assert!(destination_tee.state.lock().derivations.is_empty());
        }
    }
}

#[test]
fn v2_and_unknown_versions_or_cpus_are_exact_only() {
    for (version, family, model) in [
        (0, 0x19, 0),
        (1, 0x19, 0),
        (2, 0x19, 0),
        (6, 0x19, 0),
        (6, 0x1a, 0x90),
        (6, 0x1a, 0xaf),
        (6, 0x1a, 0xc0),
        (6, 0x1a, 0xcf),
        (u32::MAX, 0x1a, 0),
        (3, 0xff, 0),
        (4, 0x19, 0x20),
        (5, 0x1a, 0x10),
    ]
    .into_iter()
    .chain([3, 4, 5].into_iter().flat_map(|version| {
        [0x00, 0x0f, 0x8f, 0xb0, 0xbf, 0xd0, 0xff].map(|model| (version, 0x1a, model))
    })) {
        let base = [2; 8];
        let tee = MutableTee::snp(version, family, model, base);
        let config = config();
        let mut floor = RuntimeTcbFloor::new(&tee, &config).unwrap();
        let equal = floor.create_protector(&tee, &config, &DEK).unwrap();
        assert!(floor.verify_protector(&tee, &config, &equal, &DEK).unwrap());
        for index in 0..8 {
            for value in [1, 3] {
                let mut changed = base;
                changed[index] = value;
                tee.set_svn(snp_svn(changed));
                assert!(matches!(
                    floor.create_protector(&tee, &config, &DEK),
                    Err(Error(ErrorInner::TcbIncompatible))
                ));
                assert_floor(&floor, snp_svn(base));
            }
        }
        assert_eq!(tee.state.lock().derivations.len(), 2);
    }
}

#[test]
fn typed_snp_report_preserves_prefix_and_version_handling() {
    use zerocopy::FromZeros;

    for version in [2, 3, 5] {
        let mut report = SnpReport::new_zeroed();
        report.version = version;
        report.reported_tcb = u64::from_le_bytes(BASE_TCB);
        report.cpuid_fam_id = 0x19;
        report.cpuid_mod_id = 0x11;
        report.cpuid_step = 2;
        let tee = MutableTee::snp(version, 0x19, 0x11, BASE_TCB);
        for trailing in [0, 16] {
            let mut bytes = report.as_bytes().to_vec();
            bytes.resize(bytes.len() + trailing, 0xa5);
            tee.state.lock().report = bytes;
            let floor = RuntimeTcbFloor::new(&tee, &config()).unwrap();
            assert_floor(&floor, snp_svn(BASE_TCB));
            let domain = floor.snapshot.snp_domain.unwrap();
            assert_eq!(domain.version, version);
            assert_eq!(domain.cpuid, (version >= 3).then_some([0x19, 0x11]));
        }
        for length in [0, 4, 0x188, 0x18b, size_of::<SnpReport>() - 1] {
            tee.state.lock().report = report.as_bytes()[..length].to_vec();
            assert!(matches!(
                RuntimeTcbFloor::new(&tee, &config()),
                Err(Error(ErrorInner::MalformedReport))
            ));
        }
    }
}

#[test]
fn v2_does_not_infer_cpuid_from_reserved_report_bytes() {
    let tee = MutableTee::snp(2, 0x19, 0, BASE_TCB);
    let config = config();
    let mut floor = RuntimeTcbFloor::new(&tee, &config).unwrap();
    tee.state.lock().report[0x188..0x18a].copy_from_slice(&[0x1a, 0x0f]);
    floor.create_protector(&tee, &config, &DEK).unwrap();
    assert_eq!(floor.snapshot.snp_domain.as_ref().unwrap().cpuid, None);
    let mut upgraded = BASE_TCB;
    upgraded[7] += 1;
    tee.set_svn(snp_svn(upgraded));
    assert!(matches!(
        floor.create_protector(&tee, &config, &DEK),
        Err(Error(ErrorInner::TcbIncompatible))
    ));
}

#[test]
fn tdx_equal_svns_are_accepted_for_both_module_modes() {
    for module_id in [0, 1] {
        let mut tee_tcb_svn = [3; 16];
        tee_tcb_svn[1] = module_id;
        let svn = KeyDerivationSvn::Tdx {
            tee_tcb_svn,
            cpu_svn: [5; 16],
        };
        let tee = MutableTee::tdx();
        tee.set_svn(svn);
        let config = config();
        let mut floor = RuntimeTcbFloor::new(&tee, &config).unwrap();
        let protector = floor.create_protector(&tee, &config, &DEK).unwrap();
        assert!(svn_equal(header_svn(&protector), svn));
        assert!(
            floor
                .verify_protector(&tee, &config, &protector, &DEK)
                .unwrap()
        );
        assert_floor(&floor, svn);
        assert_eq!(tee.state.lock().derivations.len(), 2);
    }
}

#[test]
fn tdx_each_component_upgrade_ratchets_and_subsequent_downgrade_is_rejected() {
    for module_id in [0, 1] {
        for change_cpu in [false, true] {
            for index in 0..16 {
                // TEE bytes 0 and 2 are ordered; byte 1 is the module identity
                // and bytes 3..16 are reserved. CPU SVN orders all 16 bytes.
                if !change_cpu && !matches!(index, 0 | 2) {
                    continue;
                }
                let mut tee_tcb_svn = [3; 16];
                tee_tcb_svn[1] = module_id;
                let mut cpu_svn = [5; 16];
                let base = KeyDerivationSvn::Tdx {
                    tee_tcb_svn,
                    cpu_svn,
                };
                let tee = MutableTee::tdx();
                tee.set_svn(base);
                let config = config();
                let mut floor = RuntimeTcbFloor::new(&tee, &config).unwrap();
                let bytes = if change_cpu {
                    &mut cpu_svn
                } else {
                    &mut tee_tcb_svn
                };
                bytes[index] += 1;
                let next = KeyDerivationSvn::Tdx {
                    tee_tcb_svn,
                    cpu_svn,
                };
                tee.set_svn(next);
                let protector = floor.create_protector(&tee, &config, &DEK).unwrap();
                assert!(svn_equal(header_svn(&protector), next));
                assert_floor(&floor, next);
                assert!(
                    floor
                        .verify_protector(&tee, &config, &protector, &DEK)
                        .unwrap()
                );
                tee.set_svn(base);
                assert!(matches!(
                    floor.create_protector(&tee, &config, &DEK),
                    Err(Error(ErrorInner::TcbLowered))
                ));
                assert!(matches!(
                    floor.verify_protector(&tee, &config, &protector, &DEK),
                    Err(Error(ErrorInner::TcbLowered))
                ));
                assert_floor(&floor, next);
                let state = tee.state.lock();
                assert_eq!(state.derivations.len(), 2);
                for policy in &state.derivations {
                    assert!(svn_equal(policy.svn, next));
                }
            }
        }
    }
}

#[test]
fn tdx_reserved_changes_are_incompatible_without_ratcheting_or_derivation() {
    for module_id in [0, 1] {
        for index in 3..16 {
            // Explicit wire offsets independently check the typed ABI parser.
            // Reject changes in either direction, including zero -> nonzero.
            for (reserved, changed) in [(0, 1), (3, 4), (3, 2)] {
                for upgrade_components in [false, true] {
                    let mut tee_tcb_svn = [reserved; 16];
                    tee_tcb_svn[0] = 3;
                    tee_tcb_svn[1] = module_id;
                    tee_tcb_svn[2] = 3;
                    let base = KeyDerivationSvn::Tdx {
                        tee_tcb_svn,
                        cpu_svn: [5; 16],
                    };
                    let tee = MutableTee::tdx();
                    tee.set_svn(base);
                    let config = config();
                    let mut floor = RuntimeTcbFloor::new(&tee, &config).unwrap();
                    let candidate = floor.create_protector(&tee, &config, &DEK).unwrap();

                    tee_tcb_svn[index] = changed;
                    if upgrade_components {
                        tee_tcb_svn[0] += 1;
                        tee_tcb_svn[2] += 1;
                    }
                    tee.set_svn(KeyDerivationSvn::Tdx {
                        tee_tcb_svn,
                        cpu_svn: [if upgrade_components { 6 } else { 5 }; 16],
                    });
                    assert!(matches!(
                        floor.create_protector(&tee, &config, &DEK),
                        Err(Error(ErrorInner::TcbIncompatible))
                    ));
                    assert!(matches!(
                        floor.verify_protector(&tee, &config, &candidate, &DEK),
                        Err(Error(ErrorInner::TcbIncompatible))
                    ));
                    assert_floor(&floor, base);
                    assert_eq!(tee.state.lock().derivations.len(), 1);

                    // Rejection must not poison the floor or prevent recovery
                    // once the original compatible observation is restored.
                    tee.set_svn(base);
                    assert!(
                        floor
                            .verify_protector(&tee, &config, &candidate, &DEK)
                            .unwrap()
                    );
                    assert_floor(&floor, base);
                    assert_eq!(tee.state.lock().derivations.len(), 2);
                }
            }
        }
    }
}

#[test]
fn tdx_incomparable_components_rejected_despite_lexicographically_greater_svn() {
    for module_id in [0, 1] {
        for change_cpu in [false, true] {
            for lowered_index in 1..16 {
                if !change_cpu && lowered_index != 2 {
                    continue;
                }
                let mut tee_tcb_svn = [3; 16];
                tee_tcb_svn[1] = module_id;
                let mut cpu_svn = [5; 16];
                let base = KeyDerivationSvn::Tdx {
                    tee_tcb_svn,
                    cpu_svn,
                };
                let tee = MutableTee::tdx();
                tee.set_svn(base);
                let config = config();
                let mut floor = RuntimeTcbFloor::new(&tee, &config).unwrap();
                let bytes = if change_cpu {
                    &mut cpu_svn
                } else {
                    &mut tee_tcb_svn
                };
                // The first differing byte increases, but a later one falls.
                // Neither lexicographic ordering nor any-component growth
                // establishes a component-wise successor.
                bytes[0] += 1;
                bytes[lowered_index] -= 1;
                tee.set_svn(KeyDerivationSvn::Tdx {
                    tee_tcb_svn,
                    cpu_svn,
                });
                assert!(matches!(
                    floor.create_protector(&tee, &config, &DEK),
                    Err(Error(ErrorInner::TcbLowered))
                ));
                assert!(matches!(
                    floor.verify_protector(&tee, &config, &[], &DEK),
                    Err(Error(ErrorInner::TcbLowered))
                ));
                assert_floor(&floor, base);
                assert!(tee.state.lock().derivations.is_empty());
            }
        }
    }
}

#[test]
fn tdx_module_identity_changes_rejected_even_with_higher_components() {
    for (source_id, destination_id) in [(0, 1), (1, 0), (1, 2)] {
        let mut tee_tcb_svn = [3; 16];
        tee_tcb_svn[1] = source_id;
        let base = KeyDerivationSvn::Tdx {
            tee_tcb_svn,
            cpu_svn: [5; 16],
        };
        let tee = MutableTee::tdx();
        tee.set_svn(base);
        let config = config();
        let mut floor = RuntimeTcbFloor::new(&tee, &config).unwrap();
        tee_tcb_svn[0] = 4;
        tee_tcb_svn[2] = 4;
        tee_tcb_svn[1] = destination_id;
        tee.set_svn(KeyDerivationSvn::Tdx {
            tee_tcb_svn,
            cpu_svn: [6; 16],
        });
        assert!(matches!(
            floor.create_protector(&tee, &config, &DEK),
            Err(Error(ErrorInner::TcbIncompatible))
        ));
        assert!(matches!(
            floor.verify_protector(&tee, &config, &[], &DEK),
            Err(Error(ErrorInner::TcbIncompatible))
        ));
        assert_floor(&floor, base);
        assert!(tee.state.lock().derivations.is_empty());
    }
}

#[test]
fn tdx_lower_module_isvsvn_rejected_even_with_higher_platform_and_cpu_svns() {
    for module_id in [0, 1] {
        let mut tee_tcb_svn = [3; 16];
        tee_tcb_svn[1] = module_id;
        let base = KeyDerivationSvn::Tdx {
            tee_tcb_svn,
            cpu_svn: [5; 16],
        };
        let tee = MutableTee::tdx();
        tee.set_svn(base);
        let config = config();
        let mut floor = RuntimeTcbFloor::new(&tee, &config).unwrap();
        tee_tcb_svn[0] -= 1;
        tee_tcb_svn[2] = 4;
        tee.set_svn(KeyDerivationSvn::Tdx {
            tee_tcb_svn,
            cpu_svn: [6; 16],
        });
        assert!(matches!(
            floor.create_protector(&tee, &config, &DEK),
            Err(Error(ErrorInner::TcbLowered))
        ));
        assert!(matches!(
            floor.verify_protector(&tee, &config, &[], &DEK),
            Err(Error(ErrorInner::TcbLowered))
        ));
        assert_floor(&floor, base);
        assert!(tee.state.lock().derivations.is_empty());
    }
}

#[test]
fn tdx_derivation_failure_retains_ratchet_and_old_reports_cannot_reset_it() {
    for module_id in [0, 1] {
        for verify in [false, true] {
            let mut tee_tcb_svn = [3; 16];
            tee_tcb_svn[1] = module_id;
            let base = KeyDerivationSvn::Tdx {
                tee_tcb_svn,
                cpu_svn: [5; 16],
            };
            let tee = MutableTee::tdx();
            tee.set_svn(base);
            let config = config();
            let mut floor = RuntimeTcbFloor::new(&tee, &config).unwrap();
            tee_tcb_svn[0] += 1;
            let next = KeyDerivationSvn::Tdx {
                tee_tcb_svn,
                cpu_svn: [6; 16],
            };
            tee.set_svn(next);
            // Create independently so verification itself must ratchet.
            let candidate = super::super::create_protector(&tee, &config, &DEK).unwrap();
            assert_floor(&floor, base);
            tee.state.lock().fail_derivation = true;
            let error = if verify {
                floor
                    .verify_protector(&tee, &config, &candidate, &DEK)
                    .unwrap_err()
            } else {
                floor.create_protector(&tee, &config, &DEK).unwrap_err()
            };
            assert!(matches!(error, Error(ErrorInner::Derive(_))));
            assert_floor(&floor, next);
            {
                let state = tee.state.lock();
                assert_eq!(state.derivations.len(), 2);
                assert!(svn_equal(state.derivations[1].svn, next));
            }
            tee.state.lock().fail_derivation = false;
            tee.set_svn(base);
            assert!(matches!(
                floor.create_protector(&tee, &config, &DEK),
                Err(Error(ErrorInner::TcbLowered))
            ));
            assert!(matches!(
                floor.verify_protector(&tee, &config, &candidate, &DEK),
                Err(Error(ErrorInner::TcbLowered))
            ));
            assert_floor(&floor, next);
            assert_eq!(tee.state.lock().derivations.len(), 2);
            tee.set_svn(next);
            assert!(
                floor
                    .verify_protector(&tee, &config, &candidate, &DEK)
                    .unwrap()
            );
            assert_floor(&floor, next);
            assert_eq!(tee.state.lock().derivations.len(), 3);
        }
    }
}

#[test]
fn tdx_candidate_requires_exact_svn_despite_compatible_newer_report() {
    for module_id in [0, 1] {
        for change_cpu in [false, true] {
            for index in 0..16 {
                if !change_cpu && !matches!(index, 0 | 2) {
                    continue;
                }
                let mut tee_tcb_svn = [3; 16];
                tee_tcb_svn[1] = module_id;
                let mut cpu_svn = [5; 16];
                let base = KeyDerivationSvn::Tdx {
                    tee_tcb_svn,
                    cpu_svn,
                };
                let tee = MutableTee::tdx();
                tee.set_svn(base);
                let config = config();
                let mut floor = RuntimeTcbFloor::new(&tee, &config).unwrap();
                let bytes = if change_cpu {
                    &mut cpu_svn
                } else {
                    &mut tee_tcb_svn
                };
                bytes[index] += 1;
                let next = KeyDerivationSvn::Tdx {
                    tee_tcb_svn,
                    cpu_svn,
                };
                // Migration after observation must not change the SVN used
                // for creation, but the next verification must notice it.
                tee.state.lock().after_report = Some(next);
                let candidate = floor.create_protector(&tee, &config, &DEK).unwrap();
                assert!(svn_equal(header_svn(&candidate), base));
                assert_floor(&floor, base);
                assert_eq!(tee.state.lock().report_calls, 2);
                assert!(
                    !floor
                        .verify_protector(&tee, &config, &candidate, &DEK)
                        .unwrap()
                );
                assert_floor(&floor, next);
                {
                    let state = tee.state.lock();
                    assert_eq!(state.report_calls, 3);
                    // Reject the stale header before deriving its still-
                    // available hardware key, not through an unseal failure.
                    assert_eq!(state.derivations.len(), 1);
                    assert!(svn_equal(state.derivations[0].svn, base));
                }
                tee.set_svn(base);
                assert!(matches!(
                    floor.verify_protector(&tee, &config, &candidate, &DEK),
                    Err(Error(ErrorInner::TcbLowered))
                ));
                assert_floor(&floor, next);
                assert_eq!(tee.state.lock().derivations.len(), 1);
                tee.set_svn(next);
                let replacement = floor.create_protector(&tee, &config, &DEK).unwrap();
                assert!(svn_equal(header_svn(&replacement), next));
                assert!(
                    floor
                        .verify_protector(&tee, &config, &replacement, &DEK)
                        .unwrap()
                );
                assert_floor(&floor, next);
                assert_eq!(tee.state.lock().derivations.len(), 3);
            }
        }
    }
}

#[test]
fn mixed_tees_and_mismatched_report_svn_are_rejected() {
    for (source, destination) in [
        (MutableTee::snp(3, 0x19, 0, BASE_TCB), MutableTee::tdx()),
        (MutableTee::tdx(), MutableTee::snp(3, 0x19, 0, BASE_TCB)),
    ] {
        let config = config();
        let mut floor = RuntimeTcbFloor::new(&source, &config).unwrap();
        let source_svn = source.state.lock().svn.unwrap();
        assert!(matches!(
            floor.create_protector(&destination, &config, &DEK),
            Err(Error(ErrorInner::TcbIncompatible))
        ));
        assert!(matches!(
            floor.verify_protector(&destination, &config, &[], &DEK),
            Err(Error(ErrorInner::TcbIncompatible))
        ));
        destination.set_svn(source_svn);
        assert!(matches!(
            RuntimeTcbFloor::new(&destination, &config),
            Err(Error(ErrorInner::ReportSvnMismatch))
        ));
        assert!(matches!(
            floor.create_protector(&destination, &config, &DEK),
            Err(Error(ErrorInner::ReportSvnMismatch))
        ));
        assert_floor(&floor, source_svn);
        assert!(destination.state.lock().derivations.is_empty());
    }
}

#[test]
fn reports_must_be_full_sized_and_raw_tcb_must_match_returned_svn() {
    let tee = MutableTee::snp(3, 0x19, 0, BASE_TCB);
    let config = config();
    let mut floor = RuntimeTcbFloor::new(&tee, &config).unwrap();
    let valid = tee.state.lock().report.clone();
    for size in 0..1184 {
        tee.state.lock().report = valid[..size].to_vec();
        assert!(matches!(
            RuntimeTcbFloor::new(&tee, &config),
            Err(Error(ErrorInner::MalformedReport))
        ));
        assert!(matches!(
            floor.create_protector(&tee, &config, &DEK),
            Err(Error(ErrorInner::MalformedReport))
        ));
        assert!(matches!(
            floor.verify_protector(&tee, &config, &[], &DEK),
            Err(Error(ErrorInner::MalformedReport))
        ));
        assert_floor(&floor, snp_svn(BASE_TCB));
    }
    tee.state.lock().report = valid;
    for index in 0..8 {
        tee.state.lock().report[0x180 + index] ^= 1;
        assert!(matches!(
            RuntimeTcbFloor::new(&tee, &config),
            Err(Error(ErrorInner::ReportTcbMismatch))
        ));
        assert!(matches!(
            floor.create_protector(&tee, &config, &DEK),
            Err(Error(ErrorInner::ReportTcbMismatch))
        ));
        assert!(matches!(
            floor.verify_protector(&tee, &config, &[], &DEK),
            Err(Error(ErrorInner::ReportTcbMismatch))
        ));
        tee.state.lock().report[0x180 + index] ^= 1;
    }
    assert!(tee.state.lock().derivations.is_empty());
    // The contract accepts reports with an extension after the full ABI report.
    tee.state.lock().report.push(0);
    floor.create_protector(&tee, &config, &DEK).unwrap();
}

#[test]
fn report_failure_and_missing_svn_never_reset_floor() {
    for tee in [MutableTee::snp(3, 0x19, 0, BASE_TCB), MutableTee::tdx()] {
        let config = config();
        let mut floor = RuntimeTcbFloor::new(&tee, &config).unwrap();
        let initial = tee.state.lock().svn.unwrap();
        for report_failure in [true, false] {
            {
                let mut state = tee.state.lock();
                state.fail_report = report_failure;
                state.svn = None;
            }
            let errors = [
                RuntimeTcbFloor::new(&tee, &config).unwrap_err(),
                floor.create_protector(&tee, &config, &DEK).unwrap_err(),
                floor
                    .verify_protector(&tee, &config, &[], &DEK)
                    .unwrap_err(),
            ];
            for error in errors {
                if report_failure {
                    assert!(matches!(error, Error(ErrorInner::Report(_))));
                } else {
                    assert!(matches!(error, Error(ErrorInner::MissingKeyDerivationSvn)));
                }
            }
            assert_floor(&floor, initial);
        }
        assert!(tee.state.lock().derivations.is_empty());
        tee.set_svn(initial);
        floor.create_protector(&tee, &config, &DEK).unwrap();
    }
}

#[test]
fn policy_is_validated_before_reports_and_floor_changes() {
    let tee = MutableTee::snp(3, 0x19, 0, BASE_TCB);
    let mut config = config();
    let mut floor = RuntimeTcbFloor::new(&tee, &config).unwrap();
    config.hardware_sealing_policy = HardwareSealingPolicy::None;
    assert!(matches!(
        RuntimeTcbFloor::new(&tee, &config),
        Err(Error(ErrorInner::DisabledPolicy))
    ));
    assert!(matches!(
        floor.create_protector(&tee, &config, &DEK),
        Err(Error(ErrorInner::DisabledPolicy))
    ));
    assert!(matches!(
        floor.verify_protector(&tee, &config, &[], &DEK),
        Err(Error(ErrorInner::DisabledPolicy))
    ));
    assert_eq!(tee.state.lock().report_calls, 1);

    config.hardware_sealing_policy = HardwareSealingPolicy::Signer;
    let tdx = MutableTee::tdx();
    assert!(matches!(
        RuntimeTcbFloor::new(&tdx, &config),
        Err(Error(ErrorInner::TdxSignerPolicyUnsupported))
    ));
    assert_eq!(tdx.state.lock().report_calls, 0);
    // SNP signer policy remains supported by the floor path.
    let protector = floor.create_protector(&tee, &config, &DEK).unwrap();
    assert!(
        floor
            .verify_protector(&tee, &config, &protector, &DEK)
            .unwrap()
    );

    let mut unavailable = MutableTee::snp(3, 0x19, 0, BASE_TCB);
    unavailable.supports_derivation = false;
    assert!(matches!(
        RuntimeTcbFloor::new(&unavailable, &config),
        Err(Error(ErrorInner::UnsupportedTee))
    ));
    assert_eq!(unavailable.state.lock().report_calls, 0);
}

#[test]
fn create_uses_one_report_and_retains_ratchet_after_derivation_error() {
    let tee = MutableTee::snp(3, 0x19, 0, BASE_TCB);
    let config = config();
    let mut floor = RuntimeTcbFloor::new(&tee, &config).unwrap();
    let mut next = BASE_TCB;
    next[7] += 1;
    tee.set_svn(snp_svn(next));
    tee.state.lock().fail_derivation = true;
    assert!(matches!(
        floor.create_protector(&tee, &config, &DEK),
        Err(Error(ErrorInner::Derive(_)))
    ));
    assert_floor(&floor, snp_svn(next));
    {
        let state = tee.state.lock();
        assert_eq!(state.report_calls, 2);
        assert_eq!(state.derivations.len(), 1);
        assert!(svn_equal(state.derivations[0].svn, snp_svn(next)));
    }
    tee.state.lock().fail_derivation = false;
    tee.set_svn(snp_svn(BASE_TCB));
    assert!(matches!(
        floor.create_protector(&tee, &config, &DEK),
        Err(Error(ErrorInner::TcbLowered))
    ));
    assert_eq!(tee.state.lock().derivations.len(), 1);

    tee.set_svn(snp_svn(next));
    let mut later = next;
    later[7] += 1;
    tee.state.lock().after_report = Some(snp_svn(later));
    let candidate = floor.create_protector(&tee, &config, &DEK).unwrap();
    assert!(svn_equal(header_svn(&candidate), snp_svn(next)));
    assert_floor(&floor, snp_svn(next));
    assert_eq!(tee.state.lock().report_calls, 4);
    assert!(
        !floor
            .verify_protector(&tee, &config, &candidate, &DEK)
            .unwrap()
    );
    assert_floor(&floor, snp_svn(later));
    assert_eq!(tee.state.lock().derivations.len(), 2);
}

#[test]
fn verify_changed_report_returns_false_and_ratchets_before_header_check() {
    let tee = MutableTee::snp(3, 0x19, 0, BASE_TCB);
    let config = config();
    let mut floor = RuntimeTcbFloor::new(&tee, &config).unwrap();
    let old = floor.create_protector(&tee, &config, &DEK).unwrap();
    let mut next = BASE_TCB;
    next[7] += 1;
    tee.set_svn(snp_svn(next));
    assert!(!floor.verify_protector(&tee, &config, &old, &DEK).unwrap());
    assert_floor(&floor, snp_svn(next));
    assert_eq!(tee.state.lock().derivations.len(), 1);
    next[7] += 1;
    tee.set_svn(snp_svn(next));
    assert!(!floor.verify_protector(&tee, &config, &[], &DEK).unwrap());
    assert_floor(&floor, snp_svn(next));
    tee.set_svn(snp_svn(BASE_TCB));
    assert!(matches!(
        floor.verify_protector(&tee, &config, &old, &DEK),
        Err(Error(ErrorInner::TcbLowered))
    ));
    assert_eq!(tee.state.lock().derivations.len(), 1);
}

#[test]
fn verify_derivation_failure_keeps_new_floor_and_can_retry() {
    let tee = MutableTee::snp(3, 0x19, 0, BASE_TCB);
    let config = config();
    let mut floor = RuntimeTcbFloor::new(&tee, &config).unwrap();
    let mut next = BASE_TCB;
    next[7] += 1;
    tee.set_svn(snp_svn(next));
    // Independent candidate creation must not be what ratchets this floor.
    let candidate = super::super::create_protector(&tee, &config, &DEK).unwrap();
    assert_floor(&floor, snp_svn(BASE_TCB));
    tee.state.lock().fail_derivation = true;
    assert!(matches!(
        floor.verify_protector(&tee, &config, &candidate, &DEK),
        Err(Error(ErrorInner::Derive(_)))
    ));
    assert_floor(&floor, snp_svn(next));
    tee.state.lock().fail_derivation = false;
    tee.set_svn(snp_svn(BASE_TCB));
    assert!(matches!(
        floor.verify_protector(&tee, &config, &candidate, &DEK),
        Err(Error(ErrorInner::TcbLowered))
    ));
    tee.set_svn(snp_svn(next));
    assert!(
        floor
            .verify_protector(&tee, &config, &candidate, &DEK)
            .unwrap()
    );
}

#[test]
fn tampered_old_header_cannot_lower_floor_or_bypass_authentication() {
    let tee = MutableTee::snp(3, 0x19, 0, BASE_TCB);
    let config = config();
    let mut floor = RuntimeTcbFloor::new(&tee, &config).unwrap();
    let old = floor.create_protector(&tee, &config, &DEK).unwrap();
    let mut next = BASE_TCB;
    next[7] += 1;
    tee.set_svn(snp_svn(next));
    let mut forged = vmgs::HardwareKeyProtectorV3::read_from_bytes(&old).unwrap();
    forged.header.svn[..8].copy_from_slice(&next);
    // Matching the new observed SVN does not authenticate the old ciphertext.
    assert!(
        !floor
            .verify_protector(&tee, &config, forged.as_bytes(), &DEK)
            .unwrap()
    );
    assert_floor(&floor, snp_svn(next));
    assert_eq!(tee.state.lock().derivations.len(), 2);

    forged.header.svn[..8].fill(0);
    assert!(
        !floor
            .verify_protector(&tee, &config, forged.as_bytes(), &DEK)
            .unwrap()
    );
    forged.header.svn[..8].fill(0xff);
    assert!(
        !floor
            .verify_protector(&tee, &config, forged.as_bytes(), &DEK)
            .unwrap()
    );
    assert_floor(&floor, snp_svn(next));
    assert_eq!(tee.state.lock().derivations.len(), 2);
    tee.set_svn(snp_svn(BASE_TCB));
    assert!(matches!(
        floor.create_protector(&tee, &config, &DEK),
        Err(Error(ErrorInner::TcbLowered))
    ));
}

#[test]
fn unchanged_svn_still_rederives_and_checks_dek_and_hardware_identity() {
    for tee in [MutableTee::snp(3, 0x19, 0, BASE_TCB), MutableTee::tdx()] {
        let config = config();
        let mut floor = RuntimeTcbFloor::new(&tee, &config).unwrap();
        let protector = floor.create_protector(&tee, &config, &DEK).unwrap();
        assert!(
            floor
                .verify_protector(&tee, &config, &protector, &DEK)
                .unwrap()
        );
        assert!(
            !floor
                .verify_protector(&tee, &config, &protector, &[0xcd; 32])
                .unwrap()
        );
        tee.state.lock().secret = [0x73; 32];
        assert!(
            !floor
                .verify_protector(&tee, &config, &protector, &DEK)
                .unwrap()
        );
        let replacement = floor.create_protector(&tee, &config, &DEK).unwrap();
        assert!(
            floor
                .verify_protector(&tee, &config, &replacement, &DEK)
                .unwrap()
        );
        let state = tee.state.lock();
        assert_eq!(state.report_calls, 7);
        assert_eq!(state.derivations.len(), 6);
    }
}
