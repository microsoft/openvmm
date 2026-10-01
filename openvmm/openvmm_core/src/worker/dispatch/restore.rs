// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Snapshot restore support for the VM worker: the restore inputs taken from
//! the worker parameters, the restore state kept by [`LoadedVm`], the VP
//! prefix instantiated by an explicit restore-time activation target, the
//! steps around applying the saved state, and the post-restore start sequence
//! (input gate, readiness event, and VP release).

use super::LoadedVm;
use super::clock;
use super::clock::RestoreTime;
use crate::partition::HvlitePartition;
use anyhow::Context;
use hypervisor_resources::HypervisorKind;
use hypervisor_resources::MshvHandle;
use membacking::FileMappingMode;
use membacking::SharedMemoryBacking;
use openvmm_defs::profile::ProfileSpan;
use openvmm_defs::worker::RESTORE_READY_EVENT_V1;
use openvmm_defs::worker::SavedState;
use openvmm_defs::worker::SharedMemoryFd;
use openvmm_defs::worker::SnapshotRestoreGuards;
use openvmm_defs::worker::VmWorkerParameters;
use pal_async::driver::Driver;
use pal_async::timer::Instant;
use pal_async::timer::PolledTimer;
use state_unit::StateUnits;
use std::fs::File;
use std::future::Future;
use std::io::Write as _;
use std::time::Duration;
use vm_resource::ResourceId;
use vmm_core::partition_unit::StopGuard;

/// Snapshot-restore inputs taken from the [`VmWorkerParameters`].
pub(super) struct RestoreParameters {
    /// Restore state handed to the loaded VM.
    pub(super) state: SnapshotRestore,
    /// Saved canonical CPU contract required by restore.
    pub(super) cpu_contract: Option<Vec<u8>>,
    /// Mapping mode of the file-backed guest RAM handle.
    file_mapping_mode: FileMappingMode,
    /// Snapshot generation handles that must outlive the restored VM.
    pub(super) guards: Option<SnapshotRestoreGuards>,
}

impl RestoreParameters {
    /// Takes the restore inputs out of the worker parameters and validates
    /// the saved time contract.
    pub(super) fn take(parameters: &mut VmWorkerParameters) -> anyhow::Result<Self> {
        let guards = parameters.snapshot_restore_guards.take();
        let ready_sink = parameters.restore_ready_sink.take();
        let gate_timeout = parameters.restore_gate_timeout.take();
        let restore_time = clock::restore_time_contract(
            parameters.restore_downtime,
            parameters.restore_tsc_frequency_hz,
            parameters.restore_apic_frequency_hz,
        )?;
        tracing::debug!(?restore_time, "received snapshot restore time contract");
        let restore_vp_count = parameters.restore_vp_count.take();
        let vp_prefix = restore_vp_prefix(parameters.hypervisor.id(), restore_vp_count);
        tracing::debug!(
            ?restore_vp_count,
            ?vp_prefix,
            "received restore-time VP activation target"
        );
        let cpu_contract = parameters.restore_cpu_contract.take();
        let time_abi = parameters.restore_time.take();
        anyhow::ensure!(
            time_abi.is_none() || (restore_time.is_none() && cpu_contract.is_none()),
            "a restore carries either the time ABI inputs or the legacy clock contract"
        );
        let file_mapping_mode = if parameters.shared_memory_copy_on_write {
            FileMappingMode::CopyOnWrite
        } else {
            FileMappingMode::Shared
        };
        Ok(Self {
            state: SnapshotRestore {
                time: restore_time,
                time_abi,
                vp_prefix,
                ready_sink,
                gate_timeout,
                ..Default::default()
            },
            cpu_contract,
            file_mapping_mode,
            guards,
        })
    }

    /// Wraps the file-backed guest RAM handle with the restore's mapping mode.
    pub(super) fn shared_memory_backing(&self, fd: SharedMemoryFd) -> SharedMemoryBacking {
        SharedMemoryBacking::from_mappable_with_mode(fd.into(), self.file_mapping_mode)
    }
}

