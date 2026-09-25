// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Backend-independent microVM builder support.

use super::ApicMode;
use super::BootDeviceType;
use super::PetriVmBuilder;
use super::PetriVmmBackend;
use openvmm_defs::microvm::MachineProfile;

impl<T: PetriVmmBackend> PetriVmBuilder<T> {
    /// Select the microVM machine profile with deterministic SMP topology.
    pub fn with_microvm_machine(mut self, processor_count: u32) -> Self {
        assert!(
            openvmm_defs::microvm::microvm_processor_count_supported(processor_count),
            "microVM supports only 1, 2, 4, or 8 vCPUs"
        );
        self.config.machine_profile = MachineProfile::Microvm;
        self.config.proc_topology.vp_count = processor_count;
        self.config.proc_topology.vps_per_socket = Some(processor_count);
        self.config.proc_topology.enable_smt = Some(false);
        self.config.proc_topology.apic_mode = Some(ApicMode::Xapic);
        self.minimal_mode = true;
        self.enable_serial = true;
        self.use_virtio_vsock = false;
        self.no_vmbus = true;
        self.no_hv = true;
        self.config.vmbus_storage_controllers.clear();
        self.config.pcie_nvme_drives.clear();
        self.config.pcie_virtio_blk_drives.clear();
        self.config.physical_nvme_devices.clear();
        self.agent_image = None;
        self.openhcl_agent_image = None;
        self.boot_device_type = BootDeviceType::None;
        self
    }
}

pub(super) fn uses_pipette_as_init(machine_profile: MachineProfile) -> bool {
    machine_profile == MachineProfile::Standard
}

#[cfg(windows)]
pub(super) fn ensure_hyperv_compatible(machine_profile: MachineProfile) -> anyhow::Result<()> {
    anyhow::ensure!(
        machine_profile == MachineProfile::Standard,
        "the microVM profile is only supported by OpenVMM"
    );
    Ok(())
}
