// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! NVX time ABI v1 in the VM worker, selected by the hidden `--x-time-abi-v1`
//! development switch: the partition configuration, the backend preflight
//! and rate policy, and the `time-abi` state unit.

#![cfg(guest_arch = "x86_64")]

use crate::partition::HvlitePartition;
use inspect::Inspect;
use inspect::InspectMut;
use openvmm_defs::time_abi::SnapshotCpuProfile;
use state_unit::StateUnit;
use std::sync::Arc;
use virt::time_abi::DeclaredRates;
use virt::time_abi::IdentityMsrRoute;
use virt::time_abi::RateCheck;
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

/// Returns the identity MSR handler and the time ABI configuration of a new
/// partition with `vp_count` VPs.
pub(super) fn partition_config(vp_count: u32) -> (Arc<TimeAbiMsrs>, TimeAbiConfig) {
    let msrs = Arc::new(TimeAbiMsrs::new());
    let config = TimeAbiConfig {
        cpuid: Arc::new(virt::time_abi::identity::time_abi_cpuid(vp_count, true)),
        msrs: msrs.clone(),
    };
    (msrs, config)
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
            let (msrs, config) = partition_config(4);
            assert!(Arc::ptr_eq(&msrs, &config.msrs));
            msrs.write(VpIndex::BSP, MSR_TSC_INVARIANT_CONTROL, 1)
                .unwrap()
                .unwrap();
            let mut unit = test_unit(msrs.clone());
            let saved = unit.save().await.unwrap().unwrap();

            let (restored, _) = partition_config(4);
            let mut restored_unit = test_unit(restored.clone());
            restored_unit.restore(saved).await.unwrap();
            assert_eq!(restored.tsc_invariant_control(), 1);

            restored_unit.reset().await.unwrap();
            assert_eq!(restored.tsc_invariant_control(), 0);
        });
    }

    #[test]
    fn unit_inspects_the_report() {
        let (msrs, _) = partition_config(1);
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
        let (_, config) = partition_config(2);
        let mut cpuid = |leaf, subleaf| config.cpuid.result(leaf, subleaf, &[0; 4]);
        virt::time_abi::identity::check_identity(&mut cpuid, 2).unwrap();
    }
}
