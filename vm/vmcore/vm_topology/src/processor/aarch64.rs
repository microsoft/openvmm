// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! ARM64-specific topology definitions.

use super::ArchTopology;
use super::InvalidTopology;
use super::ProcessorTopology;
use super::TopologyBuilder;
use super::VpIndex;
use super::VpInfo;
use super::VpTopologyInfo;
use aarch64defs::MpidrEl1;

/// ARM64-specific topology information.
#[cfg_attr(feature = "inspect", derive(inspect::Inspect))]
#[derive(Debug, Copy, Clone)]
#[non_exhaustive]
pub struct Aarch64Topology {
    platform: Aarch64PlatformConfig,
}

impl ArchTopology for Aarch64Topology {
    type ArchVpInfo = Aarch64VpInfo;
    type BuilderState = Aarch64TopologyBuilderState;

    fn vp_topology(_topology: &ProcessorTopology<Self>, info: &Self::ArchVpInfo) -> VpTopologyInfo {
        VpTopologyInfo {
            socket: info.mpidr.aff2().into(),
            core: info.mpidr.aff1().into(),
            thread: info.mpidr.aff0().into(),
        }
    }
}

/// Aarch64-specific [`TopologyBuilder`] state.
pub struct Aarch64TopologyBuilderState {
    platform: Aarch64PlatformConfig,
}

/// GIC version and version-specific addressing for the virtual machine.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg_attr(feature = "inspect", derive(inspect::Inspect))]
#[cfg_attr(feature = "inspect", inspect(external_tag))]
pub enum GicVersion {
    /// GICv2 — uses a shared CPU interface region instead of per-VP redistributors.
    /// Required for platforms like Raspberry Pi 5 (GIC-400).
    V2 {
        /// Physical base address of the GIC CPU interface.
        #[cfg_attr(feature = "inspect", inspect(hex))]
        cpu_interface_base: u64,
    },
    /// GICv3 — uses per-VP redistributors. Default for most server/desktop platforms.
    V3 {
        /// Physical base address of the GIC redistributor region.
        #[cfg_attr(feature = "inspect", inspect(hex))]
        redistributors_base: u64,
    },
}

/// ARM64 platform interrupt and GIC configuration.
///
/// Groups GIC base addresses, MSI frame info, and platform interrupt
/// assignments (PMU, virtual timer) into a single struct so that the
/// topology builder takes one value instead of several positional `u32`s.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg_attr(feature = "inspect", derive(inspect::Inspect))]
pub struct Aarch64PlatformConfig {
    /// GIC distributor base address.
    #[cfg_attr(feature = "inspect", inspect(hex))]
    pub gic_distributor_base: u64,
    /// GIC version and version-specific addresses.
    pub gic_version: GicVersion,
    /// MSI controller for PCIe interrupt delivery.
    pub gic_msi: GicMsiController,
    /// Performance Monitor Unit GSIV (GIC INTID). `None` if not available.
    pub pmu_gsiv: Option<u32>,
    /// Virtual timer PPI (GIC INTID, e.g. 20 for PPI 4).
    pub virt_timer_ppi: u32,
    /// Total number of GIC interrupts (SGIs + PPIs + SPIs).
    ///
    /// KVM requires: `64 <= gic_nr_irqs <= 1023` and a multiple of 32.
    /// The maximum valid value is 992 (31 × 32).
    pub gic_nr_irqs: u32,
}

/// GIC v2m MSI frame parameters.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg_attr(feature = "inspect", derive(inspect::Inspect))]
pub struct GicV2mInfo {
    /// Physical base address of the v2m MSI frame.
    #[cfg_attr(feature = "inspect", inspect(hex))]
    pub frame_base: u64,
    /// First GIC interrupt ID in the SPI range owned by this frame.
    pub spi_base: u32,
    /// Number of SPIs owned by this frame.
    pub spi_count: u32,
}

/// GICv3 ITS (Interrupt Translation Service) parameters.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg_attr(feature = "inspect", derive(inspect::Inspect))]
pub struct GicItsInfo {
    /// Physical base address of the ITS MMIO region (must be 64 KiB aligned).
    #[cfg_attr(feature = "inspect", inspect(hex))]
    pub its_base: u64,
}

/// MSI controller configuration for PCIe interrupt delivery.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg_attr(feature = "inspect", derive(inspect::Inspect))]
#[cfg_attr(feature = "inspect", inspect(external_tag))]
pub enum GicMsiController {
    /// No MSI controller configured.
    None,
    /// GICv2m — maps MSI writes to a fixed pool of SPIs.
    V2m(GicV2mInfo),
    /// GICv3 ITS — routes MSIs via LPIs using (DeviceID, EventID) lookup.
    Its(GicItsInfo),
}

/// ARM64 specific VP info.
#[cfg_attr(feature = "inspect", derive(inspect::Inspect))]
#[derive(Debug, Copy, Clone)]
pub struct Aarch64VpInfo {
    /// The base info.
    #[cfg_attr(feature = "inspect", inspect(flatten))]
    pub base: VpInfo,
    /// The MPIDR_EL1 value of the processor.
    #[cfg_attr(feature = "inspect", inspect(hex, with = "|&x| u64::from(x)"))]
    pub mpidr: MpidrEl1,
    /// GIC Redistributor Address (GICv3 only; `None` for GICv2).
    #[cfg_attr(feature = "inspect", inspect(hex))]
    pub gicr: Option<u64>,
    /// Performance Interrupt GSIV (PMU)
    #[cfg_attr(feature = "inspect", inspect(hex))]
    pub pmu_gsiv: Option<u32>,
}

