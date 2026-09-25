// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Worker construction and lifecycle support for the microVM profile.

use super::InitializedVm;
use super::LoadedVm;
use super::LoadedVmInner;
use super::Manifest;
use super::RestartState;
use super::snapshot_rpc;
use crate::partition::HvlitePartition;
use crate::worker::memory_layout::ChipsetMmioRanges;
use anyhow::Context;
use chipset_device_resources::IRQ_LINE_SET;
use chipset_resources::microvm::MicrovmSnapshotBoundaryRequest;
use guestmem::GuestMemory;
use hvdef::Vtl;
use memory_range::MemoryRange;
use mesh::error::RemoteError;
use mesh::rpc::Rpc;
use mesh_worker::WorkerRpc;
use openvmm_defs::microvm::MachineProfile;
use openvmm_defs::rpc::PulseSaveRestoreError;
use openvmm_defs::rpc::VmRpc;
use openvmm_defs::worker::VmWorkerParameters;
use std::sync::Arc;
use std::time::Duration;
use virtio::VirtioMmioDevice;
use virtio::VirtioMmioInterruptMode;
use virtio::resolve::ResolvedVirtioDevice;
use vm_loader::InitialLoad;
use vmcore::vm_task::VmTaskDriverSource;
use vmm_core::partition_unit::StopGuard;

/// MicroVM inputs taken from the [`VmWorkerParameters`].
pub(super) struct MicrovmParameters {
    /// Snapshot boundary channels handed to the loaded VM.
    pub(super) snapshot_boundary: SnapshotBoundary,
    /// Whether this cold boot can publish a microVM snapshot.
    snapshot_capture_enabled: bool,
}

impl MicrovmParameters {
    /// Takes the microVM inputs out of the worker parameters and validates the
    /// machine configuration for the selected hypervisor.
    pub(super) fn take(parameters: &mut VmWorkerParameters) -> anyhow::Result<Self> {
        let snapshot_boundary = SnapshotBoundary {
            requests: parameters.snapshot_boundary_requests.take(),
            ready: parameters.snapshot_ready.take(),
            ..Default::default()
        };
        let snapshot_capture_enabled = parameters.snapshot_capture_enabled;
        openvmm_defs::microvm::validate_machine_config(
            &parameters.cfg,
            Some(parameters.hypervisor.id()),
        )?;
        Ok(Self {
            snapshot_boundary,
            snapshot_capture_enabled,
        })
    }

    /// Prepares the kernel command line of a microVM cold boot, before the VM
    /// is loaded.
    pub(super) fn prepare_cold_boot(
        &self,
        vm: &mut InitializedVm,
        restored_from_snapshot: bool,
    ) -> anyhow::Result<()> {
        prepare_cold_boot_command_line(
            &mut vm.cfg,
            vm.partition.as_ref(),
            restored_from_snapshot,
            self.snapshot_capture_enabled,
        )
    }
}

/// A guest snapshot-boundary request, or the closure of the request channel.
pub(super) type SnapshotBoundaryEvent = Result<MicrovmSnapshotBoundaryRequest, mesh::RecvError>;

/// Guest-requested snapshot boundary state of a [`LoadedVm`].
#[derive(Default)]
pub(super) struct SnapshotBoundary {
    /// Deferred snapshot PMIO requests awaiting an exact post-OUT boundary.
    requests: Option<mesh::Receiver<MicrovmSnapshotBoundaryRequest>>,
    /// Notifies the controller after the worker establishes a boundary.
    ready: Option<mesh::Sender<()>>,
    /// Holds the vCPUs stopped while a boundary is active.
    stop_guard: Option<StopGuard>,
    /// Completes the guest's snapshot transaction when the boundary is released.
    transaction_complete: Option<Rpc<(), ()>>,
    /// Host wall time at the stopped capture boundary.
    capture_wall_clock: Option<mesh::payload::Timestamp>,
    /// Input-gate timeout of the active boundary.
    input_gate_timeout: Option<Duration>,
}

impl SnapshotBoundary {
    /// Receives the next boundary request. Never completes when there is no
    /// request channel.
    pub(super) async fn recv(&mut self) -> SnapshotBoundaryEvent {
        match self.requests.as_mut() {
            Some(requests) => requests.recv().await,
            None => std::future::pending().await,
        }
    }
}

