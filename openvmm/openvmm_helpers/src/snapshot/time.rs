// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! The NVX time ABI records of a snapshot manifest: the time contract and the
//! CPU profile record of manifest version 6.

use super::SnapshotManifest;
use super::format::TIME_ABI_MANIFEST_VERSION;
use super::format::TIME_ABI_SNAPSHOT_FORMAT_MAGIC;
use super::microvm::SnapshotMachineContract;
use mesh::payload::Timestamp;
use openvmm_defs::time_abi::SnapshotCpuProfile;
use openvmm_defs::time_abi::SnapshotTimeContract;
use sha2::Digest;
use virt::time_abi::CaptureTimeRecord;
use virt::time_abi::Downtime;
use virt::time_abi::HostClockKind;
use virt::time_abi::HostIdentity;
use virt::time_abi::HostTimeSample;
use virt::time_abi::TIME_ABI_VERSION;
use virt::time_abi::TimeAbiCode;
use virt::time_abi::TimeAbiError;
use virt::time_abi::TimeAbiTestHooks;
use virt::time_abi::downtime::select_downtime;
use virt::time_abi::rate;
use virt::time_abi::surface;

/// The value of the retired capture wall clock in a version 6 manifest.
pub(super) const NO_TIMESTAMP: Timestamp = Timestamp {
    seconds: 0,
    nanos: 0,
};

/// The largest canonical CPU profile encoding a manifest may carry.
pub const MAX_CPU_PROFILE_BYTES: usize = 1024 * 1024;
/// The largest canonical effective-CPUID encoding a manifest may carry.
pub const MAX_EFFECTIVE_CPUID_BYTES: usize = 64 * 1024;

fn manifest_error(message: impl Into<String>) -> TimeAbiError {
    TimeAbiError::new(TimeAbiCode::ManifestTime, message)
}

fn identity(bytes: &[u8], description: &str) -> Result<[u8; 16], TimeAbiError> {
    bytes
        .try_into()
        .map_err(|_| manifest_error(format!("{description} is {} bytes, not 16", bytes.len())))
}

/// Validates a time contract (restore step 2 of the specification).
pub fn validate_time_contract(contract: &SnapshotTimeContract) -> Result<(), TimeAbiError> {
    let SnapshotTimeContract {
        time_abi_version,
        tsc_frequency_hz,
        tsc_tolerance_ppm,
        apic_frequency_hz,
        capture_tsc: _,
        capture_utc_ns: _,
        capture_monotonic_ns: _,
        host_clock,
        host_id,
        host_boot_id,
        capture_generation,
    } = contract;
    if *time_abi_version != TIME_ABI_VERSION {
        return Err(manifest_error(format!(
            "time ABI version {time_abi_version} is not {TIME_ABI_VERSION}"
        )));
    }
    if *tsc_tolerance_ppm != rate::TSC_TOLERANCE_PPM {
        return Err(manifest_error(format!(
            "TSC tolerance {tsc_tolerance_ppm} ppm is not {}",
            rate::TSC_TOLERANCE_PPM
        )));
    }
    rate::check_plausible_tsc_hz(*tsc_frequency_hz)
        .map_err(|err| manifest_error(format!("snapshot {}", err.message)))?;
    if ![rate::LAPIC_HZ_KVM, rate::LAPIC_HZ_HYPERV].contains(apic_frequency_hz) {
        return Err(manifest_error(format!(
            "LAPIC rate {apic_frequency_hz} Hz is not a backend constant"
        )));
    }
    if HostClockKind::from_manifest(host_clock).is_none() {
        return Err(manifest_error(format!(
            "host clock '{host_clock}' is unknown"
        )));
    }
    identity(host_id, "host identity")?;
    identity(host_boot_id, "host boot identity")?;
    if *capture_generation == u32::MAX {
        return Err(TimeAbiError::new(
            TimeAbiCode::GenerationExhausted,
            "the snapshot's generation counter cannot be incremented",
        ));
    }
    Ok(())
}