/// Returns the VP prefix that a restore instantiates, or `None` for the full
/// VP capacity.
///
/// Only MSHV instantiates an explicit restore-time activation target: it
/// creates application processors lazily while binding them, so discarding
/// the suffix binders avoids creating VPs that the restore will not run.
/// Other backends create their VPs with the partition and keep the full
/// capacity.
fn restore_vp_prefix(hypervisor: &str, restore_vp_count: Option<u32>) -> Option<u32> {
    restore_vp_count.filter(|_| hypervisor == <MshvHandle as ResourceId<HypervisorKind>>::ID)
}

/// Discards the binders after an explicit VP prefix and returns the number
/// of VPs to instantiate. The topology and saved VP inventory keep the full
/// capacity.
fn select_instantiated_vps<T>(
    vps: &mut Vec<T>,
    vp_capacity: u32,
    vp_prefix: Option<u32>,
    restoring: bool,
) -> anyhow::Result<u32> {
    anyhow::ensure!(
        vps.len() == vp_capacity as usize,
        "backend returned {} VP binders for topology capacity {vp_capacity}",
        vps.len()
    );
    let Some(vp_prefix) = vp_prefix else {
        return Ok(vp_capacity);
    };
    anyhow::ensure!(restoring, "a restore VP prefix requires saved state");
    anyhow::ensure!(
        (1..=vp_capacity).contains(&vp_prefix),
        "restore VP prefix {vp_prefix} is outside topology capacity 1..={vp_capacity}"
    );
    vps.truncate(vp_prefix as usize);
    Ok(vp_prefix)
}

/// Snapshot-restore state of a [`LoadedVm`].
#[derive(Default)]
pub(super) struct SnapshotRestore {
    /// Saved guest-clock contract, checked and applied around the restore.
    time: Option<RestoreTime>,
    /// The time ABI inputs of a restore, validated by the controller.
    pub(super) time_abi: Option<openvmm_defs::time_abi::RestoreTimeInput>,
    /// Whether partition time must be released before the restored VPs
    /// first run (time ABI restore step 17).
    time_abi_release: bool,
    /// VP prefix instantiated for an explicit MSHV restore-time activation
    /// target.
    vp_prefix: Option<u32>,
    /// Whether the VM was loaded from saved state.
    restored_from_snapshot: bool,
    /// Holds the VPs stopped after a restore until the VM first starts.
    start_guard: Option<StopGuard>,
    /// Single-use sink for the restore readiness event.
    ready_sink: Option<File>,
    /// Timeout for the post-restore input gate, when required.
    pub(super) gate_timeout: Option<Duration>,
    /// Deadline for the guest to acknowledge the post-restore input gate.
    pub(super) gate_deadline: Option<Instant>,
    /// Profile span covering the post-restore input gate.
    pub(super) gate_profile: Option<ProfileSpan>,
    /// Whether host input is gated by the post-restore input gate.
    pub(super) input_gated: bool,
}

impl SnapshotRestore {
    /// Discards the binders after the restore VP prefix, if any, before
    /// binding creates their VPs. Returns the number of VPs to instantiate.
    pub(super) fn select_instantiated_vps<T>(
        &self,
        vps: &mut Vec<T>,
        vp_capacity: u32,
        restoring: bool,
    ) -> anyhow::Result<u32> {
        select_instantiated_vps(vps, vp_capacity, self.vp_prefix, restoring)
    }

    /// Returns a future that completes when the armed post-restore input gate
    /// expires, and never completes while the gate is not armed.
    pub(super) fn gate_expired(&self, driver: &impl Driver) -> impl Future<Output = ()> {
        let deadline = self.gate_deadline;
        async move {
            match deadline {
                Some(deadline) => {
                    PolledTimer::new(driver).sleep_until(deadline).await;
                }
                None => std::future::pending().await,
            }
        }
    }
}

