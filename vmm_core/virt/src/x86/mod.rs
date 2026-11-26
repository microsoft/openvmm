// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! x86-specific state.

pub mod apic_software_device;
pub mod snp;
pub mod topology;
pub mod vm;
pub mod vp;

use crate::state::StateElement;
use inspect::Inspect;
use mesh_protobuf::Protobuf;
use std::fmt::Debug;
use thiserror::Error;
use vm_topology::processor::ProcessorTopology;
use vm_topology::processor::x86::ApicMode;
use vm_topology::processor::x86::X86Topology;
use vm_topology::processor::x86::X86VpInfo;
use x86defs::cpuid::CpuidFunction;
use x86defs::cpuid::ExtendedSevFeaturesEax;
use x86defs::cpuid::ExtendedSevFeaturesEbx;
use x86defs::cpuid::SgxCpuidSubleafEax;
use x86defs::cpuid::Vendor;
use x86defs::xsave::XCOMP_COMPRESSED;
use x86defs::xsave::XSAVE_VARIABLE_OFFSET;

/// VP state that can be set for initial boot.
#[derive(Debug, PartialEq, Eq, Protobuf)]
pub struct X86InitialRegs {
    /// Register state to be set on the BSP.
    pub registers: vp::Registers,
    /// MTRR state to be set on all processors.
    pub mtrrs: vp::Mtrrs,
    /// PAT state to be set on all processors.
    pub pat: vp::Pat,
}

impl X86InitialRegs {
    pub fn at_reset(caps: &X86PartitionCapabilities, bsp: &X86VpInfo) -> Self {
        Self {
            registers: vp::Registers::at_reset(caps, bsp),
            mtrrs: vp::Mtrrs::at_reset(caps, bsp),
            pat: vp::Pat::at_reset(caps, bsp),
        }
    }
}

/// Partition capabilities, used to determine which state is active on a
/// partition and what the reset state should be.
#[derive(Debug, Inspect)]
pub struct X86PartitionCapabilities {
    /// The processor vendor.
    #[inspect(display)]
    pub vendor: Vendor,
    /// The MS hypervisor is available.
    pub hv1: bool,
    /// The reference TSC page is available.
    pub hv1_reference_tsc_page: bool,
    /// Xsave information.
    pub xsave: XsaveCapabilities,
    /// X2apic is supported.
    pub x2apic: bool,
    /// X2apic is enabled at boot.
    pub x2apic_enabled: bool,
    /// The initial value for rdx.
    #[inspect(hex)]
    pub reset_rdx: u64,
    /// CET is supported.
    pub cet: bool,
    /// CET-SS is supported.
    pub cet_ss: bool,
    /// SGX is enabled.
    pub sgx: bool,
    /// TSC_AUX is supported
    pub tsc_aux: bool,
    /// Architectural TSC-deadline state is available. Backends without access
    /// to this MSR must clear this even if guest CPUID advertises the feature.
    pub tsc_deadline: bool,
    /// The address of the virtual top of memory, for encrypted VMs.
    ///
    /// This is computed from the Hyper-V isolation leaf. It is guaranteed to be
    /// a power of 2, if present.
    #[inspect(hex)]
    pub vtom: Option<u64>,
    /// The physical address width of the CPU, as reported by CPUID.
    pub physical_address_width: u8,
    /// The page-table bit that marks private memory for SNP, if supported.
    pub snp_c_bit: Option<u8>,

    /// The hypervisor can freeze time across state manipulation.
    pub can_freeze_time: bool,
    /// The hypervisor has a broken implementation querying xsave state, where
    /// supervisor states are not correctly set in xstate_bv.
    pub xsaves_state_bv_broken: bool,
    /// The hypervisor has a broken implementation setting dr6, where bit 16 is
    /// forced on even if the processor supports TSX.
    pub dr6_tsx_broken: bool,
    /// EFER.NXE is forced on. This is set for TDX 1.5 partitions, which require
    /// this.
    pub nxe_forced_on: bool,
    /// Nested virtualization is enabled for this partition.
    pub nested_virt: bool,
}

#[derive(Error, Debug)]
pub enum X86PartitionCapabilitiesError {
    #[error(
        "advertised xsave length ({advertised}) too small for features, requires ({required}) bytes"
    )]
    XSaveLengthTooSmall { advertised: u32, required: u32 },
    #[error("x2apic topology and cpuid mismatch, expected x2apic={expected}, found {found}")]
    X2ApicMismatch { expected: bool, found: bool },
}