fn verify_sha256(bytes: &[u8], digest: &[u8], description: &str) -> Result<(), TimeAbiError> {
    if digest.len() != 32 {
        return Err(manifest_error(format!(
            "{description} digest is {} bytes, not 32",
            digest.len()
        )));
    }
    if sha2::Sha256::digest(bytes).as_slice() != digest {
        return Err(TimeAbiError::new(
            TimeAbiCode::ProfileDigest,
            format!("{description} digest does not verify"),
        ));
    }
    Ok(())
}

/// Validates a CPU profile record: its shape and both digests.
pub fn validate_cpu_profile_record(record: &SnapshotCpuProfile) -> Result<(), TimeAbiError> {
    let SnapshotCpuProfile {
        id,
        sha256,
        profile,
        effective_cpuid,
        effective_cpuid_sha256,
        capture_cpu_signature: _,
    } = record;
    let id_valid = !id.is_empty()
        && id.len() <= 64
        && id.split('.').all(|component| {
            !component.is_empty()
                && component
                    .bytes()
                    .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
        });
    if !id_valid {
        return Err(manifest_error(format!(
            "CPU profile ID '{id}' is malformed"
        )));
    }
    if profile.len() > MAX_CPU_PROFILE_BYTES {
        return Err(manifest_error("CPU profile exceeds 1 MiB"));
    }
    verify_sha256(profile, sha256, "CPU profile")?;
    if effective_cpuid.is_empty()
        || effective_cpuid.len() > MAX_EFFECTIVE_CPUID_BYTES
        || !effective_cpuid
            .len()
            .is_multiple_of(surface::ENCODED_CPUID_LEAF_LEN)
    {
        return Err(manifest_error("effective CPUID record size is invalid"));
    }
    verify_sha256(effective_cpuid, effective_cpuid_sha256, "effective CPUID")
}

/// Returns the capture record of a validated time contract.
pub fn capture_record(contract: &SnapshotTimeContract) -> Result<CaptureTimeRecord, TimeAbiError> {
    validate_time_contract(contract)?;
    contract.capture_record()
}

/// Validates the time ABI part of a version 6 machine contract: both
/// records are present and valid, every retired clock field is empty, and
/// the command line sets no clock parameter.
pub fn validate_time_abi_contract(contract: &SnapshotMachineContract) -> Result<(), TimeAbiError> {
    let time = contract
        .time
        .as_ref()
        .ok_or_else(|| manifest_error("the time contract is missing"))?;
    let cpu_profile = contract
        .cpu_profile
        .as_ref()
        .ok_or_else(|| manifest_error("the CPU profile record is missing"))?;
    validate_time_contract(time)?;
    validate_cpu_profile_record(cpu_profile)?;
    let retired_fields_empty = contract.capture_wall_clock == NO_TIMESTAMP
        && contract.tsc_frequency_hz == 0
        && contract.tsc_tolerance_ppm == 0
        && contract.cpu_contract.is_empty()
        && contract.cpu_contract_sha256.is_empty()
        && contract.clock_policy.is_empty()
        && contract.apic_frequency_hz.is_none();
    if !retired_fields_empty {
        return Err(manifest_error(
            "a version 6 machine contract carries a retired clock field",
        ));
    }
    virt::time_abi::check_command_line_clock_tokens(&contract.effective_command_line)
}

