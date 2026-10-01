// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! NVX time ABI v1 in the VM worker, selected by the hidden `--x-time-abi-v1`
//! development switch: the partition configuration, the backend preflight
//! and rate policy, and the `time-abi` state unit.

#![cfg(guest_arch = "x86_64")]

use crate::partition::HvlitePartition;
use inspect::Inspect;
use inspect::InspectMut;
use state_unit::StateUnit;
use std::sync::Arc;
use virt::time_abi::DeclaredRates;
use virt::time_abi::NegotiatedRates;
use virt::time_abi::TimeAbiCode;
use virt::time_abi::TimeAbiConfig;
use virt::time_abi::TimeAbiError;
use virt::time_abi::TimeAbiMsrs;
use virt::time_abi::TimeAbiTestHooks;
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

/// Runs the backend preflight and the rate policy, and declares the rates
/// to the guest: restore steps 8 and 9 of the specification when `saved`
/// holds the snapshot's rates, or their cold-boot equivalent.
pub(super) fn declare_rates(
    partition: &dyn HvlitePartition,
    msrs: &TimeAbiMsrs,
    hypervisor: &str,
    saved: Option<DeclaredRates>,
    hooks: &TimeAbiTestHooks,
) -> anyhow::Result<NegotiatedRates> {
    let backend = partition.time_abi().ok_or_else(|| {
        TimeAbiError::new(
            TimeAbiCode::TscSyncUnsupported,
            format!("the {hypervisor} backend does not implement the time ABI"),
        )
    })?;
    let preflight = backend.preflight()?;
    let rates = virt::time_abi::negotiate_rates(backend, hypervisor, saved, hooks)?;
    msrs.declare(rates.declared)?;
    tracing::info!(
        hypervisor,
        msr_route = ?preflight.msr_route,
        sync = ?preflight.sync,
        native_tsc_hz = rates.native_tsc_hz,
        tsc_hz = rates.declared.tsc_hz,
        apic_hz = rates.declared.apic_hz,
        deviation_ppb = ?rates.check.map(|check| check.deviation_ppb),
        "time ABI rates declared"
    );
    Ok(rates)
}

/// The `time-abi` state unit. It saves and restores
/// `HV_X64_MSR_TSC_INVARIANT_CONTROL`, which is partition-wide guest state.
pub(super) struct TimeAbiUnit(pub Arc<TimeAbiMsrs>);

impl InspectMut for TimeAbiUnit {
    fn inspect_mut(&mut self, req: inspect::Request<'_>) {
        self.0.inspect(req);
    }
}

impl StateUnit for TimeAbiUnit {
    async fn start(&mut self) -> anyhow::Result<()> {
        Ok(())
    }

    async fn stop(&mut self) {}

    async fn reset(&mut self) -> anyhow::Result<()> {
        self.0.reset();
        Ok(())
    }

    async fn save(&mut self) -> Result<Option<SavedStateBlob>, SaveError> {
        Ok(Some(SavedStateBlob::new(self.0.save())))
    }

    async fn restore(&mut self, state: SavedStateBlob) -> Result<(), RestoreError> {
        self.0.restore(state.parse()?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use virt::time_abi::msr::MSR_TSC_INVARIANT_CONTROL;
    use vm_topology::processor::VpIndex;

    #[test]
    fn unit_saves_restores_and_resets_the_invariant_control() {
        futures::executor::block_on(async {
            let (msrs, config) = partition_config(4);
            assert!(Arc::ptr_eq(&msrs, &config.msrs));
            msrs.write(VpIndex::BSP, MSR_TSC_INVARIANT_CONTROL, 1)
                .unwrap()
                .unwrap();
            let mut unit = TimeAbiUnit(msrs.clone());
            let saved = unit.save().await.unwrap().unwrap();

            let (restored, _) = partition_config(4);
            let mut restored_unit = TimeAbiUnit(restored.clone());
            restored_unit.restore(saved).await.unwrap();
            assert_eq!(restored.tsc_invariant_control(), 1);

            restored_unit.reset().await.unwrap();
            assert_eq!(restored.tsc_invariant_control(), 0);
        });
    }

    #[test]
    fn partition_cpuid_carries_the_identity() {
        let (_, config) = partition_config(2);
        let mut cpuid = |leaf, subleaf| config.cpuid.result(leaf, subleaf, &[0; 4]);
        virt::time_abi::identity::check_identity(&mut cpuid, 2).unwrap();
    }
}
