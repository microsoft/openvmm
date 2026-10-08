// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! NVX time ABI v1 in the VM worker, which every microVM uses: the partition
//! configuration, the backend preflight and rate policy, the `time-abi` state
//! unit, the capture records, and the restore clock.

#![cfg(guest_arch = "x86_64")]

use super::LoadedVm;
use crate::partition::HvlitePartition;
use anyhow::Context as _;
use chipset_resources::microvm_time::RestoreTimeRecord;
use cpu_profile::CpuProfile;
use cpu_profile::EffectiveCpuid;
use cpu_profile::HostCpuSignature;
use cpu_profile::PartitionProfile;
use cpu_profile::ProfileError;
use cpu_profile::ProfileErrorCode;
use cpu_profile::fingerprint::CpuFingerprint;
use futures_concurrency::future::Join;
use inspect::Inspect;
use inspect::InspectMut;
use openvmm_defs::time_abi::EffectiveCpuidEntry;
use openvmm_defs::time_abi::RestoreTimeInput;
use openvmm_defs::time_abi::SnapshotCpuProfile;
use openvmm_defs::time_abi::SnapshotTimeContract;
use openvmm_defs::time_abi::TimeCapture;
use openvmm_defs::time_abi::decode_effective_cpuid;
use openvmm_defs::time_abi::encode_effective_cpuid;
use state_unit::SpawnedUnit;
use state_unit::StateUnit;
use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;
use virt::CpuidLeaf;
use virt::CpuidLeafSet;
use virt::time_abi::DeclaredRates;
use virt::time_abi::DowntimeSource;
use virt::time_abi::IdentityMsrRoute;
use virt::time_abi::RateCheck;
use virt::time_abi::TIME_ABI_VERSION;
use virt::time_abi::TimeAbiBackend;
use virt::time_abi::TimeAbiCode;
use virt::time_abi::TimeAbiConfig;
use virt::time_abi::TimeAbiError;
use virt::time_abi::TimeAbiMsrs;
use virt::time_abi::TimeAbiTestHooks;
use virt::time_abi::TscSyncMethod;
use virt::time_abi::surface::SupportedCpuSurface;
use vmcore::save_restore::RestoreError;
use vmcore::save_restore::SaveError;
use vmcore::save_restore::SavedStateBlob;

/// The name of the time ABI state unit.
pub(super) const TIME_ABI_UNIT: &str = "time-abi";

/// The most differences an `E_CPU_SURFACE` read-back failure names.
const MAX_REPORTED_CPUID_DIFFERENCES: usize = 16;

/// The time ABI state of a loaded VM.
pub(super) struct TimeAbiState {
    /// The `time-abi` state unit.
    pub _unit: SpawnedUnit<TimeAbiUnit>,
    /// The identity MSR handler, holding the declared rates.
    pub msrs: Arc<TimeAbiMsrs>,
    /// The CPU profile.
    pub profile: PartitionProfile,
    /// The effective CPUID of the partition.
    pub effective_cpuid: Arc<EffectiveCpuid>,
    /// The preflight report.
    pub report: TimeAbiReport,
    /// The generation counter of this VM process.
    pub generation: u32,
    /// The active test hooks.
    pub hooks: TimeAbiTestHooks,
}

/// The time ABI of a new partition, set up before the partition is created.
pub(super) struct PartitionTimeAbi {
    /// The identity MSR handler.
    pub msrs: Arc<TimeAbiMsrs>,
    /// The CPU profile.
    pub profile: PartitionProfile,
    /// The effective CPUID of the partition.
    pub effective_cpuid: Arc<EffectiveCpuid>,
}

/// Returns the identity MSR handler and the time ABI configuration of a new
/// partition with the CPU profile `profile` and its effective CPUID.
pub(super) fn partition_config(
    profile: &PartitionProfile,
    effective_cpuid: &EffectiveCpuid,
) -> (Arc<TimeAbiMsrs>, TimeAbiConfig) {
    let msrs = Arc::new(TimeAbiMsrs::new());
    let config = TimeAbiConfig {
        cpuid: Arc::new(virt::time_abi::cpuid::backend_cpuid(effective_cpuid)),
        msrs: msrs.clone(),
        cpu_profile: profile.clone(),
    };
    (msrs, config)
}

/// Selects the CPU profile of a new partition before it is created.
///
/// `requested` is `--cpu-profile` on cold boot (`auto`, `host`, or an ID),
/// or the profile a snapshot recorded on restore. An ID selects a pinned
/// profile whose generation contains the host CPU (`E_PROFILE_UNKNOWN`,
/// `E_CPU_GENERATION`), and `auto` the pinned profile of the host CPU's
/// generation. On a CPU that no pinned profile serves, `auto` falls back to
/// the host profile that `host` selects where host profiles serve the CPU,
/// and warns ([`fall_back`]), and fails elsewhere
/// (`E_PROFILE_HOST_UNKNOWN`). The `host-cpu-unknown` test hook in `hooks`
/// makes `auto` treat any CPU so. `host` derives a host profile from the
/// `hypervisor` backend's fingerprint of this host. A host profile's ID
/// selects the profile of `restored`, the record of the snapshot being
/// restored, which carries the profile's only copy
/// ([`restored_host_profile`]).
pub(super) fn select_cpu_profile(
    requested: &str,
    hypervisor: &str,
    restored: Option<&SnapshotCpuProfile>,
    hooks: &TimeAbiTestHooks,
) -> anyhow::Result<PartitionProfile> {
    let host = host_cpu()?;
    if requested == cpu_profile::HOST {
        let started = std::time::Instant::now();
        let fingerprint = host_fingerprint(hypervisor)?;
        let profile = host_partition_profile(&fingerprint, &host)?;
        tracing::warn!(
            "{}",
            host_profile_warning(
                HostProfileOrigin::Requested,
                &fingerprint,
                &profile,
                started.elapsed(),
                !cpu_profile::pinned_profile_serves(&host),
            )
        );
        return Ok(profile);
    }
    match select_on_host(requested, &host, restored, hooks)? {
        Selection::Profile(profile) => Ok(profile),
        Selection::HostFallback => fall_back(&host, || host_fingerprint(hypervisor)),
    }
}

/// What [`select_on_host`] selects.
enum Selection {
    /// A pinned profile, or the host profile of the snapshot being restored.
    Profile(PartitionProfile),
    /// `auto`'s fallback to a host profile, which [`fall_back`] derives.
    HostFallback,
}

/// Selects the CPU profile `requested` on the host CPU `host`, as
/// [`select_cpu_profile`] does for any request but `host`.
fn select_on_host(
    requested: &str,
    host: &HostCpuSignature,
    restored: Option<&SnapshotCpuProfile>,
    hooks: &TimeAbiTestHooks,
) -> Result<Selection, ProfileError> {
    if cpu_profile::is_host_profile_id(requested) {
        let record = restored
            .filter(|record| record.id == requested)
            .ok_or_else(|| {
                ProfileError::new(
                    ProfileErrorCode::ProfileUnknown,
                    format!(
                        "CPU profile {requested:?} is a host profile, which only a snapshot of it \
                         carries"
                    ),
                )
            })?;
        let profile = PartitionProfile::host(cpu_profile::host_profile_for_restore(
            &record.id,
            &record.sha256,
            &record.profile,
        )?)?;
        cpu_profile::check_generation(&profile, host)?;
        return Ok(Selection::Profile(profile));
    }
    if requested == cpu_profile::AUTO && falls_back(host, hooks) {
        return Ok(Selection::HostFallback);
    }
    // Where `auto` still fails with `E_PROFILE_HOST_UNKNOWN` on a CPU that
    // host profiles serve, pinned profiles of more than one generation serve
    // it, a catalog defect, which `--cpu-profile host` avoids.
    cpu_profile::select(requested, host)
        .map(Selection::Profile)
        .map_err(|mut err| {
            if requested == cpu_profile::AUTO
                && err.code == ProfileErrorCode::ProfileHostUnknown
                && cpu_profile::supports_host_profiles(host)
            {
                err.message.push_str("; ");
                err.message.push_str(HOST_PROFILE_HINT);
            }
            err
        })
}

/// Returns whether a cold boot with `--cpu-profile auto` falls back to a host
/// profile on the host CPU `host`: no pinned profile serves the CPU, or the
/// `host-cpu-unknown` test hook in `hooks` treats it so, and host profiles
/// serve it.
fn falls_back(host: &HostCpuSignature, hooks: &TimeAbiTestHooks) -> bool {
    (hooks.host_cpu_unknown || !cpu_profile::pinned_profile_serves(host))
        && cpu_profile::supports_host_profiles(host)
}

/// Falls back from `--cpu-profile auto` to a host profile on the host CPU
/// `host`: derives the profile from this host's fingerprint, which
/// `fingerprint` takes, as `--cpu-profile host` does, and warns at the
/// default log level with [`FALLBACK_MARKER`] ([`host_profile_warning`]),
/// naming what the fingerprint and the derivation cost.
///
/// A failure, the fingerprint's included, says that `auto` fell back
/// ([`FellBack`]), and keeps its code.
fn fall_back(
    host: &HostCpuSignature,
    fingerprint: impl FnOnce() -> anyhow::Result<CpuFingerprint>,
) -> anyhow::Result<PartitionProfile> {
    let started = std::time::Instant::now();
    fingerprint()
        .and_then(|fingerprint| {
            let profile = host_partition_profile(&fingerprint, host)?;
            tracing::warn!(
                "{}",
                host_profile_warning(
                    HostProfileOrigin::Fallback,
                    &fingerprint,
                    &profile,
                    started.elapsed(),
                    true,
                )
            );
            Ok(profile)
        })
        .map_err(|err| err.context(FellBack::new(host)))
}

/// The context of a failed cold boot whose `--cpu-profile auto` fell back to
/// a host profile ([`fall_back`]), so that its error says so.
#[derive(Debug)]
struct FellBack {
    /// The host CPU, as [`describe_signature`] names it.
    cpu: String,
}

impl FellBack {
    fn new(host: &HostCpuSignature) -> Self {
        Self {
            cpu: describe_signature(host),
        }
    }
}