#[cfg(guest_arch = "x86_64")]
pub(super) fn load_linux_x86_mptable(
    vm: &LoadedVmInner,
    kernel: &std::fs::File,
    initrd: &Option<std::fs::File>,
    cmdline: &str,
    isolation: openvmm_defs::config::LinuxIsolationConfig,
    smbios: &openvmm_defs::config::SmbiosConfig,
) -> anyhow::Result<InitialLoad<loader::importer::X86Register>> {
    anyhow::ensure!(
        vm.machine_profile == MachineProfile::Microvm,
        "Linux MP-table boot mode requires the microVM profile"
    );
    anyhow::ensure!(
        isolation == openvmm_defs::config::LinuxIsolationConfig::None
            && vm.hypervisor_cfg.with_isolation.is_none(),
        "microVM MP-table boot does not support isolation"
    );
    let apic_ids = vm
        .processor_topology
        .vps_arch()
        .map(|vp| vp.apic_id)
        .collect::<Vec<_>>();
    let reserved_memory_ranges = [MemoryRange::new(
        openvmm_defs::microvm::MICROVM_SHARED_STATUS_PAGE_GPA
            ..openvmm_defs::microvm::MICROVM_SHARED_STATUS_PAGE_GPA
                + openvmm_defs::microvm::MICROVM_SHARED_STATUS_PAGE_SIZE,
    )];
    let kernel_config = crate::worker::vm_loaders::linux::KernelConfig {
        kernel,
        initrd,
        cmdline,
        mem_layout: &vm.mem_layout,
        isolation: crate::worker::vm_loaders::linux::KernelIsolationConfig::None,
        smbios,
    };
    Ok(crate::worker::vm_loaders::microvm::load_linux_x86_mptable(
        &kernel_config,
        &vm.gm,
        &apic_ids,
        &openvmm_defs::microvm::MICROVM_LEVEL_TRIGGERED_IRQS,
        &reserved_memory_ranges,
    )?)
}

impl LoadedVm {
    pub(super) async fn establish_snapshot_boundary(
        &mut self,
        request: MicrovmSnapshotBoundaryRequest,
    ) -> bool {
        let Some(request) = snapshot_rpc::filter_boundary_request(request) else {
            return true;
        };
        if self.snapshot_boundary.stop_guard.is_some() {
            tracelimit::warn_ratelimited!("dropping duplicate microVM snapshot boundary request");
            request.release_write.send(());
            request.transaction_complete.complete(());
            return true;
        }
        let Some(snapshot_ready) = self.snapshot_boundary.ready.clone() else {
            request.release_write.send(());
            request.transaction_complete.complete(());
            return true;
        };

        let input_gate = openvmm_defs::profile::ProfileSpan::start();
        if let Err(error) = self
            .state_units
            .quiesce_input_for_save(request.input_gate_timeout)
            .await
        {
            tracelimit::error_ratelimited!(
                error = error.as_ref() as &dyn std::error::Error,
                "failed to gate host input before snapshot boundary"
            );
            if let Err(resume_error) = self
                .state_units
                .resume_input_after_save(request.input_gate_timeout)
                .await
            {
                tracelimit::error_ratelimited!(
                    error = resume_error.as_ref() as &dyn std::error::Error,
                    "host-input gate rollback is uncertain; terminating VM worker"
                );
                request.transaction_complete.complete(());
                return false;
            }
            request.release_write.send(());
            request.transaction_complete.complete(());
            return true;
        }
        input_gate.complete("capture", "input_gate", Default::default());

        let vp_stop_at_io_boundary = openvmm_defs::profile::ProfileSpan::start();
        match self
            .inner
            .partition_unit
            .temporarily_stop_vps_at_io_boundary(request.release_write, request.write_completed)
            .await
        {
            Ok(stop_guard) => {
                vp_stop_at_io_boundary.complete(
                    "capture",
                    "vp_stop_at_io_boundary",
                    Default::default(),
                );
                self.snapshot_boundary.stop_guard = Some(stop_guard);
                self.snapshot_boundary.transaction_complete = Some(request.transaction_complete);
                self.snapshot_boundary.capture_wall_clock =
                    Some(std::time::SystemTime::now().into());
                self.snapshot_boundary.input_gate_timeout = Some(request.input_gate_timeout);
                snapshot_ready.send(());
                true
            }
            Err(error) => {
                tracelimit::error_ratelimited!(
                    error = error.as_ref() as &dyn std::error::Error,
                    "failed to establish snapshot PMIO boundary; terminating VM worker"
                );
                request.transaction_complete.complete(());
                false
            }
        }
    }

