// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Worker construction and lifecycle support for the microVM profile.

use super::InitializedVm;
use super::LoadedVm;
use super::LoadedVmInner;
use super::Manifest;
use super::RestartState;
use crate::partition::HvlitePartition;
use crate::worker::memory_layout::ChipsetMmioRanges;
use anyhow::Context;
use chipset_device_resources::IRQ_LINE_SET;
use guestmem::GuestMemory;
use hvdef::Vtl;
use mesh::error::RemoteError;
use mesh_worker::WorkerRpc;
use openvmm_defs::microvm::MachineProfile;
use openvmm_defs::rpc::PulseSaveRestoreError;
use openvmm_defs::rpc::VmRpc;
use openvmm_defs::worker::VmWorkerParameters;
use std::sync::Arc;
use virtio::VirtioMmioDevice;
use virtio::resolve::ResolvedVirtioDevice;
use vm_loader::InitialLoad;
use vmcore::vm_task::VmTaskDriverSource;

/// MicroVM inputs taken from the [`VmWorkerParameters`].
pub(super) struct MicrovmParameters {}

impl MicrovmParameters {
    /// Takes the microVM inputs out of the worker parameters and validates the
    /// machine configuration for the selected hypervisor.
    pub(super) fn take(parameters: &mut VmWorkerParameters) -> anyhow::Result<Self> {
        openvmm_defs::microvm::validate_machine_config(
            &parameters.cfg,
            Some(parameters.hypervisor.id()),
        )?;
        Ok(Self {})
    }

    /// Prepares the kernel command line of a microVM cold boot, before the VM
    /// is loaded.
    pub(super) fn prepare_cold_boot(
        &self,
        vm: &mut InitializedVm,
        restored_from_snapshot: bool,
    ) -> anyhow::Result<()> {
        prepare_cold_boot_command_line(&mut vm.cfg, vm.partition.as_ref(), restored_from_snapshot)
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
        &[],
    )?)
}

impl LoadedVm {
    /// Applies the microVM restrictions to a management RPC. Rejected RPCs are
    /// completed here; the others are returned for dispatch.
    pub(super) fn filter_vm_rpc(&self, message: VmRpc) -> Option<VmRpc> {
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
) -> anyhow::Result<(u64, u64, u32, u64)> {
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
    Ok((start, len, irq, disabled_features))
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
        let (mmio_start, mmio_len, irq, disabled_features) =
            virtio_mmio_config(id, self.chipset_mmio)?;
        let id = format!("{id}-{mmio_start}");
        let gm = gm.clone();
        chipset_builder.arc_mutex_device(id).try_add(|services| {
            VirtioMmioDevice::new_with_disabled_features(
                device.0,
                &driver_source.simple(),
                gm,
                services.new_line(IRQ_LINE_SET, "interrupt", irq),
                partition.clone().into_doorbell_registration(Vtl::Vtl0),
                mmio_start,
                mmio_len,
                disabled_features,
            )
        })?;
        Ok(())
    }
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
