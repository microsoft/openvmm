// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

#![expect(non_camel_case_types, non_snake_case, non_upper_case_globals)]

mod arm64;
mod x64;

#[cfg(target_arch = "aarch64")]
pub use arm64::*;
#[cfg(target_arch = "x86_64")]
pub use x64::*;

use std::ffi::c_void;
use std::fmt::Debug;
use std::fmt::Display;
use winapi::shared::ntdef::LUID;

macro_rules! bitops_base {
    ($t:ty) => {
        impl std::ops::BitOr for $t {
            type Output = Self;
            fn bitor(mut self, rhs: Self) -> Self {
                self |= rhs;
                self
            }
        }

        impl std::ops::BitAnd for $t {
            type Output = Self;
            fn bitand(mut self, rhs: Self) -> Self {
                self &= rhs;
                self
            }
        }
    };
}

#[cfg(target_arch = "x86_64")]
pub(crate) use bitops_base;

macro_rules! bitops {
    ($t:ty) => {
        bitops_base!($t);
        impl $t {
            pub fn is_empty(&self) -> bool {
                self.0 == 0
            }
            pub fn is_set(&self, v: Self) -> bool {
                self.0 & v.0 == v.0
            }
        }
        impl std::ops::BitOrAssign for $t {
            fn bitor_assign(&mut self, rhs: Self) {
                self.0 |= rhs.0;
            }
        }
        impl std::ops::BitAndAssign for $t {
            fn bitand_assign(&mut self, rhs: Self) {
                self.0 &= rhs.0;
            }
        }
        impl std::ops::Not for $t {
            type Output = Self;

            fn not(self) -> Self {
                Self(!self.0)
            }
        }
    };
}

#[cfg(target_arch = "x86_64")]
pub(crate) use bitops;

#[repr(C)]
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct WHV_CAPABILITY_CODE(pub u32);

// Capabilities of the API implementation
pub const WHvCapabilityCodeHypervisorPresent: WHV_CAPABILITY_CODE = WHV_CAPABILITY_CODE(0x00000000);
pub const WHvCapabilityCodeFeatures: WHV_CAPABILITY_CODE = WHV_CAPABILITY_CODE(0x00000001);
pub const WHvCapabilityCodeExtendedVmExits: WHV_CAPABILITY_CODE = WHV_CAPABILITY_CODE(0x00000002);
#[cfg(target_arch = "x86_64")]
pub const WHvCapabilityCodeExceptionExitBitmap: WHV_CAPABILITY_CODE =
    WHV_CAPABILITY_CODE(0x00000003);
#[cfg(target_arch = "x86_64")]
pub const WHvCapabilityCodeX64MsrExitBitmap: WHV_CAPABILITY_CODE = WHV_CAPABILITY_CODE(0x00000004);
pub const WHvCapabilityCodeGpaRangePopulateFlags: WHV_CAPABILITY_CODE =
    WHV_CAPABILITY_CODE(0x00000005);
pub const WHvCapabilityCodeSchedulerFeatures: WHV_CAPABILITY_CODE = WHV_CAPABILITY_CODE(0x00000006);

// Capabilities of the system's processor
pub const WHvCapabilityCodeProcessorVendor: WHV_CAPABILITY_CODE = WHV_CAPABILITY_CODE(0x00001000);
pub const WHvCapabilityCodeProcessorFeatures: WHV_CAPABILITY_CODE = WHV_CAPABILITY_CODE(0x00001001);
pub const WHvCapabilityCodeProcessorClFlushSize: WHV_CAPABILITY_CODE =
    WHV_CAPABILITY_CODE(0x00001002);
#[cfg(target_arch = "x86_64")]
pub const WHvCapabilityCodeProcessorXsaveFeatures: WHV_CAPABILITY_CODE =
    WHV_CAPABILITY_CODE(0x00001003);
pub const WHvCapabilityCodeProcessorClockFrequency: WHV_CAPABILITY_CODE =
    WHV_CAPABILITY_CODE(0x00001004);
#[cfg(target_arch = "x86_64")]
pub const WHvCapabilityCodeInterruptClockFrequency: WHV_CAPABILITY_CODE =
    WHV_CAPABILITY_CODE(0x00001005);
pub const WHvCapabilityCodeProcessorFeaturesBanks: WHV_CAPABILITY_CODE =
    WHV_CAPABILITY_CODE(0x00001006);
pub const WHvCapabilityCodeProcessorFrequencyCap: WHV_CAPABILITY_CODE =
    WHV_CAPABILITY_CODE(0x00001007);
pub const WHvCapabilityCodeSyntheticProcessorFeaturesBanks: WHV_CAPABILITY_CODE =
    WHV_CAPABILITY_CODE(0x00001008);
#[cfg(target_arch = "x86_64")]
pub const WHvCapabilityCodePerfmonFeatures: WHV_CAPABILITY_CODE = WHV_CAPABILITY_CODE(0x00001009);

pub struct WHV_CAPABILITY_FEATURES(pub u64);
bitops!(WHV_CAPABILITY_FEATURES);

impl WHV_CAPABILITY_FEATURES {
    pub const PartialUnmap: Self = Self(1 << 0);
    #[cfg(target_arch = "x86_64")]
    pub const LocalApicEmulation: Self = Self(1 << 1);
    #[cfg(target_arch = "x86_64")]
    pub const Xsave: Self = Self(1 << 2);
    pub const DirtyPageTracking: Self = Self(1 << 3);
    pub const SpeculationControl: Self = Self(1 << 4);
    #[cfg(target_arch = "x86_64")]
    pub const ApicRemoteRead: Self = Self(1 << 5);
    pub const IdleSuspend: Self = Self(1 << 6);
    pub const VirtualPciDeviceSupport: Self = Self(1 << 7);
    pub const IommuSupport: Self = Self(1 << 8);
    pub const VpHotAddSupport: Self = Self(1 << 9);
}

#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct WHV_PROCESSOR_VENDOR(pub u32);

pub const WHvProcessorVendorAmd: WHV_PROCESSOR_VENDOR = WHV_PROCESSOR_VENDOR(0x0000);
pub const WHvProcessorVendorIntel: WHV_PROCESSOR_VENDOR = WHV_PROCESSOR_VENDOR(0x0001);
pub const WHvProcessorVendorHygon: WHV_PROCESSOR_VENDOR = WHV_PROCESSOR_VENDOR(0x0002);
pub const WHvProcessorVendorArm: WHV_PROCESSOR_VENDOR = WHV_PROCESSOR_VENDOR(0x0010);