    pub(super) async fn release_snapshot_boundary(&mut self) -> anyhow::Result<()> {
        let input_gate_timeout = self
            .snapshot_boundary
            .input_gate_timeout
            .context("snapshot boundary is missing its input-gate timeout")?;
        self.state_units
            .resume_input_after_save(input_gate_timeout)
            .await?;
        let transaction_complete = self
            .snapshot_boundary
            .transaction_complete
            .take()
            .context("no active microVM snapshot boundary")?;
        let stop_guard = self
            .snapshot_boundary
            .stop_guard
            .take()
            .context("snapshot boundary is missing its vCPU stop guard")?;
        self.snapshot_boundary.capture_wall_clock = None;
        self.snapshot_boundary.input_gate_timeout = None;
        transaction_complete.complete(());
        drop(stop_guard);
        Ok(())
    }

    pub(super) async fn quiesce_for_snapshot(
        &mut self,
        timeout: Duration,
    ) -> Result<openvmm_defs::rpc::SnapshotSaveResponse, openvmm_defs::rpc::SnapshotQuiesceError>
    {
        use mesh::payload::message::ProtobufMessage;

        if self.inner.machine_profile != MachineProfile::Microvm {
            return Err(openvmm_defs::rpc::SnapshotQuiesceError::Rejected(
                RemoteError::new(anyhow::anyhow!(
                    "guest-requested snapshot quiesce requires the microVM profile"
                )),
            ));
        }
        if !self.running {
            return Err(openvmm_defs::rpc::SnapshotQuiesceError::Rejected(
                RemoteError::new(anyhow::anyhow!("VM is already stopped")),
            ));
        }

        let quiesce = openvmm_defs::profile::ProfileSpan::start();
        if let Err(error) = self.state_units.quiesce_for_save(timeout).await {
            return Err(if error.has_uncertain_state() {
                openvmm_defs::rpc::SnapshotQuiesceError::Uncertain(RemoteError::new(error))
            } else {
                openvmm_defs::rpc::SnapshotQuiesceError::RollbackSafe(RemoteError::new(error))
            });
        }
        quiesce.complete("capture", "quiesce", Default::default());
        self.running = false;

        let save_state = openvmm_defs::profile::ProfileSpan::start();
        let saved_state = self.save().await.map_err(|error| {
            openvmm_defs::rpc::SnapshotQuiesceError::RollbackSafe(RemoteError::new(error))
        })?;
        save_state.complete("capture", "save_state", Default::default());
        let mapped_memory_flush = openvmm_defs::profile::ProfileSpan::start();
        self.inner
            .memory_manager
            .flush_shared_file_backing()
            .context("failed to flush mapped guest RAM")
            .map_err(|error| {
                openvmm_defs::rpc::SnapshotQuiesceError::RollbackSafe(RemoteError::new(error))
            })?;
        mapped_memory_flush.complete("capture", "mapped_memory_flush", Default::default());
        let effective_command_line = match &self.inner.load_mode {
            openvmm_defs::config::LoadMode::Linux {
                cmdline,
                boot_mode: openvmm_defs::config::LinuxDirectBootMode::MpTable,
                ..
            } => cmdline.clone(),
            _ => {
                return Err(openvmm_defs::rpc::SnapshotQuiesceError::RollbackSafe(
                    RemoteError::new(anyhow::anyhow!(
                        "microVM snapshot has no effective command line"
                    )),
                ));
            }
        };
        let tsc_frequency_hz = self
            .inner
            .partition
            .tsc_frequency_hz()
            .map_err(|error| {
                openvmm_defs::rpc::SnapshotQuiesceError::RollbackSafe(RemoteError::new(error))
            })?
            .ok_or_else(|| {
                openvmm_defs::rpc::SnapshotQuiesceError::RollbackSafe(RemoteError::new(
                    anyhow::anyhow!("backend does not expose a guest TSC frequency"),
                ))
            })?;
        let apic_frequency_hz = self
            .inner
            .partition
            .apic_frequency_hz()
            .map_err(|error| {
                openvmm_defs::rpc::SnapshotQuiesceError::RollbackSafe(RemoteError::new(error))
            })?
            .ok_or_else(|| {
                openvmm_defs::rpc::SnapshotQuiesceError::RollbackSafe(RemoteError::new(
                    anyhow::anyhow!("backend does not expose a local APIC frequency"),
                ))
            })?;
        let capture_wall_clock = self.snapshot_boundary.capture_wall_clock.ok_or_else(|| {
            openvmm_defs::rpc::SnapshotQuiesceError::RollbackSafe(RemoteError::new(
                anyhow::anyhow!("snapshot boundary has no wall-clock timestamp"),
            ))
        })?;
        Ok(openvmm_defs::rpc::SnapshotSaveResponse {
            state_unit_names: saved_state.inventory.clone(),
            saved_state: ProtobufMessage::new(saved_state),
            effective_command_line,
            tsc_frequency_hz,
            apic_frequency_hz,
            capture_wall_clock,
            cpu_contract: mesh::payload::encode(self.inner.partition.cpu_compatibility_contract()),
        })
    }

