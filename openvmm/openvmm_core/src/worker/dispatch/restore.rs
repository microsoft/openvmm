// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Snapshot restore support for the VM worker: the restore inputs taken from
//! the worker parameters, the restore state kept by [`LoadedVm`], the VP
//! prefix instantiated by an explicit restore-time activation target, the
//! steps around applying the saved state, and the post-restore start sequence
//! (input gate, readiness event, and VP release).

use super::LoadedVm;
use anyhow::Context;
use futures::FutureExt;
use futures_concurrency::future::Race;
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
    /// Mapping mode of the file-backed guest RAM handle.
    file_mapping_mode: FileMappingMode,
    /// Snapshot generation handles that must outlive the restored VM.
    pub(super) guards: Option<SnapshotRestoreGuards>,
}

impl RestoreParameters {
    /// Takes the restore inputs out of the worker parameters.
    pub(super) fn take(parameters: &mut VmWorkerParameters) -> anyhow::Result<Self> {
        let guards = parameters.snapshot_restore_guards.take();
        let ready_sink = parameters.restore_ready_sink.take();
        let gate_timeout = parameters.restore_gate_timeout.take();
        let restore_vp_count = parameters.restore_vp_count.take();
        let vp_prefix = restore_vp_prefix(parameters.hypervisor.id(), restore_vp_count);
        tracing::debug!(
            ?restore_vp_count,
            ?vp_prefix,
            "received restore-time VP activation target"
        );
        let time_abi = parameters.restore_time.take();
        let file_mapping_mode = if parameters.shared_memory_copy_on_write {
            FileMappingMode::CopyOnWrite
        } else {
            FileMappingMode::Shared
        };
        Ok(Self {
            state: SnapshotRestore {
                time_abi,
                vp_prefix,
                ready_sink,
                gate_timeout,
                ..Default::default()
            },
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
    /// The time ABI inputs of a restore, validated by the controller.
    pub(super) time_abi: Option<openvmm_defs::time_abi::RestoreTimeInput>,
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
    /// With profiling enabled, notified when the guest first selects the
    /// time ABI restore packet.
    packet_selected: Option<mesh::OneshotReceiver<()>>,
    /// Profile span from the release of the restored VPs to the guest's
    /// first selection of the restore packet.
    resume_profile: Option<ProfileSpan>,
    /// Profile span from the guest's first selection of the restore packet to
    /// its acknowledgement of a gated restore.
    repair_profile: Option<ProfileSpan>,
}

/// A post-restore event handled by the VM worker's run loop.
pub(super) enum RestoreEvent {
    /// The armed post-restore input gate expired.
    GateExpired,
    /// The guest first selected the time ABI restore packet.
    PacketSelected,
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

    /// Returns a future that completes at the next post-restore event: the
    /// expiry of the armed input gate, or the guest's first selection of the
    /// restore packet. It never completes while neither is pending.
    pub(super) fn next_event(
        &mut self,
        driver: &impl Driver,
    ) -> impl Future<Output = RestoreEvent> {
        let deadline = self.gate_deadline;
        let packet_selected = &mut self.packet_selected;
        async move {
            let gate_expired = async {
                match deadline {
                    Some(deadline) => {
                        PolledTimer::new(driver).sleep_until(deadline).await;
                    }
                    None => std::future::pending().await,
                }
                RestoreEvent::GateExpired
            };
            let selected = async {
                if let Some(selected) = packet_selected.as_mut() {
                    let result = selected.await;
                    *packet_selected = None;
                    if result.is_ok() {
                        return RestoreEvent::PacketSelected;
                    }
                }
                std::future::pending().await
            };
            (gate_expired, selected).race().await
        }
    }

    /// Ends the `restore/guest_resume` profile phase at the guest's first
    /// selection of the restore packet, and starts the `restore/guest_repair`
    /// phase of a gated restore.
    pub(super) fn restore_packet_selected(&mut self) {
        if let Some(resume) = self.resume_profile.take() {
            resume.complete("restore", "guest_resume", Default::default());
            if self.gate_profile.is_some() {
                self.repair_profile = Some(ProfileSpan::start());
            }
        }
    }

    /// Ends the `restore/guest_repair` profile phase when the guest
    /// acknowledges a gated restore.
    pub(super) fn restore_acknowledged(&mut self) {
        // The guest selects the packet before it acknowledges the restore, so
        // handle a selection that the run loop has not handled yet first.
        if let Some(result) = self
            .packet_selected
            .as_mut()
            .and_then(|selected| selected.now_or_never())
        {
            self.packet_selected = None;
            if result.is_ok() {
                self.restore_packet_selected();
            }
        }
        if let Some(repair) = self.repair_profile.take() {
            repair.complete("restore", "guest_repair", Default::default());
        }
    }
}

/// Checks that the saved state of a time ABI restore holds the partition
/// state, which the restore clock sets and advances.
#[cfg(guest_arch = "x86_64")]
fn validate_snapshot_restore_partition_presence(
    saved_state: &SavedState,
    time_abi_restore: bool,
) -> anyhow::Result<()> {
    anyhow::ensure!(
        !time_abi_restore
            || saved_state
                .units
                .iter()
                .any(|unit| unit.name == "partition"),
        "a time ABI restore requires partition state"
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
    /// Prepares to restore `saved_state` into the loaded VM: checks that a
    /// time ABI restore has partition state, and starts the profile span of
    /// the restore. A time ABI restore drops the VPs' saved TSC values, which
    /// its synchronized TSC set supersedes.
    pub(super) async fn begin_snapshot_restore(
        &mut self,
        #[cfg_attr(not(guest_arch = "x86_64"), expect(unused_variables))] saved_state: &SavedState,
    ) -> anyhow::Result<ProfileSpan> {
        self.snapshot_restore.restored_from_snapshot = true;
        #[cfg(guest_arch = "x86_64")]
        {
            let time_abi_restore = self.snapshot_restore.time_abi.is_some();
            validate_snapshot_restore_partition_presence(saved_state, time_abi_restore)?;
            if time_abi_restore {
                self.inner.partition_unit.omit_saved_tsc().await;
            }
        }
        Ok(ProfileSpan::start())
    }

    /// Completes a restore after the saved state is applied: sets and
    /// advances the guest clocks of a time ABI restore, and holds the VPs
    /// stopped until the VM first starts.
    pub(super) async fn finish_snapshot_restore(
        &mut self,
        saved_state_restore: ProfileSpan,
    ) -> anyhow::Result<()> {
        saved_state_restore.complete("restore", "saved_state_restore", Default::default());
        #[cfg(guest_arch = "x86_64")]
        if let Some(mut input) = self.snapshot_restore.time_abi.take() {
            self.snapshot_restore.packet_selected = input.packet_selected.take();
            let clock_restore = ProfileSpan::start();
            self.time_abi_restore(input).await?;
            clock_restore.complete("restore", "time_abi_clock", Default::default());
        }
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
        if self.snapshot_restore.packet_selected.is_some() {
            self.snapshot_restore.resume_profile = Some(ProfileSpan::start());
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

    #[cfg(guest_arch = "x86_64")]
    fn saved_state(unit_names: &[&str]) -> SavedState {
        use state_unit::SavedStateUnit;
        use vmcore::save_restore::NoSavedState;
        use vmcore::save_restore::SavedStateBlob;

        SavedState {
            units: unit_names
                .iter()
                .map(|name| SavedStateUnit {
                    name: (*name).to_owned(),
                    state: SavedStateBlob::new(NoSavedState),
                })
                .collect(),
            inventory: Vec::new(),
        }
    }

    #[cfg(guest_arch = "x86_64")]
    #[test]
    fn time_abi_restore_requires_partition_state() {
        validate_snapshot_restore_partition_presence(&saved_state(&[]), false).unwrap();
        validate_snapshot_restore_partition_presence(&saved_state(&["partition"]), true).unwrap();
        let error = validate_snapshot_restore_partition_presence(&saved_state(&["other"]), true)
            .unwrap_err();
        assert_eq!(
            error.to_string(),
            "a time ABI restore requires partition state"
        );

        // The payload must hold the unit; the inventory alone does not.
        let mut inventory_only = saved_state(&[]);
        inventory_only.inventory.push("partition".to_owned());
        validate_snapshot_restore_partition_presence(&inventory_only, true).unwrap_err();
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

    #[test]
    fn restore_events_report_the_first_packet_selection() {
        pal_async::DefaultPool::run_with(async |driver| {
            let (send, recv) = mesh::oneshot();
            let mut restore = SnapshotRestore {
                packet_selected: Some(recv),
                ..Default::default()
            };
            assert!(restore.next_event(&driver).now_or_never().is_none());
            send.send(());
            assert!(matches!(
                restore.next_event(&driver).await,
                RestoreEvent::PacketSelected
            ));
            assert!(restore.packet_selected.is_none());
            assert!(restore.next_event(&driver).now_or_never().is_none());
        });
    }

    #[test]
    fn restore_events_ignore_a_dropped_packet_notifier() {
        pal_async::DefaultPool::run_with(async |driver| {
            let (send, recv) = mesh::oneshot::<()>();
            let mut restore = SnapshotRestore {
                packet_selected: Some(recv),
                ..Default::default()
            };
            drop(send);
            assert!(restore.next_event(&driver).now_or_never().is_none());
            assert!(restore.packet_selected.is_none());
        });
    }

    #[test]
    fn restore_events_report_the_gate_expiry() {
        pal_async::DefaultPool::run_with(async |driver| {
            let mut restore = SnapshotRestore {
                gate_deadline: Some(Instant::now()),
                ..Default::default()
            };
            assert!(matches!(
                restore.next_event(&driver).await,
                RestoreEvent::GateExpired
            ));
        });
    }

    #[test]
    fn packet_selection_starts_the_repair_phase_of_a_gated_restore() {
        for gated in [false, true] {
            let mut restore = SnapshotRestore {
                resume_profile: Some(ProfileSpan::start()),
                gate_profile: gated.then(ProfileSpan::start),
                ..Default::default()
            };
            restore.restore_packet_selected();
            assert!(restore.resume_profile.is_none());
            assert_eq!(restore.repair_profile.is_some(), gated);
            restore.restore_acknowledged();
            assert!(restore.repair_profile.is_none());
        }
    }

    #[test]
    fn acknowledgement_handles_an_unhandled_packet_selection_first() {
        let (send, recv) = mesh::oneshot();
        let mut restore = SnapshotRestore {
            packet_selected: Some(recv),
            resume_profile: Some(ProfileSpan::start()),
            gate_profile: Some(ProfileSpan::start()),
            ..Default::default()
        };
        send.send(());
        restore.restore_acknowledged();
        assert!(restore.packet_selected.is_none());
        assert!(restore.resume_profile.is_none());
        assert!(restore.repair_profile.is_none());
    }
}