impl X86PartitionCapabilities {
    pub fn from_cpuid(
        processor_topology: &ProcessorTopology<X86Topology>,
        f: &mut dyn FnMut(u32, u32) -> [u32; 4],
    ) -> Result<Self, X86PartitionCapabilitiesError> {
        let mut this = Self {
            vendor: Vendor([0; 12]),
            hv1: false,
            hv1_reference_tsc_page: false,
            xsave: XsaveCapabilities {
                features: 0,
                supervisor_features: 0,
                standard_len: XSAVE_VARIABLE_OFFSET as u32,
                compact_len: XSAVE_VARIABLE_OFFSET as u32,
                feature_info: [Default::default(); 63],
            },
            x2apic: false,
            x2apic_enabled: false,
            reset_rdx: 0,
            cet: false,
            cet_ss: false,
            sgx: false,
            tsc_aux: false,
            tsc_deadline: false,
            vtom: None,
            physical_address_width: max_physical_address_size_from_cpuid(&mut *f),
            snp_c_bit: snp_c_bit_from_cpuid(&mut *f),
            can_freeze_time: false,
            xsaves_state_bv_broken: false,
            dr6_tsx_broken: false,
            nxe_forced_on: false,
            nested_virt: false,
        };

        let max_function = {
            let [eax, ebx, ecx, edx] = f(CpuidFunction::VendorAndMaxFunction.0, 0);
            this.vendor = Vendor::from_ebx_ecx_edx(ebx, ecx, edx);
            eax
        };

        let mut hypervisor = false;
        let mut xsave = false;
        if max_function >= CpuidFunction::VersionAndFeatures.0 {
            let result = f(CpuidFunction::VersionAndFeatures.0, 0);
            this.reset_rdx = result[0].into();
            let features = result[2] as u64 | ((result[3] as u64) << 32);
            this.x2apic = features & (1 << 21) != 0;
            this.tsc_deadline = features & (1 << 24) != 0;
            xsave = features & (1 << 26) != 0;
            hypervisor = features & (1 << 31) != 0;
        }

        let extended_features = if max_function >= CpuidFunction::ExtendedFeatures.0 {
            f(CpuidFunction::ExtendedFeatures.0, 0)
        } else {
            Default::default()
        };

        if max_function >= CpuidFunction::ExtendedFeatures.0 {
            if extended_features[2] & (1 << 7) != 0 {
                this.cet = true;
                this.cet_ss = true;
            }
            if extended_features[3] & (1 << 20) != 0 {
                this.cet = true;
            }
        }

        if max_function >= CpuidFunction::SgxEnumeration.0 {
            let sgx_result: SgxCpuidSubleafEax =
                SgxCpuidSubleafEax::from(f(CpuidFunction::SgxEnumeration.0, 2)[0]);
            if sgx_result.sgx_type() != 0 {
                this.sgx = true;
            }
        }

        if xsave {
            let result = f(CpuidFunction::ExtendedStateEnumeration.0, 0);
            this.xsave.features = result[0] as u64 | ((result[3] as u64) << 32);
            let standard_len = result[2];

            let result = f(CpuidFunction::ExtendedStateEnumeration.0, 1);
            this.xsave.supervisor_features = result[2] as u64 | ((result[3] as u64) << 32);

            let mut n = (this.xsave.features | this.xsave.supervisor_features) & !3;
            while let Some(i) = n.lowest_one() {
                n -= 1 << i;
                let result = f(CpuidFunction::ExtendedStateEnumeration.0, i);
                let feature = XsaveFeature {
                    offset: result[1],
                    len: result[0],
                    align: result[2] & 2 != 0,
                };
                this.xsave.feature_info[i as usize] = feature;
            }
            this.xsave.compact_len = this.xsave.compact_len_for(!0);
            this.xsave.standard_len = this.xsave.standard_len_for(!0);

            if this.xsave.standard_len > standard_len {
                return Err(X86PartitionCapabilitiesError::XSaveLengthTooSmall {
                    advertised: standard_len,
                    required: this.xsave.standard_len,
                });
            }
        }

        // Hypervisor info.
        if hypervisor {
            let hv_max = f(hvdef::HV_CPUID_FUNCTION_HV_VENDOR_AND_MAX_FUNCTION, 0)[0];
            if hv_max >= hvdef::HV_CPUID_FUNCTION_MS_HV_ENLIGHTENMENT_INFORMATION
                && f(hvdef::HV_CPUID_FUNCTION_HV_INTERFACE, 0)[0] == u32::from_le_bytes(*b"Hv#1")
            {
                this.hv1 = true;
                let result = f(hvdef::HV_CPUID_FUNCTION_MS_HV_FEATURES, 0);
                let privs = hvdef::HvPartitionPrivilege::from(
                    result[0] as u64 | ((result[1] as u64) << 32),
                );
                this.hv1_reference_tsc_page = privs.access_partition_reference_tsc();
                if privs.isolation()
                    && hv_max >= hvdef::HV_CPUID_FUNCTION_MS_HV_ISOLATION_CONFIGURATION
                {
                    let [eax, ebx, ecx, edx] =
                        f(hvdef::HV_CPUID_FUNCTION_MS_HV_ISOLATION_CONFIGURATION, 0);
                    let config = hvdef::HvIsolationConfiguration::from(
                        eax as u128
                            | ((ebx as u128) << 32)
                            | ((ecx as u128) << 64)
                            | ((edx as u128) << 96),
                    );
                    if config.shared_gpa_boundary_active() {
                        this.vtom = Some(1 << config.shared_gpa_boundary_bits());
                    }
                }
            }
        }

        match (processor_topology.apic_mode(), this.x2apic) {
            (ApicMode::XApic, true) => {
                return Err(X86PartitionCapabilitiesError::X2ApicMismatch {
                    expected: false,
                    found: true,
                });
            }
            (ApicMode::X2ApicSupported | ApicMode::X2ApicEnabled, false) => {
                return Err(X86PartitionCapabilitiesError::X2ApicMismatch {
                    expected: true,
                    found: false,
                });
            }
            (ApicMode::XApic, false) | (ApicMode::X2ApicSupported, true) => {}
            (ApicMode::X2ApicEnabled, true) => {
                this.x2apic_enabled = true;
            }
        }

        this.tsc_aux = {
            let rdtscp = {
                let extended_max_function = f(CpuidFunction::ExtendedMaxFunction.0, 0)[0];
                if extended_max_function >= CpuidFunction::ExtendedVersionAndFeatures.0 {
                    x86defs::cpuid::ExtendedVersionAndFeaturesEdx::from(
                        f(CpuidFunction::ExtendedVersionAndFeatures.0, 0)[3],
                    )
                    .rdtscp()
                } else {
                    false
                }
            };

            let rdpid =
                x86defs::cpuid::ExtendedFeatureSubleaf0Ecx::from(extended_features[2]).rd_pid();

            rdtscp || rdpid
        };

        Ok(this)
    }
}

