// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! NVMe Virtual Function (VF) state for SR-IOV support.

use chipset_device::io::IoResult;
use inspect::Inspect;
use pci_core::capabilities::msix::MsixEmulator;
use pci_core::capabilities::pci_express::PciExpressCapability;
use pci_core::cfg_space_emu::BarMemoryKind;
use pci_core::cfg_space_emu::ConfigSpaceType0Emulator;
use pci_core::cfg_space_emu::DeviceBars;
use pci_core::msi::MsiTarget;
use pci_core::spec::hwid::ClassCode;
use pci_core::spec::hwid::HardwareIds;
use pci_core::spec::hwid::ProgrammingInterface;
use pci_core::spec::hwid::Subclass;

use crate::BAR0_LEN;
use crate::VENDOR_ID;

/// An NVMe Virtual Function (VF) with its own PCI config space.
///
/// VFs are owned by the PF ([`super::NvmeController`]) and are created/destroyed
/// when the SR-IOV capability's VF Enable bit changes. In this phase, VFs
/// have config space identity and MSI-X capability but no NVMe controller
/// logic — that is added in later phases.
#[derive(Inspect)]
pub(crate) struct NvmeVirtualFunction {
    cfg_space: ConfigSpaceType0Emulator,
    // MSI-X emulator — unused until Phase 3 (VF MMIO handling).
    #[inspect(skip)]
    _msix: MsixEmulator,
}

impl NvmeVirtualFunction {
    /// Creates a new VF with the given identity.
    ///
    /// `vf_device_id` is the PCI device ID for the VF (from the SR-IOV
    /// capability). `msix_count` is the number of MSI-X vectors.
    /// `msi_target` should be derived from the PF's target with the
    /// correct VF devfn via `MsiTarget::with_devfn`.
    pub fn new(vf_device_id: u16, msix_count: u16, msi_target: &MsiTarget) -> Self {
        let (msix, msix_cap) = MsixEmulator::new(4, msix_count, msi_target);

        // VF BARs use Dummy backing for now. Actual MMIO intercepts are
        // wired in Phase 3.
        let bars = DeviceBars::new()
            .bar0(BAR0_LEN, BarMemoryKind::Dummy)
            .bar4(msix.bar_len(), BarMemoryKind::Dummy);

        let cfg_space = ConfigSpaceType0Emulator::new(
            HardwareIds {
                vendor_id: VENDOR_ID,
                device_id: vf_device_id,
                revision_id: 0,
                prog_if: ProgrammingInterface::MASS_STORAGE_CONTROLLER_NON_VOLATILE_MEMORY_NVME,
                sub_class: Subclass::MASS_STORAGE_CONTROLLER_NON_VOLATILE_MEMORY,
                base_class: ClassCode::MASS_STORAGE_CONTROLLER,
                type0_sub_vendor_id: 0,
                type0_sub_system_id: 0,
            },
            vec![
                Box::new(msix_cap),
                Box::new(PciExpressCapability::new(
                    pci_core::spec::caps::pci_express::DevicePortType::Endpoint,
                    None,
                )),
            ],
            Vec::new(),
            bars,
        );

        Self {
            cfg_space,
            _msix: msix,
        }
    }

    /// Read from this VF's PCI config space.
    pub fn pci_cfg_read(&mut self, offset: u16, value: &mut u32) -> IoResult {
        self.cfg_space.read_u32(offset, value)
    }

    /// Write to this VF's PCI config space.
    pub fn pci_cfg_write(&mut self, offset: u16, value: u32) -> IoResult {
        self.cfg_space.write_u32(offset, value)
    }
}
