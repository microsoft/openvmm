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
use chipset_resources::microvm::MicrovmSnapshotScratchPolicy;
use guestmem::GuestMemory;
use hvdef::Vtl;
use membacking::FileMappingMode;
use membacking::GuestMemoryBuilder;
use membacking::Mappable;
use membacking::SharedMemoryBacking;
use memory_range::MemoryRange;
use mesh::MeshPayload;
use mesh::error::RemoteError;
use mesh::rpc::Rpc;
use mesh_worker::WorkerRpc;
use openvmm_defs::microvm::MachineProfile;
use openvmm_defs::microvm::MicrovmConfig;
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

/// The microVM part of the worker [`Manifest`]: the subset of
/// [`MicrovmConfig`] that the worker consumes after validating the
/// configuration.
#[derive(MeshPayload, Default)]
pub(super) struct MicrovmManifest {
    pub(super) sandbox_blocks: Vec<openvmm_defs::microvm::MicrovmSandboxBlockConfig>,
    pub(super) memory_capacity: Option<u64>,
    pub(super) snapshot_memory_ranges: Vec<MemoryRange>,
    pub(super) restore_memory_ranges: Vec<MemoryRange>,
    /// The NVX time ABI parameters, when the time ABI is selected.
    pub(super) time_abi: Option<openvmm_defs::time_abi::TimeAbiParameters>,
    /// The CPU profile record of the snapshot that the worker restores, set
    /// by the worker when the profile is a host profile: the snapshot carries
    /// the profile's only copy, from which the partition takes its profile.
    pub(super) restored_host_profile: Option<openvmm_defs::time_abi::SnapshotCpuProfile>,
    /// The hypervisor backend ID (`kvm`, `mshv`, or `whp`), set by the
    /// worker.
    pub(super) hypervisor_id: String,
}

impl From<MicrovmConfig> for MicrovmManifest {
    fn from(config: MicrovmConfig) -> Self {
        let MicrovmConfig {
            network: _,
            filesystems: _,
            sandbox_blocks,
            filesystem_bootstrap: _,
            memory_capacity,
            snapshot_memory_ranges,
            restore_memory_ranges,
            time_abi,
        } = config;
        Self {
            sandbox_blocks,
            memory_capacity,
            snapshot_memory_ranges,
            restore_memory_ranges,
            time_abi,
            restored_host_profile: None,
            hypervisor_id: String::new(),
        }
    }
}