impl std::fmt::Display for FellBack {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "--cpu-profile auto fell back to a host CPU profile, because no built-in CPU profile \
             serves this host's CPU, {}, but this host cannot boot one",
            self.cpu
        )
    }
}

/// The marker that leads the warning of every cold boot whose
/// `--cpu-profile auto` falls back to a host profile ([`fall_back`]), which
/// NVX's tools and log scrapers match.
const FALLBACK_MARKER: &str = "NVX-CPU-PROFILE-FALLBACK:";

/// NVX's issue form that requests a built-in CPU profile for a CPU.
const CPU_PROFILE_REQUEST_URL: &str =
    "https://github.com/microsoft/nvx/issues/new?template=cpu-profile.yml";

/// How a cold boot came to use a host profile.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum HostProfileOrigin {
    /// `--cpu-profile auto` fell back to it.
    Fallback,
    /// `--cpu-profile host` selected it.
    Requested,
}

/// Returns the warning of a cold boot on `profile`, a host profile that
/// `origin` selected from `fingerprint`, this host's fingerprint: the host
/// CPU, the profile and its digest, and the limits of a host profile,
/// including `cost`, what this cold boot's fingerprint and derivation took.
/// With `invite`, for a CPU that no pinned profile serves, it also asks the
/// user to request a built-in profile for the CPU with the CPU's
/// fingerprint, through NVX's issue form, which the link prefills with the
/// CPU.
fn host_profile_warning(
    origin: HostProfileOrigin,
    fingerprint: &CpuFingerprint,
    profile: &PartitionProfile,
    cost: Duration,
    invite: bool,
) -> String {
    let cpu = describe_cpu(&fingerprint.host.cpu);
    let backend = &fingerprint.backend.name;
    let selected = format!(
        "{} (sha256:{}), a profile derived from this host's {backend} backend for development",
        profile.id(),
        hex(&profile.record().digest)
    );
    let mut warning = match origin {
        HostProfileOrigin::Fallback => format!(
            "{FALLBACK_MARKER} no built-in CPU profile serves this host's CPU, {cpu}, so \
             --cpu-profile auto fell back to {selected}."
        ),
        HostProfileOrigin::Requested if invite => format!(
            "--cpu-profile host selected {selected}; no built-in CPU profile serves this \
             host's CPU, {cpu}."
        ),
        HostProfileOrigin::Requested => format!("--cpu-profile host selected {selected}."),
    };
    warning.push_str(&format!(
        "\nIt is not pinned: a microcode, firmware, hypervisor, or OS update can change it. \
         Each cold boot fingerprints the backend first, which took {:.1} ms here, and its \
         snapshots restore only on hosts of the same CPU model and stepping whose hypervisor \
         supports it.",
        cost.as_secs_f64() * 1000.0
    ));
    if invite {
        warning.push_str(&format!(
            "\nHelp NVX add a built-in profile for this CPU: run\n  \
             openvmm --hypervisor {backend} --cpu-fingerprint fingerprint.json\n\
             and attach fingerprint.json to a CPU profile request:\n  {}",
            cpu_profile_request_url(&fingerprint.host.cpu)
        ));
    }
    warning
}

/// Returns the link to NVX's CPU profile request form for the host CPU `cpu`,
/// which prefills its title and CPU fields.
fn cpu_profile_request_url(cpu: &cpu_profile::host::HostCpu) -> String {
    let mut url = format!(
        "{CPU_PROFILE_REQUEST_URL}&title={}&signature={}",
        encode_query_value(&format!("CPU profile: {}", describe_cpu(cpu))),
        encode_query_value(&cpu_signature(
            &cpu.vendor,
            cpu.family,
            cpu.model,
            cpu.stepping
        ))
    );
    if !cpu.brand.is_empty() {
        url.push_str("&cpu=");
        url.push_str(&encode_query_value(&cpu.brand));
    }
    url
}

/// Percent-encodes `value` for a URL query: every byte but ASCII letters,
/// digits, `-`, `.`, `_`, and `~`.
fn encode_query_value(value: &str) -> String {
    let mut encoded = String::with_capacity(value.len());
    for byte in value.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'.' | b'_' | b'~') {
            encoded.push(char::from(byte));
        } else {
            encoded.push_str(&format!("%{byte:02X}"));
        }
    }
    encoded
}

/// Returns `bytes` in lowercase hexadecimal.
fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

/// Names a CPU's signature as NVX does: its vendor string and its display
/// family, model, and stepping, such as `GenuineIntel 6/173/1`.
fn cpu_signature(vendor: &str, family: u32, model: u32, stepping: u32) -> String {
    format!("{vendor} {family}/{model}/{stepping}")
}

/// Names a host CPU as NVX does: its signature ([`cpu_signature`]) and its
/// brand string, if any, such as
/// `GenuineIntel 6/173/1 (Intel(R) Xeon(R) 6973P-C)`.
fn describe_cpu(cpu: &cpu_profile::host::HostCpu) -> String {
    let signature = cpu_signature(&cpu.vendor, cpu.family, cpu.model, cpu.stepping);
    if cpu.brand.is_empty() {
        signature
    } else {
        format!("{signature} ({})", cpu.brand)
    }
}

/// Names the host CPU `host` by its signature ([`cpu_signature`]), without
/// the brand string, which a signature lacks.
fn describe_signature(host: &HostCpuSignature) -> String {
    cpu_signature(
        &host.vendor_str(),
        host.family(),
        host.model(),
        host.stepping(),
    )
}

/// What a failed cold boot with `--cpu-profile auto` suggests where host
/// profiles serve the host CPU and a host profile could boot.
const HOST_PROFILE_HINT: &str =
    "--cpu-profile host boots on a profile derived from this host, for development";

/// Returns whether `code` is a CPU profile failure of a cold boot, after the
/// selection: the backend lacks the profile, or does not present it.
fn is_profile_failure(code: TimeAbiCode) -> bool {
    matches!(
        code,
        TimeAbiCode::ProfileUnsupported
            | TimeAbiCode::ProfileTimeBits
            | TimeAbiCode::CpuSurface
            | TimeAbiCode::CpuUnlisted
    )
}

/// Explains `err`, the failure of a VM worker that cold booted with
/// `--cpu-profile requested` and the test hooks `hooks` on the host CPU
/// `host`, where its CPU profile failed.
///
/// Where `auto` fell back to a host profile ([`falls_back`]), a CPU profile
/// failure says so ([`FellBack`]), unless the fallback itself already did.
/// Elsewhere, `auto` names `--cpu-profile host` where a host profile could
/// boot instead: `auto` selected a built-in profile that this host's backend
/// does not support (`E_PROFILE_UNSUPPORTED`), and host profiles serve the
/// CPU's vendor. A host profile derives from what the backend supports. An
/// explicit profile ID and `host` get no hint.
///
/// The hint is a context of `err`, which keeps the backend's error chain:
/// the code, where the backend found the shortfall, and what it lacks.
fn hint_host_profile(
    err: anyhow::Error,
    requested: &str,
    host: &HostCpuSignature,
    hooks: &TimeAbiTestHooks,
) -> anyhow::Error {
    if requested != cpu_profile::AUTO {
        return err;
    }
    let code = leading_code(&err);
    if falls_back(host, hooks) {
        if err.downcast_ref::<FellBack>().is_some() || !code.is_some_and(is_profile_failure) {
            return err;
        }
        return err.context(FellBack::new(host));
    }
    if !cpu_profile::supports_host_profiles(host) || code != Some(TimeAbiCode::ProfileUnsupported) {
        return err;
    }
    err.context(format!(
        "this host cannot boot the built-in CPU profile that --cpu-profile auto selected; \
         {HOST_PROFILE_HINT}"
    ))
}

/// Returns `err`, the failure of a VM worker, with the hint of
/// [`hint_host_profile`] if the worker cold booted a time ABI microVM with
/// the `--cpu-profile` and test hooks of `requested` on this host. A restore
/// (`restoring`), which takes the snapshot's profile, gets none.
pub(super) fn hint_cold_boot(
    err: anyhow::Error,
    requested: Option<(&str, &TimeAbiTestHooks)>,
    restoring: bool,
) -> anyhow::Error {
    match (requested, restoring, host_cpu()) {
        (Some((requested, hooks)), false, Ok(host)) => {
            hint_host_profile(err, requested, &host, hooks)
        }
        _ => err,
    }
}

/// Returns the time ABI code that OpenVMM's exit message gives `err`: the
/// first bracketed code in its chain.
fn leading_code(err: &anyhow::Error) -> Option<TimeAbiCode> {
    let chain = format!("{err:#}");
    chain.match_indices("[E_").find_map(|(start, _)| {
        let code = &chain[start + 1..];
        TimeAbiCode::from_name(&code[..code.find(']')?])
    })
}

/// Fingerprints this host and the `hypervisor` backend, as
/// `--cpu-fingerprint` does, for a host profile.
fn host_fingerprint(hypervisor: &str) -> anyhow::Result<CpuFingerprint> {
    let probe = hypervisor_resources::probe_by_name(hypervisor)
        .with_context(|| format!("the {hypervisor} backend cannot be fingerprinted"))?;
    let backend = probe
        .cpu_fingerprint(&[])
        .with_context(|| format!("failed to fingerprint the {hypervisor} backend"))?;
    let host = cpu_profile::host::HostIdentity::collect().context("failed to identify the host")?;
    Ok(CpuFingerprint::new(
        cpu_profile::fingerprint::ToolIdentity {
            name: "openvmm".to_owned(),
            version: env!("CARGO_PKG_VERSION").to_owned(),
        },
        host,
        backend,
    ))
}

/// Derives the host profile of `fingerprint`, this host's fingerprint, under
/// the profiles' derivation policy, and checks that the host CPU `host` is in
/// its generation.
fn host_partition_profile(
    fingerprint: &CpuFingerprint,
    host: &HostCpuSignature,
) -> anyhow::Result<PartitionProfile> {
    let profile = PartitionProfile::host(cpu_profile::derive_host_profile(fingerprint)?)?;
    cpu_profile::check_generation(&profile, host)?;
    Ok(profile)
}

