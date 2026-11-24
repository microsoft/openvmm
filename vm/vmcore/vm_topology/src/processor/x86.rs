// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! X86-specific topology definitions.

use super::ArchTopology;
use super::HostTopologyError;
use super::InvalidTopology;
use super::MAX_VPS_PER_SOCKET;
use super::ProcessorTopology;
use super::TopologyBuilder;
use super::VpIndex;
use super::VpInfo;
use super::VpTopologyInfo;
use x86defs::apic::APIC_LEGACY_ID_COUNT;
use x86defs::cpuid::CacheParametersEax;
use x86defs::cpuid::CpuidFunction;
use x86defs::cpuid::ExtendedAddressSpaceSizesEcx;
use x86defs::cpuid::ExtendedTopologyEax;
use x86defs::cpuid::ExtendedTopologyEcx;
use x86defs::cpuid::ProcessorTopologyDefinitionEbx;
use x86defs::cpuid::TopologyLevelType;
use x86defs::cpuid::Vendor;

/// X86-specific topology information.
#[cfg_attr(feature = "inspect", derive(inspect::Inspect))]
#[derive(Debug, Copy, Clone)]
pub struct X86Topology {
    apic_mode: ApicMode,
}

impl ArchTopology for X86Topology {
    type ArchVpInfo = X86VpInfo;
    type BuilderState = X86TopologyBuilderState;

    fn vp_topology(topology: &ProcessorTopology<Self>, info: &Self::ArchVpInfo) -> VpTopologyInfo {
        VpTopologyInfo {
            socket: info.apic_id / topology.reserved_vps_per_socket(),
            core: info.apic_id % topology.reserved_vps_per_socket(),
            thread: if topology.smt_enabled() {
                info.apic_id & 1
            } else {
                0
            },
        }
    }
}

/// X86-specific [`TopologyBuilder`] state.
pub struct X86TopologyBuilderState {
    apic_id_offset: u32,
    x2apic: X2ApicState,
}

impl Default for X86TopologyBuilderState {
    fn default() -> Self {
        Self {
            apic_id_offset: 0,
            x2apic: X2ApicState::Supported,
        }
    }
}

/// X2APIC configuration.
#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub enum X2ApicState {
    /// Support the X2APIC, and automatically enable it if needed to address all
    /// processors.
    Supported,
    /// Do not support the X2APIC.
    Unsupported,
    /// Support and enable the X2APIC.
    Enabled,
}

impl TopologyBuilder<X86Topology> {
    /// Returns a builder for creating an x86 processor topology.
    pub fn new_x86() -> Self {
        Self {
            vps_per_socket: 1,
            smt_enabled: false,
            arch: Default::default(),
        }
    }

    /// Returns a builder initialized from host information (via CPUID).
    ///
    /// Note that this only queries SMT state and the socket size, it does not
    /// otherwise affect APIC configuration.
    #[cfg(target_arch = "x86_64")] // xtask-fmt allow-target-arch cpu-intrinsic
    pub fn from_host_topology() -> Result<Self, HostTopologyError> {
        fn cpuid(leaf: u32, sub_leaf: u32) -> [u32; 4] {
            let result = safe_intrinsics::cpuid(leaf, sub_leaf);
            [result.eax, result.ebx, result.ecx, result.edx]
        }

        Self::from_cpuid(&mut cpuid)
    }