#[repr(C)]
#[derive(Debug, Copy, Clone)]
pub struct WHV_CAPABILITY_PROCESSOR_FREQUENCY_CAP {
    pub Flags: u32,
    pub HighestFrequencyMhz: u32,
    pub NominalFrequencyMhz: u32,
    pub LowestFrequencyMhz: u32,
    pub FrequencyStepMhz: u32,
}

impl WHV_CAPABILITY_PROCESSOR_FREQUENCY_CAP {
    pub fn IsSupported(&self) -> bool {
        (self.Flags & 1) != 0
    }
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct WHV_PARTITION_HANDLE(pub isize);

impl Debug for WHV_PARTITION_HANDLE {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WHV_PARTITION_HANDLE").finish()
    }
}

#[repr(C)]
#[derive(Debug, Copy, Clone)]
pub struct WHV_MAP_GPA_RANGE_FLAGS(pub u32);
bitops!(WHV_MAP_GPA_RANGE_FLAGS);

pub const WHvMapGpaRangeFlagNone: WHV_MAP_GPA_RANGE_FLAGS = WHV_MAP_GPA_RANGE_FLAGS(0x00000000);
pub const WHvMapGpaRangeFlagRead: WHV_MAP_GPA_RANGE_FLAGS = WHV_MAP_GPA_RANGE_FLAGS(0x00000001);
pub const WHvMapGpaRangeFlagWrite: WHV_MAP_GPA_RANGE_FLAGS = WHV_MAP_GPA_RANGE_FLAGS(0x00000002);
pub const WHvMapGpaRangeFlagExecute: WHV_MAP_GPA_RANGE_FLAGS = WHV_MAP_GPA_RANGE_FLAGS(0x00000004);
pub const WHvMapGpaRangeFlagTrackDirtyPages: WHV_MAP_GPA_RANGE_FLAGS =
    WHV_MAP_GPA_RANGE_FLAGS(0x00000008);

#[repr(transparent)]
#[derive(Debug, Copy, Clone)]
pub struct WHV_PARTITION_PROPERTY_CODE(u32);

pub const WHvPartitionPropertyCodeExtendedVmExits: WHV_PARTITION_PROPERTY_CODE =
    WHV_PARTITION_PROPERTY_CODE(0x00000001);
#[cfg(target_arch = "x86_64")]
pub const WHvPartitionPropertyCodeExceptionExitBitmap: WHV_PARTITION_PROPERTY_CODE =
    WHV_PARTITION_PROPERTY_CODE(0x00000002);
pub const WHvPartitionPropertyCodeSeparateSecurityDomain: WHV_PARTITION_PROPERTY_CODE =
    WHV_PARTITION_PROPERTY_CODE(0x00000003);
#[cfg(target_arch = "x86_64")]
pub const WHvPartitionPropertyCodeX64MsrExitBitmap: WHV_PARTITION_PROPERTY_CODE =
    WHV_PARTITION_PROPERTY_CODE(0x00000005);
pub const WHvPartitionPropertyCodePrimaryNumaNode: WHV_PARTITION_PROPERTY_CODE =
    WHV_PARTITION_PROPERTY_CODE(0x00000006);
pub const WHvPartitionPropertyCodeCpuReserve: WHV_PARTITION_PROPERTY_CODE =
    WHV_PARTITION_PROPERTY_CODE(0x00000007);
pub const WHvPartitionPropertyCodeCpuCap: WHV_PARTITION_PROPERTY_CODE =
    WHV_PARTITION_PROPERTY_CODE(0x00000008);
pub const WHvPartitionPropertyCodeCpuWeight: WHV_PARTITION_PROPERTY_CODE =
    WHV_PARTITION_PROPERTY_CODE(0x00000009);
pub const WHvPartitionPropertyCodeCpuGroupId: WHV_PARTITION_PROPERTY_CODE =
    WHV_PARTITION_PROPERTY_CODE(0x0000000a);
pub const WHvPartitionPropertyCodeProcessorFrequencyCap: WHV_PARTITION_PROPERTY_CODE =
    WHV_PARTITION_PROPERTY_CODE(0x0000000b);
pub const WHvPartitionPropertyCodeAllowDeviceAssignment: WHV_PARTITION_PROPERTY_CODE =
    WHV_PARTITION_PROPERTY_CODE(0x0000000c);
pub const WHvPartitionPropertyCodeDisableSmt: WHV_PARTITION_PROPERTY_CODE =
    WHV_PARTITION_PROPERTY_CODE(0x0000000d);

pub const WHvPartitionPropertyCodeProcessorFeatures: WHV_PARTITION_PROPERTY_CODE =
    WHV_PARTITION_PROPERTY_CODE(0x00001001);
pub const WHvPartitionPropertyCodeProcessorClFlushSize: WHV_PARTITION_PROPERTY_CODE =
    WHV_PARTITION_PROPERTY_CODE(0x00001002);
#[cfg(target_arch = "x86_64")]
pub const WHvPartitionPropertyCodeCpuidExitList: WHV_PARTITION_PROPERTY_CODE =
    WHV_PARTITION_PROPERTY_CODE(0x00001003);
#[cfg(target_arch = "x86_64")]
pub const WHvPartitionPropertyCodeCpuidResultList: WHV_PARTITION_PROPERTY_CODE =
    WHV_PARTITION_PROPERTY_CODE(0x00001004);
#[cfg(target_arch = "x86_64")]
pub const WHvPartitionPropertyCodeLocalApicEmulationMode: WHV_PARTITION_PROPERTY_CODE =
    WHV_PARTITION_PROPERTY_CODE(0x00001005);
#[cfg(target_arch = "x86_64")]
pub const WHvPartitionPropertyCodeProcessorXsaveFeatures: WHV_PARTITION_PROPERTY_CODE =
    WHV_PARTITION_PROPERTY_CODE(0x00001006);
pub const WHvPartitionPropertyCodeProcessorClockFrequency: WHV_PARTITION_PROPERTY_CODE =
    WHV_PARTITION_PROPERTY_CODE(0x00001007);
#[cfg(target_arch = "x86_64")]
pub const WHvPartitionPropertyCodeInterruptClockFrequency: WHV_PARTITION_PROPERTY_CODE =
    WHV_PARTITION_PROPERTY_CODE(0x00001008);
#[cfg(target_arch = "x86_64")]
pub const WHvPartitionPropertyCodeApicRemoteReadSupport: WHV_PARTITION_PROPERTY_CODE =
    WHV_PARTITION_PROPERTY_CODE(0x00001009);
pub const WHvPartitionPropertyCodeProcessorFeaturesBanks: WHV_PARTITION_PROPERTY_CODE =
    WHV_PARTITION_PROPERTY_CODE(0x0000100A);
pub const WHvPartitionPropertyCodeReferenceTime: WHV_PARTITION_PROPERTY_CODE =
    WHV_PARTITION_PROPERTY_CODE(0x0000100B);