#[cfg(guest_arch = "x86_64")]
pub(super) fn validate_restore_cpu_contract(
    partition: &dyn HvlitePartition,
    expected_cpu_contract: Option<Vec<u8>>,
) -> anyhow::Result<()> {
    if let Some(expected_cpu_contract) = expected_cpu_contract {
        let destination_contract = partition.cpu_compatibility_contract();
        let destination_cpu_contract = mesh::payload::encode(destination_contract.clone());
        if destination_cpu_contract != expected_cpu_contract {
            let expected_contract: virt::x86::CpuCompatibilityContract =
                mesh::payload::decode(&expected_cpu_contract)
                    .context("failed to decode snapshot CPU contract")?;
            let first_cpuid_difference = expected_contract
                .cpuid
                .iter()
                .zip(&destination_contract.cpuid)
                .find(|(expected, destination)| expected != destination);
            anyhow::bail!(
                "destination CPU contract does not match the snapshot; first CPUID difference: {first_cpuid_difference:?}"
            );
        }
    }
    Ok(())
}

#[cfg(not(guest_arch = "x86_64"))]
pub(super) fn validate_restore_cpu_contract(
    _partition: &dyn HvlitePartition,
    expected_cpu_contract: Option<Vec<u8>>,
) -> anyhow::Result<()> {
    anyhow::ensure!(
        expected_cpu_contract.is_none(),
        "snapshot CPU contracts are only supported for x86-64 guests"
    );
    Ok(())
}

/// Validates the state-unit inventory of a saved-state envelope. Saved state
/// without an inventory is restored without the check.
pub(super) fn validate_inventory(
    state_units: &StateUnits,
    state: &SavedState,
) -> anyhow::Result<()> {
    if !state.inventory.is_empty() {
        state_units.validate_inventory(&state.inventory)?;
    }
    Ok(())
}

impl LoadedVm {
    /// Prepares to restore `saved_state` into the loaded VM: checks the saved
    /// time contract against the saved state and the destination clocks, and
    /// starts the profile span of the restore.
    pub(super) fn begin_snapshot_restore(
        &mut self,
        #[cfg_attr(not(guest_arch = "x86_64"), expect(unused_variables))] saved_state: &SavedState,
    ) -> anyhow::Result<ProfileSpan> {
        self.snapshot_restore.restored_from_snapshot = true;
        let restore_time = self.snapshot_restore.time;

        #[cfg(guest_arch = "x86_64")]
        clock::validate_snapshot_restore_partition_presence(saved_state, restore_time)?;

        self.validate_restore_clock(restore_time)?;
        Ok(ProfileSpan::start())
    }

    /// Completes a restore after the saved state is applied: advances the
    /// guest clocks by the snapshot downtime and holds the VPs stopped until
    /// the VM first starts.
    pub(super) async fn finish_snapshot_restore(
        &mut self,
        saved_state_restore: ProfileSpan,
    ) -> anyhow::Result<()> {
        saved_state_restore.complete("restore", "saved_state_restore", Default::default());
        #[cfg(guest_arch = "x86_64")]
        if let Some(input) = self.snapshot_restore.time_abi.take() {
            let clock_restore = ProfileSpan::start();
            self.time_abi_restore(input).await?;
            self.snapshot_restore.time_abi_release = true;
            clock_restore.complete("restore", "time_abi_clock", Default::default());
        }
        self.advance_restored_clock(self.snapshot_restore.time)
            .await?;
        self.snapshot_restore.start_guard =
            Some(self.inner.partition_unit.temporarily_stop_vps().await);
        Ok(())
    }