/// Returns the CPU profile record of a snapshot that the worker restores, if
/// its profile is a host profile: the snapshot carries the profile's only
/// copy, from which [`select_cpu_profile`] takes the partition's profile. The
/// controller's restore preflight has checked the record; the selection
/// decodes it again, because the VM worker can run in another process.
pub(super) fn restored_host_profile(record: &SnapshotCpuProfile) -> Option<SnapshotCpuProfile> {
    cpu_profile::is_host_profile_id(&record.id).then(|| record.clone())
}

/// Returns the vendor and signature of the host CPU (`E_CPU_GENERATION` if
/// the host is not x86-64).
fn host_cpu() -> Result<HostCpuSignature, TimeAbiError> {
    // The host CPU is identified with the host's CPUID instruction.
    // xtask-fmt allow-target-arch cpu-intrinsic
    #[cfg(target_arch = "x86_64")]
    {
        Ok(HostCpuSignature::current())
    }
    // xtask-fmt allow-target-arch cpu-intrinsic
    #[cfg(not(target_arch = "x86_64"))]
    {
        Err(TimeAbiError::new(
            TimeAbiCode::CpuGeneration,
            "the host CPU cannot be identified on this architecture",
        ))
    }
}

/// Returns the partition's time ABI backend (`E_TSC_SYNC_UNSUPPORTED` if the
/// backend does not implement the time ABI).
fn backend<'a>(
    partition: &'a dyn HvlitePartition,
    hypervisor: &str,
) -> Result<&'a dyn TimeAbiBackend, TimeAbiError> {
    partition.time_abi().ok_or_else(|| {
        TimeAbiError::new(
            TimeAbiCode::TscSyncUnsupported,
            format!("the {hypervisor} backend does not implement the time ABI"),
        )
    })
}

/// What the time ABI preflight established for a VM process, exposed through
/// inspect at `time-abi` together with the declared rates.
#[derive(Debug, Clone, Inspect)]
pub(super) struct TimeAbiReport {
    /// The backend.
    pub hypervisor: String,
    /// The CPU profile.
    pub cpu_profile: String,
    /// The identity MSR route.
    #[inspect(debug)]
    pub msr_route: IdentityMsrRoute,
    /// The synchronized TSC set method.
    #[inspect(debug)]
    pub sync: TscSyncMethod,
    /// The backend's native TSC rate `F_d`, after the rate test hook.
    pub native_tsc_hz: u64,
    /// On restore, the accepted rate check against the snapshot's rate.
    #[inspect(debug)]
    pub rate_check: Option<RateCheck>,
}

/// Runs the backend preflight and the rate policy, and declares the rates
/// to the guest: restore steps 8 and 9 of the specification when `saved`
/// holds the snapshot's rates, or their cold-boot equivalent.
pub(super) fn declare_rates(
    partition: &dyn HvlitePartition,
    msrs: &TimeAbiMsrs,
    hypervisor: &str,
    cpu_profile: String,
    saved: Option<DeclaredRates>,
    hooks: &TimeAbiTestHooks,
) -> anyhow::Result<TimeAbiReport> {
    let backend = backend(partition, hypervisor)?;
    let preflight = backend.preflight()?;
    let rates = virt::time_abi::negotiate_rates(backend, hypervisor, saved, hooks)?;
    msrs.declare(rates.declared)?;
    tracing::info!(
        hypervisor,
        cpu_profile,
        msr_route = ?preflight.msr_route,
        sync = ?preflight.sync,
        native_tsc_hz = rates.native_tsc_hz,
        tsc_hz = rates.declared.tsc_hz,
        apic_hz = rates.declared.apic_hz,
        deviation_ppb = ?rates.check.map(|check| check.deviation_ppb),
        "time ABI rates declared"
    );
    Ok(TimeAbiReport {
        hypervisor: hypervisor.to_owned(),
        cpu_profile,
        msr_route: preflight.msr_route,
        sync: preflight.sync,
        native_tsc_hz: rates.native_tsc_hz,
        rate_check: rates.check,
    })
}

/// Compares the effective CPUID recomputed for this partition with the one
/// the snapshot recorded: restore step 8 of the specification
/// (`E_CPU_SURFACE`). It compares the binary records, about a microsecond of
/// work, and decodes the snapshot's only to name the differences.
pub(super) fn check_recorded_cpuid(
    effective_cpuid: &EffectiveCpuid,
    record: &SnapshotCpuProfile,
) -> anyhow::Result<()> {
    if encode_effective_cpuid(record_entries(effective_cpuid)) == record.effective_cpuid {
        return Ok(());
    }
    let recorded = decode_effective_cpuid(&record.effective_cpuid).map_err(|err| {
        TimeAbiError::new(
            TimeAbiCode::ManifestTime,
            format!("the snapshot's effective CPUID record is malformed: {err}"),
        )
    })?;
    let differences = recorded_cpuid_differences(effective_cpuid, &recorded);
    let count = differences.len();
    let shown = &differences[..count.min(MAX_REPORTED_CPUID_DIFFERENCES)];
    Err(TimeAbiError::new(
        TimeAbiCode::CpuSurface,
        format!(
            "the effective CPUID of CPU profile {} differs from the snapshot's in {count} entries: {}",
            record.id,
            shown.join("; ")
        ),
    )
    .into())
}

/// Returns the entries of the snapshot record of `effective`, in its order.
fn record_entries(effective: &EffectiveCpuid) -> impl Iterator<Item = EffectiveCpuidEntry> + '_ {
    effective.results().map(|result| EffectiveCpuidEntry {
        function: result.function,
        index: result.index,
        result: result.result,
        mask: result.mask,
    })
}

/// Lists the entries in which this partition's effective CPUID and a
/// snapshot's `recorded` one differ.
fn recorded_cpuid_differences(
    effective: &EffectiveCpuid,
    recorded: &[EffectiveCpuidEntry],
) -> Vec<String> {
    let key = |entry: &EffectiveCpuidEntry| (entry.function, entry.index);
    let name = |(function, index): (u32, Option<u32>)| match index {
        Some(index) => format!("CPUID {function:#x}.{index}"),
        None => format!("CPUID {function:#x}"),
    };
    let ours: BTreeMap<_, _> = record_entries(effective)
        .map(|entry| (key(&entry), entry))
        .collect();
    let theirs: BTreeMap<_, _> = recorded.iter().map(|entry| (key(entry), *entry)).collect();
    let mut differences = Vec::new();
    for (&entry_key, entry) in &ours {
        match theirs.get(&entry_key) {
            None => differences.push(format!("{} is not in the snapshot", name(entry_key))),
            Some(theirs) if theirs != entry => differences.push(format!(
                "{} is {:#x?} under mask {:#x?}, and {:#x?} under mask {:#x?} in the snapshot",
                name(entry_key),
                entry.result,
                entry.mask,
                theirs.result,
                theirs.mask
            )),
            Some(_) => {}
        }
    }
    for &entry_key in theirs
        .keys()
        .filter(|entry_key| !ours.contains_key(entry_key))
    {
        differences.push(format!("{} is only in the snapshot", name(entry_key)));
    }
    if differences.is_empty() {
        differences.push("the snapshot lists the same entries in another order".to_owned());
    }
    differences
}

/// Checks that VP 0 observes the effective CPUID, on cold boot and restore
/// before any VP runs (`E_CPU_SURFACE`): the backend's report, looked up as
/// the guest's CPUID instruction finds it, equals every governed leaf under
/// its masks, and a leaf without a subleaf is read at subleaf 0.
///
/// The report of a pass-through backend also holds VP 0's view at the host's
/// entries outside the profile's tables
/// (`cpu_profile::unlisted_cpuid_candidates`); each must read zero, so that
/// no reserved entry exposes a host feature (`E_CPU_UNLISTED`). A table
/// backend reports none, and its guests read zero there by construction.
pub(super) fn check_presented_cpuid(
    partition: &dyn HvlitePartition,
    hypervisor: &str,
    profile: &CpuProfile,
    effective_cpuid: &EffectiveCpuid,
) -> anyhow::Result<()> {
    let leaves = backend(partition, hypervisor)?.effective_cpuid()?;
    check_presented(profile, effective_cpuid, leaves)
}

/// Checks a backend's report of VP 0's CPUID, as [`check_presented_cpuid`]
/// describes.
fn check_presented(
    profile: &CpuProfile,
    effective_cpuid: &EffectiveCpuid,
    leaves: Vec<CpuidLeaf>,
) -> anyhow::Result<()> {
    let presented = CpuidLeafSet::new(leaves);
    let differences = cpuid_differences(&presented, effective_cpuid);
    if differences.is_empty() {
        let entries: Vec<_> = presented
            .leaves()
            .iter()
            .map(|leaf| cpu_profile::cpuid::CpuidEntry::new(leaf.function, leaf.index, leaf.result))
            .collect();
        cpu_profile::check_unlisted_cpuid(profile, &entries)?;
        return Ok(());
    }
    let count = differences.len();
    let shown = &differences[..count.min(MAX_REPORTED_CPUID_DIFFERENCES)];
    Err(TimeAbiError::new(
        TimeAbiCode::CpuSurface,
        format!(
            "VP 0 does not observe the effective CPUID of CPU profile {} in {count} results: {}",
            profile.id(),
            shown.join("; ")
        ),
    )
    .into())
}

/// Lists every result of `effective` that `presented` does not match under
/// the result's masks.
fn cpuid_differences(presented: &CpuidLeafSet, effective: &EffectiveCpuid) -> Vec<String> {
    const REGISTERS: [&str; 4] = ["EAX", "EBX", "ECX", "EDX"];
    effective
        .results()
        .filter_map(|expected| {
            let subleaf = expected.index.unwrap_or(0);
            let actual = presented.result(expected.function, subleaf, &[0; 4]);
            let registers: Vec<String> = (0..4)
                .filter(|&register| {
                    (actual[register] ^ expected.result[register]) & expected.mask[register] != 0
                })
                .map(|register| {
                    format!(
                        "{} is {:#010x}, not {:#010x} under mask {:#010x}",
                        REGISTERS[register],
                        actual[register],
                        expected.result[register],
                        expected.mask[register]
                    )
                })
                .collect();
            (!registers.is_empty()).then(|| {
                let leaf = match expected.index {
                    Some(index) => format!("CPUID {:#x}.{index}", expected.function),
                    None => format!("CPUID {:#x}", expected.function),
                };
                format!("{leaf} {}", registers.join(", "))
            })
        })
        .collect()
}