pub const WHvPartitionPropertyCodeSyntheticProcessorFeaturesBanks: WHV_PARTITION_PROPERTY_CODE =
    WHV_PARTITION_PROPERTY_CODE(0x0000100C);
#[cfg(target_arch = "x86_64")]
pub const WHvPartitionPropertyCodeCpuidResultList2: WHV_PARTITION_PROPERTY_CODE =
    WHV_PARTITION_PROPERTY_CODE(0x0000100D);
#[cfg(target_arch = "x86_64")]
pub const WHvPartitionPropertyCodeProcessorPerfmonFeatures: WHV_PARTITION_PROPERTY_CODE =
    WHV_PARTITION_PROPERTY_CODE(0x0000100E);
#[cfg(target_arch = "x86_64")]
pub const WHvPartitionPropertyCodeMsrActionList: WHV_PARTITION_PROPERTY_CODE =
    WHV_PARTITION_PROPERTY_CODE(0x0000100F);
#[cfg(target_arch = "x86_64")]
pub const WHvPartitionPropertyCodeUnimplementedMsrAction: WHV_PARTITION_PROPERTY_CODE =
    WHV_PARTITION_PROPERTY_CODE(0x00001010);
pub const WHvPartitionPropertyCodePhysicalAddressWidth: WHV_PARTITION_PROPERTY_CODE =
    WHV_PARTITION_PROPERTY_CODE(0x00001011);
pub const WHvPartitionPropertyCodeArm64IcParameters: WHV_PARTITION_PROPERTY_CODE =
    WHV_PARTITION_PROPERTY_CODE(0x00001012);
pub const WHvPartitionPropertyCodeProcessorCount: WHV_PARTITION_PROPERTY_CODE =
    WHV_PARTITION_PROPERTY_CODE(0x00001fff);

#[repr(transparent)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct WHV_REGISTER_NAME(pub u32);

#[repr(C)]
#[derive(Clone, Copy, Debug, Eq, PartialEq, Default)]
pub struct WHV_REGISTER_VALUE(pub WHV_UINT128);

#[repr(transparent)]
#[derive(Debug, Copy, Clone)]
pub struct WHV_ADVISE_GPA_RANGE_CODE(u32);

pub const WHvAdviseGpaRangeCodePopulate: WHV_ADVISE_GPA_RANGE_CODE =
    WHV_ADVISE_GPA_RANGE_CODE(0x00000000);
pub const WHvAdviseGpaRangeCodePin: WHV_ADVISE_GPA_RANGE_CODE =
    WHV_ADVISE_GPA_RANGE_CODE(0x00000001);
pub const WHvAdviseGpaRangeCodeUnpin: WHV_ADVISE_GPA_RANGE_CODE =
    WHV_ADVISE_GPA_RANGE_CODE(0x00000002);

#[repr(C)]
#[derive(Copy, Clone)]
pub struct WHV_RUN_VP_EXIT_CONTEXT {
    pub ExitReason: WHV_RUN_VP_EXIT_REASON,
    pub Reserved: u32,
    #[cfg(target_arch = "x86_64")]
    pub VpContext: WHV_VP_EXIT_CONTEXT,
    #[cfg(all(target_arch = "aarch64", feature = "unstable_whp"))]
    pub Reserved1: u64,
    pub u: WHV_RUN_VP_EXIT_CONTEXT_u,
}

#[repr(C)]
#[derive(Debug, Copy, Clone, Default)]
pub struct WHV_ADVISE_GPA_RANGE_POPULATE_FLAGS(pub u32);
bitops!(WHV_ADVISE_GPA_RANGE_POPULATE_FLAGS);

impl WHV_ADVISE_GPA_RANGE_POPULATE_FLAGS {
    pub const Prefetch: Self = Self(0x1);
    pub const AvoidHardFaults: Self = Self(0x2);
}

#[repr(C)]
#[derive(Debug, Copy, Clone)]
pub struct WHV_ADVISE_GPA_RANGE_POPULATE {
    pub Flags: WHV_ADVISE_GPA_RANGE_POPULATE_FLAGS,
    pub AccessType: WHV_MEMORY_ACCESS_TYPE,
}

#[derive(Debug, Copy, Clone, Default, PartialEq, Eq)]
pub struct WHV_EXTENDED_VM_EXITS(pub u64);
bitops!(WHV_EXTENDED_VM_EXITS);

impl WHV_EXTENDED_VM_EXITS {
    #[cfg(target_arch = "x86_64")]
    pub const X64CpuidExit: Self = Self(1 << 0);
    #[cfg(target_arch = "x86_64")]
    pub const X64MsrExit: Self = Self(1 << 1);
    #[cfg(target_arch = "x86_64")]
    pub const ExceptionExit: Self = Self(1 << 2);
    #[cfg(target_arch = "x86_64")]
    pub const X64RdtscExit: Self = Self(1 << 3);
    #[cfg(target_arch = "x86_64")]
    pub const X64ApicSmiExitTrap: Self = Self(1 << 4);
    pub const HypercallExit: Self = Self(1 << 5);
    #[cfg(target_arch = "x86_64")]
    pub const X64ApicInitSipiExitTrap: Self = Self(1 << 6);
    #[cfg(target_arch = "x86_64")]
    pub const X64ApicWriteLint0ExitTrap: Self = Self(1 << 7);
    #[cfg(target_arch = "x86_64")]
    pub const X64ApicWriteLint1ExitTrap: Self = Self(1 << 8);
    #[cfg(target_arch = "x86_64")]
    pub const X64ApicWriteSvrExitTrap: Self = Self(1 << 9);
    pub const UnknownSynicConnection: Self = Self(1 << 10);
    pub const RetargetUnknownVpciDevice: Self = Self(1 << 11);
    #[cfg(target_arch = "x86_64")]
    pub const X64ApicWriteLdrExitTrap: Self = Self(1 << 12);
    #[cfg(target_arch = "x86_64")]
    pub const X64ApicWriteDfrExitTrap: Self = Self(1 << 13);
    pub const GpaAccessFaultExit: Self = Self(1 << 14);
}

#[repr(C)]
#[derive(Debug, Copy, Clone)]
pub struct WHV_PROCESSOR_FEATURES_BANKS {
    pub BanksCount: u32,
    pub Reserved0: u32,
    pub Banks: [u64; 2],
}

#[derive(Debug, Copy, Clone, Default, Eq, PartialEq)]
pub struct WHV_PROCESSOR_FEATURES(pub u64);
bitops!(WHV_PROCESSOR_FEATURES);

#[repr(C)]
#[derive(Debug, Copy, Clone, Default, Eq, PartialEq)]
pub struct WHV_PROCESSOR_FEATURES1(pub u64);
bitops!(WHV_PROCESSOR_FEATURES1);