/// Makes `manifest` a version 6 manifest carrying the time ABI records, and
/// clears the retired clock fields of its machine contract.
pub fn set_time_abi_records(
    manifest: &mut SnapshotManifest,
    time: SnapshotTimeContract,
    cpu_profile: SnapshotCpuProfile,
) -> anyhow::Result<()> {
    let contract = manifest
        .machine_contract
        .as_mut()
        .ok_or_else(|| manifest_error("the time ABI requires a microVM machine contract"))?;
    contract.capture_wall_clock = NO_TIMESTAMP;
    contract.tsc_frequency_hz = 0;
    contract.tsc_tolerance_ppm = 0;
    contract.cpu_contract.clear();
    contract.cpu_contract_sha256.clear();
    contract.clock_policy.clear();
    contract.apic_frequency_hz = None;
    contract.time = Some(time);
    contract.cpu_profile = Some(cpu_profile);
    manifest.version = TIME_ABI_MANIFEST_VERSION;
    manifest.format_magic = TIME_ABI_SNAPSHOT_FORMAT_MAGIC.to_vec();
    Ok(())
}

/// Requires the manifest version that matches the selected restore path:
/// version 6 with the time ABI, and a version before 6 without it.
pub fn check_time_abi_manifest_version(
    manifest: &SnapshotManifest,
    time_abi: bool,
) -> Result<(), TimeAbiError> {
    let is_time_abi = manifest.version == TIME_ABI_MANIFEST_VERSION;
    match (time_abi, is_time_abi) {
        (true, false) => Err(TimeAbiError::new(
            TimeAbiCode::SnapshotVersion,
            format!(
                "snapshot manifest version {} predates the time ABI; recapture the snapshot",
                manifest.version
            ),
        )),
        (false, true) => Err(TimeAbiError::new(
            TimeAbiCode::SnapshotVersion,
            "a time ABI snapshot requires --x-time-abi-v1",
        )),
        _ => Ok(()),
    }
}

/// The controller's view of a time ABI restore after restore steps 2 to 5 of
/// the specification.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TimeAbiRestorePreflight {
    /// The capture record.
    pub capture: CaptureTimeRecord,
    /// The preflight downtime. The worker measures `D` again at the restore
    /// anchor.
    pub downtime: Downtime,
    /// The generation counter `g` of the restored process.
    pub generation: u32,
    /// The CPU profile the restored VM uses: the snapshot's.
    pub cpu_profile: String,
}

/// Runs restore steps 2 to 5 of the specification on a version 6 machine
/// contract: the time ABI records, the backend, the CPU profile and host CPU
/// generation, and the downtime preflight against the destination host's
/// identity and clocks.
///
/// Until CPU profiles land, the snapshot's profile must be the interim
/// profile of `hypervisor` and the host must have the capture host's CPU
/// signature; the worker compares the effective CPUID.
pub fn preflight_time_abi_restore(
    contract: &SnapshotMachineContract,
    hypervisor: &str,
    requested_cpu_profile: &str,
    host_cpu_signature: Option<u32>,
    destination: &HostIdentity,
    now: &HostTimeSample,
    hooks: &TimeAbiTestHooks,
) -> Result<TimeAbiRestorePreflight, TimeAbiError> {
    validate_time_abi_contract(contract)?;
    let (Some(time), Some(record)) = (&contract.time, &contract.cpu_profile) else {
        unreachable!("a validated time ABI contract carries both records");
    };
    if contract.source_hypervisor != hypervisor {
        return Err(TimeAbiError::new(
            TimeAbiCode::BackendMismatch,
            format!(
                "the snapshot was captured on the {} backend and cannot be restored on {hypervisor}",
                contract.source_hypervisor
            ),
        ));
    }
    let cpu_profile = surface::resolve_cpu_profile(&record.id, hypervisor)?;
    if requested_cpu_profile != "auto" && requested_cpu_profile != record.id {
        return Err(TimeAbiError::new(
            TimeAbiCode::ProfileUnknown,
            format!(
                "--cpu-profile {requested_cpu_profile} does not name the snapshot's profile '{}'",
                record.id
            ),
        ));
    }
    if !record.profile.is_empty() {
        return Err(TimeAbiError::new(
            TimeAbiCode::ProfileDigest,
            format!(
                "the snapshot's '{}' profile document is not the pinned one",
                record.id
            ),
        ));
    }
    if host_cpu_signature != Some(record.capture_cpu_signature) {
        let host = host_cpu_signature.map_or_else(
            || "unavailable".to_owned(),
            |signature| format!("{signature:#010x}"),
        );
        return Err(TimeAbiError::new(
            TimeAbiCode::CpuGeneration,
            format!(
                "the host CPU signature {host} is not the capture host's {:#010x}",
                record.capture_cpu_signature
            ),
        ));
    }
    let capture = capture_record(time)?;
    let downtime = select_downtime(&capture, destination, now, hooks)?;
    Ok(TimeAbiRestorePreflight {
        capture,
        downtime,
        generation: time.capture_generation + 1,
        cpu_profile,
    })
}

