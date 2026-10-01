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
use inspect::Inspect;
use inspect::InspectMut;
use openvmm_defs::time_abi::RestoreTimeInput;
use openvmm_defs::time_abi::SnapshotCpuProfile;
use openvmm_defs::time_abi::SnapshotTimeContract;
use openvmm_defs::time_abi::TimeCapture;
use sha2::Digest as _;
use state_unit::SpawnedUnit;
use state_unit::StateUnit;
use std::sync::Arc;
use std::time::Duration;
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
use vmcore::save_restore::RestoreError;
use vmcore::save_restore::SaveError;
use vmcore::save_restore::SavedStateBlob;

/// The name of the time ABI state unit.
pub(super) const TIME_ABI_UNIT: &str = "time-abi";

/// The time ABI state of a loaded VM.
pub(super) struct TimeAbiState {
    /// The `time-abi` state unit.
    pub _unit: SpawnedUnit<TimeAbiUnit>,
    /// The identity MSR handler, holding the declared rates.
    pub msrs: Arc<TimeAbiMsrs>,
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
    /// The ID of the partition's CPU profile.
    pub cpu_profile: String,
}

/// Returns the identity MSR handler and the time ABI configuration of a new
/// partition with `vp_count` VPs and the CPU profile `cpu_profile`.
pub(super) fn partition_config(
    vp_count: u32,
    cpu_profile: String,
) -> (Arc<TimeAbiMsrs>, TimeAbiConfig) {
    let msrs = Arc::new(TimeAbiMsrs::new());
    let config = TimeAbiConfig {
        cpuid: Arc::new(virt::time_abi::identity::time_abi_cpuid(vp_count, true)),
        msrs: msrs.clone(),
        cpu_profile,
    };
    (msrs, config)
}

/// Selects the CPU profile of a new partition before it is created, and
/// returns its ID.
///
/// `requested` is `--cpu-profile` on cold boot (`auto` or an ID), or the
/// profile a snapshot recorded on restore. `auto` and a pinned ID select a
/// pinned profile whose generation contains the host CPU
/// (`E_PROFILE_HOST_UNKNOWN`, `E_PROFILE_UNKNOWN`, `E_CPU_GENERATION`). An
/// interim ID must be the backend's own (`E_PROFILE_UNKNOWN`).
pub(super) fn select_cpu_profile(requested: &str, hypervisor: &str) -> anyhow::Result<String> {
    if virt::time_abi::surface::is_interim_cpu_profile(requested) {
        virt::time_abi::surface::check_interim_cpu_profile(requested, hypervisor)?;
        return Ok(requested.to_owned());
    }
    let profile = cpu_profile::select(requested, &host_cpu()?)?;
    Ok(profile.id().to_owned())
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

/// Compares the effective CPUID the backend programmed with the snapshot's
/// record (`E_CPU_SURFACE`): the effective-CPUID check of restore step 8 of
/// the specification.
pub(super) fn check_cpu_surface(
    partition: &dyn HvlitePartition,
    hypervisor: &str,
    record: &SnapshotCpuProfile,
) -> anyhow::Result<()> {
    let effective = backend(partition, hypervisor)?.effective_cpuid()?;
    virt::time_abi::surface::check_effective_cpuid(&effective, &record.effective_cpuid)?;
    Ok(())
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
    let effective_cpuid = virt::time_abi::surface::encode_cpuid(&backend.effective_cpuid()?);
    // A pinned profile is recorded with its canonical document; an interim
    // profile has none.
    let (profile_sha256, profile) = match cpu_profile::pinned(&state.report.cpu_profile) {
        Some(profile) => {
            let encoded = profile.encode();
            (sha2::Sha256::digest(&encoded).to_vec(), encoded)
        }
        None => (sha2::Sha256::digest(b"").to_vec(), Vec::new()),
    };
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
            id: state.report.cpu_profile.clone(),
            sha256: profile_sha256,
            profile,
            effective_cpuid_sha256: sha2::Sha256::digest(&effective_cpuid).to_vec(),
            effective_cpuid,
            capture_cpu_signature: virt::time_abi::surface::host_cpu_signature().unwrap_or(0),
        },
    })
}

/// Releases partition time immediately before the restored VPs first run:
/// restore step 17 of the specification.
pub(super) fn release_time(
    partition: &dyn HvlitePartition,
    state: &TimeAbiState,
) -> anyhow::Result<()> {
    backend(partition, &state.report.hypervisor)?.release_time()?;
    Ok(())
}