#[derive(Debug, Copy, Clone)]
pub struct WHV_PROCESSOR_XSAVE_FEATURES(pub u64);
bitops!(WHV_PROCESSOR_XSAVE_FEATURES);

impl WHV_PROCESSOR_XSAVE_FEATURES {
    pub const XsaveSupport: Self = Self(1 << 0);
    pub const XsaveoptSupport: Self = Self(1 << 1);
    pub const AvxSupport: Self = Self(1 << 2);
    pub const Avx2Support: Self = Self(1 << 3);
    pub const FmaSupport: Self = Self(1 << 4);
    pub const MpxSupport: Self = Self(1 << 5);
    pub const Avx512Support: Self = Self(1 << 6);
    pub const Avx512DqSupport: Self = Self(1 << 7);
    pub const Avx512CdSupport: Self = Self(1 << 8);
    pub const Avx512BwSupport: Self = Self(1 << 9);
    pub const Avx512VlSupport: Self = Self(1 << 10);
    pub const XsaveCompSupport: Self = Self(1 << 11);
    pub const XsaveSupervisorSupport: Self = Self(1 << 12);
    pub const Xcr1Support: Self = Self(1 << 13);
    pub const Avx512BitalgSupport: Self = Self(1 << 14);
    pub const Avx512IfmaSupport: Self = Self(1 << 15);
    pub const Avx512VBmiSupport: Self = Self(1 << 16);
    pub const Avx512VBmi2Support: Self = Self(1 << 17);
    pub const Avx512VnniSupport: Self = Self(1 << 18);
    pub const GfniSupport: Self = Self(1 << 19);
    pub const VaesSupport: Self = Self(1 << 20);
    pub const Avx512VPopcntdqSupport: Self = Self(1 << 21);
    pub const VpclmulqdqSupport: Self = Self(1 << 22);
    pub const Avx512Bf16Support: Self = Self(1 << 23);
    pub const Avx512Vp2IntersectSupport: Self = Self(1 << 24);
    pub const Avx512Fp16Support: Self = Self(1 << 25);
    pub const XfdSupport: Self = Self(1 << 26);
    pub const AmxTileSupport: Self = Self(1 << 27);
    pub const AmxBf16Support: Self = Self(1 << 28);
    pub const AmxInt8Support: Self = Self(1 << 29);
    pub const AvxVnniSupport: Self = Self(1 << 30);
}

#[derive(Debug, Default, Copy, Clone, Eq, PartialEq)]
pub struct WHV_SYNTHETIC_PROCESSOR_FEATURES(pub u64);
bitops!(WHV_SYNTHETIC_PROCESSOR_FEATURES);

impl WHV_SYNTHETIC_PROCESSOR_FEATURES {
    /// Report a hypervisor is present. CPUID leaves
    /// 0x40000000 and 0x40000001 are supported.
    pub const HypervisorPresent: Self = Self(1 << 0);

    /// Report support for Hv1 (CPUID leaves 0x40000000 - 0x40000006).
    pub const Hv1: Self = Self(1 << 1);

    /// Access to HV_X64_MSR_VP_RUNTIME.
    /// Corresponds to AccessVpRunTimeReg privilege.
    pub const AccessVpRunTimeReg: Self = Self(1 << 2);

    /// Access to HV_X64_MSR_TIME_REF_COUNT.
    /// Corresponds to AccessPartitionReferenceCounter privilege.
    pub const AccessPartitionReferenceCounter: Self = Self(1 << 3);

    /// Access to SINT-related registers (HV_X64_MSR_SCONTROL through
    /// HV_X64_MSR_EOM and HV_X64_MSR_SINT0 through HV_X64_MSR_SINT15).
    /// Corresponds to AccessSynicRegs privilege.
    pub const AccessSynicRegs: Self = Self(1 << 4);

    /// Access to synthetic timers and associated MSRs
    /// (HV_X64_MSR_STIMER0_CONFIG through HV_X64_MSR_STIMER3_COUNT).
    /// Corresponds to AccessSyntheticTimerRegs privilege.
    pub const AccessSyntheticTimerRegs: Self = Self(1 << 5);

    /// Access to APIC MSRs (HV_X64_MSR_EOI, HV_X64_MSR_ICR and HV_X64_MSR_TPR)
    /// as well as the VP assist page.
    /// Corresponds to AccessIntrCtrlRegs privilege.
    pub const AccessIntrCtrlRegs: Self = Self(1 << 6);

    /// Access to registers associated with hypercalls (HV_X64_MSR_GUEST_OS_ID
    /// and HV_X64_MSR_HYPERCALL).
    /// Corresponds to AccessHypercallMsrs privilege.
    pub const AccessHypercallRegs: Self = Self(1 << 7);

    /// VP index can be queried. Corresponds to AccessVpIndex privilege.
    pub const AccessVpIndex: Self = Self(1 << 8);

    /// Access to the reference TSC. Corresponds to AccessPartitionReferenceTsc
    /// privilege.
    pub const AccessPartitionReferenceTsc: Self = Self(1 << 9);

    /// Partition has access to the guest idle reg. Corresponds to
    /// AccessGuestIdleReg privilege.
    ///
    #[cfg(target_arch = "x86_64")]
    pub const AccessGuestIdleReg: Self = Self(1 << 10);

    /// Partition has access to frequency regs. Corresponds to AccessFrequencyRegs
    /// privilege.
    #[cfg(target_arch = "x86_64")]
    pub const AccessFrequencyRegs: Self = Self(1 << 11);

    // pub const ReservedZ12: Self = Self(1 << 12);
    // pub const ReservedZ13: Self = Self(1 << 13);
    // pub const ReservedZ14: Self = Self(1 << 14);

    /// Extended GVA ranges for HvCallFlushVirtualAddressList hypercall.
    /// Corresponds to privilege.
    #[cfg(target_arch = "x86_64")]
    pub const EnableExtendedGvaRangesForFlushVirtualAddressList: Self = Self(1 << 15);

    // pub const ReservedZ16: Self = Self(1 << 16);
    // pub const ReservedZ17: Self = Self(1 << 17);

    /// Use fast hypercall output. Corresponds to privilege.
    pub const FastHypercallOutput: Self = Self(1 << 18);

    // pub const ReservedZ19: Self = Self(1 << 19);

    // pub const ReservedZ20: Self = Self(1 << 20);

    // pub const ReservedZ21: Self = Self(1 << 21);

    /// Synthetic timers in direct mode.
    pub const DirectSyntheticTimers: Self = Self(1 << 22);

    // pub const ReservedZ23: Self = Self(1 << 23);