#[derive(Debug, Copy, Clone, Inspect)]
pub struct XsaveCapabilities {
    pub features: u64,
    pub supervisor_features: u64,
    pub standard_len: u32,
    pub compact_len: u32,
    #[inspect(skip)] // TODO
    pub feature_info: [XsaveFeature; 63],
}

#[derive(Default, Debug, Copy, Clone)]
pub struct XsaveFeature {
    pub offset: u32,
    pub len: u32,
    pub align: bool,
}

impl XsaveCapabilities {
    fn variable_features(&self, mask: u64) -> impl Iterator<Item = (u32, &XsaveFeature)> {
        // Bits 0-1 belong to the legacy area; bit 63 marks compact format.
        let mut mask = mask & !(3 | XCOMP_COMPRESSED);
        std::iter::from_fn(move || {
            let i = mask.lowest_one()?;
            mask &= !(1 << i);
            Some((i, &self.feature_info[i as usize]))
        })
    }

    pub fn standard_len_for(&self, xfem: u64) -> u32 {
        let mut len = XSAVE_VARIABLE_OFFSET as u32;
        for (_, feature) in self.variable_features(xfem) {
            len = len.max(feature.offset + feature.len);
        }
        len
    }

    pub fn compact_len_for(&self, xfem: u64) -> u32 {
        let mut len = XSAVE_VARIABLE_OFFSET as u32;
        for (_, feature) in self.variable_features(xfem) {
            if feature.align {
                len = (len + 63) & !63;
            }
            len += feature.len;
        }
        len
    }
}

#[derive(Copy, Clone, Debug, Default, PartialEq, Eq, Protobuf, Inspect)]
#[mesh(package = "virt.x86")]
pub struct TableRegister {
    #[inspect(hex)]
    #[mesh(1)]
    pub base: u64,
    #[inspect(hex)]
    #[mesh(2)]
    pub limit: u16,
}

impl From<hvdef::HvX64TableRegister> for TableRegister {
    fn from(table: hvdef::HvX64TableRegister) -> Self {
        Self {
            base: table.base,
            limit: table.limit,
        }
    }
}

impl From<TableRegister> for hvdef::HvX64TableRegister {
    fn from(table: TableRegister) -> Self {
        Self {
            base: table.base,
            limit: table.limit,
            pad: [0; 3],
        }
    }
}

#[derive(Copy, Clone, Debug, Default, PartialEq, Eq, Protobuf, Inspect)]
#[mesh(package = "virt.x86")]
pub struct SegmentRegister {
    #[inspect(hex)]
    #[mesh(1)]
    pub base: u64,
    #[inspect(hex)]
    #[mesh(2)]
    pub limit: u32,
    #[inspect(hex)]
    #[mesh(3)]
    pub selector: u16,
    #[inspect(hex)]
    #[mesh(4)]
    pub attributes: u16,
}

impl From<x86defs::SegmentRegister> for SegmentRegister {
    fn from(seg: x86defs::SegmentRegister) -> Self {
        Self {
            base: seg.base,
            limit: seg.limit,
            selector: seg.selector,
            attributes: seg.attributes.into(),
        }
    }
}

impl From<SegmentRegister> for x86defs::SegmentRegister {
    fn from(seg: SegmentRegister) -> Self {
        Self {
            base: seg.base,
            limit: seg.limit,
            selector: seg.selector,
            attributes: seg.attributes.into(),
        }
    }
}

impl From<hvdef::HvX64SegmentRegister> for SegmentRegister {
    fn from(seg: hvdef::HvX64SegmentRegister) -> Self {
        Self {
            base: seg.base,
            limit: seg.limit,
            selector: seg.selector,
            attributes: seg.attributes,
        }
    }
}

impl From<SegmentRegister> for hvdef::HvX64SegmentRegister {
    fn from(seg: SegmentRegister) -> Self {
        Self {
            base: seg.base,
            limit: seg.limit,
            selector: seg.selector,
            attributes: seg.attributes,
        }
    }
}

/// Guest debugging state, for gdbstub or similar use cases.
#[derive(Debug, Copy, Clone, Protobuf)]
pub struct DebugState {
    /// Single step the VP.
    pub single_step: bool,
    /// Hardware breakpoints/watchpoints.
    pub breakpoints: [Option<HardwareBreakpoint>; 4],
}