    /// Resumes the VM after a rollback-safe snapshot failure and releases the
    /// snapshot boundary.
    pub(super) async fn resume_after_failed_snapshot(
        &mut self,
        timeout: Duration,
    ) -> anyhow::Result<()> {
        self.state_units.resume_after_failed_save(timeout).await?;
        self.running = true;
        self.release_snapshot_boundary().await?;
        Ok(())
    }

    /// Handles an event of the snapshot boundary request channel. Returns
    /// `false` when the VM worker must stop.
    pub(super) async fn handle_snapshot_boundary(&mut self, event: SnapshotBoundaryEvent) -> bool {
        match event {
            Ok(request) => {
                if !self.establish_snapshot_boundary(request).await {
                    if self.running {
                        self.state_units.stop().await;
                        self.running = false;
                    }
                    return false;
                }
            }
            Err(_) => {
                self.snapshot_boundary.requests = None;
            }
        }
        true
    }

    /// Applies the snapshot-boundary and microVM restrictions to a management
    /// RPC. Rejected RPCs are completed here; the others are returned for
    /// dispatch.
    pub(super) fn filter_vm_rpc(&self, message: VmRpc) -> Option<VmRpc> {
        let message = snapshot_rpc::filter(message, self.snapshot_boundary.stop_guard.is_some())?;
        if self.inner.machine_profile != MachineProfile::Microvm {
            return Some(message);
        }
        match message {
            VmRpc::Save(rpc) => {
                rpc.handle_failable_sync(|()| anyhow::bail!("save is unavailable for microVM"));
                None
            }
            VmRpc::PulseSaveRestore(rpc) => {
                rpc.complete(Err(PulseSaveRestoreError::UnsupportedMachineProfile));
                None
            }
            message => Some(message),
        }
    }

    /// Rejects worker RPCs that the microVM profile does not support. Rejected
    /// RPCs are completed here; the others are returned for dispatch.
    pub(super) fn filter_worker_rpc(
        &self,
        message: WorkerRpc<RestartState>,
    ) -> Option<WorkerRpc<RestartState>> {
        match message {
            WorkerRpc::Restart(rpc) if self.inner.machine_profile == MachineProfile::Microvm => {
                rpc.complete(Err(RemoteError::new(anyhow::anyhow!(
                    "worker restart is unavailable for microVM"
                ))));
                None
            }
            message => Some(message),
        }
    }
}

fn virtio_mmio_config(
    id: &str,
    chipset_mmio: ChipsetMmioRanges,
) -> anyhow::Result<(u64, u64, u32, u64, VirtioMmioInterruptMode)> {
    let (start, irq) = match id {
        "virtio-console" => (
            openvmm_defs::microvm::MICROVM_VIRTIO_CONSOLE_MMIO_BASE,
            openvmm_defs::microvm::MICROVM_VIRTIO_CONSOLE_IRQ,
        ),
        _ => anyhow::bail!("unsupported microVM virtio device '{id}' reached worker construction"),
    };
    let len = openvmm_defs::microvm::MICROVM_VIRTIO_MMIO_LEN;
    anyhow::ensure!(
        start >= chipset_mmio.low.start()
            && start
                .checked_add(len)
                .is_some_and(|end| end <= chipset_mmio.low.end()),
        "microVM virtio slot for '{id}' is outside the fixed low-MMIO aperture"
    );
    // The fixed slots expose split rings only.
    let disabled_features = 1 << 34;
    let interrupt_mode = VirtioMmioInterruptMode::SharedStatus {
        status_gpa: openvmm_defs::microvm::microvm_virtio_status_gpa(start)
            .context("microVM slot has no shared-status word")?,
    };
    Ok((start, len, irq, disabled_features, interrupt_mode))
}

/// Assigns the fixed virtio-mmio slots of the microVM profile to devices, in
/// device order.
pub(super) struct VirtioMmioSlots {
    chipset_mmio: ChipsetMmioRanges,
}

impl VirtioMmioSlots {
    pub(super) fn new(chipset_mmio: ChipsetMmioRanges) -> Self {
        Self { chipset_mmio }
    }