/// MicroVM inputs taken from the [`VmWorkerParameters`].
pub(super) struct MicrovmParameters {
    /// Snapshot boundary channels handed to the loaded VM.
    pub(super) snapshot_boundary: SnapshotBoundary,
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
        openvmm_defs::microvm::validate_machine_config(
            &parameters.cfg,
            Some(parameters.hypervisor.id()),
        )?;
        Ok(Self { snapshot_boundary })
    }

    /// Checks the kernel command line of a microVM cold boot, before the VM
    /// is loaded: the time ABI declares both clock rates, so the command line
    /// must not set them (`E_CMDLINE_CLOCK_TOKEN`).
    pub(super) fn check_cold_boot(
        &self,
        vm: &InitializedVm,
        restored_from_snapshot: bool,
    ) -> anyhow::Result<()> {
        check_cold_boot_command_line(&vm.cfg, restored_from_snapshot)
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
    ready: Option<mesh::Sender<MicrovmSnapshotScratchPolicy>>,
    /// Holds the vCPUs stopped while a boundary is active.
    stop_guard: Option<StopGuard>,
    /// Completes the guest's snapshot transaction when the boundary is released.
    transaction_complete: Option<Rpc<(), ()>>,
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

        if self.snapshot_restore.input_gated {
            // A boundary under the armed post-restore input gate is the
            // guest's acknowledgement of the restore.
            self.snapshot_restore.restore_acknowledged();
        }
        if !self.snapshot_restore.input_gated {
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
        }

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
                self.snapshot_boundary.input_gate_timeout = Some(request.input_gate_timeout);
                snapshot_ready.send(request.scratch_policy);
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
        let restore_gate_profile = self.snapshot_restore.gate_profile.take();
        self.snapshot_boundary.input_gate_timeout = None;
        self.snapshot_restore.gate_timeout = None;
        self.snapshot_restore.gate_deadline = None;
        self.snapshot_restore.input_gated = false;
        transaction_complete.complete(());
        drop(stop_guard);
        if let Some(profile) = restore_gate_profile {
            profile.complete_milestone("restore", "guest_repair_gate", Default::default());
        }
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

        // Time ABI capture steps 1 and 2: the LAPIC timers, then the capture
        // anchor and records. The PIT checks itself when saved.
        let time = self.capture_time_abi().await.map_err(|error| {
            openvmm_defs::rpc::SnapshotQuiesceError::RollbackSafe(RemoteError::new(error))
        })?;

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
        Ok(openvmm_defs::rpc::SnapshotSaveResponse {
            state_unit_names: saved_state.inventory.clone(),
            saved_state: ProtobufMessage::new(saved_state),
            effective_command_line,
            time,
        })
    }

    /// Takes the time ABI records of a capture: checks the LAPIC timers, then
    /// takes the capture anchor and records (capture steps 1 to 3 of the
    /// specification).
    #[cfg(guest_arch = "x86_64")]
    async fn capture_time_abi(&mut self) -> anyhow::Result<openvmm_defs::time_abi::TimeCapture> {
        let state = self
            .inner
            .time_abi
            .as_ref()
            .context("a microVM snapshot requires the time ABI")?;
        self.inner.partition_unit.check_one_shot_timers().await?;
        super::time_abi::capture_records(self.inner.partition.as_ref(), state)
    }

    #[cfg(not(guest_arch = "x86_64"))]
    async fn capture_time_abi(&mut self) -> anyhow::Result<openvmm_defs::time_abi::TimeCapture> {
        anyhow::bail!("microVM snapshots require an x86-64 guest")
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
    sandbox_blocks: &[openvmm_defs::microvm::MicrovmSandboxBlockConfig],
    sandbox_block_index: &mut usize,
    filesystem_index: &mut usize,
    chipset_mmio: ChipsetMmioRanges,
) -> anyhow::Result<(u64, u64, u32, u64, VirtioMmioInterruptMode)> {
    let (start, irq) = match id {
        "virtio-net" => (
            openvmm_defs::microvm::MICROVM_VIRTIO_NET_MMIO_BASE,
            openvmm_defs::microvm::microvm_virtio_net_irq(None)?,
        ),
        // The configuration orders virtio-fs devices by slot.
        "virtiofs" => {
            let slot = openvmm_defs::microvm::MICROVM_FILESYSTEM_SLOTS
                .get(*filesystem_index)
                .context("microVM has more virtio-fs devices than fixed slots")?;
            *filesystem_index += 1;
            (slot.mmio_base, slot.irq)
        }
        "virtio-console" => (
            openvmm_defs::microvm::MICROVM_VIRTIO_CONSOLE_MMIO_BASE,
            openvmm_defs::microvm::MICROVM_VIRTIO_CONSOLE_IRQ,
        ),
        openvmm_defs::microvm::MICROVM_VIRTIO_CONTROL_CONSOLE_ID => (
            openvmm_defs::microvm::MICROVM_VIRTIO_CONTROL_CONSOLE_MMIO_BASE,
            openvmm_defs::microvm::MICROVM_VIRTIO_CONTROL_CONSOLE_IRQ,
        ),
        "virtio-blk" => {
            let block = sandbox_blocks
                .get(*sandbox_block_index)
                .context("microVM virtio-blk device has no sandbox role")?;
            *sandbox_block_index += 1;
            (block.role.mmio_base(), block.role.irq())
        }
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
    let disabled_features = match id {
        "virtio-net" => !openvmm_defs::microvm::MICROVM_VIRTIO_NET_FEATURES,
        "virtiofs" => !openvmm_defs::microvm::MICROVM_VIRTIO_FS_FEATURES,
        "virtio-blk" => {
            let block = sandbox_blocks
                .iter()
                .find(|block| block.role.mmio_base() == start)
                .context("microVM block slot has no role")?;
            !openvmm_defs::microvm::microvm_sandbox_block_features(block.role)
        }
        _ => 1 << 34,
    };
    let interrupt_mode = VirtioMmioInterruptMode::SharedStatus {
        status_gpa: openvmm_defs::microvm::microvm_virtio_status_gpa(start)
            .context("microVM slot has no shared-status word")?,
    };
    Ok((start, len, irq, disabled_features, interrupt_mode))
}

/// Assigns the fixed virtio-mmio slots of the microVM profile to devices, in
/// device order.
pub(super) struct VirtioMmioSlots<'a> {
    sandbox_blocks: &'a [openvmm_defs::microvm::MicrovmSandboxBlockConfig],
    sandbox_block_index: usize,
    filesystem_index: usize,
    chipset_mmio: ChipsetMmioRanges,
}