#[derive(Debug, Copy, Clone, Protobuf, PartialEq, Eq)]
pub struct HardwareBreakpoint {
    /// The address to watch.
    pub address: u64,
    /// The breakpoint type.
    pub ty: BreakpointType,
    /// The size of the memory location to watch.
    pub size: BreakpointSize,
}

impl HardwareBreakpoint {
    /// Parses the hardware breakpoint from DR7, the address of the breakpoint,
    /// and the debug register index (0-3).
    pub fn from_dr7(dr7: u64, address: u64, reg: usize) -> Self {
        let v = dr7 >> (16 + reg * 4);
        let ty = match v & 3 {
            0 => BreakpointType::Execute,
            1 => BreakpointType::Invalid,
            2 => BreakpointType::Write,
            3 => BreakpointType::ReadOrWrite,
            _ => unreachable!(),
        };
        let size = match (v >> 2) & 3 {
            0 => BreakpointSize::Byte,
            1 => BreakpointSize::Word,
            2 => BreakpointSize::QWord,
            3 => BreakpointSize::DWord,
            _ => unreachable!(),
        };
        Self { address, ty, size }
    }

    /// Returns a value to OR into DR7 to enable this breakpoint.
    pub fn dr7_bits(&self, reg: usize) -> u64 {
        ((self.ty as u64 | ((self.size as u64) << 2)) << (16 + reg * 4)) | (1 << (1 + reg * 2))
    }
}

/// A hardware breakpoint type.
#[derive(Debug, Copy, Clone, Protobuf, PartialEq, Eq)]
pub enum BreakpointType {
    /// Break on execute. Size should be [`BreakpointSize::Byte`].
    Execute = 0,
    /// Invalid type, not used on x86.
    Invalid = 1,
    /// Break on write.
    Write = 2,
    /// Break on read or write.
    ReadOrWrite = 3,
}

/// The size of the debug breakpoint.
#[derive(Debug, Copy, Clone, Protobuf, PartialEq, Eq)]
pub enum BreakpointSize {
    /// 1 byte.
    Byte = 0,
    /// 2 bytes.
    Word = 1,
    /// 4 bytes.
    DWord = 3,
    /// 8 bytes.
    QWord = 2,
}

/// The requested breakpoint size is not supported.
#[derive(Debug)]
pub struct UnsupportedBreakpointSize;

impl TryFrom<usize> for BreakpointSize {
    type Error = UnsupportedBreakpointSize;

    fn try_from(value: usize) -> Result<Self, Self::Error> {
        Ok(match value {
            1 => BreakpointSize::Byte,
            2 => BreakpointSize::Word,
            4 => BreakpointSize::DWord,
            8 => BreakpointSize::QWord,
            _ => return Err(UnsupportedBreakpointSize),
        })
    }
}

fn snp_c_bit_from_cpuid(mut cpuid: impl FnMut(u32, u32) -> [u32; 4]) -> Option<u8> {
    let max_extended = cpuid(CpuidFunction::ExtendedMaxFunction.0, 0)[0];
    if max_extended < CpuidFunction::ExtendedSevFeatures.0 {
        return None;
    }

    let [eax, ebx, _, _] = cpuid(CpuidFunction::ExtendedSevFeatures.0, 0);
    ExtendedSevFeaturesEax::from(eax)
        .sev_snp()
        .then(|| ExtendedSevFeaturesEbx::from(ebx).cbit_position())
}