    /// Returns a builder initialized from cpuid information.
    ///
    /// Note that this only queries SMT state and the socket size, it does not
    /// otherwise affect APIC configuration.
    ///
    /// Fails with [`HostTopologyError::UnsupportedVpsPerSocket`] when the
    /// socket size is outside the range of [`Self::vps_per_socket`], rather
    /// than clamping it, so that the topology never silently differs from the
    /// host's.
    pub fn from_cpuid(
        cpuid: &mut dyn FnMut(u32, u32) -> [u32; 4],
    ) -> Result<Self, HostTopologyError> {
        let mut threads_per_core = 1u32;
        let mut vps_per_socket = 1u32;
        let mut found_topology = false;

        let [max_function, vendor_ebx, vendor_ecx, vendor_edx] =
            cpuid(CpuidFunction::VendorAndMaxFunction.0, 0);
        let vendor = Vendor::from_ebx_ecx_edx(vendor_ebx, vendor_ecx, vendor_edx);

        // Try to get topology from leaf 0Bh.
        if max_function >= CpuidFunction::ExtendedTopologyEnumeration.0 {
            for i in 0..=255 {
                let [eax, _ebx, ecx, _edx] = cpuid(CpuidFunction::ExtendedTopologyEnumeration.0, i);
                let shift = ExtendedTopologyEax::from(eax).x2_apic_shift();
                match TopologyLevelType(ExtendedTopologyEcx::from(ecx).level_type()) {
                    TopologyLevelType::INVALID => break,
                    TopologyLevelType::SMT => threads_per_core = 1 << shift,
                    TopologyLevelType::CORE => vps_per_socket = 1 << shift,
                    _ => {}
                }
                found_topology = true;
            }
        }

        // For AMD, try leaf 0x80000008 and 0x8000001e.
        if !found_topology && vendor.is_amd_compatible() {
            let extended_max_function = cpuid(CpuidFunction::ExtendedMaxFunction.0, 0)[0];
            if extended_max_function >= CpuidFunction::ExtendedAddressSpaceSizes.0 {
                let [_eax, _ebx, ecx, _edx] = cpuid(CpuidFunction::ExtendedAddressSpaceSizes.0, 0);
                let ecx = ExtendedAddressSpaceSizesEcx::from(ecx);

                vps_per_socket = if ecx.apic_core_id_size() != 0 {
                    1 << ecx.apic_core_id_size()
                } else {
                    ecx.nc() as u32 + 1
                };

                if extended_max_function >= CpuidFunction::ProcessorTopologyDefinition.0 {
                    let [_eax, ebx, _ecx, _edx] =
                        cpuid(CpuidFunction::ProcessorTopologyDefinition.0, 0);
                    let ebx = ProcessorTopologyDefinitionEbx::from(ebx);
                    threads_per_core = ebx.threads_per_compute_unit().max(1).into();
                    vps_per_socket /= threads_per_core;
                }

                found_topology = true;
            }
        }

        // Try to get topology from leaf 04h.
        if !found_topology {
            for i in 0..=255 {
                let [eax, _ebx, _ecx, _edx] = cpuid(CpuidFunction::CacheParameters.0, i);
                if eax == 0 {
                    break;
                }
                let eax = CacheParametersEax::from(eax);
                if eax.cache_level() == 1 {
                    found_topology = true;
                    threads_per_core = eax.threads_sharing_cache_minus_one() + 1;
                    vps_per_socket = (eax.cores_per_socket_minus_one() + 1) * threads_per_core;
                    break;
                }
            }
        }

        if !found_topology {
            return Err(HostTopologyError::NotFound);
        }

        if threads_per_core > 2 {
            return Err(HostTopologyError::UnsupportedThreadsPerCore(
                threads_per_core,
            ));
        }

        if !(1..=MAX_VPS_PER_SOCKET).contains(&vps_per_socket) {
            return Err(HostTopologyError::UnsupportedVpsPerSocket(vps_per_socket));
        }

        Ok(Self {
            smt_enabled: threads_per_core > 1 && vps_per_socket > 1,
            vps_per_socket,
            arch: Default::default(),
        })
    }

    /// Sets the APIC ID offset. Each APIC ID will be offset by this value,
    /// rounded up to the socket size.
    pub fn apic_id_offset(&mut self, offset: u32) -> &mut Self {
        self.arch.apic_id_offset = offset;
        self
    }

    /// Sets the X2APIC configuration.
    pub fn x2apic(&mut self, x2apic: X2ApicState) -> &mut Self {
        self.arch.x2apic = x2apic;
        self
    }

