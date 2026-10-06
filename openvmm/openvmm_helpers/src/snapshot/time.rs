// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! The NVX time ABI records of a snapshot manifest: the time contract and the
//! CPU profile record of a microVM machine contract.

use super::SnapshotManifest;
use super::microvm::SnapshotMachineContract;
use openvmm_defs::time_abi::SnapshotCpuProfile;
use openvmm_defs::time_abi::SnapshotTimeContract;
use openvmm_defs::time_abi::decode_effective_cpuid;
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

/// The largest canonical CPU profile encoding a manifest may carry.
pub const MAX_CPU_PROFILE_BYTES: usize = 1024 * 1024;

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

/// Validates a CPU profile record's shape: the ID, the 32-byte digest, the
/// document's size, and a well-formed effective CPUID record. It computes no
/// digest and decodes no document, because manifests are validated several
/// times per restore. The restore preflight compares the digest and the
/// document with the pinned profile's precomputed ones, and the worker
/// compares the effective CPUID record with its recomputation.
pub fn validate_cpu_profile_record(record: &SnapshotCpuProfile) -> Result<(), TimeAbiError> {
    let SnapshotCpuProfile {
        id,
        sha256,
        profile,
        effective_cpuid,
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
    if sha256.len() != 32 {
        return Err(manifest_error(format!(
            "CPU profile digest is {} bytes, not 32",
            sha256.len()
        )));
    }
    if profile.is_empty() || profile.len() > MAX_CPU_PROFILE_BYTES {
        return Err(manifest_error("CPU profile record size is invalid"));
    }
    decode_effective_cpuid(effective_cpuid)
        .map_err(|err| manifest_error(format!("effective CPUID record: {err}")))?;
    Ok(())
}

/// Returns the capture record of a validated time contract.
pub fn capture_record(contract: &SnapshotTimeContract) -> Result<CaptureTimeRecord, TimeAbiError> {
    validate_time_contract(contract)?;
    contract.capture_record()
}

/// Validates the time ABI records of a microVM machine contract: both are
/// present and valid, and the command line sets no clock parameter.
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
    virt::time_abi::check_command_line_clock_tokens(&contract.effective_command_line)
}