    /// Adds a virtio-mmio device at its fixed microVM slot.
    pub(super) fn add_device(
        &mut self,
        chipset_builder: &vmotherboard::ChipsetBuilder<'_>,
        driver_source: &VmTaskDriverSource,
        gm: &GuestMemory,
        partition: &Arc<dyn HvlitePartition>,
        id: &str,
        device: ResolvedVirtioDevice,
    ) -> anyhow::Result<()> {
        let (mmio_start, mmio_len, irq, disabled_features, interrupt_mode) =
            virtio_mmio_config(id, self.chipset_mmio)?;
        let id = format!("{id}-{mmio_start}");
        let gm = gm.clone();
        chipset_builder.arc_mutex_device(id).try_add(|services| {
            VirtioMmioDevice::new_with_disabled_features_and_interrupt_mode(
                device.0,
                &driver_source.simple(),
                gm,
                services.new_line(IRQ_LINE_SET, "interrupt", irq),
                partition.clone().into_doorbell_registration(Vtl::Vtl0),
                mmio_start,
                mmio_len,
                disabled_features,
                interrupt_mode,
            )
        })?;
        Ok(())
    }
}

pub(super) fn uses_versioned_cpu_contract(machine_profile: MachineProfile) -> bool {
    machine_profile == MachineProfile::Microvm
}

/// Returns the number of virtio-mmio slots that the memory layout allocates.
/// MicroVM devices use fixed slots in the low MMIO aperture instead.
pub(super) fn virtio_mmio_count(
    machine_profile: MachineProfile,
    virtio_mmio_count: usize,
) -> usize {
    if machine_profile == MachineProfile::Microvm {
        0
    } else {
        virtio_mmio_count
    }
}

pub(super) fn level_triggered_irqs(machine_profile: MachineProfile) -> &'static [u32] {
    match machine_profile {
        MachineProfile::Microvm => &openvmm_defs::microvm::MICROVM_LEVEL_TRIGGERED_IRQS,
        MachineProfile::Standard => &[],
    }
}

pub(super) fn trace_unknown_pio(machine_profile: MachineProfile) -> bool {
    machine_profile == MachineProfile::Standard
}

#[cfg(guest_arch = "x86_64")]
fn prepare_cold_boot_command_line(
    cfg: &mut Manifest,
    partition: &dyn HvlitePartition,
    restored_from_snapshot: bool,
    snapshot_capture_enabled: bool,
) -> anyhow::Result<()> {
    if restored_from_snapshot || cfg.machine_profile != MachineProfile::Microvm {
        return Ok(());
    }

    let cmdline = match &mut cfg.load_mode {
        openvmm_defs::config::LoadMode::Linux {
            cmdline,
            boot_mode: openvmm_defs::config::LinuxDirectBootMode::MpTable,
            ..
        } => Some(cmdline),
        _ => None,
    };
    let Some(cmdline) = cmdline else {
        anyhow::bail!("microVM has no supported cold-boot command line");
    };
    match partition
        .tsc_frequency_hz()
        .context("failed to query the backend guest TSC frequency")?
    {
        Some(frequency_hz) => {
            crate::worker::vm_loaders::microvm::propagate_snapshot_tsc_frequency(
                cmdline,
                frequency_hz,
                snapshot_capture_enabled,
            )
            .context("failed to propagate the guest TSC frequency")?;
        }
        None => tracing::warn!(
            "backend does not expose a guest TSC frequency; preserving the microVM command line"
        ),
    }
    match partition
        .apic_frequency_hz()
        .context("failed to query the backend guest LAPIC frequency")?
    {
        Some(frequency_hz) => {
            crate::worker::vm_loaders::microvm::propagate_apic_frequency(cmdline, frequency_hz)
                .context("failed to propagate the guest LAPIC frequency")?;
        }
        None => tracing::warn!(
            "backend does not expose a guest LAPIC frequency; retaining guest timer calibration"
        ),
    }
    Ok(())
}

#[cfg(not(guest_arch = "x86_64"))]
fn prepare_cold_boot_command_line(
    _cfg: &mut Manifest,
    _partition: &dyn HvlitePartition,
    _restored_from_snapshot: bool,
    _snapshot_capture_enabled: bool,
) -> anyhow::Result<()> {
    Ok(())
}

#[cfg(guest_arch = "x86_64")]
pub(super) fn x86_topology_builder(
    machine_profile: MachineProfile,
) -> anyhow::Result<vm_topology::processor::TopologyBuilder<vm_topology::processor::x86::X86Topology>>
{
    if machine_profile == MachineProfile::Microvm {
        Ok(vm_topology::processor::TopologyBuilder::new_x86())
    } else {
        Ok(vm_topology::processor::TopologyBuilder::from_host_topology()?)
    }
}
