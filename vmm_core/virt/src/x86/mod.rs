// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! x86-specific state.

pub mod apic_software_device;
pub mod snp;
pub mod topology;
pub mod tsc;
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
    /// The guest's CPUID enumerates a control of `IA32_SPEC_CTRL` (IBRS,
    /// STIBP, or SSBD), so the guest can write the MSR, which the VP's saved
    /// state then holds. Backends without access to the MSR must clear this
    /// even if guest CPUID enumerates a control.
    pub spec_ctrl: bool,
    /// The guest's CPUID enumerates WAITPKG, so the guest can write
    /// `IA32_UMWAIT_CONTROL`, which the VP's saved state then holds. Backends
    /// without access to the MSR must clear this even if guest CPUID
    /// enumerates WAITPKG.
    pub umwait_control: bool,
    /// The guest's CPUID enumerates MPX, so the guest can write
    /// `IA32_BNDCFGS`, which the VP's saved state then holds. Backends without
    /// access to the MSR must clear this even if guest CPUID enumerates MPX.
    pub bndcfgs: bool,
    /// The backend holds the guest's `IA32_DEBUGCTL` in the VP's saved state.
    /// No CPUID bit decides whether the guest can write the MSR, which every
    /// CPU has, so `from_cpuid` leaves this clear and a backend that can
    /// access the MSR sets it.
    pub debug_ctl: bool,
    /// The backend holds the guest's `IA32_MISC_ENABLE`,
    /// `MSR_MISC_FEATURES_ENABLES`, and `IA32_POWER_CTL` in the VP's saved
    /// state. Like [`Self::debug_ctl`], no CPUID bit decides it, so a backend
    /// that can access the MSRs sets it.
    pub misc_msrs: bool,
    /// The backend holds the guest's machine-check state in the VP's saved
    /// state. Like [`Self::debug_ctl`], no CPUID bit decides it, so a backend
    /// that can access the registers sets it.
    pub machine_check: bool,
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
    /// The hypervisor normalizes some of the state that a restore sets, so that
    /// the state that the restore's check reads back can differ from the one
    /// that it set in the ways that [`StateElement::restore_eq`] documents for
    /// each element. KVM does: see [`vp::Registers`], [`vp::Activity`],
    /// [`vp::VirtualMsrs`], and [`vp::TscAux`]. Without it, a restore's check
    /// requires equal states.
    ///
    /// [`StateElement::restore_eq`]: crate::state::StateElement::restore_eq
    pub normalizes_restored_state: bool,
}

