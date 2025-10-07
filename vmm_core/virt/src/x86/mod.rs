// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! x86-specific state.

pub mod apic_software_device;
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
use x86defs::cpuid::SgxCpuidSubleafEax;
use x86defs::cpuid::Vendor;
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
    /// The address of the virtual top of memory, for encrypted VMs.
    ///
    /// This is computed from the Hyper-V isolation leaf. It is guaranteed to be
    /// a power of 2, if present.
    #[inspect(hex)]
    pub vtom: Option<u64>,
    /// The physical address width of the CPU, as reported by CPUID.
    pub physical_address_width: u8,

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
            vtom: None,
            physical_address_width: max_physical_address_size_from_cpuid(&mut *f),
            can_freeze_time: false,
            xsaves_state_bv_broken: false,
            dr6_tsx_broken: false,
            nxe_forced_on: false,
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
            while n != 0 {
                let i = n.trailing_zeros();
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
    pub fn standard_len_for(&self, xfem: u64) -> u32 {
        let mut len = XSAVE_VARIABLE_OFFSET as u32;
        for i in 2..63 {
            if xfem & (1 << i) != 0 {
                let feature = &self.feature_info[i as usize];
                len = len.max(feature.offset + feature.len);
            }
        }
        len
    }

    pub fn compact_len_for(&self, xfem: u64) -> u32 {
        let mut len = XSAVE_VARIABLE_OFFSET as u32;
        for i in 2..63 {
            if xfem & (1 << i) != 0 {
                let feature = &self.feature_info[i as usize];
                if feature.align {
                    len = (len + 63) & !63;
                }
                len += feature.len;
            }
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