    /// Use extended processor masks.
    pub const ExtendedProcessorMasks: Self = Self(1 << 24);

    // On AMD64, HvCallFlushVirtualAddressSpace / HvCallFlushVirtualAddressList are supported, on
    // ARM64 HvCallFlushVirtualAddressSpace / HvCallFlushTlb are supported.
    pub const TbFlushHypercalls: Self = Self(1 << 25);

    /// HvCallSendSyntheticClusterIpi is supported.
    pub const SyntheticClusterIpi: Self = Self(1 << 26);

    /// HvCallNotifyLongSpinWait is supported.
    pub const NotifyLongSpinWait: Self = Self(1 << 27);

    /// HvCallQueryNumaDistance is supported.
    pub const QueryNumaDistance: Self = Self(1 << 28);

    /// HvCallSignalEvent is supported. Corresponds to privilege.
    pub const SignalEvents: Self = Self(1 << 29);

    /// HvCallRetargetDeviceInterrupt is supported.
    pub const RetargetDeviceInterrupt: Self = Self(1 << 30);

    /// HvCallRestorePartitionTime is supported.
    #[cfg(target_arch = "x86_64")]
    pub const RestoreTime: Self = Self(1 << 31);

    /// EnlightenedVmcs nested enlightenment is supported.
    #[cfg(target_arch = "x86_64")]
    pub const EnlightenedVmcs: Self = Self(1 << 32);

    /// Non-zero values can be written to DEBUG_CTL.
    #[cfg(target_arch = "x86_64")]
    pub const NestedDebugCtl: Self = Self(1 << 33);

    /// Synthetic time-unhalted timer MSRs are supported.
    #[cfg(target_arch = "x86_64")]
    pub const SyntheticTimeUnhaltedTimer: Self = Self(1 << 34);

    /// SPEC_CTRL MSR behavior when the VP is idle
    #[cfg(target_arch = "x86_64")]
    pub const IdleSpecCtrl: Self = Self(1 << 35);

    /// Register intercepts supported in V1. As more registers are supported in the future
    /// releases, new bits will be added here to prevent migration between incompatible hosts.
    ///
    /// List of registers supported in V1.
    /// 1. TPIDRRO_EL0
    /// 2. TPIDR_EL1
    /// 3. SCTLR_EL1 - Supports write intercept mask.
    /// 4. VBAR_EL1
    /// 5. TCR_EL1 - Supports write intercept mask.
    /// 6. MAIR_EL1 - Supports write intercept mask.
    /// 7. CPACR_EL1 - Supports write intercept mask.
    /// 8. CONTEXTIDR_EL1
    /// 9. PAuth keys(total 10 registers)
    /// 10. HvArm64RegisterSyntheticException
    #[cfg(target_arch = "aarch64")]
    pub const RegisterInterceptsV1: Self = Self(1 << 36);

    /// HvCallWakeVps is supported.
    pub const WakeVps: Self = Self(1 << 37);

    /// HvCallGet/SetVpRegisters is supported.
    /// Corresponds to the AccessVpRegisters privilege.
    /// This feature only affects exo partitions.
    pub const AccessVpRegs: Self = Self(1 << 38);

    /// HvCallSyncContext/Ex is supported.
    #[cfg(target_arch = "aarch64")]
    pub const SyncContext: Self = Self(1 << 39);

    /// Management VTL synic support is allowed.
    /// Corresponds to the ManagementVtlSynicSupport privilege.
    pub const ManagementVtlSynicSupport: Self = Self(1 << 40);

    /// Hypervisor supports guest mechanism to signal pending interrupts to paravisor.
    #[cfg(target_arch = "x86_64")]
    pub const ProxyInterruptDoorbellSupport: Self = Self(1 << 41);

    /// InterceptSystemResetAvailable is exposed.
    #[cfg(target_arch = "aarch64")]
    pub const InterceptSystemReset: Self = Self(1 << 42);

    /// Hypercalls for host MMIO operations are available.
    pub const MmioHypercalls: Self = Self(1 << 43);

    /// SPIs are advertised to VTL2.
    #[cfg(target_arch = "aarch64")]
    pub const ManagementVtlSpiSupport: Self = Self(1 << 44);
}

#[repr(C)]
#[derive(Debug, Copy, Clone)]
pub struct WHV_SYNTHETIC_PROCESSOR_FEATURES_BANKS {
    pub BanksCount: u32,
    pub Reserved0: u32,
    pub Banks: [u64; 1],
}

#[repr(C)]
#[derive(Debug, Copy, Clone)]
pub struct WHV_TRANSLATE_GVA_RESULT {
    pub ResultCode: WHV_TRANSLATE_GVA_RESULT_CODE,
    pub Reserved: u32,
}

#[repr(transparent)]
#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub struct WHV_TRANSLATE_GVA_FLAGS(pub u32);
bitops!(WHV_TRANSLATE_GVA_FLAGS);

pub const WHvTranslateGvaFlagNone: WHV_TRANSLATE_GVA_FLAGS = WHV_TRANSLATE_GVA_FLAGS(0x00000000);
pub const WHvTranslateGvaFlagValidateRead: WHV_TRANSLATE_GVA_FLAGS =
    WHV_TRANSLATE_GVA_FLAGS(0x00000001);
pub const WHvTranslateGvaFlagValidateWrite: WHV_TRANSLATE_GVA_FLAGS =
    WHV_TRANSLATE_GVA_FLAGS(0x00000002);
pub const WHvTranslateGvaFlagValidateExecute: WHV_TRANSLATE_GVA_FLAGS =
    WHV_TRANSLATE_GVA_FLAGS(0x00000004);
#[cfg(target_arch = "x86_64")]
pub const WHvTranslateGvaFlagPrivilegeExempt: WHV_TRANSLATE_GVA_FLAGS =
    WHV_TRANSLATE_GVA_FLAGS(0x00000008);
pub const WHvTranslateGvaFlagSetPageTableBits: WHV_TRANSLATE_GVA_FLAGS =
    WHV_TRANSLATE_GVA_FLAGS(0x00000010);

#[repr(C)]
#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub struct WHV_TRANSLATE_GVA_RESULT_CODE(pub u32);

pub const WHvTranslateGvaResultSuccess: WHV_TRANSLATE_GVA_RESULT_CODE =
    WHV_TRANSLATE_GVA_RESULT_CODE(0);

// Translation failures
pub const WHvTranslateGvaResultPageNotPresent: WHV_TRANSLATE_GVA_RESULT_CODE =
    WHV_TRANSLATE_GVA_RESULT_CODE(1);
pub const WHvTranslateGvaResultPrivilegeViolation: WHV_TRANSLATE_GVA_RESULT_CODE =
    WHV_TRANSLATE_GVA_RESULT_CODE(2);