impl AsRef<VpInfo> for Aarch64VpInfo {
    fn as_ref(&self) -> &VpInfo {
        &self.base
    }
}

impl TopologyBuilder<Aarch64Topology> {
    /// Returns a builder for creating an aarch64 processor topology.
    pub fn new_aarch64(platform: Aarch64PlatformConfig) -> Self {
        Self {
            vps_per_socket: 1,
            smt_enabled: false,
            arch: Aarch64TopologyBuilderState { platform },
        }
    }

    /// Builds a processor topology with `proc_count` processors.
    pub fn build(
        &self,
        proc_count: u32,
    ) -> Result<ProcessorTopology<Aarch64Topology>, InvalidTopology> {
        if proc_count >= 256 {
            return Err(InvalidTopology::TooManyVps {
                requested: proc_count,
                max: u8::MAX.into(),
            });
        }
        if let GicVersion::V2 { .. } = self.arch.platform.gic_version {
            if proc_count > 8 {
                return Err(InvalidTopology::TooManyCpusForGicV2(proc_count));
            }
        }
        if !(16..32).contains(&self.arch.platform.virt_timer_ppi) {
            return Err(InvalidTopology::InvalidPpiIntid(
                self.arch.platform.virt_timer_ppi,
            ));
        }
        if let Some(gsiv) = self.arch.platform.pmu_gsiv {
            if !(16..32).contains(&gsiv) {
                return Err(InvalidTopology::InvalidPpiIntid(gsiv));
            }
        }
        let nr = self.arch.platform.gic_nr_irqs;
        if !(64..=992).contains(&nr) || !nr.is_multiple_of(32) {
            return Err(InvalidTopology::InvalidGicNrIrqs(nr));
        }
        let mpidrs = (0..proc_count).map(|vp_index| {
            // TODO: construct mpidr appropriately for the specified
            // topology.
            let uni_proc = proc_count == 1;
            let mut aff = (0..4).map(|i| (vp_index >> (8 * i)) as u8);
            MpidrEl1::new()
                .with_res1_31(true)
                .with_u(uni_proc)
                .with_aff0(aff.next().unwrap())
                .with_aff1(aff.next().unwrap())
                .with_aff2(aff.next().unwrap())
                .with_aff3(aff.next().unwrap())
        });
        let gic_version = self.arch.platform.gic_version;
        self.build_with_vp_info(mpidrs.enumerate().map(move |(id, mpidr)| {
            // GICv3 assigns a per-VP redistributor region; GICv2 has no
            // redistributors so the field is zero.
            let gicr = match gic_version {
                GicVersion::V3 {
                    redistributors_base,
                } => Some(redistributors_base + id as u64 * aarch64defs::GIC_REDISTRIBUTOR_SIZE),
                GicVersion::V2 { .. } => None,
            };
            Aarch64VpInfo {
                base: VpInfo {
                    vp_index: VpIndex::new(id as u32),
                    vnode: 0,
                },
                mpidr,
                gicr,
                pmu_gsiv: self.arch.platform.pmu_gsiv,
            }
        }))
    }

    /// Builds a processor topology with processors with the specified information.
    pub fn build_with_vp_info(
        &self,
        vps: impl IntoIterator<Item = Aarch64VpInfo>,
    ) -> Result<ProcessorTopology<Aarch64Topology>, InvalidTopology> {
        let vps = Vec::from_iter(vps);
        let mut smt_enabled = false;
        for (i, vp) in vps.iter().enumerate() {
            if i != vp.base.vp_index.index() as usize {
                return Err(InvalidTopology::InvalidVpIndices);
            }

            if vp.mpidr.mt() {
                smt_enabled = true;
            }
        }

        Ok(ProcessorTopology {
            vps,
            smt_enabled,
            vps_per_socket: self.vps_per_socket,
            arch: Aarch64Topology {
                platform: self.arch.platform,
            },
        })
    }
}

impl ProcessorTopology<Aarch64Topology> {
    /// Returns the GIC version and version-specific addresses.
    pub fn gic_version(&self) -> GicVersion {
        self.arch.platform.gic_version
    }

    /// Returns the GIC distributor base
    pub fn gic_distributor_base(&self) -> u64 {
        self.arch.platform.gic_distributor_base
    }

    /// Returns the PMU GSIV
    pub fn pmu_gsiv(&self) -> Option<u32> {
        self.arch.platform.pmu_gsiv
    }

    /// Returns the MSI controller configuration.
    pub fn gic_msi(&self) -> GicMsiController {
        self.arch.platform.gic_msi
    }

    /// Returns the virtual timer PPI (GIC INTID).
    pub fn virt_timer_ppi(&self) -> u32 {
        self.arch.platform.virt_timer_ppi
    }

    /// Returns the total number of GIC interrupts to configure.
    pub fn gic_nr_irqs(&self) -> u32 {
        self.arch.platform.gic_nr_irqs
    }
}