#[cfg(test)]
pub(super) mod tests {
    use super::*;

    pub(in crate::snapshot) fn test_time_contract() -> SnapshotTimeContract {
        SnapshotTimeContract {
            time_abi_version: 1,
            tsc_frequency_hz: 2_100_000_000,
            tsc_tolerance_ppm: 250,
            apic_frequency_hz: rate::LAPIC_HZ_HYPERV,
            capture_tsc: 12_345_678,
            capture_utc_ns: 1_700_000_000_000_000_000,
            capture_monotonic_ns: 123_000_000_000,
            host_clock: "linux-boottime".to_owned(),
            host_id: vec![1; 16],
            host_boot_id: vec![2; 16],
            capture_generation: 3,
        }
    }

    pub(in crate::snapshot) fn test_cpu_profile() -> SnapshotCpuProfile {
        let profile = b"profile".to_vec();
        let effective_cpuid = vec![0; 24];
        SnapshotCpuProfile {
            id: "intel.skylake-sp.kvm.v1".to_owned(),
            sha256: sha2::Sha256::digest(&profile).to_vec(),
            profile,
            effective_cpuid_sha256: sha2::Sha256::digest(&effective_cpuid).to_vec(),
            effective_cpuid,
            capture_cpu_signature: 0x0005_0657,
        }
    }

    fn code(result: Result<(), TimeAbiError>) -> TimeAbiCode {
        result.unwrap_err().code
    }

    #[test]
    fn time_contract_rules() {
        validate_time_contract(&test_time_contract()).unwrap();
        let mutations: [(fn(&mut SnapshotTimeContract), TimeAbiCode); 8] = [
            (|c| c.time_abi_version = 2, TimeAbiCode::ManifestTime),
            (|c| c.tsc_tolerance_ppm = 251, TimeAbiCode::ManifestTime),
            (|c| c.tsc_frequency_hz = 100, TimeAbiCode::ManifestTime),
            (
                |c| c.apic_frequency_hz = 100_000_000,
                TimeAbiCode::ManifestTime,
            ),
            (
                |c| c.host_clock = "utc".to_owned(),
                TimeAbiCode::ManifestTime,
            ),
            (
                |c| {
                    c.host_id.pop();
                },
                TimeAbiCode::ManifestTime,
            ),
            (|c| c.host_boot_id.push(0), TimeAbiCode::ManifestTime),
            (
                |c| c.capture_generation = u32::MAX,
                TimeAbiCode::GenerationExhausted,
            ),
        ];
        for (mutate, expected) in mutations {
            let mut contract = test_time_contract();
            mutate(&mut contract);
            assert_eq!(
                code(validate_time_contract(&contract)),
                expected,
                "{contract:?}"
            );
        }
    }