    /// Starts the state units for [`LoadedVm::resume`], sequencing the
    /// post-restore input gate, the restore readiness event, and the release
    /// of the restored VPs around the start.
    pub(super) async fn start_state_units(&mut self) -> anyhow::Result<()> {
        if let Some(timeout) = self.snapshot_restore.gate_timeout {
            self.state_units
                .quiesce_input_for_save(timeout)
                .await
                .context("failed to establish post-restore input gate")?;
            self.snapshot_restore.input_gated = true;
        }
        let device_start = ProfileSpan::start();
        self.state_units
            .start()
            .await
            .context("VM state units failed to start")?;
        if self.snapshot_restore.restored_from_snapshot && openvmm_defs::profile::enabled() {
            let faults = self.inner.memory_manager.fault_counters();
            device_start.complete(
                "restore",
                "device_start",
                openvmm_defs::profile::ProfileCounters {
                    gpa_faults: Some(faults.guest_faults),
                    populated_bytes: Some(faults.populated_bytes),
                    ..Default::default()
                },
            );
        }
        // Release partition time just before the readiness event and the VP
        // release, so a failure never publishes readiness.
        #[cfg(guest_arch = "x86_64")]
        if std::mem::take(&mut self.snapshot_restore.time_abi_release) {
            let released = self
                .inner
                .time_abi
                .as_ref()
                .context("a time ABI restore requires a time ABI partition")
                .and_then(|state| {
                    super::time_abi::release_time(self.inner.partition.as_ref(), state)
                });
            if let Err(error) = released {
                self.state_units.stop().await;
                return Err(error.context("failed to release restored partition time"));
            }
        }
        if let Some(mut sink) = self.snapshot_restore.ready_sink.take() {
            let signal_result = sink.write_all(RESTORE_READY_EVENT_V1).and_then(|()| {
                #[cfg(windows)]
                {
                    sink.sync_all()
                }
                #[cfg(not(windows))]
                {
                    sink.flush()
                }
            });
            if let Err(error) = signal_result {
                self.state_units.stop().await;
                return Err(error).context("failed to publish restore readiness event");
            }
        }
        if let Some(timeout) = self.snapshot_restore.gate_timeout {
            self.snapshot_restore.gate_deadline = Some(Instant::now().saturating_add(timeout));
            self.snapshot_restore.gate_profile = Some(ProfileSpan::start());
        }
        self.snapshot_restore.start_guard.take();
        Ok(())
    }

    /// Stops the VM after the guest failed to acknowledge the post-restore
    /// input gate in time.
    pub(super) async fn handle_restore_gate_timeout(&mut self) {
        tracing::error!("post-restore input gate acknowledgement timed out");
        if self.running {
            self.state_units.stop().await;
            self.running = false;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use test_with_tracing::test;

    fn binders() -> Vec<u32> {
        (0..8).collect()
    }

    #[test]
    fn limits_only_explicit_mshv_restores() {
        assert_eq!(restore_vp_prefix("mshv", Some(2)), Some(2));
        assert_eq!(restore_vp_prefix("mshv", None), None);
        for hypervisor in ["kvm", "whp", "hvf"] {
            assert_eq!(restore_vp_prefix(hypervisor, Some(2)), None);
            assert_eq!(restore_vp_prefix(hypervisor, None), None);
        }
    }

    #[test]
    fn restore_vp_prefix_discards_suffix_binders() {
        for prefix in [1, 2, 4, 8] {
            let mut vps = binders();
            assert_eq!(
                select_instantiated_vps(&mut vps, 8, Some(prefix), true).unwrap(),
                prefix
            );
            assert_eq!(vps, (0..prefix).collect::<Vec<_>>());
        }
    }

    #[test]
    fn untargeted_loads_keep_every_binder() {
        for restoring in [false, true] {
            let mut vps = binders();
            assert_eq!(
                select_instantiated_vps(&mut vps, 8, None, restoring).unwrap(),
                8
            );
            assert_eq!(vps, binders());
        }
    }

    #[test]
    fn rejects_invalid_restore_vp_prefix() {
        for prefix in [0, 9] {
            let mut vps = binders();
            let error = select_instantiated_vps(&mut vps, 8, Some(prefix), true).unwrap_err();
            assert_eq!(
                error.to_string(),
                format!("restore VP prefix {prefix} is outside topology capacity 1..=8")
            );
            assert_eq!(vps, binders());
        }

        let mut vps = binders();
        let error = select_instantiated_vps(&mut vps, 8, Some(2), false).unwrap_err();
        assert_eq!(
            error.to_string(),
            "a restore VP prefix requires saved state"
        );
        assert_eq!(vps, binders());
    }

    #[test]
    fn rejects_a_partial_backend_binder_set() {
        let mut vps = (0..4).collect::<Vec<u32>>();
        let error = select_instantiated_vps(&mut vps, 8, None, true).unwrap_err();
        assert_eq!(
            error.to_string(),
            "backend returned 4 VP binders for topology capacity 8"
        );
    }
}