/// Checks that the backend supports the CPU profile, from the CPU surface it
/// reports (`E_PROFILE_UNSUPPORTED`), at every cold boot and restore. A
/// backend that reports no surface fails the check.
pub(super) fn check_profile_support(
    partition: &dyn HvlitePartition,
    hypervisor: &str,
    profile: &CpuProfile,
) -> anyhow::Result<()> {
    let surface = backend(partition, hypervisor)?
        .supported_cpu_surface()?
        .ok_or_else(|| {
            TimeAbiError::new(
                TimeAbiCode::ProfileUnsupported,
                format!(
                    "the {hypervisor} backend reports no CPU surface to verify CPU profile {} against",
                    profile.id()
                ),
            )
        })?;
    cpu_profile::verify_support(profile, &host_cpu_surface(&surface, hypervisor))?;
    Ok(())
}

/// Converts a backend's CPU surface to the profile crate's form. KVM's is a
/// table; MSHV and WHP build theirs from the host's CPUID, which does not show
/// what a guest reads outside the profile's tables.
fn host_cpu_surface(
    surface: &SupportedCpuSurface,
    hypervisor: &str,
) -> cpu_profile::HostCpuSurface {
    let mut cpuid = surface
        .cpuid
        .iter()
        .map(|leaf| cpu_profile::cpuid::CpuidEntry::new(leaf.function, leaf.index, leaf.result))
        .collect();
    cpu_profile::cpuid::normalize(&mut cpuid);
    cpu_profile::HostCpuSurface {
        cpuid,
        presentation: match hypervisor {
            "kvm" => cpu_profile::CpuidPresentation::Table,
            _ => cpu_profile::CpuidPresentation::PassThroughHostView,
        },
        physical_address_width: surface.physical_address_width,
        msrs: surface
            .msrs
            .iter()
            .map(|msr| cpu_profile::SupportedMsr {
                index: msr.index,
                supported: msr.supported,
                controllable: msr.controllable,
            })
            .collect(),
    }
}

/// Takes the time ABI records of a capture: capture steps 2 and 3 of the
/// specification, the capture anchor and host identities, then the declared
/// rates, the CPU profile record, and the generation counter. The caller has
/// stopped every VP and checked the LAPIC timers (step 1).
pub(super) fn capture_records(
    partition: &dyn HvlitePartition,
    state: &TimeAbiState,
) -> anyhow::Result<TimeCapture> {
    let backend = backend(partition, &state.report.hypervisor)?;
    let rates = state
        .msrs
        .declared()
        .context("the time ABI rates are not declared")?;
    let anchor = backend.capture_anchor()?;
    let identity = virt::time_abi::host::host_identity()?;
    let record = state.profile.record();
    tracing::info!(
        tsc = anchor.tsc,
        utc_ns = anchor.sample.utc_ns,
        pairing_ns = anchor.pairing_ns,
        generation = state.generation,
        "time ABI capture anchor"
    );
    Ok(TimeCapture {
        time: SnapshotTimeContract {
            time_abi_version: TIME_ABI_VERSION,
            tsc_frequency_hz: rates.tsc_hz,
            tsc_tolerance_ppm: virt::time_abi::rate::TSC_TOLERANCE_PPM,
            apic_frequency_hz: rates.apic_hz,
            capture_tsc: anchor.tsc,
            capture_utc_ns: anchor.sample.utc_ns,
            capture_monotonic_ns: anchor.sample.monotonic_ns,
            host_clock: identity.clock.as_str().to_owned(),
            host_id: identity.host_id.to_vec(),
            host_boot_id: identity.boot_id.to_vec(),
            capture_generation: state.generation,
        },
        cpu_profile: SnapshotCpuProfile {
            id: record.id.to_owned(),
            sha256: record.digest.to_vec(),
            profile: record.encoding.to_vec(),
            effective_cpuid: encode_effective_cpuid(record_entries(&state.effective_cpuid)),
            capture_cpu_signature: virt::time_abi::surface::host_cpu_signature().unwrap_or(0),
        },
    })
}

impl LoadedVm {
    /// Restores the guest clocks: restore steps 12 to 16 of the
    /// specification, after the saved state is restored and before any
    /// restored VP runs.
    ///
    /// It performs the synchronized TSC set at the downtime selected at the
    /// restore anchor, then concurrently advances and sets every LAPIC and
    /// advances VM time and the RTC, and seals the time fields of the restore
    /// packet. The LAPIC advance rejects a periodic or TSC-deadline timer, so
    /// restore needs no separate timer check, and the PIT checks itself when
    /// restored.
    pub(super) async fn time_abi_restore(&mut self, input: RestoreTimeInput) -> anyhow::Result<()> {
        let state = self
            .inner
            .time_abi
            .as_ref()
            .context("a time ABI restore requires a time ABI partition")?;
        let hypervisor = state.report.hypervisor.clone();
        let hooks = state.hooks.clone();
        let rate_deviation = state
            .report
            .rate_check
            .map_or(0, |check| check.rate_deviation);
        let capture = input.contract.capture_record()?;
        let tsc_hz = input.contract.tsc_frequency_hz;
        let apic_hz = input.contract.apic_frequency_hz;

        let started = std::time::Instant::now();
        let partition = self.inner.partition.clone();
        let mut downtime = None;
        let set =
            backend(partition.as_ref(), &hypervisor)?.set_synchronized_tsc(&mut |sample| {
                let selected = virt::time_abi::downtime::select_downtime(
                    &capture,
                    &input.destination,
                    sample,
                    &hooks,
                )?;
                let target =
                    virt::time_abi::downtime::tsc_target(capture.tsc, selected.nanos, tsc_hz)?;
                downtime = Some(selected);
                Ok(target)
            })?;
        let downtime =
            downtime.context("the backend set the TSC without selecting the downtime")?;
        let tsc_set = started.elapsed();
        if let Some(step_ns) = downtime.host_wall_clock_step_ns {
            tracing::warn!(
                step_ns,
                "host wall clock was stepped since capture; the downtime uses host monotonic time"
            );
        }
        tracing::info!(
            downtime_ns = downtime.nanos,
            source = ?downtime.source,
            target = set.target,
            method = ?set.method,
            vps = set.readback.len(),
            "time ABI synchronized TSC set"
        );

        // Steps 14 and 15 touch disjoint state, since no backend's LAPIC
        // reads VM time under the time ABI, so they run concurrently. Both
        // run to completion, so a failure never cancels a state unit
        // operation midway.
        let downtime_ns = downtime.nanos;
        let partition_unit = &mut self.inner.partition_unit;
        let state_units = &mut self.state_units;
        let lapic_advance = async move {
            let result = partition_unit
                .advance_lapic_timers(downtime_ns, apic_hz)
                .await;
            (result, started.elapsed())
        };
        let vm_time_advance = async move {
            let result = state_units
                .advance_time(Duration::from_nanos(downtime_ns))
                .await;
            (result, started.elapsed())
        };
        let ((lapic_result, lapic_advanced), (vm_time_result, vm_time_advanced)) =
            (lapic_advance, vm_time_advance).join().await;
        lapic_result?;
        vm_time_result.context("failed to advance restored VM time")?;
        let finished = started.elapsed();
        // Per-phase durations attribute the restore.time_abi_clock profile
        // phase. The LAPIC and VM time phases overlap; each is measured from
        // the end of the TSC set.
        tracing::info!(
            tsc_set_us = tsc_set.as_micros() as u64,
            lapic_us = (lapic_advanced - tsc_set).as_micros() as u64,
            vm_time_us = (vm_time_advanced - tsc_set).as_micros() as u64,
            total_us = finished.as_micros() as u64,
            "time ABI restore clock"
        );

        input.restore_record.send(RestoreTimeRecord {
            downtime_ns: downtime.nanos,
            downtime_utc: downtime.source == DowntimeSource::Utc,
            rate_deviation,
            test_hooks: hooks.active(),
        });
        Ok(())
    }
}

/// The `time-abi` state unit. It saves and restores
/// `HV_X64_MSR_TSC_INVARIANT_CONTROL`, which is partition-wide guest state,
/// and inspects the declared rates and the preflight report.
pub(super) struct TimeAbiUnit {
    pub msrs: Arc<TimeAbiMsrs>,
    pub report: TimeAbiReport,
}

impl InspectMut for TimeAbiUnit {
    fn inspect_mut(&mut self, req: inspect::Request<'_>) {
        req.respond().merge(&*self.msrs).merge(&self.report);
    }
}

impl StateUnit for TimeAbiUnit {
    async fn start(&mut self) -> anyhow::Result<()> {
        Ok(())
    }

    async fn stop(&mut self) {}

    async fn reset(&mut self) -> anyhow::Result<()> {
        self.msrs.reset();
        Ok(())
    }

    async fn save(&mut self) -> Result<Option<SavedStateBlob>, SaveError> {
        Ok(Some(SavedStateBlob::new(self.msrs.save())))
    }

