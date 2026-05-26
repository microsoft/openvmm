// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! PCIe device MSI/DMA wiring helpers.
//!
//! This module builds the layered `GuestMemory`, `SignalMsi`, and `IrqFd`
//! for PCIe devices, applying platform-specific MSI controller wrapping
//! (ITS on aarch64, future IR on x86) and SMMU DMA/MSI translation.

use crate::partition::HvlitePartition;
use guestmem::GuestMemory;
use hvdef::Vtl;
use std::sync::Arc;
use vm_topology::processor::ProcessorTopology;

/// Input parameters for [`build_pcie_msi_context`].
pub(super) struct PcieWiringParams<'a> {
    /// The partition providing base `SignalMsi` and `IrqFd`.
    pub partition: &'a dyn HvlitePartition,
    /// Raw guest memory (wrapped with SMMU translation when applicable).
    pub guest_memory: &'a GuestMemory,
    /// The device's assigned bus range (for IOMMU stream ID composition).
    #[cfg(guest_arch = "aarch64")]
    pub bus_range: &'a pci_core::bus_range::AssignedBusRange,
    /// PCIe segment number (for MSI controller device ID composition).
    pub segment: u16,
    /// Processor topology (determines which MSI controller wrapping to apply).
    pub processor_topology: &'a ProcessorTopology,
    /// SMMU shared state if this device is behind an SMMU, or `None`.
    #[cfg(guest_arch = "aarch64")]
    pub smmu: Option<&'a Arc<smmu::SmmuSharedState>>,
}

/// The layered GuestMemory, SignalMsi, and IrqFd produced by
/// [`build_pcie_msi_context`].
pub(super) struct PcieDeviceInterrupts {
    /// Guest memory for the device — either the raw memory or an
    /// SMMU-translating wrapper.
    pub guest_memory: GuestMemory,
    /// MSI signaling target, optionally wrapped with platform MSI controller
    /// and/or SMMU translation. `None` if the partition does not provide
    /// MSI support.
    pub signal_msi: Option<Arc<dyn pci_core::msi::SignalMsi>>,
    /// IrqFd for kernel-accelerated MSI delivery, optionally wrapped with
    /// platform MSI controller and/or SMMU translation. `None` if the
    /// partition does not provide irqfd support.
    pub irqfd: Option<Arc<dyn vmcore::irqfd::IrqFd>>,
    /// Whether the device is behind a software IOMMU (e.g., emulated SMMU)
    /// that cannot program the host IOMMU for passthrough DMA.
    pub software_iommu: bool,
}

impl PcieDeviceInterrupts {
    /// Connect the signal_msi and irqfd to an [`MsiConnection`].
    pub fn connect_to(self, msi_conn: &pci_core::msi::MsiConnection) {
        if let Some(target) = self.signal_msi {
            msi_conn.connect(target);
        }
        if let Some(fd) = self.irqfd {
            msi_conn.connect_irqfd(fd);
        }
    }
}

/// Build the layered GuestMemory, SignalMsi, and IrqFd for a PCIe device.
///
/// Applies platform MSI controller wrapping (ITS on aarch64, identity on x86
/// for now) and optional SMMU DMA/MSI translation.
pub(super) fn build_pcie_msi_context(params: &PcieWiringParams<'_>) -> PcieDeviceInterrupts {
    let base_signal_msi = params
        .partition
        .as_signal_msi(Vtl::Vtl0)
        .map(|s| wrap_platform_msi(s, params.segment, params.processor_topology));
    let base_irqfd = params
        .partition
        .irqfd()
        .map(|fd| wrap_platform_irqfd(fd, params.segment, params.processor_topology));

    // When an SMMU covers this device, wrap GuestMemory, SignalMsi, and
    // IrqFd with SMMU translation. stream_id_base is 0 because each SMMU
    // is 1:1 with its root complex — stream IDs are plain BDFs.
    //
    // The translating GuestMemory is created unconditionally when an SMMU
    // is present — DMA translation must not depend on MSI availability.
    #[cfg(guest_arch = "aarch64")]
    if let Some(shared) = params.smmu {
        let translating_gm =
            shared.create_translating_memory(params.bus_range.clone(), 0, params.guest_memory);
        let smmu_msi = base_signal_msi.map(|inner_msi| {
            Arc::new(smmu::SmmuSignalMsi::new(shared.clone(), 0, inner_msi))
                as Arc<dyn pci_core::msi::SignalMsi>
        });
        let irqfd =
            base_irqfd.map(|fd| shared.create_irqfd(0, fd) as Arc<dyn vmcore::irqfd::IrqFd>);
        return PcieDeviceInterrupts {
            guest_memory: translating_gm,
            signal_msi: smmu_msi,
            irqfd,
            software_iommu: true,
        };
    }

    PcieDeviceInterrupts {
        guest_memory: params.guest_memory.clone(),
        signal_msi: base_signal_msi,
        irqfd: base_irqfd,
        software_iommu: false,
    }
}

/// Wrap a `SignalMsi` with platform-specific MSI controller translation.
///
/// On aarch64 with ITS: wraps with segment-based device ID composition.
/// On x86: identity (future: interrupt remapping).
pub(super) fn wrap_platform_msi(
    signal_msi: Arc<dyn pci_core::msi::SignalMsi>,
    #[cfg_attr(not(guest_arch = "aarch64"), expect(unused_variables))] segment: u16,
    #[cfg_attr(not(guest_arch = "aarch64"), expect(unused_variables))]
    processor_topology: &ProcessorTopology,
) -> Arc<dyn pci_core::msi::SignalMsi> {
    #[cfg(guest_arch = "aarch64")]
    if matches!(
        processor_topology.gic_msi(),
        vm_topology::processor::aarch64::GicMsiController::Its(_)
    ) {
        return Arc::new(pcie::its::ItsSignalMsi::new(signal_msi, segment));
    }
    signal_msi
}

/// Wrap an `IrqFd` with platform-specific MSI controller translation.
///
/// On aarch64 with ITS: wraps with segment-based device ID composition.
/// On x86: identity (future: interrupt remapping).
pub(super) fn wrap_platform_irqfd(
    irqfd: Arc<dyn vmcore::irqfd::IrqFd>,
    #[cfg_attr(not(guest_arch = "aarch64"), expect(unused_variables))] segment: u16,
    #[cfg_attr(not(guest_arch = "aarch64"), expect(unused_variables))]
    processor_topology: &ProcessorTopology,
) -> Arc<dyn vmcore::irqfd::IrqFd> {
    #[cfg(guest_arch = "aarch64")]
    if matches!(
        processor_topology.gic_msi(),
        vm_topology::processor::aarch64::GicMsiController::Its(_)
    ) {
        return Arc::new(pcie::its::ItsIrqFd::new(irqfd, segment));
    }
    irqfd
}
