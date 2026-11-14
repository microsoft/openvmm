// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Worker construction and lifecycle support for the microVM profile.

use super::InitializedVm;
use super::LoadedVm;
#[cfg(guest_arch = "x86_64")]
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
use virtio::VirtioMmioDevice;
use virtio::VirtioMmioInterruptMode;
use virtio::resolve::ResolvedVirtioDevice;
#[cfg(guest_arch = "x86_64")]
use vm_loader::InitialLoad;
use vm_topology::memory::MemoryLayout;
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
    let reserved_memory_ranges = reserved_memory_ranges();
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
        openvmm_defs::microvm::MICROVM_IOAPIC_INPUTS,
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
            // The gate waits for every unit. When a unit fails it, roll it
            // back and let the guest continue without a snapshot; only a
            // failed rollback ends the VM.
            let input_gate = openvmm_defs::profile::ProfileSpan::start();
            if let Err(error) = self.state_units.quiesce_input_for_save().await {
                tracelimit::error_ratelimited!(
                    error = error.as_ref() as &dyn std::error::Error,
                    "failed to gate host input before snapshot boundary"
                );
                if let Err(resume_error) = self.state_units.resume_input_after_save().await {
                    tracelimit::error_ratelimited!(
                        error = resume_error.as_ref() as &dyn std::error::Error,
                        "failed to roll back the host-input gate; terminating VM worker"
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

    /// Releases the guest-requested snapshot boundary of a running VM: resumes
    /// host input, completes the guest's snapshot transaction, and lets the
    /// vCPUs run.
    ///
    /// The boundary of a quiesced VM, whose snapshot the controller holds, is
    /// released only by [`Self::resume_after_failed_snapshot`], which starts
    /// the VM's state units first: the boundary keeps refusing an ordinary
    /// resume until then.
    pub(super) async fn release_snapshot_boundary(&mut self) -> anyhow::Result<()> {
        // Resume host input only while a boundary holds it.
        anyhow::ensure!(
            self.snapshot_boundary.transaction_complete.is_some(),
            "no active microVM snapshot boundary"
        );
        anyhow::ensure!(
            self.running,
            "the snapshot boundary of a quiesced microVM is released only by its rollback"
        );
        self.state_units.resume_input_after_save().await?;
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

    /// Quiesces the VM for a guest-requested snapshot and saves its state,
    /// leaving it stopped.
    ///
    /// The VM must be running at a snapshot boundary that its guest requested,
    /// which holds the stopped VM until the snapshot's rollback: outside one,
    /// the request is rejected and changes nothing, since an ordinary resume
    /// would run the VM again after its state was saved.
    ///
    /// The quiesce stops every state unit and has no deadline: a unit that
    /// never stops stalls the capture until the process that drives the VM
    /// ends it, with nothing published and the source never resumed. A unit
    /// whose task does not answer fails the quiesce instead: it can never
    /// start again, so the VM can neither roll back nor run, and the worker
    /// tears it down. Each error after the quiesce leaves every unit stopped,
    /// so the VM can roll back with [`Self::resume_after_failed_snapshot`].
    pub(super) async fn quiesce_for_snapshot(
        &mut self,
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
        if self.snapshot_boundary.stop_guard.is_none() {
            return Err(openvmm_defs::rpc::SnapshotQuiesceError::Rejected(
                RemoteError::new(anyhow::anyhow!(
                    "guest-requested snapshot quiesce requires an active snapshot boundary"
                )),
            ));
        }

        let quiesce = openvmm_defs::profile::ProfileSpan::start();
        // The VM no longer runs even if a unit does not stop, which leaves
        // the units running for the worker to tear down.
        self.running = false;
        self.state_units.stop().await.map_err(|error| {
            openvmm_defs::rpc::SnapshotQuiesceError::Unrecoverable(RemoteError::new(error))
        })?;
        quiesce.complete("capture", "quiesce", Default::default());

        // The time ABI's capture checks and records: the LAPIC timers, then
        // the capture anchor and records. The PIT checks itself when saved.
        #[cfg_attr(not(guest_arch = "x86_64"), expect(unused_mut))]
        let mut time = self.capture_time_abi().await.map_err(|error| {
            openvmm_defs::rpc::SnapshotQuiesceError::RollbackSafe(RemoteError::new(error))
        })?;

        let save_state = openvmm_defs::profile::ProfileSpan::start();
        let saved_state = self.save().await.map_err(|error| {
            openvmm_defs::rpc::SnapshotQuiesceError::RollbackSafe(RemoteError::new(error))
        })?;
        #[cfg(guest_arch = "x86_64")]
        super::time_abi::record_vm_time_cut(&mut time.time, &saved_state).map_err(|error| {
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
    /// takes the capture anchor and records.
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

    /// Resumes the VM after a snapshot failure that followed a complete
    /// quiesce, and releases the snapshot boundary. This is the only request
    /// that runs a quiesced VM again.
    ///
    /// A unit whose task does not answer leaves the units that did start
    /// stopped again, and fails the rollback. A unit that such a unit depends
    /// on, or that does not answer its stop, stays running instead, and the
    /// worker then tears the VM down.
    pub(super) async fn resume_after_failed_snapshot(&mut self) -> anyhow::Result<()> {
        self.state_units.start().await?;
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
                    if let Err(error) = self.pause().await {
                        tracing::error!(
                            error = error.as_ref() as &dyn std::error::Error,
                            "failed to stop the VM after its snapshot boundary failed"
                        );
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

/// Returns the fixed slot of the microVM virtio device `id`: its MMIO window,
/// interrupt, exact guest-visible feature mask, and interrupt-status delivery.
fn virtio_mmio_config(
    id: &str,
    sandbox_blocks: &[openvmm_defs::microvm::MicrovmSandboxBlockConfig],
    sandbox_block_index: &mut usize,
    filesystem_index: &mut usize,
    chipset_mmio: ChipsetMmioRanges,
) -> anyhow::Result<(u64, u64, u32, u64, VirtioMmioInterruptMode)> {
    let (start, irq, features) = match id {
        "virtio-net" => (
            openvmm_defs::microvm::MICROVM_VIRTIO_NET_MMIO_BASE,
            openvmm_defs::microvm::MICROVM_VIRTIO_NET_IRQ,
            openvmm_defs::microvm::MICROVM_VIRTIO_NET_FEATURES,
        ),
        // The configuration orders virtio-fs devices by slot.
        "virtiofs" => {
            let slot = openvmm_defs::microvm::MICROVM_FILESYSTEM_SLOTS
                .get(*filesystem_index)
                .context("microVM has more virtio-fs devices than fixed slots")?;
            *filesystem_index += 1;
            (
                slot.mmio_base,
                slot.irq,
                openvmm_defs::microvm::MICROVM_VIRTIO_FS_FEATURES,
            )
        }
        "virtio-console" => (
            openvmm_defs::microvm::MICROVM_VIRTIO_CONSOLE_MMIO_BASE,
            openvmm_defs::microvm::MICROVM_VIRTIO_CONSOLE_IRQ,
            openvmm_defs::microvm::MICROVM_VIRTIO_CONSOLE_FEATURES,
        ),
        openvmm_defs::microvm::MICROVM_VIRTIO_CONTROL_CONSOLE_ID => (
            openvmm_defs::microvm::MICROVM_VIRTIO_CONTROL_CONSOLE_MMIO_BASE,
            openvmm_defs::microvm::MICROVM_VIRTIO_CONTROL_CONSOLE_IRQ,
            openvmm_defs::microvm::MICROVM_VIRTIO_CONSOLE_FEATURES,
        ),
        "virtio-blk" => {
            let block = sandbox_blocks
                .get(*sandbox_block_index)
                .context("microVM virtio-blk device has no sandbox role")?;
            *sandbox_block_index += 1;
            (
                block.role.mmio_base(),
                block.role.irq(),
                openvmm_defs::microvm::microvm_sandbox_block_features(block.role),
            )
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
    let interrupt_mode = VirtioMmioInterruptMode::SharedStatus {
        status_gpa: openvmm_defs::microvm::microvm_virtio_status_gpa(start)
            .context("microVM slot has no shared-status word")?,
    };
    Ok((start, len, irq, features, interrupt_mode))
}

/// The guest memory that the microVM e820 map reserves inside RAM besides the
/// ISA hole: the shared virtio interrupt-status page.
fn reserved_memory_ranges() -> [MemoryRange; 1] {
    [MemoryRange::new(
        openvmm_defs::microvm::MICROVM_SHARED_STATUS_PAGE_GPA
            ..openvmm_defs::microvm::MICROVM_SHARED_STATUS_PAGE_GPA
                + openvmm_defs::microvm::MICROVM_SHARED_STATUS_PAGE_SIZE,
    )]
}

pub(super) fn linux_ram_start_address(profile: MachineProfile) -> u64 {
    if cfg!(guest_arch = "aarch64") && profile == MachineProfile::Standard {
        1 << 30
    } else {
        0
    }
}

/// Assigns the fixed virtio-mmio slots of the microVM profile to devices, in
/// device order.
pub(super) struct VirtioMmioSlots<'a> {
    sandbox_blocks: &'a [openvmm_defs::microvm::MicrovmSandboxBlockConfig],
    sandbox_block_index: usize,
    filesystem_index: usize,
    chipset_mmio: ChipsetMmioRanges,
    mem_layout: &'a MemoryLayout,
    /// The RAM ranges of the e820 map, computed for the first device.
    dma_ranges: Option<Vec<MemoryRange>>,
    #[cfg(guest_arch = "aarch64")]
    fixed_devices: Vec<crate::worker::vm_loaders::FixedMmioDevice<'static>>,
}

impl<'a> VirtioMmioSlots<'a> {
    pub(super) fn new(
        sandbox_blocks: &'a [openvmm_defs::microvm::MicrovmSandboxBlockConfig],
        chipset_mmio: ChipsetMmioRanges,
        mem_layout: &'a MemoryLayout,
    ) -> Self {
        Self {
            sandbox_blocks,
            sandbox_block_index: 0,
            filesystem_index: 0,
            chipset_mmio,
            mem_layout,
            dma_ranges: None,
            #[cfg(guest_arch = "aarch64")]
            fixed_devices: Vec::new(),
        }
    }

    /// Returns RAM excluding shared status. The x86 binding additionally
    /// excludes its ISA hole; ARM does not reserve legacy PC memory.
    fn dma_ranges(&mut self) -> anyhow::Result<&[MemoryRange]> {
        if self.dma_ranges.is_none() {
            #[cfg(guest_arch = "x86_64")]
            {
                self.dma_ranges = Some(
                    loader::linux::mptable_boot::mptable_ram_ranges(
                        self.mem_layout,
                        &reserved_memory_ranges(),
                    )
                    .context("failed to find the RAM ranges of the microVM e820 map")?,
                );
            }
            #[cfg(guest_arch = "aarch64")]
            {
                self.dma_ranges = Some(
                    memory_range::subtract_ranges(
                        self.mem_layout.ram().iter().map(|ram| ram.range),
                        reserved_memory_ranges(),
                    )
                    .collect(),
                );
            }
        }
        Ok(self.dma_ranges.as_deref().unwrap_or_default())
    }

    /// Adds a virtio-mmio device at its fixed microVM slot, restricting its
    /// DMA to the RAM ranges of the e820 map.
    pub(super) fn add_device(
        &mut self,
        chipset_builder: &vmotherboard::ChipsetBuilder<'_>,
        driver_source: &VmTaskDriverSource,
        gm: &GuestMemory,
        partition: &Arc<dyn HvlitePartition>,
        id: &str,
        device: ResolvedVirtioDevice,
    ) -> anyhow::Result<()> {
        let (mmio_start, mmio_len, irq, features, interrupt_mode) = virtio_mmio_config(
            id,
            self.sandbox_blocks,
            &mut self.sandbox_block_index,
            &mut self.filesystem_index,
            self.chipset_mmio,
        )?;
        let irq = if cfg!(guest_arch = "aarch64") {
            openvmm_defs::microvm::microvm_virtio_slot_index(mmio_start)
                .context("microVM slot has no ARM interrupt source")?
        } else {
            irq
        };
        let dma_ranges = self
            .dma_ranges()?
            .iter()
            .map(|range| range.start()..range.end())
            .collect::<Vec<_>>();
        let id = format!("{id}-{mmio_start}");
        let gm = gm.clone();
        chipset_builder.arc_mutex_device(id).try_add(|services| {
            let mut device = VirtioMmioDevice::new_with_disabled_features_and_interrupt_mode(
                device.0,
                &driver_source.simple(),
                gm,
                services.new_line(IRQ_LINE_SET, "interrupt", irq),
                partition.clone().into_doorbell_registration(Vtl::Vtl0),
                mmio_start,
                mmio_len,
                !features,
                interrupt_mode,
            )
            .context("failed to create the virtio-mmio transport")?;
            device
                .restrict_dma(dma_ranges)
                .context("failed to restrict device DMA to RAM")?;
            anyhow::Ok(device)
        })?;
        #[cfg(guest_arch = "aarch64")]
        self.fixed_devices
            .push(crate::worker::vm_loaders::FixedMmioDevice {
                name: "virtio",
                compatible: "virtio,mmio",
                range: MemoryRange::new(mmio_start..mmio_start + mmio_len),
                interrupt: Some(crate::worker::vm_loaders::FixedGicInterrupt {
                    intid: *vmm_core::emuplat::gic::SPI_RANGE.start() + irq,
                    edge_triggered: true,
                }),
                dma_coherent: true,
            });
        Ok(())
    }

    #[cfg(guest_arch = "aarch64")]
    pub(super) fn into_fixed_devices(
        mut self,
    ) -> Vec<crate::worker::vm_loaders::FixedMmioDevice<'static>> {
        self.fixed_devices
            .push(crate::worker::vm_loaders::FixedMmioDevice {
                name: "nvx-platform",
                compatible: "microsoft,nvx-platform-control-v1",
                range: MemoryRange::new(
                    chipset_resources::microvm::MICROVM_PLATFORM_CONTROL_BASE
                        ..chipset_resources::microvm::MICROVM_PLATFORM_CONTROL_BASE
                            + chipset_resources::microvm::MICROVM_PLATFORM_CONTROL_SIZE,
                ),
                interrupt: None,
                dma_coherent: false,
            });
        self.fixed_devices
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

#[cfg(guest_arch = "x86_64")]
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

#[cfg(guest_arch = "aarch64")]
pub(super) fn build_aarch64_topology(
    config: &openvmm_defs::config::ProcessorTopologyConfig,
    platform: &virt::PlatformInfo,
) -> anyhow::Result<super::Aarch64TopologyResult> {
    openvmm_defs::microvm::validate_machine_gic_msi(MachineProfile::Microvm, config.arch.as_ref())?;
    anyhow::ensure!(platform.supports_gic_v3, "ARM microVM requires GICv3");
    anyhow::ensure!(
        openvmm_defs::microvm::microvm_processor_count_supported(config.proc_count)
            && config.enable_smt == Some(false)
            && config.vps_per_socket == Some(config.proc_count),
        "ARM microVM requires 1/2/4/8 VPs in one socket without SMT"
    );
    anyhow::ensure!(
        matches!(&config.arch, Some(openvmm_defs::config::ArchTopologyConfig::Aarch64(arch))
            if matches!(&arch.gic_config, Some(openvmm_defs::config::GicConfig::V3(Some(gic)))
                if gic.gic_distributor_base == openvmm_defs::microvm::MICROVM_ARM64_GIC_DISTRIBUTOR_BASE
                    && gic.gic_redistributors_base == openvmm_defs::microvm::MICROVM_ARM64_GIC_REDISTRIBUTORS_BASE)
                && matches!(arch.pmu_gsiv, openvmm_defs::config::PmuGsivConfig::Disabled)),
        "ARM microVM requires its fixed GIC geometry and disabled PMU"
    );
    let mut builder = vm_topology::processor::TopologyBuilder::new_aarch64(
        vm_topology::processor::aarch64::Aarch64PlatformConfig {
            gic_distributor_base: openvmm_defs::microvm::MICROVM_ARM64_GIC_DISTRIBUTOR_BASE,
            gic_version: vm_topology::processor::aarch64::GicVersion::V3 {
                redistributors_base: openvmm_defs::microvm::MICROVM_ARM64_GIC_REDISTRIBUTORS_BASE,
            },
            gic_msi: vm_topology::processor::aarch64::GicMsiController::None,
            pmu_gsiv: None,
            virt_timer_ppi: openvmm_defs::config::DEFAULT_VIRT_TIMER_PPI,
            gic_nr_irqs: openvmm_defs::config::DEFAULT_GIC_NR_IRQS,
        },
    );
    builder.smt_enabled(false).vps_per_socket(config.proc_count);
    Ok(super::Aarch64TopologyResult {
        processor_topology: builder.build(config.proc_count)?,
        spi_layout: crate::worker::spi_layout::resolve_spi_layout(
            &crate::worker::spi_layout::SpiLayoutInput {
                gic_nr_irqs: openvmm_defs::config::DEFAULT_GIC_NR_IRQS,
                v2m_spi_count: None,
                smmu_count: 0,
            },
        )?,
    })
}

// The microVM runs on Linux and Windows hosts alone.
#[cfg(all(test, any(target_os = "linux", windows)))]
mod tests {
    use super::MachineProfile;
    use super::virtio_mmio_config;
    use crate::worker::memory_layout::ChipsetMmioRanges;
    use chipset_device::io::IoResult;
    use chipset_device::mmio::MmioIntercept;
    use guestmem::GuestMemory;
    use inspect::InspectMut;
    use memory_range::MemoryRange;
    use openvmm_defs::microvm::MicrovmSandboxBlockConfig;
    use openvmm_defs::microvm::MicrovmSandboxBlockRole;
    use pal_async::DefaultDriver;
    use pal_async::async_test;
    use test_with_tracing::test;
    use virtio::DeviceTraits;
    use virtio::QueueResources;
    use virtio::VirtioDevice;
    use virtio::VirtioMmioDevice;
    use virtio::queue::QueueState;
    use virtio::spec::VirtioDeviceFeatures;
    use virtio::spec::mmio::VirtioMmioRegister;
    use vmcore::line_interrupt::LineInterrupt;
    use vmcore::vm_task::SingleDriverBackend;
    use vmcore::vm_task::VmTaskDriverSource;

    #[test]
    fn fixed_slot_sources_preserve_reserved_holes_and_status_offsets() {
        for (base, source) in [
            (0xd000_0000, 0),
            (0xd000_1000, 1),
            (0xd000_2000, 2),
            (0xd000_3000, 3),
            (0xd000_6000, 6),
            (0xd000_7000, 7),
        ] {
            assert_eq!(
                openvmm_defs::microvm::microvm_virtio_slot_index(base),
                Some(source)
            );
            assert_eq!(
                openvmm_defs::microvm::microvm_virtio_status_gpa(base),
                Some(0x30000 + u64::from(source) * 4)
            );
            assert_eq!(
                openvmm_defs::microvm::microvm_virtio_irq(base),
                Some(16 + source)
            );
        }
        for base in [0xd000_4000, 0xd000_5000, 0xd000_8000, 0xd000_0001] {
            assert_eq!(openvmm_defs::microvm::microvm_virtio_slot_index(base), None);
            assert_eq!(openvmm_defs::microvm::microvm_virtio_status_gpa(base), None);
        }
    }

    #[async_test]
    async fn arm_status_bootstrap_uses_backed_trusted_ram_but_device_dma_still_refuses_it(
        driver: DefaultDriver,
    ) {
        assert_eq!(super::linux_ram_start_address(MachineProfile::Microvm), 0);
        assert_eq!(
            super::linux_ram_start_address(MachineProfile::Standard),
            if cfg!(guest_arch = "aarch64") {
                1 << 30
            } else {
                0
            }
        );
        let layout = super::MemoryLayout::new(
            1 << 20,
            &[MemoryRange::new(3 << 30..4 << 30)],
            &[],
            &[],
            None,
        )
        .unwrap();
        let chipset_mmio = ChipsetMmioRanges {
            low: MemoryRange::new(3 << 30..4 << 30),
            high: MemoryRange::EMPTY,
            vtl2: MemoryRange::EMPTY,
        };
        let mut slots = super::VirtioMmioSlots::new(&[], chipset_mmio, &layout);
        let ranges = slots
            .dma_ranges()
            .unwrap()
            .iter()
            .map(|r| r.start()..r.end())
            .collect::<Vec<_>>();
        let gm = GuestMemory::allocate(1 << 20);
        let dma = gm.restrict(ranges.clone(), None).unwrap();
        assert!(dma.read_plain::<u32>(0x30004).is_err());
        assert!(dma.write_plain(0x30004, &1u32).is_err());
        dma.write_plain(0x31000, &1u32).unwrap();
        let (base, len, _, features, mode) =
            virtio_mmio_config("virtiofs", &[], &mut 0, &mut 0, chipset_mmio).unwrap();
        let drivers = VmTaskDriverSource::new(SingleDriverBackend::new(driver));
        let mut device = VirtioMmioDevice::new_with_disabled_features_and_interrupt_mode(
            Box::new(EveryFeatureDevice),
            &drivers.simple(),
            gm.clone(),
            LineInterrupt::detached(),
            None,
            base,
            len,
            !features,
            mode,
        )
        .unwrap();
        device.restrict_dma(ranges).unwrap();
        assert_eq!(gm.read_plain::<u32>(0x30004).unwrap(), 0);
        gm.write_plain(0x30004, &3u32).unwrap();
        let mut observed = [0; 4];
        assert!(matches!(
            device.mmio_read(
                base + u64::from(VirtioMmioRegister::INTERRUPT_STATUS.0),
                &mut observed
            ),
            IoResult::Ok
        ));
        assert_eq!(u32::from_le_bytes(observed), 3);
        assert!(dma.read_plain::<u32>(0x30004).is_err());
    }

    #[cfg(guest_arch = "aarch64")]
    #[test]
    fn arm_dma_excludes_only_shared_status_and_mmio_not_the_isa_hole() {
        let aperture = MemoryRange::new(0xc000_0000..0x1_0000_0000);
        let layout =
            vm_topology::memory::MemoryLayout::new(4 << 30, &[aperture], &[], &[], None).unwrap();
        let mut slots = super::VirtioMmioSlots::new(
            &[],
            ChipsetMmioRanges {
                low: aperture,
                high: MemoryRange::EMPTY,
                vtl2: MemoryRange::EMPTY,
            },
            &layout,
        );
        assert_eq!(
            slots.dma_ranges().unwrap(),
            [
                MemoryRange::new(0..0x30000),
                MemoryRange::new(0x31000..0xc000_0000),
                MemoryRange::new(0x1_0000_0000..0x1_4000_0000),
            ]
        );
    }

    #[cfg(guest_arch = "aarch64")]
    #[test]
    fn arm_dma_pa36_boundary_excludes_aperture_status_and_addresses_above_ram() {
        let aperture = MemoryRange::new(3 << 30..4 << 30);
        let limit = 1u64 << 36;
        let layout =
            vm_topology::memory::MemoryLayout::new(63 << 30, &[aperture], &[], &[], None).unwrap();
        let mut slots = super::VirtioMmioSlots::new(
            &[],
            ChipsetMmioRanges {
                low: aperture,
                high: MemoryRange::EMPTY,
                vtl2: MemoryRange::EMPTY,
            },
            &layout,
        );
        let dma = slots.dma_ranges().unwrap();
        assert_eq!(
            dma,
            [
                MemoryRange::new(0..0x30000),
                MemoryRange::new(0x31000..3 << 30),
                MemoryRange::new(4 << 30..limit),
            ]
        );
        let permitted = |start, end| {
            let request = MemoryRange::new(start..end);
            dma.iter().any(|range| range.contains(&request))
        };
        assert!(permitted(limit - 4096, limit));
        assert!(permitted(0x31000, 0x32000));
        for (start, end) in [
            (limit, limit + 4096),
            (limit - 4096, limit + 4096),
            (0x30000, 0x31000),
            (3 << 30, (3 << 30) + 4096),
            ((3 << 30) - 4096, (3 << 30) + 4096),
        ] {
            assert!(!permitted(start, end));
        }
    }

    #[cfg(guest_arch = "aarch64")]
    #[test]
    fn arm_topology_uses_fixed_gic_mpidrs_and_no_optional_controllers() {
        let platform = virt::PlatformInfo {
            platform_gsiv: Some(23),
            supports_gic_v3: true,
            supports_its: true,
            device_assignment_msi_iova: virt::DeviceAssignmentMsiIova::Unsupported,
        };
        for count in [1, 2, 4, 8] {
            let config = openvmm_defs::config::ProcessorTopologyConfig {
                proc_count: count,
                vps_per_socket: Some(count),
                enable_smt: Some(false),
                arch: Some(openvmm_defs::config::ArchTopologyConfig::Aarch64(
                    openvmm_defs::config::Aarch64TopologyConfig {
                        gic_config: Some(openvmm_defs::config::GicConfig::V3(Some(
                            openvmm_defs::config::GicV3Config {
                                gic_distributor_base:
                                    openvmm_defs::microvm::MICROVM_ARM64_GIC_DISTRIBUTOR_BASE,
                                gic_redistributors_base:
                                    openvmm_defs::microvm::MICROVM_ARM64_GIC_REDISTRIBUTORS_BASE,
                            },
                        ))),
                        pmu_gsiv: openvmm_defs::config::PmuGsivConfig::Disabled,
                        gic_msi: openvmm_defs::config::GicMsiConfig::Auto,
                    },
                )),
            };
            let result = super::build_aarch64_topology(&config, &platform).unwrap();
            let topology = result.processor_topology;
            assert_eq!(topology.vp_count(), count);
            assert_eq!(topology.gic_distributor_base(), 0xffff_0000);
            assert_eq!(topology.virt_timer_ppi(), 20);
            assert!(topology.pmu_gsiv().is_none());
            assert!(matches!(
                topology.gic_msi(),
                vm_topology::processor::aarch64::GicMsiController::None
            ));
            for (index, vp) in topology.vps_arch().enumerate() {
                assert_eq!(vp.mpidr.aff0() as usize, index);
                assert_eq!(
                    (vp.mpidr.aff1(), vp.mpidr.aff2(), vp.mpidr.aff3()),
                    (0, 0, 0)
                );
                assert!(!vp.mpidr.mt());
                assert_eq!(
                    vp.gicr,
                    Some(0xefff_0000 + index as u64 * aarch64defs::GIC_REDISTRIBUTOR_SIZE)
                );
            }
            let mut unsupported = platform.clone();
            unsupported.supports_gic_v3 = false;
            assert!(super::build_aarch64_topology(&config, &unsupported).is_err());
            for msi in [
                openvmm_defs::config::GicMsiConfig::Its,
                openvmm_defs::config::GicMsiConfig::V2m { spi_count: None },
                openvmm_defs::config::GicMsiConfig::V2m {
                    spi_count: Some(64),
                },
            ] {
                let mut explicit = openvmm_defs::config::ProcessorTopologyConfig {
                    proc_count: config.proc_count,
                    vps_per_socket: config.vps_per_socket,
                    enable_smt: config.enable_smt,
                    arch: config.arch.clone(),
                };
                let Some(openvmm_defs::config::ArchTopologyConfig::Aarch64(arch)) =
                    &mut explicit.arch
                else {
                    panic!("the fixture must have ARM topology");
                };
                arch.gic_msi = msi;
                let error = super::build_aarch64_topology(&explicit, &platform)
                    .err()
                    .expect("the worker must reject explicit MSI");
                assert!(error.to_string().contains("explicit GIC MSI"));
            }
        }
    }

    /// A device that offers every feature, so that its transport offers the
    /// guest exactly the features that the microVM configuration admits.
    #[derive(InspectMut)]
    struct EveryFeatureDevice;

    impl VirtioDevice for EveryFeatureDevice {
        fn traits(&self) -> DeviceTraits {
            DeviceTraits {
                device_features: VirtioDeviceFeatures::from_bits(u64::MAX),
                max_queues: 1,
                ..Default::default()
            }
        }

        async fn read_registers_u32(&mut self, _offset: u16) -> u32 {
            0
        }

        async fn write_registers_u32(&mut self, _offset: u16, _val: u32) {}

        async fn start_queue(
            &mut self,
            _idx: u16,
            _resources: QueueResources,
            _features: &VirtioDeviceFeatures,
            _initial_state: Option<QueueState>,
        ) -> anyhow::Result<()> {
            anyhow::bail!("the test never starts a queue")
        }

        async fn stop_queue(&mut self, _idx: u16) -> Option<QueueState> {
            None
        }
    }

    /// Returns the numbers of the feature bits that the transport at
    /// `mmio_base` offers the guest, read as a driver reads them, one 32-bit
    /// bank at a time.
    fn offered_feature_bits(device: &mut VirtioMmioDevice, mmio_base: u64) -> Vec<u32> {
        let select = mmio_base + u64::from(VirtioMmioRegister::DEVICE_FEATURES_SEL.0);
        let register = mmio_base + u64::from(VirtioMmioRegister::DEVICE_FEATURES.0);
        let mut features = 0;
        for bank in 0..2u32 {
            assert!(matches!(
                device.mmio_write(select, &bank.to_le_bytes()),
                IoResult::Ok
            ));
            let mut value = [0; 4];
            assert!(matches!(
                device.mmio_read(register, &mut value),
                IoResult::Ok
            ));
            features |= u64::from(u32::from_le_bytes(value)) << (32 * bank);
        }
        (0..u64::BITS)
            .filter(|bit| features & (1 << bit) != 0)
            .collect()
    }

    #[async_test]
    async fn microvm_devices_offer_exactly_their_feature_masks(driver: DefaultDriver) {
        let driver_source = VmTaskDriverSource::new(SingleDriverBackend::new(driver));
        // The memory holds the page of the slots' shared interrupt-status
        // words.
        let gm = GuestMemory::allocate(
            (openvmm_defs::microvm::MICROVM_SHARED_STATUS_PAGE_GPA
                + openvmm_defs::microvm::MICROVM_SHARED_STATUS_PAGE_SIZE) as usize,
        );
        let chipset_mmio = ChipsetMmioRanges {
            low: MemoryRange::new(0xc000_0000..0x1_0000_0000),
            high: MemoryRange::EMPTY,
            vtl2: MemoryRange::EMPTY,
        };
        let blocks = [
            MicrovmSandboxBlockConfig {
                role: MicrovmSandboxBlockRole::Distro,
                read_only: true,
            },
            MicrovmSandboxBlockConfig {
                role: MicrovmSandboxBlockRole::Scratch,
                read_only: false,
            },
        ];
        let mut sandbox_block_index = 0;
        let mut filesystem_index = 0;
        // Each device in the order of the slots of its kind, with its window,
        // its IOAPIC input, and the features that the guest sees: never
        // access-platform, which the transport offers every device, nor the
        // console size, which the virtio-console device offers.
        let devices: [(&str, u64, u32, &[u32]); 6] = [
            ("virtio-net", 0xd000_0000, 16, &[5, 32]),
            ("virtiofs", 0xd000_1000, 17, &[28, 29, 32]),
            ("virtio-console", 0xd000_2000, 18, &[28, 29, 32]),
            ("virtio-blk", 0xd000_3000, 19, &[2, 5, 6, 9, 10, 28, 29, 32]),
            ("virtio-blk", 0xd000_6000, 22, &[2, 6, 9, 10, 28, 29, 32]),
            (
                openvmm_defs::microvm::MICROVM_VIRTIO_CONTROL_CONSOLE_ID,
                0xd000_7000,
                23,
                &[28, 29, 32],
            ),
        ];
        for (id, mmio_base, input, bits) in devices {
            let (start, len, irq, features, interrupt_mode) = virtio_mmio_config(
                id,
                &blocks,
                &mut sandbox_block_index,
                &mut filesystem_index,
                chipset_mmio,
            )
            .unwrap();
            assert_eq!(start, mmio_base, "{id}");
            assert_eq!(irq, input, "{id}");
            let mut device = VirtioMmioDevice::new_with_disabled_features_and_interrupt_mode(
                Box::new(EveryFeatureDevice),
                &driver_source.simple(),
                gm.clone(),
                LineInterrupt::detached(),
                None,
                start,
                len,
                !features,
                interrupt_mode,
            )
            .unwrap();
            assert_eq!(
                offered_feature_bits(&mut device, start),
                bits,
                "{id} at {start:#x}"
            );
        }
    }
}