impl LoadedVm {
    /// Restores the guest clocks: restore steps 11 to 16 of the
    /// specification, after the saved state is restored and before any
    /// restored VP runs.
    ///
    /// It checks the LAPIC timers, performs the synchronized TSC set at the
    /// downtime selected at the restore anchor, advances and sets every
    /// LAPIC, advances VM time and the RTC, and seals the time fields of the
    /// restore packet. The PIT checks itself when restored.
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

        self.inner.partition_unit.check_one_shot_timers().await?;

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

        self.inner
            .partition_unit
            .advance_lapic_timers(downtime.nanos, apic_hz)
            .await?;
        self.state_units
            .advance_time(Duration::from_nanos(downtime.nanos))
            .await
            .context("failed to advance restored VM time")?;

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
    use virt::time_abi::msr::MSR_TSC_INVARIANT_CONTROL;
    use vm_topology::processor::VpIndex;

    const TEST_PROFILE: &str = "intel.icelake-sp.v1";

    fn test_unit(msrs: Arc<TimeAbiMsrs>) -> TimeAbiUnit {
        TimeAbiUnit {
            msrs,
            report: TimeAbiReport {
                hypervisor: "kvm".to_owned(),
                cpu_profile: "interim.host.kvm.v1".to_owned(),
                msr_route: IdentityMsrRoute::ExitToVmm,
                sync: TscSyncMethod::CommonOffset,
                native_tsc_hz: 2_100_000_000,
                rate_check: None,
            },
        }
    }

    #[test]
    fn unit_saves_restores_and_resets_the_invariant_control() {
        futures::executor::block_on(async {
            let (msrs, config) = partition_config(4, TEST_PROFILE.to_owned());
            assert!(Arc::ptr_eq(&msrs, &config.msrs));
            msrs.write(VpIndex::BSP, MSR_TSC_INVARIANT_CONTROL, 1)
                .unwrap()
                .unwrap();
            let mut unit = test_unit(msrs.clone());
            let saved = unit.save().await.unwrap().unwrap();

            let (restored, _) = partition_config(4, TEST_PROFILE.to_owned());
            let mut restored_unit = test_unit(restored.clone());
            restored_unit.restore(saved).await.unwrap();
            assert_eq!(restored.tsc_invariant_control(), 1);

            restored_unit.reset().await.unwrap();
            assert_eq!(restored.tsc_invariant_control(), 0);
        });
    }

    #[test]
    fn unit_inspects_the_report() {
        let (msrs, _) = partition_config(1, TEST_PROFILE.to_owned());
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

    #[test]
    fn partition_cpuid_carries_the_identity() {
        let (_, config) = partition_config(2, TEST_PROFILE.to_owned());
        assert_eq!(config.cpu_profile, TEST_PROFILE);
        let mut cpuid = |leaf, subleaf| config.cpuid.result(leaf, subleaf, &[0; 4]);
        virt::time_abi::identity::check_identity(&mut cpuid, 2).unwrap();
    }

    fn code(result: anyhow::Result<String>) -> String {
        let message = format!("{:#}", result.unwrap_err());
        message
            .strip_prefix('[')
            .and_then(|rest| rest.split_once(']'))
            .map_or(message.clone(), |(code, _)| code.to_owned())
    }

    #[test]
    fn cpu_profile_selection() {
        assert_eq!(
            select_cpu_profile("interim.host.kvm.v1", "kvm").unwrap(),
            "interim.host.kvm.v1"
        );
        assert_eq!(
            code(select_cpu_profile("interim.host.kvm.v1", "mshv")),
            "E_PROFILE_UNKNOWN"
        );
        assert_eq!(
            code(select_cpu_profile("intel.cascadelake.v1", "kvm")),
            "E_PROFILE_UNKNOWN"
        );

        // `auto` selects the pinned profile of the host's generation, if any,
        // and a pinned ID selects its profile only in its generation.
        let host = host_cpu().unwrap();
        match select_cpu_profile("auto", "whp") {
            Ok(id) => assert!(
                cpu_profile::check_generation(cpu_profile::pinned(&id).unwrap(), &host).is_ok()
            ),
            Err(err) => assert_eq!(code(Err(err)), "E_PROFILE_HOST_UNKNOWN"),
        }
        for profile in cpu_profile::pinned_profiles() {
            let selected = select_cpu_profile(profile.id(), "kvm");
            if cpu_profile::check_generation(profile, &host).is_ok() {
                assert_eq!(selected.unwrap(), profile.id());
            } else {
                assert_eq!(code(selected), "E_CPU_GENERATION");
            }
        }
    }
}