pub const WHvTranslateGvaResultInvalidPageTableFlags: WHV_TRANSLATE_GVA_RESULT_CODE =
    WHV_TRANSLATE_GVA_RESULT_CODE(3);

// GPA access failures
pub const WHvTranslateGvaResultGpaUnmapped: WHV_TRANSLATE_GVA_RESULT_CODE =
    WHV_TRANSLATE_GVA_RESULT_CODE(4);
pub const WHvTranslateGvaResultGpaNoReadAccess: WHV_TRANSLATE_GVA_RESULT_CODE =
    WHV_TRANSLATE_GVA_RESULT_CODE(5);
pub const WHvTranslateGvaResultGpaNoWriteAccess: WHV_TRANSLATE_GVA_RESULT_CODE =
    WHV_TRANSLATE_GVA_RESULT_CODE(6);
pub const WHvTranslateGvaResultGpaIllegalOverlayAccess: WHV_TRANSLATE_GVA_RESULT_CODE =
    WHV_TRANSLATE_GVA_RESULT_CODE(7);
pub const WHvTranslateGvaResultIntercept: WHV_TRANSLATE_GVA_RESULT_CODE =
    WHV_TRANSLATE_GVA_RESULT_CODE(8);

#[repr(C)]
#[derive(Debug, Copy, Clone)]
pub struct WHV_DOORBELL_MATCH_DATA {
    pub GuestAddress: u64,
    pub Value: u64,
    pub Length: u32,
    pub Flags: u32,
}

#[repr(C)]
#[derive(Debug, Copy, Clone)]
pub struct WHV_MEMORY_RANGE_ENTRY {
    pub GuestAddress: u64,
    pub SizeInBytes: u64,
}

#[repr(C)]
#[derive(Debug, Copy, Clone)]
pub struct WHV_SYNIC_EVENT_PARAMETERS {
    pub VpIndex: u32,
    pub TargetSint: u8,
    pub Reserved: u8,
    pub FlagNumber: u16,
}

#[repr(C)]
#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub struct WHV_RUN_VP_EXIT_REASON(pub u32);

#[repr(transparent)]
#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub struct WHV_MEMORY_ACCESS_TYPE(pub u32);

pub const WHvMemoryAccessRead: WHV_MEMORY_ACCESS_TYPE = WHV_MEMORY_ACCESS_TYPE(0);
pub const WHvMemoryAccessWrite: WHV_MEMORY_ACCESS_TYPE = WHV_MEMORY_ACCESS_TYPE(1);
pub const WHvMemoryAccessExecute: WHV_MEMORY_ACCESS_TYPE = WHV_MEMORY_ACCESS_TYPE(2);

impl Display for WHV_MEMORY_ACCESS_TYPE {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let string = match *self {
            WHvMemoryAccessRead => "read",
            WHvMemoryAccessWrite => "write",
            WHvMemoryAccessExecute => "execute",
            _ => "unknown",
        };

        f.write_str(string)
    }
}

#[repr(C, align(16))]
#[derive(Default, Debug, Copy, Clone, PartialEq, Eq)]
pub struct WHV_UINT128([u8; 16]);

impl From<u128> for WHV_UINT128 {
    fn from(v: u128) -> Self {
        Self(v.to_ne_bytes())
    }
}

impl From<u64> for WHV_UINT128 {
    fn from(v: u64) -> Self {
        (v as u128).into()
    }
}

impl From<u32> for WHV_UINT128 {
    fn from(v: u32) -> Self {
        (v as u128).into()
    }
}

impl From<u16> for WHV_UINT128 {
    fn from(v: u16) -> Self {
        (v as u128).into()
    }
}

impl From<u8> for WHV_UINT128 {
    fn from(v: u8) -> Self {
        (v as u128).into()
    }
}

impl From<WHV_UINT128> for u128 {
    fn from(v: WHV_UINT128) -> Self {
        u128::from_ne_bytes(v.0)
    }
}

#[repr(C)]
#[derive(Debug, Copy, Clone)]
pub struct WHV_NOTIFICATION_PORT_HANDLE(pub isize);

#[repr(C)]
#[derive(Debug, Copy, Clone)]
pub struct WHV_NOTIFICATION_PORT_PROPERTY_CODE(pub u32);

pub const WHvNotificationPortPropertyPreferredTargetVp: WHV_NOTIFICATION_PORT_PROPERTY_CODE =
    WHV_NOTIFICATION_PORT_PROPERTY_CODE(1);
pub const WHvNotificationPortPropertyPreferredTargetDuration: WHV_NOTIFICATION_PORT_PROPERTY_CODE =
    WHV_NOTIFICATION_PORT_PROPERTY_CODE(5);

#[repr(C)]
#[derive(Debug, Copy, Clone)]
pub struct WHV_NOTIFICATION_PORT_TYPE(pub u32);

pub const WHvNotificationPortTypeEvent: WHV_NOTIFICATION_PORT_TYPE = WHV_NOTIFICATION_PORT_TYPE(2);
pub const WHvNotificationPortTypeDoorbell: WHV_NOTIFICATION_PORT_TYPE =
    WHV_NOTIFICATION_PORT_TYPE(4);

#[repr(C)]
#[derive(Copy, Clone)]
pub struct WHV_NOTIFICATION_PORT_PARAMETERS {
    pub NotificationPortType: WHV_NOTIFICATION_PORT_TYPE,
    pub Reserved: u32,
    pub u: WHV_NOTIFICATION_PORT_PARAMETERS_u,
}

#[repr(C)]
#[derive(Copy, Clone)]
pub union WHV_NOTIFICATION_PORT_PARAMETERS_u {
    pub Doorbell: WHV_DOORBELL_MATCH_DATA,
    pub Event: WHV_NOTIFICATION_PORT_PARAMETERS_u_Event,
}

#[repr(C)]
#[derive(Copy, Clone)]
pub struct WHV_NOTIFICATION_PORT_PARAMETERS_u_Event {
    pub ConnectionId: u32,
}

#[repr(C)]
#[derive(Debug, Copy, Clone)]
pub struct WHV_CPUID_OUTPUT {
    pub Eax: u32,
    pub Ebx: u32,
    pub Ecx: u32,
    pub Edx: u32,
}

#[repr(C)]
#[derive(Debug, Copy, Clone)]
pub struct WHV_VIRTUAL_PROCESSOR_PROPERTY_CODE(u32);

pub const WHvVirtualProcessorPropertyCodeNumaNode: WHV_VIRTUAL_PROCESSOR_PROPERTY_CODE =
    WHV_VIRTUAL_PROCESSOR_PROPERTY_CODE(0x00000000);

