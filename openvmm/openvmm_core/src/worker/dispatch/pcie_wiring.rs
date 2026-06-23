// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! PCIe MSI routing and DMA wiring helpers.
//!
//! This module provides layered MSI routing and DMA translation for PCIe
//! entities (root complexes, switches, and devices).
//!
//! [`PcieMsiPlatform`] captures platform-specific context (ITS on aarch64,
//! IOMMU interrupt remapping on x86_64) and wraps `SignalMsi`/`IrqFd` via
//! [`PcieMsiPlatform::wrap_msi`] → [`PcieMsiRouting`].
//!
//! [`build_device_wiring`] extends this with IOMMU DMA translation
//! (SMMU on aarch64, AMD IOMMU on x86_64) → [`PcieDeviceWiring`].

use crate::partition::HvlitePartition;
use guestmem::GuestMemory;
use hvdef::Vtl;
use std::sync::Arc;
use vm_topology::processor::ProcessorTopology;

/// Platform-specific MSI wrapping context for PCIe entities.
///
/// Encapsulates ITS device ID composition (aarch64) and IOMMU interrupt
/// remapping (x86_64). Construct one of these and call [`wrap_msi`] to
/// get correctly-wrapped `SignalMsi` and `IrqFd` for any PCIe entity —
/// root complexes, switches, and devices alike.
///
/// [`wrap_msi`]: PcieMsiPlatform::wrap_msi
pub(super) struct PcieMsiPlatform<'a> {
    /// The partition providing base `SignalMsi` and `IrqFd`.
    pub partition: &'a dyn HvlitePartition,
    /// PCIe segment number (for ITS device ID composition on aarch64).
    #[cfg_attr(not(guest_arch = "aarch64"), expect(dead_code))]
    pub segment: u16,
    /// Processor topology (determines ITS wrapping on aarch64).
    #[cfg_attr(not(guest_arch = "aarch64"), expect(dead_code))]
    pub processor_topology: &'a ProcessorTopology,
    /// AMD IOMMU shared state for interrupt remapping, or `None` if this
    /// entity is not behind an AMD IOMMU.
    #[cfg(guest_arch = "x86_64")]
    pub iommu: Option<&'a Arc<amd_iommu::IommuSharedState>>,
}

/// Wrapped `SignalMsi` and `IrqFd` for a PCIe entity.
///
/// Produced by [`PcieMsiPlatform::wrap_msi`]. Use [`connect_to`] to
/// wire these into an [`MsiConnection`].
///
/// [`connect_to`]: PcieMsiRouting::connect_to
/// [`MsiConnection`]: pci_core::msi::MsiConnection
pub(super) struct PcieMsiRouting {
    /// MSI signaling target with platform wrapping applied. `None` if
    /// the partition does not provide MSI support.
    pub signal_msi: Option<Arc<dyn pci_core::msi::SignalMsi>>,
    /// IrqFd for kernel-accelerated MSI delivery with platform wrapping
    /// applied. `None` if the partition does not provide irqfd support,
    /// or if IOMMU interrupt remapping is active (irqfd is not yet
    /// supported through the emulated IOMMU).
    pub irqfd: Option<Arc<dyn vmcore::irqfd::IrqFd>>,
}

impl PcieMsiRouting {
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

impl PcieMsiPlatform<'_> {
    /// Wrap the partition's base `SignalMsi` and `IrqFd` with platform-
    /// specific MSI controller translation and IOMMU interrupt remapping.
    ///
    /// On aarch64 with ITS: wraps with segment-based device ID composition.
    /// On x86_64 with AMD IOMMU: wraps with interrupt remapping; irqfd
    /// is disabled because kernel-mediated MSI routes bypass emulated
    /// interrupt remapping.
    pub fn wrap_msi(&self) -> PcieMsiRouting {
        let mut signal_msi: Option<Arc<dyn pci_core::msi::SignalMsi>> =
            self.partition.as_signal_msi(Vtl::Vtl0);
        let mut irqfd: Option<Arc<dyn vmcore::irqfd::IrqFd>> = self.partition.irqfd();

        // aarch64 ITS: wrap with segment-based device ID composition.
        #[cfg(guest_arch = "aarch64")]
        if matches!(
            self.processor_topology.gic_msi(),
            vm_topology::processor::aarch64::GicMsiController::Its(_)
        ) {
            signal_msi =
                signal_msi.map(|s| Arc::new(pcie::its::ItsSignalMsi::new(s, self.segment)) as _);
            irqfd = irqfd.map(|fd| Arc::new(pcie::its::ItsIrqFd::new(fd, self.segment)) as _);
        }

        // x86_64 AMD IOMMU: wrap with interrupt remapping.
        //
        // TODO: irqfd is disabled because kernel-mediated MSI routes
        // bypass our emulated interrupt remapping. We could support
        // irqfd by wrapping IrqFdRoute::enable() to do the IRTE lookup
        // and push the remapped address/data to the kernel, then
        // re-pushing on INVALIDATE_INTERRUPT_TABLE commands.
        #[cfg(guest_arch = "x86_64")]
        if let Some(shared) = &self.iommu {
            signal_msi = signal_msi.map(|s| shared.wrap_signal_msi(s) as _);
            irqfd = None;
        }

        PcieMsiRouting { signal_msi, irqfd }
    }
}

