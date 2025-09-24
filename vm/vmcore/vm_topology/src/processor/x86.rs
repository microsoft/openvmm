// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! X86-specific topology definitions.

use super::ArchTopology;
use super::HostTopologyError;
use super::InvalidTopology;
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
            // FUTURE: support multiple NUMA nodes per socket.
            let vnode = n / vps_per_socket;
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