    #[test]
    fn cpu_profile_record_rules() {
        validate_cpu_profile_record(&test_cpu_profile()).unwrap();
        let mutations: [(fn(&mut SnapshotCpuProfile), TimeAbiCode); 9] = [
            (|r| r.id.clear(), TimeAbiCode::ManifestTime),
            (|r| r.id = "Intel_SKX".to_owned(), TimeAbiCode::ManifestTime),
            (
                |r| r.id = "intel..kvm.v1".to_owned(),
                TimeAbiCode::ManifestTime,
            ),
            (
                |r| r.id = "intel.skylake-sp.kvm.v1.".to_owned(),
                TimeAbiCode::ManifestTime,
            ),
            (|r| r.profile.push(0), TimeAbiCode::ProfileDigest),
            (|r| r.sha256.truncate(31), TimeAbiCode::ManifestTime),
            (|r| r.effective_cpuid[0] = 1, TimeAbiCode::ProfileDigest),
            (|r| r.effective_cpuid.clear(), TimeAbiCode::ManifestTime),
            (
                |r| r.profile = vec![0; MAX_CPU_PROFILE_BYTES + 1],
                TimeAbiCode::ManifestTime,
            ),
        ];
        for (mutate, expected) in mutations {
            let mut record = test_cpu_profile();
            mutate(&mut record);
            assert_eq!(
                code(validate_cpu_profile_record(&record)),
                expected,
                "{}",
                record.id
            );
        }
    }

    #[test]
    fn capture_record_from_contract() {
        let record = capture_record(&test_time_contract()).unwrap();
        assert_eq!(record.tsc, 12_345_678);
        assert_eq!(record.sample.utc_ns, 1_700_000_000_000_000_000);
        assert_eq!(record.sample.monotonic_ns, 123_000_000_000);
        assert_eq!(record.identity.host_id, [1; 16]);
        assert_eq!(record.identity.boot_id, [2; 16]);
        assert_eq!(record.identity.clock, HostClockKind::LinuxBoottime);
    }

    fn version_6_manifest() -> SnapshotManifest {
        let mut manifest = crate::snapshot::tests::test_manifest();
        manifest.machine_contract = Some(crate::snapshot::microvm::test_machine_contract());
        set_time_abi_records(&mut manifest, test_time_contract(), test_cpu_profile()).unwrap();
        manifest
    }

    fn validate(manifest: &SnapshotManifest) -> anyhow::Result<()> {
        crate::snapshot::format::validate_manifest_header(manifest)?;
        crate::snapshot::format::validate_manifest_version(manifest)?;
        crate::snapshot::microvm::validate_microvm_machine_contract(
            manifest,
            &crate::snapshot::microvm::test_machine_contract(),
        )
    }

    #[test]
    fn version_6_manifest_is_accepted() {
        let manifest = version_6_manifest();
        assert_eq!(manifest.version, 6);
        assert_eq!(manifest.format_magic, b"OPENVMM_SNAPSHOT_V6\0");
        // The destination's pre-v6 clock fields are not compared.
        validate(&manifest).unwrap();
        let contract = manifest.machine_contract.as_ref().unwrap();
        assert_eq!(contract.tsc_frequency_hz, 0);
        assert!(contract.cpu_contract.is_empty() && contract.clock_policy.is_empty());
    }

    #[test]
    fn version_6_manifest_requires_records() {
        let mut manifest = version_6_manifest();
        manifest.machine_contract.as_mut().unwrap().time = None;
        let err = validate(&manifest).unwrap_err().to_string();
        assert!(err.contains("[E_MANIFEST_TIME]"), "{err}");

        let mut manifest = version_6_manifest();
        manifest.machine_contract.as_mut().unwrap().cpu_profile = None;
        let err = validate(&manifest).unwrap_err().to_string();
        assert!(err.contains("[E_MANIFEST_TIME]"), "{err}");

        let mut manifest = version_6_manifest();
        manifest.machine_contract = None;
        let err = validate(&manifest).unwrap_err().to_string();
        assert!(err.contains("[E_MANIFEST_TIME]"), "{err}");
    }

