// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! NVX time ABI v1 in the VM worker, selected by the hidden `--x-time-abi-v1`
//! development switch: the partition configuration, the backend preflight
//! and rate policy, the `time-abi` state unit, the capture records, and the
//! restore clock.

#![cfg(guest_arch = "x86_64")]

use super::LoadedVm;
use crate::partition::HvlitePartition;
use anyhow::Context as _;
use chipset_resources::microvm_time::RestoreTimeRecord;
use cpu_profile::CpuProfile;
use cpu_profile::EffectiveCpuid;
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
use vm_topology::processor::ProcessorTopology;
use vm_topology::processor::x86::ApicMode;
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
    pub profile: &'static CpuProfile,
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
    pub profile: &'static CpuProfile,
    /// The effective CPUID of the partition.
    pub effective_cpuid: Arc<EffectiveCpuid>,
}

/// Returns the identity MSR handler and the time ABI configuration of a new
/// partition with the CPU profile `profile` and its effective CPUID.
pub(super) fn partition_config(
    profile: &CpuProfile,
    effective_cpuid: &EffectiveCpuid,
) -> (Arc<TimeAbiMsrs>, TimeAbiConfig) {
    let msrs = Arc::new(TimeAbiMsrs::new());
    let config = TimeAbiConfig {
        cpuid: Arc::new(backend_cpuid(effective_cpuid)),
        msrs: msrs.clone(),
        cpu_profile: profile.id().to_owned(),
    };
    (msrs, config)
}

/// Selects the CPU profile of a new partition before it is created.
///
/// `requested` is `--cpu-profile` on cold boot (`auto` or an ID), or the
/// profile a snapshot recorded on restore. It selects a pinned profile whose
/// generation contains the host CPU (`E_PROFILE_HOST_UNKNOWN`,
/// `E_PROFILE_UNKNOWN`, `E_CPU_GENERATION`).
pub(super) fn select_cpu_profile(requested: &str) -> anyhow::Result<&'static CpuProfile> {
    Ok(cpu_profile::select(requested, &host_cpu()?)?)
}

/// Builds the effective CPUID of a partition with `profile` and `topology`:
/// the profile completed with OpenVMM's topology leaves, each extended
/// topology leaf's terminating subleaf, the APIC mode, and
/// the time ABI's identity and explicit zero leaves. Fails with
/// `E_CPU_SURFACE` if they do not complete the profile exactly.
pub(super) fn effective_cpuid(
    profile: &CpuProfile,
    topology: &ProcessorTopology,
) -> anyhow::Result<EffectiveCpuid> {
    let result = |leaf: &CpuidLeaf| cpu_profile::CpuidResult {
        function: leaf.function,
        index: leaf.index,
        result: leaf.result,
        mask: leaf.mask,
    };
    let mut topology_leaves = Vec::new();
    virt::x86::topology::topology_cpuid(
        topology,
        &|leaf, subleaf| profile.lookup(leaf, subleaf),
        &mut topology_leaves,
    )
    .map_err(|err| {
        TimeAbiError::new(
            TimeAbiCode::CpuSurface,
            format!(
                "cannot build the topology CPUID of CPU profile {}: {err}",
                profile.id()
            ),
        )
    })?;
    virt::x86::topology::terminate_extended_topology(topology, &mut topology_leaves);
    let mut vm: Vec<_> = topology_leaves.iter().map(result).collect();
    vm.push(cpu_profile::x2apic_cpuid(!matches!(
        topology.apic_mode(),
        ApicMode::XApic
    )));
    let identity: Vec<_> = virt::time_abi::identity::identity_cpuid_leaves(topology.vp_count())
        .iter()
        .map(result)
        .chain(virt::time_abi::identity::identity_zero_cpuid_leaves().map(|leaf| result(&leaf)))
        .collect();
    Ok(profile.effective_cpuid(&vm, &identity)?)
}

/// Returns the CPUID results a backend programs for `effective`: every
/// result of the effective CPUID, with the per-VP APIC identity bits
/// unmasked, so that the backend sets each VP's own.
fn backend_cpuid(effective: &EffectiveCpuid) -> CpuidLeafSet {
    CpuidLeafSet::new(
        effective
            .results()
            .map(|result| {
                let per_vp = virt::x86::topology::per_vp_cpuid_bits(result.function);
                CpuidLeaf {
                    function: result.function,
                    index: result.index,
                    result: result.result,
                    mask: [0, 1, 2, 3].map(|register| result.mask[register] & !per_vp[register]),
                }
            })
            .collect(),
    )
}

/// Returns the vendor and signature of the host CPU (`E_CPU_GENERATION` if
/// the host is not x86-64).
fn host_cpu() -> Result<cpu_profile::HostCpuSignature, TimeAbiError> {
    // The host CPU is identified with the host's CPUID instruction.
    // xtask-fmt allow-target-arch cpu-intrinsic
    #[cfg(target_arch = "x86_64")]
    {
        Ok(cpu_profile::HostCpuSignature::current())
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
    let pinned = cpu_profile::pinned_record(state.profile.id())
        .with_context(|| format!("CPU profile {} is not pinned", state.profile.id()))?;
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
            id: pinned.id.to_owned(),
            sha256: pinned.digest.to_vec(),
            profile: pinned.encoding.to_vec(),
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
    use virt::time_abi::msr::MSR_TSC_INVARIANT_CONTROL;
    use vm_topology::processor::TopologyBuilder;
    use vm_topology::processor::VpIndex;
    use vm_topology::processor::x86::X2ApicState;

    const TEST_PROFILE: &str = "intel.icelake-sp.v1";

    fn test_profile() -> &'static CpuProfile {
        cpu_profile::pinned(TEST_PROFILE).unwrap()
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
        let (msrs, config) = partition_config(profile, &effective);
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
                    let (_, config) = partition_config(profile, &effective);
                    assert_eq!(config.cpu_profile, profile.id());

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
                    let (_, config) = partition_config(profile, &effective);
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

                let (_, config) = partition_config(profile, &effective);
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
        for requested in ["interim.host.kvm.v1", "intel.cascadelake.v1"] {
            assert_eq!(code(select_cpu_profile(requested)), "E_PROFILE_UNKNOWN");
        }

        // `auto` selects the pinned profile of the host's generation, if any,
        // and a pinned ID selects its profile only in its generation.
        let host = host_cpu().unwrap();
        match select_cpu_profile("auto") {
            Ok(profile) => assert!(cpu_profile::check_generation(profile, &host).is_ok()),
            Err(err) => assert_eq!(code::<()>(Err(err)), "E_PROFILE_HOST_UNKNOWN"),
        }
        for profile in cpu_profile::pinned_profiles() {
            let selected = select_cpu_profile(profile.id());
            if cpu_profile::check_generation(profile, &host).is_ok() {
                assert_eq!(selected.unwrap().id(), profile.id());
            } else {
                assert_eq!(code(selected), "E_CPU_GENERATION");
            }
        }
    }
}
