// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! ARM64-specific topology definitions.

use super::ArchTopology;
use super::InvalidTopology;
use super::ProcessorTopology;
use super::THREADS_PER_CORE;
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

    fn vp_topology(topology: &ProcessorTopology<Self>, info: &Self::ArchVpInfo) -> VpTopologyInfo {
        // MPIDR is an identity, not a topology description: the non-SMT
        // encoding below packs the VP index without regard to sockets or
        // cores, and affinity levels only carry their conventional meanings
        // when MT is set. Ask the configured topology instead of decoding
        // affinity, accepting that the answer is only as good as that
        // configuration.
        topology.logical_topology(info.base.vp_index)
    }
}

/// Builds the MPIDR for a VP on a platform without SMT.
///
/// `Aff0` holds 16 VPs per affinity group because GICv3 targeted SGIs select
/// `Aff0` through a 16-bit target list, and reaching beyond that requires
/// Range Selector Support.
///
/// Note that this says nothing about sockets or cores. Logical topology is
/// reported separately, via [`ProcessorTopology::vp_topology`].
fn non_smt_mpidr(vp_index: u32) -> MpidrEl1 {
    MpidrEl1::new()
        .with_aff0((vp_index % AFF0_PER_GROUP) as u8)
        .with_aff1((vp_index / AFF0_PER_GROUP) as u8)
        .with_aff2((vp_index / (AFF0_PER_GROUP << 8)) as u8)
        .with_aff3((vp_index / (AFF0_PER_GROUP << 16)) as u8)
}

/// Builds the MPIDR for a VP on a platform with SMT.
///
/// `MPIDR.MT` is set, so `Aff0` is a thread id and the core index packs into
/// the fields above it. Sibling threads therefore share everything above
/// `Aff0`.
///
/// Like the non-SMT encoding, this describes no socket placement; that is
/// PPTT's job.
fn smt_mpidr(vp_index: u32) -> MpidrEl1 {
    let core = vp_index / THREADS_PER_CORE;
    MpidrEl1::new()
        .with_mt(true)
        .with_aff0((vp_index % THREADS_PER_CORE) as u8)
        .with_aff1(core as u8)
        .with_aff2((core >> 8) as u8)
        .with_aff3((core >> 16) as u8)
}

/// The number of `Aff0` values a GICv3 SGI can target in one affinity group
/// without Range Selector Support.
const AFF0_PER_GROUP: u32 = 16;

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
    /// Physical base address of the guest-visible v2m MSI frame.
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