#[repr(C)]
#[derive(Copy, Clone)]
pub struct WHV_VIRTUAL_PROCESSOR_PROPERTY {
    pub PropertyCode: WHV_VIRTUAL_PROCESSOR_PROPERTY_CODE,
    pub Reserved: u32,
    pub u: WHV_VIRTUAL_PROCESSOR_PROPERTY_u,
}

#[repr(C)]
#[derive(Copy, Clone)]
pub union WHV_VIRTUAL_PROCESSOR_PROPERTY_u {
    pub NumaNode: u16,
    pub Padding: u64,
}

#[repr(C)]
#[derive(Debug, Copy, Clone)]
pub struct WHV_MSR_ACTION_ENTRY {
    pub Index: u32,
    pub ReadAction: u8,  // WHV_MSR_ACTION
    pub WriteAction: u8, // WHV_MSR_ACTION
    pub Reserved: u16,
}

#[repr(C)]
#[derive(Debug, Copy, Clone)]
pub struct WHV_MSR_ACTION(pub u32);

pub const WHvMsrActionArchitectureDefault: WHV_MSR_ACTION = WHV_MSR_ACTION(0);
pub const WHvMsrActionIgnoreWriteReadZero: WHV_MSR_ACTION = WHV_MSR_ACTION(1);
pub const WHvMsrActionExit: WHV_MSR_ACTION = WHV_MSR_ACTION(2);

#[repr(C)]
#[derive(Debug, Copy, Clone)]
pub struct WHV_SCHEDULER_FEATURES(pub u32);
bitops!(WHV_SCHEDULER_FEATURES);

impl WHV_SCHEDULER_FEATURES {
    pub const CpuReserve: Self = Self(1 << 0);
    pub const CpuCap: Self = Self(1 << 1);
    pub const CpuWeight: Self = Self(1 << 2);
    pub const CpuGroupId: Self = Self(1 << 3);
    pub const DisableSmt: Self = Self(1 << 4);
}

#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
pub struct WHV_ALLOCATE_VPCI_RESOURCE_FLAGS(pub u32);
bitops!(WHV_ALLOCATE_VPCI_RESOURCE_FLAGS);

pub const WHvAllocateVpciResourceFlagAllowDirectP2P: WHV_ALLOCATE_VPCI_RESOURCE_FLAGS =
    WHV_ALLOCATE_VPCI_RESOURCE_FLAGS(0x00000001);

pub const WHV_MAX_DEVICE_ID_SIZE_IN_CHARS: usize = 200;

#[repr(C)]
#[derive(Clone, Copy)]
pub struct WHV_SRIOV_RESOURCE_DESCRIPTOR {
    pub PnpInstanceId: [u16; WHV_MAX_DEVICE_ID_SIZE_IN_CHARS],
    pub VirtualFunctionId: LUID,
    pub VirtualFunctionIndex: u16,
    pub Reserved: u16,
}

#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WHV_VPCI_DEVICE_NOTIFICATION_TYPE(pub u32);

pub const WHvVpciDeviceNotificationUndefined: WHV_VPCI_DEVICE_NOTIFICATION_TYPE =
    WHV_VPCI_DEVICE_NOTIFICATION_TYPE(0);
pub const WHvVpciDeviceNotificationMmioRemapping: WHV_VPCI_DEVICE_NOTIFICATION_TYPE =
    WHV_VPCI_DEVICE_NOTIFICATION_TYPE(1);
pub const WHvVpciDeviceNotificationSurpriseRemoval: WHV_VPCI_DEVICE_NOTIFICATION_TYPE =
    WHV_VPCI_DEVICE_NOTIFICATION_TYPE(2);

#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct WHV_VPCI_DEVICE_NOTIFICATION {
    pub NotificationType: WHV_VPCI_DEVICE_NOTIFICATION_TYPE,
    pub Reserved1: u32,
    pub Reserved2: u64,
}

#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
pub struct WHV_CREATE_VPCI_DEVICE_FLAGS(u32);
bitops!(WHV_CREATE_VPCI_DEVICE_FLAGS);

pub const WHvCreateVpciDeviceFlagNone: WHV_CREATE_VPCI_DEVICE_FLAGS =
    WHV_CREATE_VPCI_DEVICE_FLAGS(0x00000000);
pub const WHvCreateVpciDeviceFlagPhysicallyBacked: WHV_CREATE_VPCI_DEVICE_FLAGS =
    WHV_CREATE_VPCI_DEVICE_FLAGS(0x00000001);
pub const WHvCreateVpciDeviceFlagUseLogicalInterrupts: WHV_CREATE_VPCI_DEVICE_FLAGS =
    WHV_CREATE_VPCI_DEVICE_FLAGS(0x00000002);

#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct WHV_VPCI_DEVICE_PROPERTY_CODE(u32);

pub const WHvVpciDevicePropertyCodeUndefined: WHV_VPCI_DEVICE_PROPERTY_CODE =
    WHV_VPCI_DEVICE_PROPERTY_CODE(0);
pub const WHvVpciDevicePropertyCodeHardwareIDs: WHV_VPCI_DEVICE_PROPERTY_CODE =
    WHV_VPCI_DEVICE_PROPERTY_CODE(1);
pub const WHvVpciDevicePropertyCodeProbedBARs: WHV_VPCI_DEVICE_PROPERTY_CODE =
    WHV_VPCI_DEVICE_PROPERTY_CODE(2);

#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
pub struct WHV_VPCI_HARDWARE_IDS {
    pub VendorID: u16,
    pub DeviceID: u16,
    pub RevisionID: u8,
    pub ProgIf: u8,
    pub SubClass: u8,
    pub BaseClass: u8,
    pub SubVendorID: u16,
    pub SubSystemID: u16,
}

#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
pub struct WHV_VPCI_PROBED_BARS {
    pub Value: [u32; 6],
}

#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
pub struct WHV_VPCI_MMIO_RANGE_FLAGS(pub u32);
bitops!(WHV_VPCI_MMIO_RANGE_FLAGS);

pub const WHvVpciMmioRangeFlagReadAccess: WHV_VPCI_MMIO_RANGE_FLAGS =
    WHV_VPCI_MMIO_RANGE_FLAGS(0x00000001);
pub const WHvVpciMmioRangeFlagWriteAccess: WHV_VPCI_MMIO_RANGE_FLAGS =
    WHV_VPCI_MMIO_RANGE_FLAGS(0x00000002);

#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct WHV_VPCI_DEVICE_REGISTER_SPACE(pub i32);