    /// Builds a processor topology with `proc_count` processors.
    pub fn build(
        &self,
        proc_count: u32,
    ) -> Result<ProcessorTopology<X86Topology>, InvalidTopology> {
        let vps_per_socket = self.vps_per_socket.next_power_of_two();
        let socket_offset = self.arch.apic_id_offset / vps_per_socket;
        let vps = (0..proc_count).map(|n| {
            let vp_index = VpIndex::new(n);
            // Default vnode assignment by socket. The caller can override this
            // with `ProcessorTopology::set_vnodes()` for NUMA configurations.
            let vnode = n / self.vps_per_socket;
            let socket = socket_offset + n / self.vps_per_socket;
            let proc = n % self.vps_per_socket;
            let apic_id = socket * vps_per_socket + proc;
            X86VpInfo {
                base: VpInfo { vp_index, vnode },
                apic_id,
            }
        });

        self.build_with_vp_info(vps)
    }

    /// Builds a processor topology with processors with the specified information.
    pub fn build_with_vp_info(
        &self,
        vps: impl IntoIterator<Item = X86VpInfo>,
    ) -> Result<ProcessorTopology<X86Topology>, InvalidTopology> {
        let vps = Vec::from_iter(vps);

        if vps
            .iter()
            .enumerate()
            .any(|(i, vp)| i != vp.base.vp_index.index() as usize)
        {
            return Err(InvalidTopology::InvalidVpIndices);
        }

        let max_apic_id = vps
            .iter()
            .map(|x| x.apic_id)
            .max()
            .ok_or(InvalidTopology::NoVps)?;
        let apic_mode = match self.arch.x2apic {
            X2ApicState::Supported => {
                if max_apic_id >= APIC_LEGACY_ID_COUNT {
                    ApicMode::X2ApicEnabled
                } else {
                    ApicMode::X2ApicSupported
                }
            }
            X2ApicState::Unsupported => {
                if max_apic_id >= APIC_LEGACY_ID_COUNT {
                    return Err(InvalidTopology::ApicIdLimitExceeded(max_apic_id));
                }
                ApicMode::XApic
            }
            X2ApicState::Enabled => ApicMode::X2ApicEnabled,
        };
        Ok(ProcessorTopology {
            vps,
            smt_enabled: self.smt_enabled && self.vps_per_socket > 1,
            vps_per_socket: self.vps_per_socket,
            arch: X86Topology { apic_mode },
        })
    }
}

impl ProcessorTopology<X86Topology> {
    /// Returns the largest APIC ID in use.
    pub fn max_apic_id(&self) -> u32 {
        self.vps.iter().map(|x| x.apic_id).max().unwrap_or(0)
    }

    /// Returns the APIC mode configured for the processors.
    pub fn apic_mode(&self) -> ApicMode {
        self.arch.apic_mode
    }
}

/// x86-specific VP info.
#[cfg_attr(feature = "inspect", derive(inspect::Inspect))]
#[derive(Debug, Copy, Clone)]
pub struct X86VpInfo {
    /// The base info.
    #[cfg_attr(feature = "inspect", inspect(flatten))]
    pub base: VpInfo,
    /// The APIC ID of the processor.
    pub apic_id: u32,
}

impl AsRef<VpInfo> for X86VpInfo {
    fn as_ref(&self) -> &VpInfo {
        &self.base
    }
}

impl AsMut<VpInfo> for X86VpInfo {
    fn as_mut(&mut self) -> &mut VpInfo {
        &mut self.base
    }
}

/// The APIC mode for the virtual processors.
#[derive(Debug, Copy, Clone, PartialEq, Eq)]
#[cfg_attr(feature = "inspect", derive(inspect::Inspect))]
pub enum ApicMode {
    /// xAPIC mode.
    #[cfg_attr(feature = "inspect", inspect(rename = "xapic"))]
    XApic,
    /// x2APIC mode supported but disabled at boot.
    #[cfg_attr(feature = "inspect", inspect(rename = "x2apic_supported"))]
    X2ApicSupported,
    /// x2APIC mode supported and enabled at boot.
    #[cfg_attr(feature = "inspect", inspect(rename = "x2apic_enabled"))]
    X2ApicEnabled,
}

