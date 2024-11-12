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
    gic: GicInfo,
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
    gic: GicInfo,
}

/// GIC information
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg_attr(feature = "inspect", derive(inspect::Inspect))]
pub struct GicInfo {
    /// GIC distributor base
    #[cfg_attr(feature = "inspect", inspect(hex))]
    pub gic_distributor_base: u64,
    /// GIC redistributors base
    #[cfg_attr(feature = "inspect", inspect(hex))]
    pub gic_redistributors_base: u64,
}

/// ARM64 specific VP info.
#[cfg_attr(feature = "inspect", derive(inspect::Inspect))]
#[derive(Debug, Copy, Clone)]
pub struct Aarch64VpInfo {
    /// The base info.
    #[cfg_attr(feature = "inspect", inspect(flatten))]
    pub base: VpInfo,
    /// The MPIDR_EL1 value of the processor.
    #[cfg_attr(
        feature = "inspect",
        inspect(with = "|&x| inspect::AsHex(u64::from(x))")
    )]
    pub mpidr: MpidrEl1,
    /// GIC Redistributor Address
    #[cfg_attr(feature = "inspect", inspect(hex))]
    pub gicr: u64,
}

impl AsRef<VpInfo> for Aarch64VpInfo {
    fn as_ref(&self) -> &VpInfo {
        &self.base
    }
}

impl TopologyBuilder<Aarch64Topology> {
    /// Returns a builder for creating an x86 processor topology.
    pub fn new_aarch64(gic: GicInfo) -> Self {
        Self {
            vps_per_socket: 1,
            smt_enabled: false,
            arch: Aarch64TopologyBuilderState { gic },
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
        self.build_with_vp_info(mpidrs.enumerate().map(|(id, mpidr)| Aarch64VpInfo {
            base: VpInfo {
                vp_index: VpIndex::new(id as u32),
                vnode: 0,
            },
            mpidr,
            gicr: self.arch.gic.gic_redistributors_base
                + id as u64 * aarch64defs::GIC_REDISTRIBUTOR_SIZE,
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
            arch: Aarch64Topology { gic: self.arch.gic },
        })
    }
}

impl ProcessorTopology<Aarch64Topology> {
    /// Returns the GIC distributor base
    pub fn gic_distributor_base(&self) -> u64 {
        self.arch.gic.gic_distributor_base
    }

    /// Returns the GIC redistributors base
    pub fn gic_redistributors_base(&self) -> u64 {
        self.arch.gic.gic_redistributors_base
    }
}