pub const WHvVpciConfigSpace: WHV_VPCI_DEVICE_REGISTER_SPACE = WHV_VPCI_DEVICE_REGISTER_SPACE(-1);
pub const WHvVpciBar0: WHV_VPCI_DEVICE_REGISTER_SPACE = WHV_VPCI_DEVICE_REGISTER_SPACE(0);
pub const WHvVpciBar1: WHV_VPCI_DEVICE_REGISTER_SPACE = WHV_VPCI_DEVICE_REGISTER_SPACE(1);
pub const WHvVpciBar2: WHV_VPCI_DEVICE_REGISTER_SPACE = WHV_VPCI_DEVICE_REGISTER_SPACE(2);
pub const WHvVpciBar3: WHV_VPCI_DEVICE_REGISTER_SPACE = WHV_VPCI_DEVICE_REGISTER_SPACE(3);
pub const WHvVpciBar4: WHV_VPCI_DEVICE_REGISTER_SPACE = WHV_VPCI_DEVICE_REGISTER_SPACE(4);
pub const WHvVpciBar5: WHV_VPCI_DEVICE_REGISTER_SPACE = WHV_VPCI_DEVICE_REGISTER_SPACE(5);

#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct WHV_VPCI_MMIO_MAPPING {
    pub Location: WHV_VPCI_DEVICE_REGISTER_SPACE,
    pub Flags: WHV_VPCI_MMIO_RANGE_FLAGS,
    pub SizeInBytes: u64,
    pub OffsetInBytes: u64,
    pub VirtualAddress: *mut c_void,
}

#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct WHV_VPCI_DEVICE_REGISTER {
    pub Location: WHV_VPCI_DEVICE_REGISTER_SPACE,
    pub SizeInBytes: u32,
    pub OffsetInBytes: u64,
}

#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct WHV_VPCI_INTERRUPT_TARGET {
    pub Vector: u32,
    pub Flags: WHV_VPCI_INTERRUPT_TARGET_FLAGS,
    pub ProcessorCount: u32,
    pub Processors: [u32; 0],
}

#[repr(C)]
#[derive(Debug, Copy, Clone, Default)]
pub struct WHV_VPCI_INTERRUPT_TARGET_FLAGS(pub u32);
bitops!(WHV_VPCI_INTERRUPT_TARGET_FLAGS);

pub const WHvVpciInterruptTargetFlagMulticast: WHV_VPCI_INTERRUPT_TARGET_FLAGS =
    WHV_VPCI_INTERRUPT_TARGET_FLAGS(1);

#[repr(C)]
#[derive(Copy, Clone, Debug)]
pub struct WHV_TRIGGER_TYPE(u32);

pub const WHvTriggerTypeInterrupt: WHV_TRIGGER_TYPE = WHV_TRIGGER_TYPE(0);
pub const WHvTriggerTypeSynicEvent: WHV_TRIGGER_TYPE = WHV_TRIGGER_TYPE(1);
pub const WHvTriggerTypeDeviceInterrupt: WHV_TRIGGER_TYPE = WHV_TRIGGER_TYPE(2);

#[repr(C)]
#[derive(Copy, Clone)]
pub struct WHV_TRIGGER_PARAMETERS {
    pub TriggerType: WHV_TRIGGER_TYPE,
    pub Reserved: u32,
    pub u: WHV_TRIGGER_PARAMETERS_u,
}

#[repr(C)]
#[derive(Copy, Clone)]
pub union WHV_TRIGGER_PARAMETERS_u {
    #[cfg(target_arch = "x86_64")]
    pub Interrupt: WHV_INTERRUPT_CONTROL,
    pub SynicEvent: WHV_SYNIC_EVENT_PARAMETERS,
    pub DeviceInterrupt: WHV_TRIGGER_PARAMETERS_u_DeviceInterrupt,
}

#[repr(C)]
#[derive(Debug, Copy, Clone)]
pub struct WHV_TRIGGER_PARAMETERS_u_DeviceInterrupt {
    pub LogicalDeviceId: u64,
    pub MsiAddress: u64,
    pub MsiData: u32,
    pub Reserved: u32,
}

#[repr(C)]
#[derive(Debug, Copy, Clone)]
pub struct WHV_TRIGGER_HANDLE(isize);

#[repr(transparent)]
#[derive(Debug, Copy, Clone)]
pub struct WHV_VIRTUAL_PROCESSOR_STATE_TYPE(u32);

pub const WHvVirtualProcessorStateTypeSynicMessagePage: WHV_VIRTUAL_PROCESSOR_STATE_TYPE =
    WHV_VIRTUAL_PROCESSOR_STATE_TYPE(0x00000000);
pub const WHvVirtualProcessorStateTypeSynicEventFlagPage: WHV_VIRTUAL_PROCESSOR_STATE_TYPE =
    WHV_VIRTUAL_PROCESSOR_STATE_TYPE(0x00000001);
pub const WHvVirtualProcessorStateTypeSynicTimerState: WHV_VIRTUAL_PROCESSOR_STATE_TYPE =
    WHV_VIRTUAL_PROCESSOR_STATE_TYPE(0x00000002);
pub const WHvVirtualProcessorStateTypeInterruptControllerState2: WHV_VIRTUAL_PROCESSOR_STATE_TYPE =
    WHV_VIRTUAL_PROCESSOR_STATE_TYPE(0x00001000);
pub const WHvVirtualProcessorStateTypeXsaveState: WHV_VIRTUAL_PROCESSOR_STATE_TYPE =
    WHV_VIRTUAL_PROCESSOR_STATE_TYPE(0x00001001);

#[repr(u32)]
#[derive(Debug, Copy, Clone)]
pub enum WHV_ARM64_IC_EMULATION_MODE {
    None = 0,
    GicV3 = 1,
}

#[repr(C, packed)]
#[derive(Debug, Copy, Clone)]
pub struct WHV_ARM64_IC_GIC_V3_PARAMETERS {
    pub GicdBaseAddress: u64,
    pub GitsTranslatorBaseAddress: u64,
    pub Reserved: u32,
    pub GicLpiIntIdBits: u32,
    pub GicPpiOverflowInterruptFromCntv: u32,
    pub GicPpiPerformanceMonitorsInterrupt: u32,
    pub Reserved1: [u32; 6],
}

// Legacy Hyper-V defaults
pub const DEFAULT_GITS_TRANSLATER_BASE_ADDRESS: u64 = 0;
pub const DEFAULT_GIC_LPI_INT_ID_BITS: u32 = 1;
pub const DEFAULT_GIC_PPI_OVERFLOW_INTERRUPT_FROM_CNTV: u32 = 0x14;

#[repr(C, packed)]
#[derive(Debug, Copy, Clone)]
pub struct WHV_ARM64_IC_PARAMETERS {
    pub EmulationMode: WHV_ARM64_IC_EMULATION_MODE,
    pub Reserved: u32,
    pub GicV3Parameters: WHV_ARM64_IC_GIC_V3_PARAMETERS,
}