impl<'a> VirtioMmioSlots<'a> {
    pub(super) fn new(
        sandbox_blocks: &'a [openvmm_defs::microvm::MicrovmSandboxBlockConfig],
        chipset_mmio: ChipsetMmioRanges,
    ) -> Self {
        Self {
            sandbox_blocks,
            sandbox_block_index: 0,
            filesystem_index: 0,
            chipset_mmio,
        }
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
        let (mmio_start, mmio_len, irq, disabled_features, interrupt_mode) = virtio_mmio_config(
            id,
            self.sandbox_blocks,
            &mut self.sandbox_block_index,
            &mut self.filesystem_index,
            self.chipset_mmio,
        )?;
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

/// Adds the split guest RAM backings of a microVM snapshot restore. The
/// backed RAM ranges are removed from `ranges_by_node`, so the generic
/// per-node backings skip them.
pub(super) fn add_snapshot_restore_backing(
    mut memory_builder: GuestMemoryBuilder,
    cfg: &Manifest,
    ranges_by_node: &mut [Vec<MemoryRange>],
    nodes_with_ranges: usize,
    existing_mappable: &mut Option<(Mappable, FileMappingMode)>,
) -> anyhow::Result<GuestMemoryBuilder> {
    if cfg.microvm.snapshot_memory_ranges.is_empty() {
        return Ok(memory_builder);
    }

    anyhow::ensure!(
        cfg.machine_profile == MachineProfile::Microvm && nodes_with_ranges == 1,
        "snapshot RAM range restore requires a single-node microVM"
    );
    let active_ranges = ranges_by_node
        .iter_mut()
        .find(|ranges| !ranges.is_empty())
        .expect("nodes_with_ranges is one");
    let mut restored_ranges = cfg.microvm.snapshot_memory_ranges.clone();
    restored_ranges.extend_from_slice(&cfg.microvm.restore_memory_ranges);
    anyhow::ensure!(
        coalesce_adjacent_ranges(&restored_ranges)? == *active_ranges,
        "snapshot base and expansion ranges do not match the selected RAM layout"
    );

    let (mappable, file_mapping_mode) = existing_mappable
        .take()
        .context("snapshot RAM ranges require an existing memory backing")?;
    let mem = cfg.numa.nodes[0]
        .mem
        .as_ref()
        .context("snapshot RAM ranges require node 0 memory configuration")?;
    let base_backing =
        membacking::RamBackingRequest::new(cfg.microvm.snapshot_memory_ranges.clone())
            .prefetch(mem.prefetch_memory)
            .transparent_hugepages(mem.transparent_hugepages)
            .host_numa_node(mem.host_numa_node)
            .existing_mappable(mappable)
            .file_mapping_mode(file_mapping_mode);
    memory_builder = memory_builder.add_backing(base_backing);

    if !cfg.microvm.restore_memory_ranges.is_empty() {
        let expansion_backing =
            membacking::RamBackingRequest::new(cfg.microvm.restore_memory_ranges.clone())
                .prefetch(mem.prefetch_memory)
                .private_memory(true)
                .transparent_hugepages(mem.transparent_hugepages)
                .host_numa_node(mem.host_numa_node);
        memory_builder = memory_builder.add_backing(expansion_backing);
    }
    active_ranges.clear();
    Ok(memory_builder)
}

fn coalesce_adjacent_ranges(ranges: &[MemoryRange]) -> anyhow::Result<Vec<MemoryRange>> {
    let mut coalesced: Vec<MemoryRange> = Vec::with_capacity(ranges.len());
    for &range in ranges {
        anyhow::ensure!(!range.is_empty(), "snapshot RAM range is empty");
        if let Some(previous) = coalesced.last_mut() {
            anyhow::ensure!(
                range.start() >= previous.end(),
                "snapshot RAM ranges overlap or are out of order"
            );
            if range.start() == previous.end() {
                *previous = MemoryRange::new(previous.start()..range.end());
                continue;
            }
        }
        coalesced.push(range);
    }
    Ok(coalesced)
}

pub(super) fn uses_lazy_memory_registration(
    cfg: &Manifest,
    shared_memory: Option<&SharedMemoryBacking>,
) -> bool {
    #[cfg(all(windows, feature = "virt_whp"))]
    let has_vpci_resources = !cfg.vpci_resources.is_empty();
    #[cfg(not(all(windows, feature = "virt_whp")))]
    let has_vpci_resources = false;
    let prefetch_memory = cfg
        .numa
        .nodes
        .iter()
        .any(|node| node.mem.as_ref().is_some_and(|mem| mem.prefetch_memory));

    cfg!(all(windows, feature = "virt_whp", guest_arch = "x86_64"))
        && cfg.machine_profile == MachineProfile::Microvm
        && cfg.hypervisor.with_vtl2.is_none()
        && cfg.microvm.restore_memory_ranges.is_empty()
        && !has_vpci_resources
        && !prefetch_memory
        && shared_memory.is_some_and(SharedMemoryBacking::is_copy_on_write)
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
fn check_cold_boot_command_line(
    cfg: &Manifest,
    restored_from_snapshot: bool,
) -> anyhow::Result<()> {
    if restored_from_snapshot || cfg.machine_profile != MachineProfile::Microvm {
        return Ok(());
    }

    let openvmm_defs::config::LoadMode::Linux {
        cmdline,
        boot_mode: openvmm_defs::config::LinuxDirectBootMode::MpTable,
        ..
    } = &cfg.load_mode
    else {
        anyhow::bail!("microVM has no supported cold-boot command line");
    };
    virt::time_abi::check_command_line_clock_tokens(cmdline)?;
    Ok(())
}

#[cfg(not(guest_arch = "x86_64"))]
fn check_cold_boot_command_line(
    _cfg: &Manifest,
    _restored_from_snapshot: bool,
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