/// Query the max physical address size of the system.
pub fn max_physical_address_size_from_cpuid(mut cpuid: impl FnMut(u32, u32) -> [u32; 4]) -> u8 {
    const DEFAULT_PHYSICAL_ADDRESS_SIZE: u8 = 32;

    let max_extended = {
        let result = cpuid(CpuidFunction::ExtendedMaxFunction.0, 0);
        result[0]
    };

    if max_extended >= CpuidFunction::ExtendedAddressSpaceSizes.0 {
        let result = cpuid(CpuidFunction::ExtendedAddressSpaceSizes.0, 0);
        (result[0] & 0xFF) as u8
    } else {
        DEFAULT_PHYSICAL_ADDRESS_SIZE
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use std::collections::HashMap;
    use test_with_tracing::test;
    use vm_topology::processor::TopologyBuilder;
    use vm_topology::processor::x86::X2ApicState;
    use x86defs::cpuid::ExtendedFeatureSubleaf0Ecx;
    use x86defs::cpuid::ExtendedFeatureSubleaf0Edx;
    use x86defs::cpuid::ExtendedVersionAndFeaturesEdx;
    use x86defs::cpuid::VersionAndFeaturesEcx;

    /// CPUID results keyed by leaf and subleaf. Leaves that aren't listed
    /// return all zeros, like a processor that doesn't implement them.
    #[derive(Clone)]
    pub(crate) struct FakeCpuid(HashMap<(u32, u32), [u32; 4]>);

    impl FakeCpuid {
        /// A processor from `vendor` whose highest standard leaf is `max`.
        pub(crate) fn new(vendor: Vendor, max: u32) -> Self {
            let word = |i: usize| u32::from_le_bytes(vendor.0[i..i + 4].try_into().unwrap());
            let mut leaves = HashMap::new();
            leaves.insert(
                (CpuidFunction::VendorAndMaxFunction.0, 0),
                [max, word(0), word(8), word(4)],
            );
            Self(leaves)
        }

        pub(crate) fn with(mut self, leaf: u32, subleaf: u32, value: [u32; 4]) -> Self {
            self.0.insert((leaf, subleaf), value);
            self
        }

        /// Returns the result for `leaf` and `subleaf`.
        pub(crate) fn query(&self, leaf: u32, subleaf: u32) -> [u32; 4] {
            self.0.get(&(leaf, subleaf)).copied().unwrap_or_default()
        }

        pub(crate) fn caps(
            &self,
            x2apic: X2ApicState,
        ) -> Result<X86PartitionCapabilities, X86PartitionCapabilitiesError> {
            let topology = TopologyBuilder::new_x86().x2apic(x2apic).build(1).unwrap();
            X86PartitionCapabilities::from_cpuid(&topology, &mut |leaf, subleaf| {
                self.query(leaf, subleaf)
            })
        }
    }

    /// Capabilities of an Intel processor without x2APIC, XSAVE, or a
    /// hypervisor.
    pub(crate) fn basic_caps() -> X86PartitionCapabilities {
        FakeCpuid::new(Vendor::INTEL, 1)
            .caps(X2ApicState::Unsupported)
            .unwrap()
    }

    fn leaf1(ecx: VersionAndFeaturesEcx) -> [u32; 4] {
        [0x000906ea, 0, ecx.into(), 0]
    }

    // Test: Derive capabilities from an Intel processor that reports only
    //       leaf 1, with x2APIC and TSC deadline support.
    // Expected: The vendor, reset RDX (leaf 1 EAX), x2APIC, and TSC deadline
    //       are taken from CPUID; everything that needs a missing leaf is off,
    //       and the physical address width defaults to 32 bits.
    #[test]
    fn from_cpuid_decodes_basic_intel_features() {
        let caps = FakeCpuid::new(Vendor::INTEL, 1)
            .with(
                1,
                0,
                leaf1(
                    VersionAndFeaturesEcx::new()
                        .with_x2_apic(true)
                        .with_tsc_deadline_tmr(true),
                ),
            )
            .caps(X2ApicState::Supported)
            .unwrap();

        assert_eq!(caps.vendor, Vendor::INTEL);
        assert_eq!(caps.reset_rdx, 0x000906ea);
        assert!(caps.x2apic);
        assert!(!caps.x2apic_enabled);
        assert!(caps.tsc_deadline);
        assert!(!caps.hv1 && !caps.hv1_reference_tsc_page);
        assert!(!caps.cet && !caps.cet_ss && !caps.sgx && !caps.tsc_aux);
        assert_eq!(caps.xsave.features, 0);
        assert_eq!(caps.xsave.standard_len, XSAVE_VARIABLE_OFFSET as u32);
        assert_eq!(caps.vtom, None);
        assert_eq!(caps.physical_address_width, 32);
        assert_eq!(caps.snp_c_bit, None);
    }

    // Test: Derive capabilities where the topology's APIC mode and CPUID's
    //       x2APIC bit disagree, and where both enable x2APIC.
    // Expected: A mismatch in either direction is an error naming the
    //       expected and found values; an x2APIC-enabled topology with x2APIC
    //       support enables x2APIC at boot.
    #[test]
    fn from_cpuid_requires_x2apic_to_match_topology() {
        let with_x2apic = FakeCpuid::new(Vendor::INTEL, 1).with(
            1,
            0,
            leaf1(VersionAndFeaturesEcx::new().with_x2_apic(true)),
        );
        let without_x2apic = FakeCpuid::new(Vendor::INTEL, 1);

        assert!(matches!(
            with_x2apic.caps(X2ApicState::Unsupported),
            Err(X86PartitionCapabilitiesError::X2ApicMismatch {
                expected: false,
                found: true
            })
        ));
        assert!(matches!(
            without_x2apic.caps(X2ApicState::Supported),
            Err(X86PartitionCapabilitiesError::X2ApicMismatch {
                expected: true,
                found: false
            })
        ));
        assert!(
            with_x2apic
                .caps(X2ApicState::Enabled)
                .unwrap()
                .x2apic_enabled
        );
    }

    // Test: Report CET shadow stacks (leaf 7 ECX), CET indirect branch
    //       tracking only (leaf 7 EDX), and SGX (leaf 0x12 subleaf 2), and
    //       report the same leaves when the maximum leaf doesn't reach them.
    // Expected: Shadow stacks enable both CET flags, IBT enables CET only, and
    //       a nonzero SGX type enables SGX; leaves above the maximum are
    //       ignored.
    #[test]
    fn from_cpuid_reads_cet_and_sgx_only_below_max_leaf() {
        let shadow_stack = [
            0,
            0,
            ExtendedFeatureSubleaf0Ecx::new().with_cet_ss(true).into(),
            0,
        ];
        let ibt = [
            0,
            0,
            0,
            ExtendedFeatureSubleaf0Edx::new().with_cet_ibt(true).into(),
        ];
        let sgx = [1, 0, 0, 0];

        let caps = FakeCpuid::new(Vendor::INTEL, 0x12)
            .with(7, 0, shadow_stack)
            .with(0x12, 2, sgx)
            .caps(X2ApicState::Unsupported)
            .unwrap();
        assert!(caps.cet && caps.cet_ss && caps.sgx);

        let caps = FakeCpuid::new(Vendor::INTEL, 7)
            .with(7, 0, ibt)
            .with(0x12, 2, sgx)
            .caps(X2ApicState::Unsupported)
            .unwrap();
        assert!(caps.cet && !caps.cet_ss && !caps.sgx);

        let caps = FakeCpuid::new(Vendor::INTEL, 6)
            .with(7, 0, shadow_stack)
            .caps(X2ApicState::Unsupported)
            .unwrap();
        assert!(!caps.cet && !caps.cet_ss);
    }

    /// A processor with XSAVE: x87, SSE, and AVX (576 + 256 bytes) as user
    /// features, plus a 16-byte supervisor feature 11, advertising
    /// `standard_len` for the standard format.
    fn xsave_cpuid(standard_len: u32) -> FakeCpuid {
        FakeCpuid::new(Vendor::INTEL, 0xd)
            .with(1, 0, leaf1(VersionAndFeaturesEcx::new().with_xsave(true)))
            .with(0xd, 0, [0x7, 0, standard_len, 0])
            .with(0xd, 1, [0, 0, 1 << 11, 0])
            .with(0xd, 2, [256, 576, 0, 0])
            .with(0xd, 11, [16, 0, 0, 0])
    }

    // Test: Derive XSAVE capabilities from leaf 0xD with user and supervisor
    //       features, once with a correct advertised size and once with a
    //       size smaller than the features need.
    // Expected: Features, per-feature layout, and both buffer lengths are
    //       computed from the subleaves; an advertised size that's too small
    //       is an error reporting both sizes.
    #[test]
    fn from_cpuid_computes_xsave_layout_and_rejects_short_size() {
        let caps = xsave_cpuid(832).caps(X2ApicState::Unsupported).unwrap();

        assert_eq!(caps.xsave.features, 0x7);
        assert_eq!(caps.xsave.supervisor_features, 1 << 11);
        assert_eq!(caps.xsave.feature_info[2].offset, 576);
        assert_eq!(caps.xsave.feature_info[2].len, 256);
        assert_eq!(caps.xsave.standard_len, 576 + 256);
        assert_eq!(caps.xsave.compact_len, 576 + 256 + 16);

        assert!(matches!(
            xsave_cpuid(768).caps(X2ApicState::Unsupported),
            Err(X86PartitionCapabilitiesError::XSaveLengthTooSmall {
                advertised: 768,
                required: 832
            })
        ));
    }

    /// A processor running under a hypervisor whose interface signature is
    /// `interface`, granting `privileges`, with isolation configuration
    /// `isolation`.
    fn hypervisor_cpuid(
        hypervisor_present: bool,
        interface: &[u8; 4],
        privileges: hvdef::HvPartitionPrivilege,
        isolation: hvdef::HvIsolationConfiguration,
    ) -> FakeCpuid {
        let privileges = u64::from(privileges);
        let isolation = u128::from(isolation);
        FakeCpuid::new(Vendor::INTEL, 1)
            .with(
                1,
                0,
                leaf1(VersionAndFeaturesEcx::new().with_hypervisor_present(hypervisor_present)),
            )
            .with(
                hvdef::HV_CPUID_FUNCTION_HV_VENDOR_AND_MAX_FUNCTION,
                0,
                [
                    hvdef::HV_CPUID_FUNCTION_MS_HV_ISOLATION_CONFIGURATION,
                    0,
                    0,
                    0,
                ],
            )
            .with(
                hvdef::HV_CPUID_FUNCTION_HV_INTERFACE,
                0,
                [u32::from_le_bytes(*interface), 0, 0, 0],
            )
            .with(
                hvdef::HV_CPUID_FUNCTION_MS_HV_FEATURES,
                0,
                [privileges as u32, (privileges >> 32) as u32, 0, 0],
            )
            .with(
                hvdef::HV_CPUID_FUNCTION_MS_HV_ISOLATION_CONFIGURATION,
                0,
                [
                    isolation as u32,
                    (isolation >> 32) as u32,
                    (isolation >> 64) as u32,
                    (isolation >> 96) as u32,
                ],
            )
    }

    // Test: Derive capabilities under a Hyper-V compatible hypervisor that
    //       grants the reference TSC page and isolation with a 2^46 shared GPA
    //       boundary; then under a hypervisor with a different interface
    //       signature, and with the leaves present but the hypervisor bit
    //       clear.
    // Expected: Only the Hv#1 hypervisor enables hv1, the reference TSC page,
    //       and a VTOM of 2^46; the others leave all three off.
    #[test]
    fn from_cpuid_reads_hyperv_leaves_only_for_hv1() {
        let privileges = hvdef::HvPartitionPrivilege::new()
            .with_access_partition_reference_tsc(true)
            .with_isolation(true);
        let isolation = hvdef::HvIsolationConfiguration::new()
            .with_shared_gpa_boundary_active(true)
            .with_shared_gpa_boundary_bits(46);

        let caps = hypervisor_cpuid(true, b"Hv#1", privileges, isolation)
            .caps(X2ApicState::Unsupported)
            .unwrap();
        assert!(caps.hv1 && caps.hv1_reference_tsc_page);
        assert_eq!(caps.vtom, Some(1 << 46));

        for cpuid in [
            hypervisor_cpuid(true, b"Xen0", privileges, isolation),
            hypervisor_cpuid(false, b"Hv#1", privileges, isolation),
        ] {
            let caps = cpuid.caps(X2ApicState::Unsupported).unwrap();
            assert!(!caps.hv1 && !caps.hv1_reference_tsc_page);
            assert_eq!(caps.vtom, None);
        }
    }

    // Test: Derive capabilities where TSC_AUX is reachable through RDTSCP
    //       (extended leaf 0x80000001), through RDPID (leaf 7), or neither,
    //       with a 48-bit physical address width in leaf 0x80000008.
    // Expected: Either instruction enables TSC_AUX and neither leaves it off;
    //       the physical address width is read from the extended leaf.
    #[test]
    fn from_cpuid_enables_tsc_aux_for_rdtscp_or_rdpid() {
        let extended = FakeCpuid::new(Vendor::AMD, 7)
            .with(0x8000_0000, 0, [0x8000_0008, 0, 0, 0])
            .with(0x8000_0008, 0, [48, 0, 0, 0]);
        let rdtscp = extended.clone().with(
            0x8000_0001,
            0,
            [
                0,
                0,
                0,
                ExtendedVersionAndFeaturesEdx::new()
                    .with_rdtscp(true)
                    .into(),
            ],
        );
        let rdpid = extended.clone().with(
            7,
            0,
            [
                0,
                0,
                ExtendedFeatureSubleaf0Ecx::new().with_rd_pid(true).into(),
                0,
            ],
        );

        for (cpuid, tsc_aux) in [(rdtscp, true), (rdpid, true), (extended, false)] {
            let caps = cpuid.caps(X2ApicState::Unsupported).unwrap();
            assert_eq!(caps.vendor, Vendor::AMD);
            assert_eq!(caps.tsc_aux, tsc_aux);
            assert_eq!(caps.physical_address_width, 48);
        }
    }

    // Test: Convert segment and table registers with distinct values in every
    //       field to the x86defs and hvdef forms and back.
    // Expected: Every field round-trips unchanged through both forms.
    #[test]
    fn segment_and_table_registers_round_trip() {
        let seg = SegmentRegister {
            base: 0x1234_5678_9abc_def0,
            limit: 0xfedc_ba98,
            selector: 0x1b,
            attributes: 0xa09b,
        };
        assert_eq!(
            SegmentRegister::from(x86defs::SegmentRegister::from(seg)),
            seg
        );
        assert_eq!(
            SegmentRegister::from(hvdef::HvX64SegmentRegister::from(seg)),
            seg
        );

        let table = TableRegister {
            base: 0xffff_8000_1234_0000,
            limit: 0x0fff,
        };
        assert_eq!(
            TableRegister::from(hvdef::HvX64TableRegister::from(table)),
            table
        );
    }

    #[test]
    fn xsave_variable_features_preserve_order_and_layout() {
        let mut caps = XsaveCapabilities {
            features: 0,
            supervisor_features: 0,
            standard_len: 0,
            compact_len: 0,
            feature_info: [XsaveFeature::default(); 63],
        };
        caps.feature_info[2] = XsaveFeature {
            offset: 1024,
            len: 16,
            align: false,
        };
        caps.feature_info[5] = XsaveFeature {
            offset: 2048,
            len: 32,
            align: true,
        };
        caps.feature_info[62] = XsaveFeature {
            offset: 4096,
            len: 8,
            align: false,
        };

        let base_len = XSAVE_VARIABLE_OFFSET as u32;
        for (mask, indices, standard_len, compact_len) in [
            (0, &[][..], base_len, base_len),
            (3 | XCOMP_COMPRESSED, &[][..], base_len, base_len),
            (1 << 2, &[2][..], 1040, base_len + 16),
            (1 << 62, &[62][..], 4104, base_len + 8),
            (
                3 | (1 << 2) | (1 << 5) | (1 << 62) | XCOMP_COMPRESSED,
                &[2, 5, 62][..],
                4104,
                680,
            ),
        ] {
            assert_eq!(
                caps.variable_features(mask)
                    .map(|(i, _)| i)
                    .collect::<Vec<_>>(),
                indices
            );
            assert_eq!(caps.standard_len_for(mask), standard_len);
            assert_eq!(caps.compact_len_for(mask), compact_len);
        }
    }

    #[test]
    fn snp_c_bit_requires_snp_cpuid_support() {
        let cpuid = |function, _| match CpuidFunction(function) {
            CpuidFunction::ExtendedMaxFunction => [CpuidFunction::ExtendedSevFeatures.0, 0, 0, 0],
            CpuidFunction::ExtendedSevFeatures => [
                ExtendedSevFeaturesEax::new().with_sev_snp(true).into(),
                ExtendedSevFeaturesEbx::new().with_cbit_position(51).into(),
                0,
                0,
            ],
            _ => [0; 4],
        };
        assert_eq!(snp_c_bit_from_cpuid(cpuid), Some(51));

        let cpuid_without_snp = |function, _| match CpuidFunction(function) {
            CpuidFunction::ExtendedMaxFunction => [CpuidFunction::ExtendedSevFeatures.0, 0, 0, 0],
            _ => [0; 4],
        };
        assert_eq!(snp_c_bit_from_cpuid(cpuid_without_snp), None);
    }

    const BREAKPOINT_TYPES: [BreakpointType; 3] = [
        BreakpointType::Execute,
        BreakpointType::Write,
        BreakpointType::ReadOrWrite,
    ];
    const BREAKPOINT_SIZES: [BreakpointSize; 4] = [
        BreakpointSize::Byte,
        BreakpointSize::Word,
        BreakpointSize::DWord,
        BreakpointSize::QWord,
    ];

    // Test: Encode every breakpoint type and size into DR7 for each debug
    //       register, then decode it back.
    // Expected: Decoding returns the original breakpoint.
    #[test]
    fn hardware_breakpoint_round_trips_through_dr7() {
        const ADDRESS: u64 = 0xffff_8000_1234_5678;
        for reg in 0..4 {
            for ty in BREAKPOINT_TYPES {
                for size in BREAKPOINT_SIZES {
                    let bp = HardwareBreakpoint {
                        address: ADDRESS,
                        ty,
                        size,
                    };
                    assert_eq!(
                        HardwareBreakpoint::from_dr7(bp.dr7_bits(reg), ADDRESS, reg),
                        bp,
                        "reg {reg}"
                    );
                }
            }
        }
    }

    // IGNORED: this test fails because of a known bug. BreakpointType::Write has
    // value 2, so dr7_bits() programs the DR7 R/W field as 10b (an I/O breakpoint,
    // undefined without CR4.DE) instead of 01b (break on data writes), and
    // from_dr7() decodes a hardware 01b as Invalid. Write watchpoints from the gdb
    // stub therefore do not trigger on KVM or OpenHCL. Re-enable this test together
    // with the fix (swap the Write and Invalid/I/O values) by removing #[ignore].
    //
    // Test: Encode every breakpoint type and size into DR7 for each debug
    //       register.
    // Expected: The bits match the Intel SDM DR7 layout: the register's
    //       global-enable bit, R/W = 00b execute, 01b data write, 11b data
    //       read/write, and LEN = 00b 1 byte, 01b 2 bytes, 11b 4 bytes,
    //       10b 8 bytes.
    #[test]
    #[ignore = "known bug: DR7 write breakpoints use the I/O encoding (see comment)"]
    fn hardware_breakpoint_dr7_bits_match_architectural_encoding() {
        for reg in 0..4 {
            for ty in BREAKPOINT_TYPES {
                let rw: u64 = match ty {
                    BreakpointType::Execute => 0b00,
                    BreakpointType::Write => 0b01,
                    BreakpointType::ReadOrWrite => 0b11,
                    BreakpointType::Invalid => unreachable!(),
                };
                for size in BREAKPOINT_SIZES {
                    let len: u64 = match size {
                        BreakpointSize::Byte => 0b00,
                        BreakpointSize::Word => 0b01,
                        BreakpointSize::DWord => 0b11,
                        BreakpointSize::QWord => 0b10,
                    };
                    let bp = HardwareBreakpoint {
                        address: 0,
                        ty,
                        size,
                    };

                    let expected = (1 << (1 + reg * 2)) | ((rw | (len << 2)) << (16 + reg * 4));
                    assert_eq!(bp.dr7_bits(reg), expected, "reg {reg}, {ty:?}, {size:?}");
                }
            }
        }
    }

    // Test: Convert supported and unsupported byte sizes to a breakpoint size.
    // Expected: 1, 2, 4, and 8 map to their sizes; anything else is rejected.
    #[test]
    fn breakpoint_size_accepts_only_supported_lengths() {
        assert_eq!(BreakpointSize::try_from(1).unwrap(), BreakpointSize::Byte);
        assert_eq!(BreakpointSize::try_from(2).unwrap(), BreakpointSize::Word);
        assert_eq!(BreakpointSize::try_from(4).unwrap(), BreakpointSize::DWord);
        assert_eq!(BreakpointSize::try_from(8).unwrap(), BreakpointSize::QWord);
        for size in [0, 3, 5, 16] {
            assert!(BreakpointSize::try_from(size).is_err(), "size {size}");
        }
    }
}

/// Error returned by MSR routines.
#[derive(Debug)]
pub enum MsrError {
    /// The MSR is not implemented. Depending on the configuration, this should
    /// either be ignored (returning 0 for reads) or should result in a #GP.
    Unknown,
    /// The MSR is implemented but this is an invalid read or write and should
    /// always result in a #GP.
    InvalidAccess,
}

/// Extension trait to chain MSR accesses together.
pub trait MsrErrorExt: Sized {
    /// Calls `f` if `self` is `Err(Msr::Unknown)`.
    fn or_else_if_unknown(self, f: impl FnOnce() -> Self) -> Self;
}

impl<T> MsrErrorExt for Result<T, MsrError> {
    fn or_else_if_unknown(self, f: impl FnOnce() -> Self) -> Self {
        match self {
            Err(MsrError::Unknown) => f(),
            r => r,
        }
    }
}