#[derive(Error, Debug)]
pub enum X86PartitionCapabilitiesError {
    #[error(
        "advertised xsave length ({advertised}) too small for features, requires ({required}) bytes"
    )]
    XSaveLengthTooSmall { advertised: u32, required: u32 },
    #[error(
        "xsave length ({required}) larger than the hypervisor's xsave buffer ({supported} bytes)"
    )]
    XSaveLengthTooLarge { required: u32, supported: u32 },
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
            spec_ctrl: false,
            umwait_control: false,
            bndcfgs: false,
            debug_ctl: false,
            misc_msrs: false,
            machine_check: false,
            tsc_deadline: false,
            vtom: None,
            physical_address_width: max_physical_address_size_from_cpuid(&mut *f),
            snp_c_bit: snp_c_bit_from_cpuid(&mut *f),
            can_freeze_time: false,
            xsaves_state_bv_broken: false,
            dr6_tsx_broken: false,
            nxe_forced_on: false,
            nested_virt: false,
            normalizes_restored_state: false,
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

        let extended_max_function = f(CpuidFunction::ExtendedMaxFunction.0, 0)[0];

        this.tsc_aux = {
            let rdtscp = if extended_max_function >= CpuidFunction::ExtendedVersionAndFeatures.0 {
                x86defs::cpuid::ExtendedVersionAndFeaturesEdx::from(
                    f(CpuidFunction::ExtendedVersionAndFeatures.0, 0)[3],
                )
                .rdtscp()
            } else {
                false
            };

            let rdpid =
                x86defs::cpuid::ExtendedFeatureSubleaf0Ecx::from(extended_features[2]).rd_pid();

            rdtscp || rdpid
        };

        // Intel enumerates the controls of IA32_SPEC_CTRL in CPUID.(7,0):EDX,
        // and AMD in CPUID.0x80000008:EBX.
        let address_space_sizes =
            if extended_max_function >= CpuidFunction::ExtendedAddressSpaceSizes.0 {
                f(CpuidFunction::ExtendedAddressSpaceSizes.0, 0)
            } else {
                Default::default()
            };
        this.spec_ctrl = enumerates_spec_ctrl(extended_features, address_space_sizes);
        this.umwait_control = enumerates_waitpkg(extended_features);
        this.bndcfgs = enumerates_mpx(extended_features);

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

    /// Returns the length of the standard-format XSAVE area that holds the
    /// components in `xfem`, which saturates at `u32::MAX`.
    pub fn standard_len_for(&self, xfem: u64) -> u32 {
        let mut len = XSAVE_VARIABLE_OFFSET as u32;
        for (_, feature) in self.variable_features(xfem) {
            len = len.max(feature.offset.saturating_add(feature.len));
        }
        len
    }

    /// Returns the length of the compact-format XSAVE area that holds the
    /// components in `xfem`, which saturates at `u32::MAX`.
    pub fn compact_len_for(&self, xfem: u64) -> u32 {
        let mut len = XSAVE_VARIABLE_OFFSET as u32;
        for (_, feature) in self.variable_features(xfem) {
            if feature.align {
                // Rounding up must not wrap a saturated length back below
                // `u32::MAX`.
                len = len.checked_next_multiple_of(64).unwrap_or(u32::MAX);
            }
            len = len.saturating_add(feature.len);
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

/// Returns whether CPUID enumerates a control of `IA32_SPEC_CTRL`, so that a
/// guest can write the MSR: Intel's IBRS, STIBP, or SSBD, in
/// `CPUID.(7,0):EDX[26]`, `[27]`, and `[31]`, or AMD's, in
/// `CPUID.0x80000008:EBX[14]`, `[15]`, and `[24]`.
///
/// `extended_features` is the result of `CPUID.(7,0)`, and
/// `address_space_sizes` that of `CPUID.0x80000008`, each zero if the CPU has
/// no such leaf.
pub fn enumerates_spec_ctrl(extended_features: [u32; 4], address_space_sizes: [u32; 4]) -> bool {
    let intel = x86defs::cpuid::ExtendedFeatureSubleaf0Edx::from(extended_features[3]);
    let amd = x86defs::cpuid::ExtendedAddressSpaceSizesEbx::from(address_space_sizes[1]);
    intel.ibrs() || intel.stibp() || intel.ssbd() || amd.ibrs() || amd.stibp() || amd.ssbd()
}

/// Returns whether CPUID enumerates WAITPKG, the `UMWAIT` and `TPAUSE`
/// instructions, so that a guest can write `IA32_UMWAIT_CONTROL`:
/// `CPUID.(7,0):ECX[5]`.
///
/// `extended_features` is the result of `CPUID.(7,0)`, zero if the CPU has no
/// such leaf.
pub fn enumerates_waitpkg(extended_features: [u32; 4]) -> bool {
    x86defs::cpuid::ExtendedFeatureSubleaf0Ecx::from(extended_features[2]).umwait_tpause()
}

/// Returns whether CPUID enumerates MPX, so that a guest can write
/// `IA32_BNDCFGS`: `CPUID.(7,0):EBX[14]`.
///
/// `extended_features` is the result of `CPUID.(7,0)`, zero if the CPU has no
/// such leaf.
pub fn enumerates_mpx(extended_features: [u32; 4]) -> bool {
    x86defs::cpuid::ExtendedFeatureSubleaf0Ebx::from(extended_features[1]).mpx()
}

#[cfg(test)]
mod tests {
    use super::*;
    use test_with_tracing::test;

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

    /// Returns the standard and compact XSAVE lengths of components
    /// `features`, which are numbered from 2.
    fn xsave_lengths(features: &[XsaveFeature]) -> (u32, u32) {
        let mut caps = XsaveCapabilities {
            features: 0,
            supervisor_features: 0,
            standard_len: 0,
            compact_len: 0,
            feature_info: [XsaveFeature::default(); 63],
        };
        for (index, &feature) in (2..).zip(features) {
            caps.features |= 1 << index;
            caps.feature_info[index] = feature;
        }
        (caps.standard_len_for(!0), caps.compact_len_for(!0))
    }

    #[test]
    fn xsave_lengths_saturate_at_the_largest_length() {
        let feature = |offset, len, align| XsaveFeature { offset, len, align };
        let base = XSAVE_VARIABLE_OFFSET as u32;

        // A length of 2^32 - 1 fits, and one of 2^32 does not.
        for len in [u32::MAX - base, u32::MAX - base + 1] {
            assert_eq!(
                xsave_lengths(&[feature(base, len, false)]),
                (u32::MAX, u32::MAX)
            );
        }
        assert_eq!(
            xsave_lengths(&[feature(u32::MAX - 0x100, 0x100, false)]),
            (u32::MAX, base + 0x100)
        );
        assert_eq!(
            xsave_lengths(&[feature(u32::MAX - 0x100, 0x101, false)]),
            (u32::MAX, base + 0x101)
        );

        // Aligning to 64 bytes does not overflow up to the last multiple
        // of 64, and past it saturates, as does anything that follows a
        // saturated length.
        let last = 0xffff_ffc0;
        assert_eq!(
            xsave_lengths(&[feature(base, last - base, false), feature(base, 0x20, true)]),
            (last, last + 0x20)
        );
        assert_eq!(
            xsave_lengths(&[
                feature(base, last + 1 - base, false),
                feature(base, 0x20, true)
            ]),
            (last + 1, u32::MAX)
        );
        assert_eq!(
            xsave_lengths(&[
                feature(base, u32::MAX - base, false),
                feature(base, 0x20, true),
                feature(base, 0x20, false)
            ]),
            (u32::MAX, u32::MAX)
        );
    }

    /// Returns the capabilities of a CPU with XSAVE whose components from 2
    /// are `(size, offset, align)`, and whose XSAVE area for XCR0 is
    /// `xsave_len` bytes.
    fn xsave_capabilities(
        components: &[(u32, u32, bool)],
        xsave_len: u32,
    ) -> Result<X86PartitionCapabilities, X86PartitionCapabilitiesError> {
        let topology = vm_topology::processor::TopologyBuilder::new_x86()
            .build(1)
            .unwrap();
        let x2apic = topology.apic_mode() != ApicMode::XApic;
        X86PartitionCapabilities::from_cpuid(
            &topology,
            &mut |leaf, subleaf| match (leaf, subleaf) {
                (0, _) => [0xd, 0x756e_6547, 0x6c65_746e, 0x4965_6e69],
                (1, _) => [0, 0, (1 << 26) | (u32::from(x2apic) << 21), 0],
                (0xd, 0) => [(1 << (components.len() + 2)) - 1, 0, xsave_len, 0],
                (0xd, i @ 2..) => components
                    .get(i as usize - 2)
                    .map_or([0; 4], |&(size, offset, align)| {
                        [size, offset, u32::from(align) << 1, 0]
                    }),
                _ => [0; 4],
            },
        )
    }

    #[test]
    fn from_cpuid_saturates_xsave_lengths_that_overflow() {
        for (components, standard_len, compact_len) in [
            // A component that ends past 2^32.
            (vec![(0x100, 0xffff_ff00, false)], u32::MAX, 0x340),
            // Components whose compact places add up past 2^32.
            (
                vec![(0x8000_0000, 0x240, false), (0x8000_0000, 0x240, false)],
                0x8000_0240,
                u32::MAX,
            ),
            // A component aligned after a length past the last multiple of 64.
            (
                vec![(u32::MAX - 0x240 - 31, 0x240, false), (0, 0x240, true)],
                u32::MAX - 31,
                u32::MAX,
            ),
        ] {
            let caps = xsave_capabilities(&components, u32::MAX).unwrap();
            assert_eq!(caps.xsave.standard_len, standard_len, "{components:x?}");
            assert_eq!(caps.xsave.compact_len, compact_len, "{components:x?}");
        }

        // The saturated standard length is still compared with the area that
        // the CPU advertises.
        let error = xsave_capabilities(&[(0x100, 0xffff_ff00, false)], 0x1000).unwrap_err();
        assert!(
            matches!(
                error,
                X86PartitionCapabilitiesError::XSaveLengthTooSmall {
                    advertised: 0x1000,
                    required: u32::MAX,
                }
            ),
            "{error}"
        );
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

    #[test]
    fn spec_ctrl_controls_are_the_ibrs_stibp_and_ssbd_bits_of_either_vendor() {
        // CPUID.(7,0):EDX[26], [27], and [31] for Intel, whichever other
        // features the register enumerates, and none of AMD's bits in it.
        for bit in 0..32 {
            assert_eq!(
                enumerates_spec_ctrl([0, 0, 0, 1 << bit], [0; 4]),
                matches!(bit, 26 | 27 | 31),
                "CPUID.(7,0):EDX[{bit}]"
            );
        }
        // CPUID.0x80000008:EBX[14], [15], and [24] for AMD. IBPB, VIRT_SSBD,
        // and the rest of its speculation features neither need the MSR nor
        // are controls of it, and PSFD, a bit of the MSR, needs one of the
        // controls to reach it.
        for bit in 0..32 {
            assert_eq!(
                enumerates_spec_ctrl([0; 4], [0, 1 << bit, 0, 0]),
                matches!(bit, 14 | 15 | 24),
                "CPUID.0x80000008:EBX[{bit}]"
            );
        }
        // The other registers of the leaves enumerate none.
        assert!(!enumerates_spec_ctrl([!0, !0, !0, 0], [!0, 0, !0, !0]));
    }

    /// Returns whether the partition capabilities of a CPU with `leaf7_edx`
    /// and `ext8_ebx` in `CPUID.(7,0):EDX` and `CPUID.0x80000008:EBX`, and
    /// the highest leaves `max_function` and `max_extended`, include the
    /// SPEC_CTRL state.
    fn has_spec_ctrl(max_function: u32, max_extended: u32, leaf7_edx: u32, ext8_ebx: u32) -> bool {
        let topology = vm_topology::processor::TopologyBuilder::new_x86()
            .build(1)
            .unwrap();
        let x2apic = topology.apic_mode() != ApicMode::XApic;
        X86PartitionCapabilities::from_cpuid(
            &topology,
            &mut |leaf, subleaf| match (leaf, subleaf) {
                (0, _) => [max_function, 0x756e_6547, 0x6c65_746e, 0x4965_6e69],
                (1, _) => [0, 0, u32::from(x2apic) << 21, 0],
                (7, 0) => [0, 0, 0, leaf7_edx],
                (0x8000_0000, _) => [max_extended, 0, 0, 0],
                (0x8000_0008, 0) => [0, ext8_ebx, 0, 0],
                _ => [0; 4],
            },
        )
        .unwrap()
        .spec_ctrl
    }

    #[test]
    fn spec_ctrl_state_needs_a_control_in_a_leaf_that_the_cpu_has() {
        const IBRS: u32 = 1 << 26;
        const AMD_IBRS: u32 = 1 << 14;
        assert!(!has_spec_ctrl(0xd, 0x8000_0008, 0, 0));
        assert!(has_spec_ctrl(0xd, 0x8000_0008, IBRS, 0));
        assert!(has_spec_ctrl(0xd, 0x8000_0008, 0, AMD_IBRS));
        // A leaf above the highest is not CPUID's to enumerate.
        assert!(!has_spec_ctrl(6, 0x8000_0008, IBRS, 0));
        assert!(!has_spec_ctrl(0xd, 0x8000_0007, 0, AMD_IBRS));
        assert!(has_spec_ctrl(6, 0x8000_0008, 0, AMD_IBRS));
        assert!(has_spec_ctrl(0xd, 0x8000_0007, IBRS, 0));
    }

    /// Returns the ID and partition capabilities of every pinned CPU profile.
    // The time ABI's CPUID helpers exist only for x86-64 guests.
    #[cfg(guest_arch = "x86_64")]
    fn pinned_profile_capabilities() -> Vec<(&'static str, X86PartitionCapabilities)> {
        let topology = vm_topology::processor::TopologyBuilder::new_x86()
            .vps_per_socket(2)
            .x2apic(vm_topology::processor::x86::X2ApicState::Supported)
            .build(2)
            .unwrap();
        cpu_profile::pinned_profiles()
            .iter()
            .map(|profile| {
                let effective =
                    crate::time_abi::cpuid::effective_cpuid(profile, &topology).unwrap();
                let mut lookup = |leaf, subleaf| effective.lookup(leaf, subleaf);
                let caps = X86PartitionCapabilities::from_cpuid(
                    &topology,
                    &mut crate::time_abi::identity::capabilities_cpuid(&mut lookup),
                )
                .unwrap();
                (profile.id(), caps)
            })
            .collect()
    }

    /// The pinned profiles whose CPUID enumerates controls of SPEC_CTRL: the
    /// Skylake-SP and Alder Lake CPUs have them, and the Ice Lake-SP, Emerald
    /// Rapids, Granite Rapids, and AMD profiles hide them.
    #[test]
    #[cfg(guest_arch = "x86_64")]
    fn spec_ctrl_state_is_present_for_the_profiles_that_enumerate_a_control() {
        for (id, caps) in pinned_profile_capabilities() {
            assert_eq!(
                caps.spec_ctrl,
                matches!(id, "intel.skylake-sp.v1" | "intel.alderlake.v1"),
                "{id}"
            );
        }
    }

    #[test]
    fn waitpkg_is_bit_5_of_the_extended_features_ecx() {
        for bit in 0..32 {
            assert_eq!(
                enumerates_waitpkg([0, 0, 1 << bit, 0]),
                bit == 5,
                "CPUID.(7,0):ECX[{bit}]"
            );
        }
        // The other registers of the leaf enumerate none.
        assert!(!enumerates_waitpkg([!0, !0, 0, !0]));
    }

    /// Returns the partition capabilities of a CPU whose highest basic leaf is
    /// `max_function` and whose `CPUID.(7,0):ECX` is `leaf7_ecx`.
    fn capabilities_with_leaf7_ecx(max_function: u32, leaf7_ecx: u32) -> X86PartitionCapabilities {
        let topology = vm_topology::processor::TopologyBuilder::new_x86()
            .build(1)
            .unwrap();
        let x2apic = topology.apic_mode() != ApicMode::XApic;
        X86PartitionCapabilities::from_cpuid(
            &topology,
            &mut |leaf, subleaf| match (leaf, subleaf) {
                (0, _) => [max_function, 0x756e_6547, 0x6c65_746e, 0x4965_6e69],
                (1, _) => [0, 0, u32::from(x2apic) << 21, 0],
                (7, 0) => [0, 0, leaf7_ecx, 0],
                _ => [0; 4],
            },
        )
        .unwrap()
    }

    #[test]
    fn umwait_control_state_needs_waitpkg_in_a_leaf_that_the_cpu_has() {
        const WAITPKG: u32 = 1 << 5;
        assert!(!capabilities_with_leaf7_ecx(0xd, 0).umwait_control);
        assert!(!capabilities_with_leaf7_ecx(0xd, !WAITPKG).umwait_control);
        assert!(capabilities_with_leaf7_ecx(0xd, WAITPKG).umwait_control);
        // A leaf above the highest is not CPUID's to enumerate.
        assert!(!capabilities_with_leaf7_ecx(6, WAITPKG).umwait_control);
    }

    /// The pinned profiles whose CPUID enumerates WAITPKG: the Alder Lake and
    /// Granite Rapids CPUs have it, and the Skylake-SP, Ice Lake-SP, Emerald
    /// Rapids, and AMD profiles hide it.
    #[test]
    #[cfg(guest_arch = "x86_64")]
    fn umwait_control_state_is_present_for_the_profiles_that_enumerate_waitpkg() {
        for (id, caps) in pinned_profile_capabilities() {
            assert_eq!(
                caps.umwait_control,
                matches!(id, "intel.alderlake.v1" | "intel.graniterapids.v1"),
                "{id}"
            );
        }
    }

    /// No CPUID bit decides the state that every CPU has, so `from_cpuid`
    /// leaves its capabilities clear, whatever the CPU enumerates, for a
    /// backend that can access the state to set.
    #[test]
    fn state_that_no_cpuid_bit_decides_is_left_to_the_backend() {
        let caps = capabilities_with_leaf7_ecx(0xd, !0);
        assert!(!caps.debug_ctl);
        assert!(!caps.misc_msrs);
        assert!(!caps.machine_check);
    }

    #[test]
    fn mpx_is_bit_14_of_the_extended_features_ebx() {
        for bit in 0..32 {
            assert_eq!(
                enumerates_mpx([0, 1 << bit, 0, 0]),
                bit == 14,
                "CPUID.(7,0):EBX[{bit}]"
            );
        }
        // The other registers of the leaf enumerate none.
        assert!(!enumerates_mpx([!0, 0, !0, !0]));
    }

    #[test]
    fn bndcfgs_state_needs_mpx_in_a_leaf_that_the_cpu_has() {
        const MPX: u32 = 1 << 14;
        let has_bndcfgs = |max_function, leaf7_ebx| {
            let topology = vm_topology::processor::TopologyBuilder::new_x86()
                .build(1)
                .unwrap();
            let x2apic = topology.apic_mode() != ApicMode::XApic;
            X86PartitionCapabilities::from_cpuid(&topology, &mut |leaf, subleaf| match (
                leaf, subleaf,
            ) {
                (0, _) => [max_function, 0x756e_6547, 0x6c65_746e, 0x4965_6e69],
                (1, _) => [0, 0, u32::from(x2apic) << 21, 0],
                (7, 0) => [0, leaf7_ebx, 0, 0],
                _ => [0; 4],
            })
            .unwrap()
            .bndcfgs
        };
        assert!(!has_bndcfgs(0xd, 0));
        assert!(!has_bndcfgs(0xd, !MPX));
        assert!(has_bndcfgs(0xd, MPX));
        // A leaf above the highest is not CPUID's to enumerate.
        assert!(!has_bndcfgs(6, MPX));
    }

    /// The pinned profiles whose CPUID enumerates MPX: the Skylake-SP CPUs
    /// have it, and every later Intel generation and every AMD one lacks it.
    #[test]
    #[cfg(guest_arch = "x86_64")]
    fn bndcfgs_state_is_present_for_the_profiles_that_enumerate_mpx() {
        for (id, caps) in pinned_profile_capabilities() {
            assert_eq!(caps.bndcfgs, id == "intel.skylake-sp.v1", "{id}");
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