    #[test]
    fn version_6_manifest_rejects_retired_fields_and_clock_tokens() {
        let mut manifest = version_6_manifest();
        manifest.machine_contract.as_mut().unwrap().tsc_frequency_hz = 1;
        let err = validate(&manifest).unwrap_err().to_string();
        assert!(err.contains("retired clock field"), "{err}");

        let mut manifest = version_6_manifest();
        manifest
            .machine_contract
            .as_mut()
            .unwrap()
            .set_effective_command_line("console=hvc0 tsc_early_khz=2100000".to_owned());
        let err = validate(&manifest).unwrap_err().to_string();
        assert!(err.contains("[E_CMDLINE_CLOCK_TOKEN]"), "{err}");

        let mut manifest = version_6_manifest();
        manifest
            .machine_contract
            .as_mut()
            .unwrap()
            .time
            .as_mut()
            .unwrap()
            .tsc_tolerance_ppm = 100;
        let err = validate(&manifest).unwrap_err().to_string();
        assert!(err.contains("[E_MANIFEST_TIME]"), "{err}");
    }

    #[test]
    fn earlier_versions_cannot_carry_records() {
        let mut manifest = version_6_manifest();
        manifest.version = crate::snapshot::MANIFEST_VERSION;
        manifest.format_magic = crate::snapshot::format::SNAPSHOT_FORMAT_MAGIC.to_vec();
        let err = validate(&manifest).unwrap_err().to_string();
        assert!(err.contains("cannot carry time ABI records"), "{err}");
    }

    #[test]
    fn manifest_version_must_match_the_restore_path() {
        let version_6 = version_6_manifest();
        let version_5 = crate::snapshot::tests::test_manifest();
        check_time_abi_manifest_version(&version_6, true).unwrap();
        check_time_abi_manifest_version(&version_5, false).unwrap();
        for (manifest, time_abi) in [(&version_5, true), (&version_6, false)] {
            assert_eq!(
                check_time_abi_manifest_version(manifest, time_abi)
                    .unwrap_err()
                    .code,
                TimeAbiCode::SnapshotVersion
            );
        }
    }

    const SIGNATURE: u32 = 0x0005_0657;

    /// A version 6 machine contract captured on WHP with the interim profile,
    /// and a destination sample 5 s after its capture anchor on the same host
    /// boot.
    fn interim_restore() -> (SnapshotMachineContract, HostIdentity, HostTimeSample) {
        let effective_cpuid = vec![7; 48];
        let record = SnapshotCpuProfile {
            id: "interim.host.whp.v1".to_owned(),
            sha256: sha2::Sha256::digest(b"").to_vec(),
            profile: Vec::new(),
            effective_cpuid_sha256: sha2::Sha256::digest(&effective_cpuid).to_vec(),
            effective_cpuid,
            capture_cpu_signature: SIGNATURE,
        };
        let mut manifest = crate::snapshot::tests::test_manifest();
        manifest.machine_contract = Some(crate::snapshot::microvm::test_machine_contract());
        set_time_abi_records(&mut manifest, test_time_contract(), record).unwrap();
        let capture = capture_record(&test_time_contract()).unwrap();
        let now = HostTimeSample {
            utc_ns: capture.sample.utc_ns + 5_000_000_000,
            monotonic_ns: capture.sample.monotonic_ns + 5_000_000_000,
        };
        (manifest.machine_contract.unwrap(), capture.identity, now)
    }

    fn preflight(
        contract: &SnapshotMachineContract,
        hypervisor: &str,
        requested: &str,
        signature: Option<u32>,
        destination: &HostIdentity,
        now: &HostTimeSample,
        hooks: &TimeAbiTestHooks,
    ) -> Result<TimeAbiRestorePreflight, TimeAbiError> {
        preflight_time_abi_restore(
            contract,
            hypervisor,
            requested,
            signature,
            destination,
            now,
            hooks,
        )
    }