/// Returns the machine contract of a microVM snapshot's manifest, which
/// carries the time ABI records (`E_MANIFEST_TIME` when it is missing).
pub fn required_machine_contract(
    manifest: &SnapshotManifest,
) -> Result<&SnapshotMachineContract, TimeAbiError> {
    manifest.machine_contract.as_ref().ok_or_else(|| {
        manifest_error("the microVM snapshot has no machine contract, so no time ABI records")
    })
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
/// The snapshot's profile must be pinned in this OpenVMM with the same
/// digest, and the embedded document must be the pinned profile's canonical
/// encoding (`E_PROFILE_UNKNOWN`, `E_PROFILE_DIGEST`). Both are compared with
/// the build's precomputed record, without encoding or hashing. A host
/// profile (`--cpu-profile host`) has no pinned record, so its embedded
/// document is decoded and checked against the recorded digest instead
/// (`E_PROFILE_DIGEST`). `host` must be in the profile's generation. The
/// worker recomputes the effective CPUID and compares it with the record.
pub fn preflight_time_abi_restore(
    contract: &SnapshotMachineContract,
    hypervisor: &str,
    requested_cpu_profile: &str,
    host: Option<cpu_profile::HostCpuSignature>,
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
    let host_profile = cpu_profile::is_host_profile_id(&record.id);
    if requested_cpu_profile != cpu_profile::AUTO
        && requested_cpu_profile != record.id
        && !(host_profile && requested_cpu_profile == cpu_profile::HOST)
    {
        return Err(TimeAbiError::new(
            TimeAbiCode::ProfileUnknown,
            format!(
                "--cpu-profile {requested_cpu_profile} does not name the snapshot's profile '{}'",
                record.id
            ),
        ));
    }
    let host = host.ok_or_else(|| {
        TimeAbiError::new(
            TimeAbiCode::CpuGeneration,
            "the host CPU cannot be identified",
        )
    })?;
    if host_profile {
        let profile =
            cpu_profile::host_profile_for_restore(&record.id, &record.sha256, &record.profile)?;
        cpu_profile::check_generation(&profile, &host)?;
    } else {
        let profile = cpu_profile::pinned_for_restore(&record.id, &record.sha256)?;
        let pinned = cpu_profile::pinned_record(&record.id).ok_or_else(|| {
            TimeAbiError::new(
                TimeAbiCode::ProfileUnknown,
                format!("CPU profile {} is not pinned", record.id),
            )
        })?;
        if record.profile != pinned.encoding {
            return Err(TimeAbiError::new(
                TimeAbiCode::ProfileDigest,
                format!(
                    "the snapshot's embedded CPU profile is not the canonical encoding of the pinned profile {}",
                    record.id
                ),
            ));
        }
        cpu_profile::check_generation(profile, &host)?;
    }
    let capture = capture_record(time)?;
    let downtime = select_downtime(&capture, destination, now, hooks)?;
    Ok(TimeAbiRestorePreflight {
        capture,
        downtime,
        generation: time.capture_generation + 1,
        cpu_profile: record.id.clone(),
    })
}

/// Returns the vendor and signature of the host CPU, or `None` on a host that
/// is not x86-64.
pub fn host_cpu() -> Option<cpu_profile::HostCpuSignature> {
    // The host CPU is identified with the host's CPUID instruction.
    // xtask-fmt allow-target-arch cpu-intrinsic
    #[cfg(target_arch = "x86_64")]
    {
        Some(cpu_profile::HostCpuSignature::current())
    }
    // xtask-fmt allow-target-arch cpu-intrinsic
    #[cfg(not(target_arch = "x86_64"))]
    {
        None
    }
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

    /// The CPU profile of the test records.
    pub(in crate::snapshot) const TEST_PROFILE: &str = "intel.icelake-sp.v1";
    /// A host CPU signature in the test profile's generation.
    const TEST_SIGNATURE: u32 = 0x0006_06a6;

    /// Returns the effective CPUID of a partition with `vp_count` VPs and
    /// `profile`: the topology fields OpenVMM sets for one socket, of the
    /// profile's vendor, an xAPIC, and no identity leaves.
    fn test_effective_cpuid(
        profile: &cpu_profile::CpuProfile,
        vp_count: u32,
    ) -> cpu_profile::EffectiveCpuid {
        use cpu_profile::CpuVendor;
        use cpu_profile::CpuidResult;
        let mut vm = vec![
            CpuidResult::new(1, [0, vp_count << 16, 0, 0]).masked([0, 0xffff_0000, 0, 0]),
            cpu_profile::x2apic_cpuid(false),
        ];
        match profile.cpu_vendor() {
            CpuVendor::Intel => vm.push(
                CpuidResult::new(4, [(vp_count - 1) << 26, 0, 0, 0]).masked([0xffff_c000, 0, 0, 0]),
            ),
            CpuVendor::Amd => {
                let apic_id_size = vp_count.next_power_of_two().trailing_zeros();
                vm.push(
                    CpuidResult::new(0x8000_0008, [0, 0, (vp_count - 1) | apic_id_size << 12, 0])
                        .masked([0, 0, 0xf0ff, 0]),
                );
                // The L3 cache is the socket's; the others are each VP's.
                for subleaf in 0.. {
                    let eax = profile.lookup(0x8000_001d, subleaf)[0];
                    if eax & 0x1f == 0 {
                        break;
                    }
                    let sharing = if (eax >> 5) & 7 == 3 { vp_count - 1 } else { 0 };
                    vm.push(
                        CpuidResult::new(0x8000_001d, [sharing << 14, 0, 0, 0])
                            .indexed(subleaf)
                            .masked([0x03ff_c000, 0, 0, 0]),
                    );
                }
                vm.push(CpuidResult::new(0x8000_001e, [0; 4]).masked([!0, 0xffff, 0x7ff, 0]));
            }
        }
        let max_basic = profile.lookup(0, 0)[0];
        for leaf in cpu_profile::VM_OWNED_LEAVES
            .into_iter()
            .filter(|&leaf| leaf <= max_basic)
        {
            vm.push(CpuidResult::new(leaf, [0, 1, 0x100, 0]).indexed(0));
            vm.push(CpuidResult::new(leaf, [0, vp_count, 0x201, 0]).indexed(1));
        }
        profile.effective_cpuid(&vm, &[]).unwrap()
    }

    /// Returns the snapshot record of `effective`.
    fn effective_cpuid_record(effective: &cpu_profile::EffectiveCpuid) -> Vec<u8> {
        openvmm_defs::time_abi::encode_effective_cpuid(effective.results().map(|result| {
            openvmm_defs::time_abi::EffectiveCpuidEntry {
                function: result.function,
                index: result.index,
                result: result.result,
                mask: result.mask,
            }
        }))
    }

    pub(in crate::snapshot) fn test_cpu_profile() -> SnapshotCpuProfile {
        let pinned = cpu_profile::pinned(TEST_PROFILE).unwrap();
        let record = cpu_profile::pinned_record(TEST_PROFILE).unwrap();
        SnapshotCpuProfile {
            id: TEST_PROFILE.to_owned(),
            sha256: record.digest.to_vec(),
            profile: record.encoding.to_vec(),
            effective_cpuid: effective_cpuid_record(&test_effective_cpuid(pinned, 2)),
            capture_cpu_signature: TEST_SIGNATURE,
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
        let mutations: [(fn(&mut SnapshotCpuProfile), TimeAbiCode); 11] = [
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
            (|r| r.profile.clear(), TimeAbiCode::ManifestTime),
            (|r| r.sha256.truncate(31), TimeAbiCode::ManifestTime),
            (|r| r.sha256.push(0), TimeAbiCode::ManifestTime),
            (|r| r.effective_cpuid.clear(), TimeAbiCode::ManifestTime),
            (
                |r| {
                    r.effective_cpuid.pop();
                },
                TimeAbiCode::ManifestTime,
            ),
            // The subleaf flag of the first entry.
            (|r| r.effective_cpuid[4] = 2, TimeAbiCode::ManifestTime),
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

        // Validation checks only the shape. The restore preflight rejects a
        // document or digest that is not the named pinned profile's, and the
        // worker an effective CPUID record that is not this partition's.
        let mut renamed = test_cpu_profile();
        renamed.id = "intel.skylake-sp.v1".to_owned();
        validate_cpu_profile_record(&renamed).unwrap();
        let mut other_document = test_cpu_profile();
        other_document.profile.push(b' ');
        validate_cpu_profile_record(&other_document).unwrap();
        let mut other_cpuid = test_cpu_profile();
        other_cpuid.effective_cpuid[12] ^= 1;
        validate_cpu_profile_record(&other_cpuid).unwrap();
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
        manifest
    }

    fn validate(manifest: &SnapshotManifest) -> anyhow::Result<()> {
        crate::snapshot::format::validate_manifest_header(manifest)?;
        crate::snapshot::format::validate_manifest_contents(manifest)?;
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
        validate(&manifest).unwrap();
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

        // A manifest without a machine contract is a regular VM's; a microVM
        // restore requires the contract.
        let mut manifest = version_6_manifest();
        manifest.machine_contract = None;
        crate::snapshot::format::validate_manifest_contents(&manifest).unwrap();
        let err = required_machine_contract(&manifest).unwrap_err();
        assert_eq!(err.code, TimeAbiCode::ManifestTime);
    }

    #[test]
    fn version_6_manifest_rejects_clock_tokens_and_invalid_records() {
        for token in ["tsc_early_khz=2100000", "lapic_timer_hz=200000000"] {
            let mut manifest = version_6_manifest();
            manifest
                .machine_contract
                .as_mut()
                .unwrap()
                .set_effective_command_line(format!("console=hvc0 {token}"));
            let err = validate(&manifest).unwrap_err().to_string();
            assert!(err.contains("[E_CMDLINE_CLOCK_TOKEN]"), "{err}");
        }

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

    /// A version 6 machine contract captured on WHP with the test profile,
    /// and a destination sample 5 s after its capture anchor on the same host
    /// boot.
    fn test_restore() -> (SnapshotMachineContract, HostIdentity, HostTimeSample) {
        let capture = capture_record(&test_time_contract()).unwrap();
        let now = HostTimeSample {
            utc_ns: capture.sample.utc_ns + 5_000_000_000,
            monotonic_ns: capture.sample.monotonic_ns + 5_000_000_000,
        };
        (
            crate::snapshot::microvm::test_machine_contract(),
            capture.identity,
            now,
        )
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
            signature
                .map(|signature| cpu_profile::HostCpuSignature::new(*b"GenuineIntel", signature)),
            destination,
            now,
            hooks,
        )
    }

    #[test]
    fn restore_preflight_selects_the_downtime_and_generation() {
        let (contract, destination, now) = test_restore();
        let hooks = TimeAbiTestHooks::default();
        for requested in ["auto", TEST_PROFILE] {
            let result = preflight(
                &contract,
                "whp",
                requested,
                Some(TEST_SIGNATURE),
                &destination,
                &now,
                &hooks,
            )
            .unwrap();
            assert_eq!(result.generation, 4);
            assert_eq!(result.cpu_profile, TEST_PROFILE);
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
            Some(TEST_SIGNATURE),
            &destination,
            &now,
            &hooks,
        )
        .unwrap();
        assert_eq!(result.downtime.source, virt::time_abi::DowntimeSource::Utc);
    }

    #[test]
    fn restore_preflight_failures_have_stable_codes() {
        let (contract, destination, now) = test_restore();
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
        let host = Some(TEST_SIGNATURE);

        assert_eq!(
            fail(&contract, "kvm", "auto", host, &now, &hooks),
            TimeAbiCode::BackendMismatch
        );
        assert_eq!(
            fail(&contract, "whp", "intel.skylake-sp.v1", host, &now, &hooks),
            TimeAbiCode::ProfileUnknown
        );
        // A Skylake-SP host, and a host whose CPU cannot be identified.
        assert_eq!(
            fail(&contract, "whp", "auto", Some(0x0005_0654), &now, &hooks),
            TimeAbiCode::CpuGeneration
        );
        assert_eq!(
            fail(&contract, "whp", "auto", None, &now, &hooks),
            TimeAbiCode::CpuGeneration
        );

        // A snapshot that recorded an interim profile, before CPU profiles.
        let mut interim = contract.clone();
        let record = interim.cpu_profile.as_mut().unwrap();
        record.id = "interim.host.whp.v1".to_owned();
        record.profile = Vec::new();
        record.sha256 = vec![0; 32];
        record.effective_cpuid = vec![7; 48];
        assert_eq!(
            fail(&interim, "whp", "auto", host, &now, &hooks),
            TimeAbiCode::ManifestTime
        );

        // A profile that this OpenVMM does not pin.
        let mut unknown = contract.clone();
        let record = unknown.cpu_profile.as_mut().unwrap();
        record.id = "intel.icelake-sp.v9".to_owned();
        record.profile = String::from_utf8(record.profile.clone())
            .unwrap()
            .replace("\"intel.icelake-sp.v1\"", "\"intel.icelake-sp.v9\"")
            .into_bytes();
        record.sha256 = vec![9; 32];
        assert_eq!(
            fail(&unknown, "whp", "auto", host, &now, &hooks),
            TimeAbiCode::ProfileUnknown
        );

        // The record's digest and document are another pinned profile's.
        let mut swapped = contract.clone();
        let record = swapped.cpu_profile.as_mut().unwrap();
        let other = cpu_profile::pinned_record("intel.skylake-sp.v1").unwrap();
        record.profile = other.encoding.to_vec();
        record.sha256 = other.digest.to_vec();
        assert_eq!(
            fail(&swapped, "whp", "auto", host, &now, &hooks),
            TimeAbiCode::ProfileDigest
        );

        // The record names another pinned profile than its digest.
        let mut renamed = contract.clone();
        renamed.cpu_profile.as_mut().unwrap().id = "intel.skylake-sp.v1".to_owned();
        assert_eq!(
            fail(&renamed, "whp", "auto", host, &now, &hooks),
            TimeAbiCode::ProfileDigest
        );

        // The digest is the pinned profile's, but the document is not its
        // canonical encoding.
        let mut altered = contract.clone();
        altered.cpu_profile.as_mut().unwrap().profile.push(b' ');
        assert_eq!(
            fail(&altered, "whp", "auto", host, &now, &hooks),
            TimeAbiCode::ProfileDigest
        );

        let mut exhausted = contract.clone();
        exhausted.time.as_mut().unwrap().capture_generation = u32::MAX;
        assert_eq!(
            fail(&exhausted, "whp", "auto", host, &now, &hooks),
            TimeAbiCode::GenerationExhausted
        );

        let rollback = HostTimeSample {
            utc_ns: now.utc_ns,
            monotonic_ns: now.monotonic_ns - 10_000_000_000,
        };
        assert_eq!(
            fail(&contract, "whp", "auto", host, &rollback, &hooks),
            TimeAbiCode::DowntimeNegative
        );
        let excessive = TimeAbiTestHooks {
            downtime_add_s: 30 * 24 * 60 * 60,
            ..Default::default()
        };
        assert_eq!(
            fail(&contract, "whp", "auto", host, &now, &excessive),
            TimeAbiCode::DowntimeExcessive
        );
        // `--cpu-profile host` names no pinned profile.
        assert_eq!(
            fail(&contract, "whp", cpu_profile::HOST, host, &now, &hooks),
            TimeAbiCode::ProfileUnknown
        );
    }

    /// Returns the host profile that `--cpu-profile host` derives on a WHP
    /// host of the test profile's generation whose surface is the test
    /// profile's.
    fn test_host_profile() -> cpu_profile::CpuProfile {
        let pinned = cpu_profile::pinned(TEST_PROFILE).unwrap();
        let cpuid = pinned
            .cpuid()
            .iter()
            .map(|entry| {
                let (leaf, subleaf) = entry.key();
                cpu_profile::cpuid::CpuidEntry::new(leaf, subleaf, entry.values())
            })
            .collect();
        let host = cpu_profile::host::HostIdentity {
            cpu: cpu_profile::host::HostCpu {
                vendor: "GenuineIntel".to_owned(),
                signature: cpu_profile::Hex32(TEST_SIGNATURE),
                family: 6,
                model: 106,
                stepping: 6,
                brand: String::new(),
                microcode: Vec::new(),
                invariant_tsc: true,
                tsc_deadline: true,
                tsc_adjust: true,
            },
            os: cpu_profile::host::HostOs {
                kind: "windows".to_owned(),
                release: None,
                version: None,
                cpu_flags: Vec::new(),
                clocksource: None,
                available_clocksources: Vec::new(),
            },
            hypervisor: None,
        };
        let fingerprint = cpu_profile::fingerprint::CpuFingerprint::new(
            cpu_profile::fingerprint::ToolIdentity {
                name: "test".to_owned(),
                version: "0".to_owned(),
            },
            host,
            cpu_profile::fingerprint::BackendFingerprint::new("whp", "test", cpuid),
        );
        cpu_profile::derive_host_profile(&fingerprint).unwrap()
    }

    #[test]
    fn restore_preflight_checks_a_recorded_host_profile() {
        let (mut contract, destination, now) = test_restore();
        let hooks = TimeAbiTestHooks::default();
        let profile = test_host_profile();
        assert_eq!(profile.id(), "intel.host.v1");
        *contract.cpu_profile.as_mut().unwrap() = SnapshotCpuProfile {
            id: profile.id().to_owned(),
            sha256: profile.digest().to_vec(),
            profile: profile.encode(),
            effective_cpuid: effective_cpuid_record(&test_effective_cpuid(&profile, 2)),
            capture_cpu_signature: TEST_SIGNATURE,
        };
        let result = |contract: &SnapshotMachineContract, requested: &str, signature: u32| {
            preflight(
                contract,
                "whp",
                requested,
                Some(signature),
                &destination,
                &now,
                &hooks,
            )
        };
        for requested in ["auto", cpu_profile::HOST, profile.id()] {
            let restored = result(&contract, requested, TEST_SIGNATURE).unwrap();
            assert_eq!(restored.cpu_profile, profile.id());
            assert_eq!(restored.generation, 4);
        }
        let code = |result: Result<TimeAbiRestorePreflight, TimeAbiError>| result.unwrap_err().code;
        // A pinned profile is not the snapshot's.
        assert_eq!(
            code(result(&contract, TEST_PROFILE, TEST_SIGNATURE)),
            TimeAbiCode::ProfileUnknown
        );
        // A host profile serves only its own CPU model and stepping.
        assert_eq!(
            code(result(&contract, "auto", TEST_SIGNATURE + 1)),
            TimeAbiCode::CpuGeneration
        );
        // The digest must match the embedded document.
        let mut corrupted = contract.clone();
        corrupted.cpu_profile.as_mut().unwrap().sha256[0] ^= 1;
        assert_eq!(
            code(result(&corrupted, "auto", TEST_SIGNATURE)),
            TimeAbiCode::ProfileDigest
        );
        // A pinned profile's document under the host profile's ID.
        let mut swapped = contract.clone();
        let record = swapped.cpu_profile.as_mut().unwrap();
        let pinned = cpu_profile::pinned_record(TEST_PROFILE).unwrap();
        record.profile = pinned.encoding.to_vec();
        record.sha256 = pinned.digest.to_vec();
        assert_eq!(
            code(result(&swapped, "auto", TEST_SIGNATURE)),
            TimeAbiCode::ProfileDigest
        );
    }

    #[test]
    fn effective_cpuid_records_fit_the_manifest() {
        for profile in cpu_profile::pinned_profiles() {
            let record = effective_cpuid_record(&test_effective_cpuid(profile, 8));
            let entries = record.len() / openvmm_defs::time_abi::EFFECTIVE_CPUID_ENTRY_BYTES;
            assert!(
                entries < openvmm_defs::time_abi::MAX_EFFECTIVE_CPUID_ENTRIES / 4,
                "{}: {entries} entries",
                profile.id()
            );
            let pinned = cpu_profile::pinned_record(profile.id()).unwrap();
            assert!(pinned.encoding.len() < MAX_CPU_PROFILE_BYTES / 16);
        }
    }
}
