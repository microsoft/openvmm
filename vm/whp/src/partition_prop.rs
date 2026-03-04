// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Module defining associations between partition properties and types.

use crate::abi;
use windows_sys::core::BOOL;

pub trait AssociatedType {
    type Type: ?Sized;
    const CODE: abi::WHV_PARTITION_PROPERTY_CODE;
    fn code(&self) -> abi::WHV_PARTITION_PROPERTY_CODE {
        Self::CODE
    }
}

macro_rules! pp {
    ($($(#[$attr:meta])* ($internal_name:ident, $code:ident, $ty:ty),)*) => {
        $(
            #[allow(dead_code)]
            $(#[$attr])*
            pub struct $internal_name;

            $(#[$attr])*
            impl AssociatedType for $internal_name {
                type Type = $ty;
                const CODE: abi::WHV_PARTITION_PROPERTY_CODE = abi::$code;
            }
        )*
    };
}

pp! {
    (ExtendedVmExits, WHvPartitionPropertyCodeExtendedVmExits, abi::WHV_EXTENDED_VM_EXITS),
    #[cfg(target_arch = "x86_64")]
    (ExceptionExitBitmap, WHvPartitionPropertyCodeExceptionExitBitmap, u64),
    (SeparateSecurityDomain, WHvPartitionPropertyCodeSeparateSecurityDomain, BOOL),
    #[cfg(target_arch = "x86_64")]
    (X64MsrExitBitmap, WHvPartitionPropertyCodeX64MsrExitBitmap, abi::WHV_X64_MSR_EXIT_BITMAP),
    (PrimaryNumaNode, WHvPartitionPropertyCodePrimaryNumaNode, u16),
    (CpuReserve, WHvPartitionPropertyCodeCpuReserve, u32),
    (CpuCap, WHvPartitionPropertyCodeCpuCap, u32),
    (CpuWeight, WHvPartitionPropertyCodeCpuWeight, u32),
    (CpuGroupId, WHvPartitionPropertyCodeCpuGroupId, u64),
    (ProcessorFrequencyCap, WHvPartitionPropertyCodeProcessorFrequencyCap, u32),
    (AllowDeviceAssignment, WHvPartitionPropertyCodeAllowDeviceAssignment, BOOL),
    (DisableSmt, WHvPartitionPropertyCodeDisableSmt, BOOL),

    (ProcessorFeatures, WHvPartitionPropertyCodeProcessorFeatures, abi::WHV_PROCESSOR_FEATURES),
    (ProcessorClFlushSize, WHvPartitionPropertyCodeProcessorClFlushSize, u8),
    #[cfg(target_arch = "x86_64")]
    (CpuidExitList, WHvPartitionPropertyCodeCpuidExitList, [u32]),
    #[cfg(target_arch = "x86_64")]
    (CpuidResultList, WHvPartitionPropertyCodeCpuidResultList, [abi::WHV_X64_CPUID_RESULT]),
    #[cfg(target_arch = "x86_64")]
    (LocalApicEmulationMode, WHvPartitionPropertyCodeLocalApicEmulationMode, abi::WHV_X64_LOCAL_APIC_EMULATION_MODE),
    #[cfg(target_arch = "x86_64")]
    (ProcessorXsaveFeatures, WHvPartitionPropertyCodeProcessorXsaveFeatures, abi::WHV_PROCESSOR_XSAVE_FEATURES),
    (ProcessorClockFrequency, WHvPartitionPropertyCodeProcessorClockFrequency, u64),
    #[cfg(target_arch = "x86_64")]
    (InterruptClockFrequency, WHvPartitionPropertyCodeInterruptClockFrequency, u64),
    #[cfg(target_arch = "x86_64")]
    (ApicRemoteReadSupport, WHvPartitionPropertyCodeApicRemoteReadSupport, BOOL),
    (ProcessorFeaturesBanks, WHvPartitionPropertyCodeProcessorFeaturesBanks, abi::WHV_PROCESSOR_FEATURES_BANKS),
    (ReferenceTime, WHvPartitionPropertyCodeReferenceTime, u64),
    (SyntheticProcessorFeaturesBanks, WHvPartitionPropertyCodeSyntheticProcessorFeaturesBanks, abi::WHV_SYNTHETIC_PROCESSOR_FEATURES_BANKS),
    #[cfg(target_arch = "x86_64")]
    (CpuidResultList2, WHvPartitionPropertyCodeCpuidResultList2, [abi::WHV_X64_CPUID_RESULT2]),
    #[cfg(target_arch = "x86_64")]
    (ProcessorPerfmonFeatures, WHvPartitionPropertyCodeProcessorPerfmonFeatures, abi::WHV_PROCESSOR_PERFMON_FEATURES),
    #[cfg(target_arch = "x86_64")]
    (MsrActionList, WHvPartitionPropertyCodeMsrActionList, [abi::WHV_MSR_ACTION_ENTRY]),
    #[cfg(target_arch = "x86_64")]
    (UnimplementedMsrAction, WHvPartitionPropertyCodeUnimplementedMsrAction, abi::WHV_MSR_ACTION),
    (PhysicalAddressWidth, WHvPartitionPropertyCodePhysicalAddressWidth, u32),
    #[cfg(target_arch = "aarch64")]
    (Arm64IcParameters, WHvPartitionPropertyCodeArm64IcParameters, abi::WHV_ARM64_IC_PARAMETERS),
    (ProcessorCount, WHvPartitionPropertyCodeProcessorCount, u32),
}