    #[test]
    fn restore_preflight_selects_the_downtime_and_generation() {
        let (contract, destination, now) = interim_restore();
        let hooks = TimeAbiTestHooks::default();
        for requested in ["auto", "interim.host.whp.v1"] {
            let result = preflight(
                &contract,
                "whp",
                requested,
                Some(SIGNATURE),
                &destination,
                &now,
                &hooks,
            )
            .unwrap();
            assert_eq!(result.generation, 4);
            assert_eq!(result.cpu_profile, "interim.host.whp.v1");
            assert_eq!(result.downtime.nanos, 5_000_000_000);
            assert_eq!(
                result.downtime.source,
                virt::time_abi::DowntimeSource::HostMonotonic
            );
        }

        let hooks = TimeAbiTestHooks {
            force_utc_downtime: true,
            ..Default::default()
        };
        let result = preflight(
            &contract,
            "whp",
            "auto",
            Some(SIGNATURE),
            &destination,
            &now,
            &hooks,
        )
        .unwrap();
        assert_eq!(result.downtime.source, virt::time_abi::DowntimeSource::Utc);
    }

    #[test]
    fn restore_preflight_failures_have_stable_codes() {
        let (contract, destination, now) = interim_restore();
        let hooks = TimeAbiTestHooks::default();
        let fail = |contract: &SnapshotMachineContract,
                    hypervisor: &str,
                    requested: &str,
                    signature: Option<u32>,
                    now: &HostTimeSample,
                    hooks: &TimeAbiTestHooks| {
            preflight(
                contract,
                hypervisor,
                requested,
                signature,
                &destination,
                now,
                hooks,
            )
            .unwrap_err()
            .code
        };

        assert_eq!(
            fail(&contract, "kvm", "auto", Some(SIGNATURE), &now, &hooks),
            TimeAbiCode::BackendMismatch
        );
        assert_eq!(
            fail(
                &contract,
                "whp",
                "intel.icelake-sp.whp.v1",
                Some(SIGNATURE),
                &now,
                &hooks
            ),
            TimeAbiCode::ProfileUnknown
        );
        assert_eq!(
            fail(&contract, "whp", "auto", Some(SIGNATURE + 1), &now, &hooks),
            TimeAbiCode::CpuGeneration
        );
        assert_eq!(
            fail(&contract, "whp", "auto", None, &now, &hooks),
            TimeAbiCode::CpuGeneration
        );

        let mut pinned = contract.clone();
        let record = pinned.cpu_profile.as_mut().unwrap();
        record.id = "intel.icelake-sp.whp.v1".to_owned();
        assert_eq!(
            fail(&pinned, "whp", "auto", Some(SIGNATURE), &now, &hooks),
            TimeAbiCode::ProfileUnknown
        );

        let mut document = contract.clone();
        let record = document.cpu_profile.as_mut().unwrap();
        record.profile = b"profile".to_vec();
        record.sha256 = sha2::Sha256::digest(&record.profile).to_vec();
        assert_eq!(
            fail(&document, "whp", "auto", Some(SIGNATURE), &now, &hooks),
            TimeAbiCode::ProfileDigest
        );

        let mut exhausted = contract.clone();
        exhausted.time.as_mut().unwrap().capture_generation = u32::MAX;
        assert_eq!(
            fail(&exhausted, "whp", "auto", Some(SIGNATURE), &now, &hooks),
            TimeAbiCode::GenerationExhausted
        );

        let rollback = HostTimeSample {
            utc_ns: now.utc_ns,
            monotonic_ns: now.monotonic_ns - 10_000_000_000,
        };
        assert_eq!(
            fail(&contract, "whp", "auto", Some(SIGNATURE), &rollback, &hooks),
            TimeAbiCode::DowntimeNegative
        );
        let excessive = TimeAbiTestHooks {
            downtime_add_s: 30 * 24 * 60 * 60,
            ..Default::default()
        };
        assert_eq!(
            fail(&contract, "whp", "auto", Some(SIGNATURE), &now, &excessive),
            TimeAbiCode::DowntimeExcessive
        );
    }
}