#[cfg(test)]
mod tests {
    use super::*;
    use test_with_tracing::test;

    /// Returns the CPUID of an Intel host whose leaf 0Bh reports two threads
    /// per core and `1 << core_shift` VPs per socket.
    fn intel_host(core_shift: u32) -> impl FnMut(u32, u32) -> [u32; 4] {
        move |leaf, subleaf| {
            let level = |shift, level_type: TopologyLevelType| {
                let eax = ExtendedTopologyEax::new().with_x2_apic_shift(shift);
                let ecx = ExtendedTopologyEcx::new()
                    .with_level_number(subleaf as u8)
                    .with_level_type(level_type.0);
                [eax.into(), 0, ecx.into(), 0]
            };
            match (CpuidFunction(leaf), subleaf) {
                (CpuidFunction::VendorAndMaxFunction, _) => {
                    let (ebx, ecx, edx) = Vendor::INTEL.to_ebx_ecx_edx();
                    [CpuidFunction::ExtendedTopologyEnumeration.0, ebx, ecx, edx]
                }
                (CpuidFunction::ExtendedTopologyEnumeration, 0) => level(1, TopologyLevelType::SMT),
                (CpuidFunction::ExtendedTopologyEnumeration, 1) => {
                    level(core_shift, TopologyLevelType::CORE)
                }
                _ => [0; 4],
            }
        }
    }

    /// Returns the CPUID of an AMD host without leaf 0Bh, whose leaf 80000008h
    /// counts `nc + 1` threads per socket and whose leaf 8000001Eh has a
    /// `ThreadsPerComputeUnit` of 2.
    fn amd_host(nc: u8) -> impl FnMut(u32, u32) -> [u32; 4] {
        move |leaf, _| match CpuidFunction(leaf) {
            CpuidFunction::VendorAndMaxFunction => {
                let (ebx, ecx, edx) = Vendor::AMD.to_ebx_ecx_edx();
                [
                    CpuidFunction::ExtendedTopologyEnumeration.0 - 1,
                    ebx,
                    ecx,
                    edx,
                ]
            }
            CpuidFunction::ExtendedMaxFunction => {
                [CpuidFunction::ProcessorTopologyDefinition.0, 0, 0, 0]
            }
            CpuidFunction::ExtendedAddressSpaceSizes => {
                let ecx = ExtendedAddressSpaceSizesEcx::new().with_nc(nc);
                [0, 0, ecx.into(), 0]
            }
            CpuidFunction::ProcessorTopologyDefinition => {
                let ebx = ProcessorTopologyDefinitionEbx::new().with_threads_per_compute_unit(2);
                [0, ebx.into(), 0, 0]
            }
            _ => [0; 4],
        }
    }

    /// Leaf 0Bh's core level can describe up to 2^31 VPs per socket, and the
    /// builder supports at most 32768.
    #[test]
    fn host_vps_per_socket_above_the_range_are_refused() {
        assert!(matches!(
            TopologyBuilder::from_cpuid(&mut intel_host(16)),
            Err(HostTopologyError::UnsupportedVpsPerSocket(65536))
        ));
        let topology = TopologyBuilder::from_cpuid(&mut intel_host(15))
            .unwrap()
            .build(1)
            .unwrap();
        assert_eq!(topology.vps_per_socket(), 32768);
    }

    /// Leaf 80000008h can count fewer threads per socket than a core has,
    /// which leaves no VPs in a socket.
    #[test]
    fn host_vps_per_socket_below_the_range_are_refused() {
        assert!(matches!(
            TopologyBuilder::from_cpuid(&mut amd_host(0)),
            Err(HostTopologyError::UnsupportedVpsPerSocket(0))
        ));
        let topology = TopologyBuilder::from_cpuid(&mut amd_host(1))
            .unwrap()
            .build(1)
            .unwrap();
        assert_eq!(topology.vps_per_socket(), 1);
    }
}