impl AsMut<VpInfo> for Aarch64VpInfo {
    fn as_mut(&mut self) -> &mut VpInfo {
        &mut self.base
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
        let smt_enabled = self.effective_smt();
        let uni_proc = proc_count == 1;
        let mpidrs = (0..proc_count).map(|vp_index| {
            let mpidr = if smt_enabled {
                smt_mpidr(vp_index)
            } else {
                non_smt_mpidr(vp_index)
            };
            mpidr.with_res1_31(true).with_u(uni_proc)
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
                    vnode: id as u32 / self.vps_per_socket,
                },
                mpidr,
                gicr,
                pmu_gsiv: self.arch.platform.pmu_gsiv,
            }
        }))
    }

    /// Returns whether SMT applies to this configuration.
    ///
    /// A socket holding a single VP has no sibling to pair with, so requesting
    /// SMT there would claim a thread of a core that does not exist. x86 clamps
    /// the same way.
    fn effective_smt(&self) -> bool {
        self.smt_enabled && self.vps_per_socket > 1
    }

    /// Builds a processor topology with processors with the specified information.
    ///
    /// The MPIDRs are taken as given; they are not checked against the socket
    /// and SMT configuration, and nothing derives topology from them. Logical
    /// topology comes from `vps_per_socket` and `smt_enabled`, so a caller that
    /// supplies VPs arranged some other way will have that arrangement
    /// reported as whatever it declared through the builder.
    pub fn build_with_vp_info(
        &self,
        vps: impl IntoIterator<Item = Aarch64VpInfo>,
    ) -> Result<ProcessorTopology<Aarch64Topology>, InvalidTopology> {
        let vps = Vec::from_iter(vps);
        for (i, vp) in vps.iter().enumerate() {
            if i != vp.base.vp_index.index() as usize {
                return Err(InvalidTopology::InvalidVpIndices);
            }
        }

        Ok(ProcessorTopology {
            vps,
            smt_enabled: self.effective_smt(),
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

#[cfg(test)]
mod tests {
    use super::*;

    fn platform() -> Aarch64PlatformConfig {
        Aarch64PlatformConfig {
            gic_distributor_base: 0xffff0000,
            gic_version: GicVersion::V3 {
                redistributors_base: 0xefff0000,
            },
            gic_msi: GicMsiController::None,
            pmu_gsiv: None,
            virt_timer_ppi: 20,
            gic_nr_irqs: 992,
        }
    }

    fn builder() -> TopologyBuilder<Aarch64Topology> {
        TopologyBuilder::new_aarch64(platform())
    }

    /// Returns `(mpidr, socket, core, thread)` for each VP.
    fn describe(topology: &ProcessorTopology<Aarch64Topology>) -> Vec<(u64, u32, u32, u32)> {
        topology
            .vps_arch()
            .map(|vp| {
                let t = topology.vp_topology(vp.base.vp_index);
                (vp.mpidr.into(), t.socket, t.core, t.thread)
            })
            .collect()
    }

    /// Strips the RES1 and uniprocessor bits so tests can compare affinities.
    fn affinity(mpidr: u64) -> u64 {
        mpidr & (u64::from(MpidrEl1::AFFINITY_MASK) | 1 << 24)
    }

    /// A socket of one has no sibling to pair with, so SMT is dropped rather
    /// than claiming a thread of a core that does not exist.
    #[test]
    fn single_vp_ignores_smt() {
        let topology = builder().smt_enabled(true).build(1).unwrap();
        assert!(!topology.smt_enabled());
        let mpidr = topology.vp_arch(VpIndex::new(0)).mpidr;
        assert!(mpidr.u());
        assert!(!mpidr.mt());
        assert_eq!(describe(&topology), [(u64::from(mpidr), 0, 0, 0)]);
    }

    /// Aff0 holds 16 VPs, then rolls into Aff1. A GICv3 targeted SGI cannot
    /// reach an Aff0 above 15, which is what this boundary exists for.
    #[test]
    fn seventeen_vps_roll_into_aff1() {
        let topology = builder().vps_per_socket(17).build(17).unwrap();
        let vps = describe(&topology);
        assert_eq!(affinity(vps[15].0), 0x0f);
        assert_eq!(affinity(vps[16].0), 0x100);
        for (i, (mpidr, socket, core, thread)) in vps.into_iter().enumerate() {
            assert!(MpidrEl1::from(mpidr).aff0() < 16);
            assert_eq!((socket, core, thread), (0, i as u32, 0));
        }
    }

    /// Logical topology follows `vps_per_socket`; MPIDRs keep packing linearly
    /// and say nothing about sockets.
    #[test]
    fn multiple_sockets_without_smt() {
        let topology = builder().vps_per_socket(4).build(8).unwrap();
        let vps = describe(&topology);
        for (i, (mpidr, socket, core, thread)) in vps.iter().copied().enumerate() {
            assert_eq!(affinity(mpidr), i as u64);
            assert_eq!((socket, core, thread), (i as u32 / 4, i as u32 % 4, 0));
        }
        let vnodes: Vec<_> = topology.vps().map(|vp| vp.vnode).collect();
        assert_eq!(vnodes, [0, 0, 0, 0, 1, 1, 1, 1]);
    }

    /// The exact register values a guest sees, RES1 and MT bits included,
    /// rather than just the affinity fields.
    #[test]
    fn mpidr_register_values() {
        for (vp, expected) in [
            (0, 0x8000_0000),
            (1, 0x8000_0001),
            (15, 0x8000_000f),
            (16, 0x8000_0100),
            (17, 0x8000_0101),
        ] {
            assert_eq!(
                u64::from(non_smt_mpidr(vp).with_res1_31(true)),
                expected,
                "non-SMT VP {vp}"
            );
        }

        for (vp, expected) in [
            (0, 0x8100_0000),
            (1, 0x8100_0001),
            (2, 0x8100_0100),
            (3, 0x8100_0101),
        ] {
            assert_eq!(
                u64::from(smt_mpidr(vp).with_res1_31(true)),
                expected,
                "SMT VP {vp}"
            );
        }
    }

    /// Sockets do not appear in the MPIDR: the core index keeps packing past
    /// the socket boundary, and only the logical topology splits there.
    #[test]
    fn smt_sockets_do_not_change_affinity() {
        let topology = builder()
            .vps_per_socket(2)
            .smt_enabled(true)
            .build(4)
            .unwrap();
        assert!(topology.smt_enabled());
        assert_eq!(
            describe(&topology)
                .into_iter()
                .map(|(mpidr, socket, core, thread)| (affinity(mpidr), socket, core, thread))
                .collect::<Vec<_>>(),
            [
                (0x0100_0000, 0, 0, 0),
                (0x0100_0001, 0, 0, 1),
                (0x0100_0100, 1, 0, 0),
                (0x0100_0101, 1, 0, 1),
            ]
        );
    }

    /// An odd socket size just leaves the last core with one thread. The MPIDRs
    /// stay unique, so there is nothing to reject.
    ///
    /// MPIDR pairs threads across the whole VM while the logical topology pairs
    /// them within a socket, so the two disagree once a socket holds an odd
    /// number of VPs. PPTT is what describes topology, so that is tolerable.
    #[test]
    fn odd_socket_size_under_smt() {
        let topology = builder()
            .vps_per_socket(3)
            .smt_enabled(true)
            .build(3)
            .unwrap();
        assert_eq!(
            describe(&topology)
                .into_iter()
                .map(|(_, socket, core, thread)| (socket, core, thread))
                .collect::<Vec<_>>(),
            [(0, 0, 0), (0, 0, 1), (0, 1, 0)]
        );
    }

    /// OpenHCL passes host MPIDRs through unmodified, including ones this code
    /// would never generate.
    #[test]
    fn caller_supplied_mpidrs_are_preserved() {
        let mpidrs = [0x81, 0x40, 0x0];
        let topology = builder()
            .vps_per_socket(3)
            .build_with_vp_info(mpidrs.iter().enumerate().map(|(i, &mpidr)| Aarch64VpInfo {
                base: VpInfo {
                    vp_index: VpIndex::new(i as u32),
                    vnode: 0,
                },
                mpidr: MpidrEl1::from(mpidr),
                gicr: None,
                pmu_gsiv: None,
            }))
            .unwrap();

        assert_eq!(
            topology
                .vps_arch()
                .map(|vp| u64::from(vp.mpidr))
                .collect::<Vec<_>>(),
            mpidrs
        );
        // Topology still comes from the VP index, not from the odd affinities.
        assert_eq!(
            describe(&topology)
                .into_iter()
                .map(|(_, socket, core, thread)| (socket, core, thread))
                .collect::<Vec<_>>(),
            [(0, 0, 0), (0, 1, 0), (0, 2, 0)]
        );
    }

    #[test]
    fn gicv2_vp_limit() {
        let gicv2 = || {
            TopologyBuilder::new_aarch64(Aarch64PlatformConfig {
                gic_version: GicVersion::V2 {
                    cpu_interface_base: 0xefff0000,
                },
                ..platform()
            })
        };
        assert!(gicv2().vps_per_socket(8).build(8).is_ok());
        assert!(matches!(
            gicv2().vps_per_socket(9).build(9),
            Err(InvalidTopology::TooManyCpusForGicV2(9))
        ));
    }
}