/// Input parameters for [`build_device_wiring`].
pub(super) struct PcieDeviceWiringParams<'a> {
    /// Platform MSI context (ITS, IOMMU interrupt remapping).
    pub msi_platform: PcieMsiPlatform<'a>,
    /// Raw guest memory (wrapped with IOMMU DMA translation when applicable).
    pub guest_memory: &'a GuestMemory,
    /// The device's assigned bus range (for IOMMU stream/device ID).
    pub bus_range: &'a pci_core::bus_range::AssignedBusRange,
    /// SMMU shared state if this device is behind an SMMU, or `None`.
    #[cfg(guest_arch = "aarch64")]
    pub smmu: Option<&'a Arc<smmu::SmmuSharedState>>,
}

/// The layered GuestMemory and MSI routing for a PCIe device.
///
/// Produced by [`build_device_wiring`]. Extends [`PcieMsiRouting`] with
/// IOMMU DMA translation and a `software_iommu` flag.
pub(super) struct PcieDeviceWiring {
    /// Guest memory for the device — either the raw memory or an
    /// IOMMU-translating wrapper.
    pub guest_memory: GuestMemory,
    /// Wrapped MSI routing for the device.
    pub msi: PcieMsiRouting,
    /// Whether the device is behind a software IOMMU (e.g., emulated
    /// SMMU or AMD IOMMU) that cannot program the host IOMMU for
    /// passthrough DMA.
    pub software_iommu: bool,
}

impl PcieDeviceWiring {
    /// Connect the MSI routing to an [`MsiConnection`].
    pub fn connect_to(self, msi_conn: &pci_core::msi::MsiConnection) {
        self.msi.connect_to(msi_conn);
    }
}

/// Build the layered GuestMemory and MSI routing for a PCIe device.
///
/// Calls [`PcieMsiPlatform::wrap_msi`] for MSI/IrqFd wrapping, then
/// adds IOMMU DMA translation (SMMU on aarch64, AMD IOMMU on x86_64)
/// to produce the device's `GuestMemory`.
pub(super) fn build_device_wiring(params: PcieDeviceWiringParams<'_>) -> PcieDeviceWiring {
    let msi = params.msi_platform.wrap_msi();

    // aarch64 SMMU: wrap GuestMemory and SignalMsi/IrqFd with SMMU
    // translation. stream_id_base is 0 because each SMMU is 1:1 with
    // its root complex — stream IDs are plain BDFs.
    //
    // The translating GuestMemory is created unconditionally when an
    // SMMU is present — DMA translation must not depend on MSI
    // availability.
    #[cfg(guest_arch = "aarch64")]
    if let Some(shared) = params.smmu {
        let translator = shared.translator(0);
        let translating_gm = iommu_common::TranslatingMemory::new_guest_memory(
            "smmu-translating",
            translator,
            params.bus_range.clone(),
            params.guest_memory.clone(),
        );
        let smmu_msi = msi.signal_msi.map(|inner_msi| {
            Arc::new(smmu::SmmuSignalMsi::new(shared.clone(), 0, inner_msi))
                as Arc<dyn pci_core::msi::SignalMsi>
        });
        let irqfd = msi
            .irqfd
            .map(|fd| shared.wrap_irqfd(0, fd) as Arc<dyn vmcore::irqfd::IrqFd>);
        return PcieDeviceWiring {
            guest_memory: translating_gm,
            msi: PcieMsiRouting {
                signal_msi: smmu_msi,
                irqfd,
            },
            software_iommu: true,
        };
    }

    // x86_64 AMD IOMMU: wrap GuestMemory with DMA translation.
    // MSI interrupt remapping was already applied by wrap_msi().
    #[cfg(guest_arch = "x86_64")]
    if let Some(shared) = params.msi_platform.iommu {
        let translator = shared.translator();
        let translating_gm = iommu_common::TranslatingMemory::new_guest_memory(
            "amd-iommu-translating",
            translator,
            params.bus_range.clone(),
            params.guest_memory.clone(),
        );
        return PcieDeviceWiring {
            guest_memory: translating_gm,
            msi,
            software_iommu: true,
        };
    }

    PcieDeviceWiring {
        guest_memory: params.guest_memory.clone(),
        msi,
        software_iommu: false,
    }
}