    async fn restore(&mut self, state: SavedStateBlob) -> Result<(), RestoreError> {
        self.msrs.restore(state.parse()?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use openvmm_defs::time_abi::EFFECTIVE_CPUID_ENTRY_BYTES;
    use virt::time_abi::cpuid::effective_cpuid;
    use virt::time_abi::msr::MSR_TSC_INVARIANT_CONTROL;
    use vm_topology::processor::ProcessorTopology;
    use vm_topology::processor::TopologyBuilder;
    use vm_topology::processor::VpIndex;
    use vm_topology::processor::x86::X2ApicState;

    const TEST_PROFILE: &str = "intel.icelake-sp.v1";

    /// The signature of the test profile's CPU, an Ice Lake-SP Xeon.
    const TEST_SIGNATURE: u32 = 0x0006_06a6;

    /// The signature of a Granite Rapids Xeon, family 6 model 173 stepping 1,
    /// which no pinned profile serves.
    const GRANITE_RAPIDS: u32 = 0x000a_06d1;

    /// The brand string of the Granite Rapids test CPU.
    const GRANITE_RAPIDS_BRAND: &str = "Intel(R) Xeon(R) 6973P-C";

    /// No test hook.
    const NO_HOOKS: TimeAbiTestHooks = TimeAbiTestHooks {
        force_utc_downtime: false,
        boot_id_mismatch: false,
        dest_rate_offset_ppm: 0,
        downtime_add_s: 0,
        utc_offset_ms: 0,
        sample_delay_us: 0,
        host_cpu_unknown: false,
    };

    /// The `host-cpu-unknown` test hook.
    const HOST_CPU_UNKNOWN: TimeAbiTestHooks = TimeAbiTestHooks {
        host_cpu_unknown: true,
        ..NO_HOOKS
    };

    fn test_profile() -> &'static CpuProfile {
        cpu_profile::pinned(TEST_PROFILE).unwrap()
    }

    /// Returns the profile of `selection`, which must not be a fallback.
    fn selected(
        selection: Result<Selection, ProfileError>,
    ) -> Result<PartitionProfile, ProfileError> {
        selection.map(|selection| match selection {
            Selection::Profile(profile) => profile,
            Selection::HostFallback => panic!("the selection fell back to a host profile"),
        })
    }

    /// Returns whether `selection` is `auto`'s fallback to a host profile.
    fn is_fallback(selection: &Result<Selection, ProfileError>) -> bool {
        matches!(selection, Ok(Selection::HostFallback))
    }

    /// Returns the partition profile of the pinned `profile`.
    fn partition_profile(profile: &CpuProfile) -> PartitionProfile {
        PartitionProfile::pinned(profile.id()).unwrap()
    }

    fn topology(vp_count: u32, x2apic: X2ApicState) -> ProcessorTopology {
        TopologyBuilder::new_x86()
            .vps_per_socket(vp_count)
            .x2apic(x2apic)
            .build(vp_count)
            .unwrap()
    }

    /// Returns the MSR handler, the configuration, and the effective CPUID of
    /// a partition with `vp_count` VPs and the test profile.
    fn test_config(vp_count: u32) -> (Arc<TimeAbiMsrs>, TimeAbiConfig, EffectiveCpuid) {
        let profile = test_profile();
        let effective =
            effective_cpuid(profile, &topology(vp_count, X2ApicState::Supported)).unwrap();
        let (msrs, config) = partition_config(&partition_profile(profile), &effective);
        (msrs, config, effective)
    }

    fn test_unit(msrs: Arc<TimeAbiMsrs>) -> TimeAbiUnit {
        TimeAbiUnit {
            msrs,
            report: TimeAbiReport {
                hypervisor: "kvm".to_owned(),
                cpu_profile: TEST_PROFILE.to_owned(),
                msr_route: IdentityMsrRoute::ExitToVmm,
                sync: TscSyncMethod::CommonOffset,
                native_tsc_hz: 2_100_000_000,
                rate_check: None,
            },
        }
    }

    fn code<T: std::fmt::Debug>(result: anyhow::Result<T>) -> String {
        let message = format!("{:#}", result.unwrap_err());
        message
            .strip_prefix('[')
            .and_then(|rest| rest.split_once(']'))
            .map_or(message.clone(), |(code, _)| code.to_owned())
    }

    #[test]
    fn unit_saves_restores_and_resets_the_invariant_control() {
        futures::executor::block_on(async {
            let (msrs, config, _) = test_config(4);
            assert!(Arc::ptr_eq(&msrs, &config.msrs));
            msrs.write(VpIndex::BSP, MSR_TSC_INVARIANT_CONTROL, 1)
                .unwrap()
                .unwrap();
            let mut unit = test_unit(msrs.clone());
            let saved = unit.save().await.unwrap().unwrap();

            let (restored, _, _) = test_config(4);
            let mut restored_unit = test_unit(restored.clone());
            restored_unit.restore(saved).await.unwrap();
            assert_eq!(restored.tsc_invariant_control(), 1);

            restored_unit.reset().await.unwrap();
            assert_eq!(restored.tsc_invariant_control(), 0);
        });
    }

    #[test]
    fn unit_inspects_the_report() {
        let (msrs, _, _) = test_config(1);
        let mut unit = test_unit(msrs);
        let mut inspection = inspect::inspect("", &mut unit);
        futures::executor::block_on(inspection.resolve());
        let inspect::Node::Dir(entries) = inspection.results() else {
            panic!("the unit inspects as a directory");
        };
        let names: Vec<_> = entries.iter().map(|entry| entry.name.as_str()).collect();
        for name in [
            "tsc_invariant_control",
            "hypervisor",
            "cpu_profile",
            "msr_route",
            "sync",
            "native_tsc_hz",
        ] {
            assert!(names.contains(&name), "{names:?}");
        }
    }

    /// Every pinned profile completes, for every topology, into a partition
    /// CPUID that carries the whole effective CPUID, the identity, and the
    /// time bits, with only the per-VP bits left to the backend.
    #[test]
    fn partition_cpuid_is_the_complete_profile() {
        for profile in cpu_profile::pinned_profiles() {
            for vp_count in [1, 2, 8] {
                for x2apic in [X2ApicState::Unsupported, X2ApicState::Supported] {
                    let effective = effective_cpuid(profile, &topology(vp_count, x2apic)).unwrap();
                    let (_, config) = partition_config(&partition_profile(profile), &effective);
                    assert_eq!(config.cpu_profile.id(), profile.id());

                    let mut cpuid = |leaf, subleaf| config.cpuid.result(leaf, subleaf, &[0; 4]);
                    virt::time_abi::identity::check_identity(&mut cpuid, vp_count).unwrap();
                    virt::time_abi::identity::check_time_bits(&mut cpuid, true).unwrap();
                    assert!(
                        cpuid_differences(&config.cpuid, &effective).is_empty(),
                        "{} at {vp_count} VPs",
                        profile.id()
                    );
                    let x2apic_bit = effective.lookup(1, 0)[2] & (1 << 21) != 0;
                    assert_eq!(x2apic_bit, x2apic == X2ApicState::Supported);

                    for leaf in config.cpuid.leaves() {
                        let per_vp = virt::x86::topology::per_vp_cpuid_bits(leaf.function);
                        for (mask, bits) in leaf.mask.iter().zip(per_vp) {
                            assert_eq!(mask & bits, 0, "{leaf:?}");
                        }
                    }
                }
            }
        }
    }

    /// A pass-through backend's report may hold VP 0's view at the host's
    /// entries outside the profile: zero there passes, and anything else is
    /// `E_CPU_UNLISTED`. A report of the table alone passes, for every pinned
    /// profile and topology.
    #[test]
    fn presented_cpuid_rejects_non_zero_unlisted_entries() {
        for profile in cpu_profile::pinned_profiles() {
            for vp_count in [1, 2, 8, 64] {
                for x2apic in [X2ApicState::Unsupported, X2ApicState::Supported] {
                    let effective = effective_cpuid(profile, &topology(vp_count, x2apic)).unwrap();
                    let (_, config) = partition_config(&partition_profile(profile), &effective);
                    check_presented(profile, &effective, config.cpuid.leaves().to_vec())
                        .unwrap_or_else(|err| {
                            panic!("{} with {vp_count} VPs, {x2apic:?}: {err:#}", profile.id())
                        });
                }
            }
        }

        let (_, config, effective) = test_config(2);
        let profile = test_profile();
        let table = config.cpuid.leaves().to_vec();

        // Leaf 0x1D is above Ice Lake-SP's maximum basic leaf (0x1B).
        let mut zero = table.clone();
        zero.push(CpuidLeaf::new(0x1d, [0; 4]));
        check_presented(profile, &effective, zero).unwrap();

        let mut leaked = table.clone();
        leaked.push(CpuidLeaf::new(0x1d, [1, 0, 0, 0]));
        assert_eq!(
            code(check_presented(profile, &effective, leaked.clone())),
            "E_CPU_UNLISTED"
        );

        // A governed difference is reported first.
        leaked.retain(|leaf| leaf.function != 0x8000_0002);
        assert_eq!(
            code(check_presented(profile, &effective, leaked)),
            "E_CPU_SURFACE"
        );
    }

    /// The effective CPUID ends leaf 0Bh with Intel's terminating subleaf,
    /// which carries VP 0's x2APIC ID; the backend sets each VP's own. No
    /// pinned profile reaches leaf 1Fh.
    #[test]
    fn effective_cpuid_terminates_the_extended_topology() {
        for profile in cpu_profile::pinned_profiles() {
            assert!(profile.lookup(0, 0)[0] < 0x1f, "{}", profile.id());
            for vp_count in [1, 2, 8] {
                let effective =
                    effective_cpuid(profile, &topology(vp_count, X2ApicState::Supported)).unwrap();
                let subleaves = |function| {
                    effective
                        .results()
                        .filter(|result| result.function == function)
                        .map(|result| (result.index, result.result, result.mask))
                        .collect::<Vec<_>>()
                };
                let leaf_b = subleaves(0xb);
                assert_eq!(leaf_b.len(), 3, "{}: {leaf_b:x?}", profile.id());
                assert_eq!(leaf_b[2], (Some(2), [0, 0, 2, 0], [!0; 4]));
                assert!(subleaves(0x1f).is_empty());

                let (_, config) = partition_config(&partition_profile(profile), &effective);
                let terminator = config
                    .cpuid
                    .leaves()
                    .iter()
                    .find(|leaf| leaf.function == 0xb && leaf.index == Some(2))
                    .unwrap();
                assert_eq!(terminator.mask, [!0, !0, !0, 0]);
            }
        }
    }

    #[test]
    fn presented_cpuid_must_match_under_the_masks() {
        let (_, config, effective) = test_config(2);
        let presented = config.cpuid.leaves().to_vec();
        assert!(cpuid_differences(&CpuidLeafSet::new(presented.clone()), &effective).is_empty());

        // A feature bit the profile sets (AVX2) that VP 0 does not see.
        let mut changed = presented.clone();
        changed.push(
            CpuidLeaf::new(7, [0; 4])
                .indexed(0)
                .masked([0, 1 << 5, 0, 0]),
        );
        let differences = cpuid_differences(&CpuidLeafSet::new(changed), &effective);
        assert_eq!(differences.len(), 1, "{differences:?}");
        assert!(
            differences[0].starts_with("CPUID 0x7.0 EBX is"),
            "{differences:?}"
        );

        // Runtime-owned bits, such as OSXSAVE, are not part of the profile.
        let mut running = presented.clone();
        running.push(CpuidLeaf::new(1, [0, 0, 1 << 27, 0]).masked([0, 0, 1 << 27, 0]));
        assert!(cpuid_differences(&CpuidLeafSet::new(running), &effective).is_empty());

        // A leaf VP 0 does not report reads as zeros.
        let without_brand: Vec<_> = presented
            .iter()
            .copied()
            .filter(|leaf| leaf.function != 0x8000_0002)
            .collect();
        let differences = cpuid_differences(&CpuidLeafSet::new(without_brand), &effective);
        assert_eq!(differences.len(), 1, "{differences:?}");
        assert!(differences[0].starts_with("CPUID 0x80000002 "));
    }

    #[test]
    fn recorded_cpuid_must_match() {
        let (_, _, effective) = test_config(2);
        let pinned = cpu_profile::pinned_record(TEST_PROFILE).unwrap();
        let record = SnapshotCpuProfile {
            id: TEST_PROFILE.to_owned(),
            sha256: pinned.digest.to_vec(),
            profile: pinned.encoding.to_vec(),
            effective_cpuid: encode_effective_cpuid(record_entries(&effective)),
            capture_cpu_signature: 0,
        };
        check_recorded_cpuid(&effective, &record).unwrap();

        // Another topology changes the topology leaves.
        let (_, _, other) = test_config(4);
        let message = format!("{:#}", check_recorded_cpuid(&other, &record).unwrap_err());
        assert!(message.starts_with("[E_CPU_SURFACE]"), "{message}");
        assert!(message.contains("CPUID 0xb.1"), "{message}");

        // One flipped bit is named.
        let mut flipped = record.clone();
        flipped.effective_cpuid[3 * 4] ^= 1;
        let message = format!(
            "{:#}",
            check_recorded_cpuid(&effective, &flipped).unwrap_err()
        );
        assert!(message.starts_with("[E_CPU_SURFACE]"), "{message}");
        assert!(message.contains("CPUID 0x0 is"), "{message}");

        // A missing entry, and the same entries in another order.
        let mut truncated = record.clone();
        truncated
            .effective_cpuid
            .truncate(record.effective_cpuid.len() - EFFECTIVE_CPUID_ENTRY_BYTES);
        assert_eq!(
            code(check_recorded_cpuid(&effective, &truncated)),
            "E_CPU_SURFACE"
        );
        let mut reordered = record.clone();
        let (first, second) = reordered
            .effective_cpuid
            .split_at_mut(EFFECTIVE_CPUID_ENTRY_BYTES);
        first.swap_with_slice(&mut second[..EFFECTIVE_CPUID_ENTRY_BYTES]);
        assert_eq!(
            code(check_recorded_cpuid(&effective, &reordered)),
            "E_CPU_SURFACE"
        );

        let mut malformed = record.clone();
        malformed.effective_cpuid.pop();
        assert_eq!(
            code(check_recorded_cpuid(&effective, &malformed)),
            "E_MANIFEST_TIME"
        );
    }

    /// Capture copies the pinned profile's precomputed record, so restore
    /// never encodes or hashes a profile.
    #[test]
    fn pinned_records_are_the_profiles() {
        for profile in cpu_profile::pinned_profiles() {
            let pinned = cpu_profile::pinned_record(profile.id()).unwrap();
            assert_eq!(pinned.id, profile.id());
            assert_eq!(pinned.encoding, profile.encode().as_slice());
            assert_eq!(pinned.digest, profile.digest());
        }
    }

    #[test]
    fn cpu_profile_selection() {
        for requested in [
            "interim.host.kvm.v1",
            "intel.cascadelake.v1",
            "intel.host.v1",
        ] {
            for hooks in [&NO_HOOKS, &HOST_CPU_UNKNOWN] {
                assert_eq!(
                    code(select_cpu_profile(requested, "kvm", None, hooks)),
                    "E_PROFILE_UNKNOWN"
                );
            }
        }

        // `auto` selects the pinned profile of the host's generation, if any,
        // and otherwise falls back to a host profile where one can serve the
        // host. The `nosuch` backend, which cannot be fingerprinted, stops a
        // fallback at its first step, which says that `auto` fell back.
        let host = host_cpu().unwrap();
        let fell_back = |result: anyhow::Result<PartitionProfile>| {
            assert_eq!(
                format!("{:#}", result.unwrap_err()),
                format!(
                    "{}: the nosuch backend cannot be fingerprinted",
                    FellBack::new(&host)
                )
            );
        };
        let auto = select_cpu_profile(cpu_profile::AUTO, "nosuch", None, &NO_HOOKS);
        if cpu_profile::pinned_profile_serves(&host) {
            let profile = auto.unwrap();
            assert!(!profile.is_host());
            cpu_profile::check_generation(&profile, &host).unwrap();
        } else if cpu_profile::supports_host_profiles(&host) {
            fell_back(auto);
        } else {
            assert_eq!(code(auto), "E_PROFILE_HOST_UNKNOWN");
        }
        // The `host-cpu-unknown` hook makes `auto` fall back on any CPU that
        // host profiles serve.
        let hooked = select_cpu_profile(cpu_profile::AUTO, "nosuch", None, &HOST_CPU_UNKNOWN);
        if cpu_profile::supports_host_profiles(&host) {
            fell_back(hooked);
        } else {
            assert_eq!(code(hooked), "E_PROFILE_HOST_UNKNOWN");
        }

        // A pinned ID selects its profile only in its generation, whatever
        // the hooks.
        for profile in cpu_profile::pinned_profiles() {
            for hooks in [&NO_HOOKS, &HOST_CPU_UNKNOWN] {
                let selected = select_cpu_profile(profile.id(), "kvm", None, hooks);
                if cpu_profile::check_generation(profile, &host).is_ok() {
                    assert_eq!(selected.unwrap().id(), profile.id());
                } else {
                    assert_eq!(code(selected), "E_CPU_GENERATION");
                }
            }
        }
    }

    /// `auto` falls back to a host profile only where one can boot: on Intel
    /// and AMD CPUs that no pinned profile serves, or that the
    /// `host-cpu-unknown` test hook treats so. Another vendor's CPU still
    /// fails, without naming `--cpu-profile host`, and an explicit profile
    /// never falls back.
    #[test]
    fn auto_falls_back_to_host_profiles_only_where_they_serve() {
        let tiger_lake = HostCpuSignature::new(*b"GenuineIntel", 0x0008_06c1);
        let granite_rapids = HostCpuSignature::new(*b"GenuineIntel", GRANITE_RAPIDS);
        let zen3 = HostCpuSignature::new(*b"AuthenticAMD", 0x00a2_0f12);
        let icelake = HostCpuSignature::new(*b"GenuineIntel", TEST_SIGNATURE);
        let milan = HostCpuSignature::new(*b"AuthenticAMD", 0x00a0_0f11);
        let hygon = HostCpuSignature::new(*b"HygonGenuine", 0x0090_0f01);
        let select = |requested, host, hooks| select_on_host(requested, host, None, hooks);

        for host in [&tiger_lake, &granite_rapids, &zen3] {
            for hooks in [&NO_HOOKS, &HOST_CPU_UNKNOWN] {
                assert!(falls_back(host, hooks), "{host}");
                assert!(is_fallback(&select(cpu_profile::AUTO, host, hooks)));
            }
        }
        for (host, id) in [(&icelake, TEST_PROFILE), (&milan, "amd.milan.v1")] {
            assert!(!falls_back(host, &NO_HOOKS), "{host}");
            assert_eq!(
                selected(select(cpu_profile::AUTO, host, &NO_HOOKS))
                    .unwrap()
                    .id(),
                id
            );
            assert!(falls_back(host, &HOST_CPU_UNKNOWN), "{host}");
            assert!(is_fallback(&select(
                cpu_profile::AUTO,
                host,
                &HOST_CPU_UNKNOWN
            )));
        }
        for hooks in [&NO_HOOKS, &HOST_CPU_UNKNOWN] {
            assert!(!falls_back(&hygon, hooks));
            let error = selected(select(cpu_profile::AUTO, &hygon, hooks)).unwrap_err();
            assert_eq!(error.code, ProfileErrorCode::ProfileHostUnknown, "{error}");
            assert!(!error.message.contains("--cpu-profile host"), "{error}");

            let error = selected(select(TEST_PROFILE, &tiger_lake, hooks)).unwrap_err();
            assert_eq!(error.code, ProfileErrorCode::CpuGeneration, "{error}");
            assert!(!error.message.contains("--cpu-profile host"), "{error}");
            assert_eq!(
                selected(select(TEST_PROFILE, &icelake, hooks))
                    .unwrap()
                    .id(),
                TEST_PROFILE
            );
        }
    }

    /// `auto`'s fallback derives the host profile that `--cpu-profile host`
    /// derives, checks its generation, and warns with the stable marker, the
    /// host CPU, the profile and its digest, the limits of a host profile,
    /// and an invitation to request a built-in profile through NVX's issue
    /// form, which the link prefills with the CPU.
    #[test]
    fn auto_falls_back_to_a_host_profile_and_warns() {
        let host = HostCpuSignature::new(*b"GenuineIntel", GRANITE_RAPIDS);
        let fingerprint = test_fingerprint("kvm", GRANITE_RAPIDS, GRANITE_RAPIDS_BRAND);
        let profile = fall_back(&host, || Ok(fingerprint.clone())).unwrap();
        assert!(profile.is_host());
        assert_eq!(profile.id(), "intel.host.v1");
        assert_eq!(
            *profile,
            cpu_profile::derive_host_profile(&fingerprint).unwrap()
        );
        cpu_profile::check_generation(&profile, &host).unwrap();

        let digest = profile.digest_string();
        let warning = host_profile_warning(
            HostProfileOrigin::Fallback,
            &fingerprint,
            &profile,
            Duration::from_micros(12_345),
            true,
        );
        let lines: Vec<_> = warning.lines().collect();
        assert_eq!(
            lines[0],
            format!(
                "NVX-CPU-PROFILE-FALLBACK: no built-in CPU profile serves this host's CPU, \
                 GenuineIntel 6/173/1 (Intel(R) Xeon(R) 6973P-C), so --cpu-profile auto fell \
                 back to intel.host.v1 ({digest}), a profile derived from this host's kvm \
                 backend for development."
            )
        );
        assert_eq!(
            lines[1],
            "It is not pinned: a microcode, firmware, hypervisor, or OS update can change it. \
             Each cold boot fingerprints the backend first, which took 12.3 ms here, and its \
             snapshots restore only on hosts of the same CPU model and stepping whose \
             hypervisor supports it."
        );
        assert_eq!(
            lines[2..],
            [
                "Help NVX add a built-in profile for this CPU: run",
                "  openvmm --hypervisor kvm --cpu-fingerprint fingerprint.json",
                "and attach fingerprint.json to a CPU profile request:",
                "  https://github.com/microsoft/nvx/issues/new?template=cpu-profile.yml\
                 &title=CPU%20profile%3A%20GenuineIntel%206%2F173%2F1%20%28Intel%28R%29%20Xeon\
                 %28R%29%206973P-C%29&signature=GenuineIntel%206%2F173%2F1\
                 &cpu=Intel%28R%29%20Xeon%28R%29%206973P-C",
            ]
        );
    }

    /// An explicit `--cpu-profile host` warns without the fallback's marker,
    /// and invites a request for a built-in profile only on a CPU that no
    /// pinned profile serves.
    #[test]
    fn explicit_host_profiles_invite_requests_only_where_no_profile_serves() {
        let fingerprint = test_fingerprint("whp", GRANITE_RAPIDS, GRANITE_RAPIDS_BRAND);
        let host = HostCpuSignature::new(*b"GenuineIntel", GRANITE_RAPIDS);
        let profile = host_partition_profile(&fingerprint, &host).unwrap();
        let digest = profile.digest_string();
        let invited = host_profile_warning(
            HostProfileOrigin::Requested,
            &fingerprint,
            &profile,
            Duration::from_micros(4_960),
            true,
        );
        assert!(
            invited.starts_with(&format!(
                "--cpu-profile host selected intel.host.v1 ({digest}), a profile derived from \
                 this host's whp backend for development; no built-in CPU profile serves this \
                 host's CPU, GenuineIntel 6/173/1 (Intel(R) Xeon(R) 6973P-C).\nIt is not pinned"
            )),
            "{invited}"
        );
        assert!(!invited.contains(FALLBACK_MARKER), "{invited}");
        assert!(
            invited.contains("\n  openvmm --hypervisor whp --cpu-fingerprint fingerprint.json\n"),
            "{invited}"
        );
        assert!(
            invited.contains(&format!("\n  {CPU_PROFILE_REQUEST_URL}&title=")),
            "{invited}"
        );

        let (served, _) = test_host_profile("whp");
        let served = PartitionProfile::host(served).unwrap();
        let fingerprint = test_fingerprint("whp", TEST_SIGNATURE, "");
        let warning = host_profile_warning(
            HostProfileOrigin::Requested,
            &fingerprint,
            &served,
            Duration::from_micros(4_960),
            false,
        );
        assert_eq!(
            warning.lines().collect::<Vec<_>>(),
            [
                format!(
                    "--cpu-profile host selected intel.host.v1 ({}), a profile derived from \
                     this host's whp backend for development.",
                    served.digest_string()
                )
                .as_str(),
                "It is not pinned: a microcode, firmware, hypervisor, or OS update can change \
                 it. Each cold boot fingerprints the backend first, which took 5.0 ms here, and \
                 its snapshots restore only on hosts of the same CPU model and stepping whose \
                 hypervisor supports it.",
            ]
        );
    }

    #[test]
    fn cpu_profile_requests_prefill_the_cpu() {
        assert_eq!(
            encode_query_value("az-AZ_09.~ /(R)&=?#%+\u{e9}"),
            "az-AZ_09.~%20%2F%28R%29%26%3D%3F%23%25%2B%C3%A9"
        );
        // A CPU without a brand string prefills only its signature.
        let fingerprint = test_fingerprint("mshv", GRANITE_RAPIDS, "");
        assert_eq!(describe_cpu(&fingerprint.host.cpu), "GenuineIntel 6/173/1");
        assert_eq!(
            cpu_profile_request_url(&fingerprint.host.cpu),
            format!(
                "{CPU_PROFILE_REQUEST_URL}&title=CPU%20profile%3A%20GenuineIntel%206%2F173%2F1\
                 &signature=GenuineIntel%206%2F173%2F1"
            )
        );
        assert_eq!(
            describe_signature(&HostCpuSignature::new(*b"AuthenticAMD", 0x00a6_0f12)),
            "AuthenticAMD 25/97/2"
        );
    }

    /// A fallback that cannot boot says that `auto` fell back, keeps the
    /// failure's code, and names no `--cpu-profile host`, which it already
    /// tried: a failed fingerprint or derivation, such as a backend that
    /// lacks a CPU feature that the time ABI requires (`E_PROFILE_UNSUPPORTED`),
    /// and a backend that cannot boot the derived profile.
    #[test]
    fn a_fallback_that_cannot_boot_says_that_auto_fell_back() {
        let host = HostCpuSignature::new(*b"GenuineIntel", GRANITE_RAPIDS);
        let fell_back = FellBack::new(&host).to_string();
        assert_eq!(
            fell_back,
            "--cpu-profile auto fell back to a host CPU profile, because no built-in CPU \
             profile serves this host's CPU, GenuineIntel 6/173/1, but this host cannot boot one"
        );

        let no_rdtscp =
            test_fingerprint_with("whp", GRANITE_RAPIDS, GRANITE_RAPIDS_BRAND, |cpuid| {
                for entry in cpuid.iter_mut() {
                    if entry.key() == (0x8000_0001, None) {
                        let mut registers = entry.registers();
                        registers[3] &= !(1 << 27);
                        *entry = cpu_profile::cpuid::CpuidEntry::new(0x8000_0001, None, registers);
                    }
                }
            });
        let err = fall_back(&host, || Ok(no_rdtscp)).unwrap_err();
        let message = format!("{err:#}");
        assert!(
            message.starts_with(&format!("{fell_back}: [E_PROFILE_UNSUPPORTED] ")),
            "{message}"
        );
        assert!(message.contains("RDTSCP"), "{message}");
        assert_eq!(leading_code(&err), Some(TimeAbiCode::ProfileUnsupported));
        assert!(!message.contains(HOST_PROFILE_HINT), "{message}");

        let err = fall_back(&host, || {
            Err(anyhow::anyhow!("failed to fingerprint the kvm backend"))
        })
        .unwrap_err();
        assert_eq!(
            format!("{err:#}"),
            format!("{fell_back}: failed to fingerprint the kvm backend")
        );

        // The cold boot's hint does not repeat what the fallback said.
        let wrapped = err.context("failed to create the partition");
        let hinted = format!(
            "{:#}",
            hint_host_profile(wrapped, cpu_profile::AUTO, &host, &NO_HOOKS)
        );
        assert_eq!(hinted.matches(&fell_back).count(), 1, "{hinted}");

        // A backend that cannot boot the profile that `auto` fell back to.
        let unsupported = || {
            anyhow::Error::new(TimeAbiError::new(
                TimeAbiCode::ProfileUnsupported,
                "the kvm backend does not support CPU profile intel.host.v1",
            ))
            .context("failed to create the prototype partition")
        };
        let icelake = HostCpuSignature::new(*b"GenuineIntel", TEST_SIGNATURE);
        for (host, hooks) in [(&host, &NO_HOOKS), (&icelake, &HOST_CPU_UNKNOWN)] {
            let err = hint_host_profile(unsupported(), cpu_profile::AUTO, host, hooks);
            let message = format!("{err:#}");
            assert_eq!(
                message,
                format!("{}: {:#}", FellBack::new(host), unsupported())
            );
            assert_eq!(leading_code(&err), Some(TimeAbiCode::ProfileUnsupported));
            assert!(!message.contains(HOST_PROFILE_HINT), "{message}");
        }
    }

    /// A cold boot with `auto` names `--cpu-profile host` where the backend
    /// does not support the built-in profile that `auto` selected, on Intel
    /// and AMD CPUs only, and says that `auto` fell back where it fell back
    /// to a host profile. The hint keeps the backend's error chain.
    #[test]
    fn auto_names_host_profiles_where_the_backend_lacks_the_profile() {
        let unsupported = || {
            anyhow::Error::new(TimeAbiError::new(
                TimeAbiCode::ProfileUnsupported,
                "the kvm backend does not support CPU profile amd.milan.v1",
            ))
            .context("failed to create the prototype partition")
        };
        let icelake = HostCpuSignature::new(*b"GenuineIntel", TEST_SIGNATURE);
        let milan = HostCpuSignature::new(*b"AuthenticAMD", 0x00a0_0f11);
        let tiger_lake = HostCpuSignature::new(*b"GenuineIntel", 0x0008_06c1);
        let hygon = HostCpuSignature::new(*b"HygonGenuine", 0x0090_0f01);
        for host in [&icelake, &milan] {
            let err = hint_host_profile(unsupported(), cpu_profile::AUTO, host, &NO_HOOKS);
            let message = format!("{err:#}");
            assert!(message.starts_with("this host cannot boot the built-in CPU profile that --cpu-profile auto selected; --cpu-profile host boots"), "{message}");
            assert!(
                message.ends_with(&format!("{:#}", unsupported())),
                "{message}"
            );
            assert_eq!(
                leading_code(&err),
                Some(TimeAbiCode::ProfileUnsupported),
                "{message}"
            );
        }
        // Where `auto` fell back, every CPU profile failure says so instead.
        for code in [
            TimeAbiCode::ProfileUnsupported,
            TimeAbiCode::ProfileTimeBits,
            TimeAbiCode::CpuSurface,
            TimeAbiCode::CpuUnlisted,
        ] {
            let err = anyhow::Error::new(TimeAbiError::new(code, "intel.host.v1 failed"));
            let message = format!(
                "{:#}",
                hint_host_profile(err, cpu_profile::AUTO, &tiger_lake, &NO_HOOKS)
            );
            assert!(
                message.starts_with(&FellBack::new(&tiger_lake).to_string()),
                "{message}"
            );
            assert!(!message.contains(HOST_PROFILE_HINT), "{message}");
        }
        // Another vendor's CPU, an explicit profile, `host`, and other
        // failures get no hint, after a fallback too.
        for (requested, host, err) in [
            (cpu_profile::AUTO, &hygon, unsupported()),
            (TEST_PROFILE, &icelake, unsupported()),
            (cpu_profile::HOST, &milan, unsupported()),
            (cpu_profile::HOST, &tiger_lake, unsupported()),
            (
                cpu_profile::AUTO,
                &milan,
                anyhow::Error::new(TimeAbiError::new(TimeAbiCode::CpuSurface, "VP 0 differs"))
                    .context("failed to create the prototype partition"),
            ),
            (
                cpu_profile::AUTO,
                &milan,
                anyhow::anyhow!("no time ABI code"),
            ),
            (
                cpu_profile::AUTO,
                &tiger_lake,
                anyhow::anyhow!("no time ABI code"),
            ),
            (
                cpu_profile::AUTO,
                &tiger_lake,
                anyhow::Error::new(TimeAbiError::new(
                    TimeAbiCode::TscSyncUnsupported,
                    "no synchronized TSC set",
                )),
            ),
        ] {
            let before = format!("{err:#}");
            let message = format!("{:#}", hint_host_profile(err, requested, host, &NO_HOOKS));
            assert_eq!(message, before);
        }
        // Neither does a restore, nor a VM without the time ABI.
        for (requested, restoring) in [
            (Some((cpu_profile::AUTO, &NO_HOOKS)), true),
            (Some((cpu_profile::AUTO, &HOST_CPU_UNKNOWN)), true),
            (None, false),
        ] {
            let message = format!("{:#}", hint_cold_boot(unsupported(), requested, restoring));
            assert_eq!(message, format!("{:#}", unsupported()));
        }
    }

    #[test]
    fn a_host_profile_needs_a_backend_fingerprint() {
        for hooks in [&NO_HOOKS, &HOST_CPU_UNKNOWN] {
            let error = select_cpu_profile(cpu_profile::HOST, "nosuch", None, hooks).unwrap_err();
            assert_eq!(
                format!("{error:#}"),
                "the nosuch backend cannot be fingerprinted"
            );
        }
    }

    /// Returns the fingerprint of a `backend` host of the GenuineIntel CPU
    /// with `signature` and `brand`, whose surface is the test profile's
    /// after `edit` changes its CPUID.
    fn test_fingerprint_with(
        backend: &str,
        signature: u32,
        brand: &str,
        edit: impl FnOnce(&mut Vec<cpu_profile::cpuid::CpuidEntry>),
    ) -> CpuFingerprint {
        let mut cpuid: Vec<_> = test_profile()
            .cpuid()
            .iter()
            .map(|entry| {
                let (leaf, subleaf) = entry.key();
                let mut registers = entry.values();
                if (leaf, subleaf) == (1, None) {
                    registers[0] = signature;
                }
                cpu_profile::cpuid::CpuidEntry::new(leaf, subleaf, registers)
            })
            .collect();
        edit(&mut cpuid);
        let host = HostCpuSignature::new(*b"GenuineIntel", signature);
        let host = cpu_profile::host::HostIdentity {
            cpu: cpu_profile::host::HostCpu {
                vendor: "GenuineIntel".to_owned(),
                signature: cpu_profile::Hex32(signature),
                family: host.family(),
                model: host.model(),
                stepping: host.stepping(),
                brand: brand.to_owned(),
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
        CpuFingerprint::new(
            cpu_profile::fingerprint::ToolIdentity {
                name: "test".to_owned(),
                version: "0".to_owned(),
            },
            host,
            cpu_profile::fingerprint::BackendFingerprint::new(backend, "test", cpuid),
        )
    }

    /// Returns the fingerprint of a `backend` host of the GenuineIntel CPU
    /// with `signature` and `brand`, whose surface is the test profile's.
    fn test_fingerprint(backend: &str, signature: u32, brand: &str) -> CpuFingerprint {
        test_fingerprint_with(backend, signature, brand, |_| {})
    }

    /// Returns a snapshot's record of the host profile `profile`, captured
    /// on a CPU with `signature`.
    fn host_profile_record(profile: &CpuProfile, signature: u32) -> SnapshotCpuProfile {
        SnapshotCpuProfile {
            id: profile.id().to_owned(),
            sha256: profile.digest().to_vec(),
            profile: profile.encode(),
            effective_cpuid: Vec::new(),
            capture_cpu_signature: signature,
        }
    }

    /// Returns the host profile that `--cpu-profile host` derives on a
    /// `backend` host of the test profile's CPU whose surface is the test
    /// profile's, and a snapshot's record of it.
    fn test_host_profile(backend: &str) -> (CpuProfile, SnapshotCpuProfile) {
        let fingerprint = test_fingerprint(backend, TEST_SIGNATURE, "");
        let profile = cpu_profile::derive_host_profile(&fingerprint).unwrap();
        let record = host_profile_record(&profile, TEST_SIGNATURE);
        (profile, record)
    }

    #[test]
    fn restore_takes_a_host_profile_from_the_snapshot() {
        let (profile, record) = test_host_profile("whp");
        let host = HostCpuSignature::new(*b"GenuineIntel", TEST_SIGNATURE);
        let select = |requested: &str, host, restored| {
            selected(select_on_host(requested, host, restored, &NO_HOOKS))
        };
        assert_eq!(restored_host_profile(&record), Some(record.clone()));
        let selected_profile = select(profile.id(), &host, Some(&record)).unwrap();
        assert!(selected_profile.is_host());
        assert_eq!(*selected_profile, profile);
        assert_eq!(
            selected_profile.record().encoding,
            record.profile.as_slice()
        );
        assert_eq!(
            selected_profile.record().digest.as_slice(),
            record.sha256.as_slice()
        );

        // Each VM takes its own snapshot's host profile, even one of the same
        // ID, here derived from another backend's fingerprint.
        let (other, other_record) = test_host_profile("kvm");
        assert_eq!(other.id(), profile.id());
        let reselected = select(other.id(), &host, Some(&other_record)).unwrap();
        assert_eq!(*reselected, other);
        assert_ne!(reselected.record().digest, selected_profile.record().digest);

        let error_code = |result: Result<PartitionProfile, ProfileError>| result.unwrap_err().code;
        // Only the snapshot of a host profile carries it.
        assert_eq!(
            error_code(select(profile.id(), &host, None)),
            ProfileErrorCode::ProfileUnknown
        );
        let mut renamed = record.clone();
        renamed.id = "intel.host.v2".to_owned();
        assert_eq!(
            error_code(select(profile.id(), &host, Some(&renamed))),
            ProfileErrorCode::ProfileUnknown
        );
        // A host profile serves only its own CPU model and stepping.
        let elsewhere = HostCpuSignature::new(*b"GenuineIntel", TEST_SIGNATURE + 1);
        assert_eq!(
            error_code(select(profile.id(), &elsewhere, Some(&record))),
            ProfileErrorCode::CpuGeneration
        );
        // A pinned profile's document under the host profile's ID.
        let pinned = cpu_profile::pinned_record(TEST_PROFILE).unwrap();
        let mut swapped = record.clone();
        swapped.profile = pinned.encoding.to_vec();
        swapped.sha256 = pinned.digest.to_vec();
        assert_eq!(
            error_code(select(profile.id(), &host, Some(&swapped))),
            ProfileErrorCode::ProfileDigest
        );
        // A pinned profile's snapshot needs no record: its ID selects it.
        let mut pinned_snapshot = swapped.clone();
        pinned_snapshot.id = TEST_PROFILE.to_owned();
        assert_eq!(restored_host_profile(&pinned_snapshot), None);
        let pinned_profile = select(TEST_PROFILE, &host, None).unwrap();
        assert!(!pinned_profile.is_host());
        assert_eq!(pinned_profile.record(), pinned);
    }

    /// A snapshot of a cold boot whose `auto` fell back records the host
    /// profile, which a restore takes from the snapshot, as it takes any
    /// host profile's, with any test hooks: a restore never falls back, and
    /// serves only the capture host's CPU model and stepping.
    #[test]
    fn restore_takes_a_fallback_profile_from_the_snapshot() {
        let host = HostCpuSignature::new(*b"GenuineIntel", GRANITE_RAPIDS);
        let fingerprint = test_fingerprint("kvm", GRANITE_RAPIDS, GRANITE_RAPIDS_BRAND);
        let fallback = fall_back(&host, || Ok(fingerprint)).unwrap();
        let record = host_profile_record(&fallback, GRANITE_RAPIDS);
        assert_eq!(fallback.record().encoding, record.profile.as_slice());
        assert_eq!(
            fallback.record().digest.as_slice(),
            record.sha256.as_slice()
        );
        assert_eq!(restored_host_profile(&record), Some(record.clone()));
        for hooks in [&NO_HOOKS, &HOST_CPU_UNKNOWN] {
            let restored =
                selected(select_on_host(fallback.id(), &host, Some(&record), hooks)).unwrap();
            assert!(restored.is_host());
            assert_eq!(*restored, *fallback);
            assert_eq!(restored.record(), fallback.record());

            let stepping_2 = HostCpuSignature::new(*b"GenuineIntel", GRANITE_RAPIDS + 1);
            assert_eq!(
                selected(select_on_host(
                    fallback.id(),
                    &stepping_2,
                    Some(&record),
                    hooks
                ))
                .unwrap_err()
                .code,
                ProfileErrorCode::CpuGeneration
            );
        }
    }
}
