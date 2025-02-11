// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Provides common CPUID values and functions.

use x86defs::cpuid::CpuidFunction;
use zerocopy::FromBytes;
use zerocopy::FromZeros;
use zerocopy::Immutable;
use zerocopy::IntoBytes;
use zerocopy::KnownLayout;

/// CPUID information used to build the CPUID page for SNP.
#[repr(C)]
#[derive(Debug, IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct SNP_REQUIRED_CPUID_LEAF {
    pub eax: u32,
    pub ecx: u32,
}

macro_rules! cpuid_leaf {
    ($eax: expr, $ecx: expr) => {
        SNP_REQUIRED_CPUID_LEAF {
            eax: $eax.0,
            ecx: $ecx,
        }
    };
}

/// The list of static CPUID leaves stored in the CPUID page for SNP without a paravisor.
/// The extended SEV leaf must be first in the list of required CPUID leaves.
pub const SNP_REQUIRED_CPUID_LEAF_LIST_UEFI: [SNP_REQUIRED_CPUID_LEAF; 20] = [
    cpuid_leaf!(CpuidFunction::ExtendedSevFeatures, 0),
    cpuid_leaf!(CpuidFunction::VersionAndFeatures, 0),
    cpuid_leaf!(CpuidFunction::CacheAndTlbInformation, 0),
    cpuid_leaf!(CpuidFunction::MonitorMwait, 0),
    cpuid_leaf!(CpuidFunction::PowerManagement, 0),
    cpuid_leaf!(CpuidFunction::DirectCacheAccessParameters, 0),
    cpuid_leaf!(CpuidFunction::PerformanceMonitoring, 0),
    cpuid_leaf!(CpuidFunction::ExtendedFeatures, 0),
    cpuid_leaf!(CpuidFunction::CacheParameters, 0),
    cpuid_leaf!(CpuidFunction::CacheParameters, 1),
    cpuid_leaf!(CpuidFunction::CacheParameters, 2),
    cpuid_leaf!(CpuidFunction::ExtendedTopologyEnumeration, 0),
    cpuid_leaf!(CpuidFunction::ExtendedTopologyEnumeration, 1),
    cpuid_leaf!(CpuidFunction::ExtendedVersionAndFeatures, 0),
    cpuid_leaf!(CpuidFunction::ExtendedL1CacheParameters, 0),
    cpuid_leaf!(CpuidFunction::ExtendedL2CacheParameters, 0),
    cpuid_leaf!(CpuidFunction::ExtendedPowerManagement, 0),
    cpuid_leaf!(CpuidFunction::ExtendedAddressSpaceSizes, 0),
    cpuid_leaf!(CpuidFunction::ExtendedSvmVersionAndFeatures, 0),
    cpuid_leaf!(CpuidFunction::ProcessorTopologyDefinition, 0),
];

/// The list of static CPUID leaves stored in the CPUID page for SNP with an underhill based
/// paravisor. The extended SEV leaf must be first in the list of required CPUID leaves.
pub const SNP_REQUIRED_CPUID_LEAF_LIST_PARAVISOR: [SNP_REQUIRED_CPUID_LEAF; 35] = [
    cpuid_leaf!(CpuidFunction::ExtendedSevFeatures, 0),
    cpuid_leaf!(CpuidFunction::VendorAndMaxFunction, 0),
    cpuid_leaf!(CpuidFunction::VersionAndFeatures, 0),
    cpuid_leaf!(CpuidFunction::ExtendedMaxFunction, 0),
    cpuid_leaf!(CpuidFunction::MonitorMwait, 0),
    cpuid_leaf!(CpuidFunction::DirectCacheAccessParameters, 0),
    cpuid_leaf!(CpuidFunction::PerformanceMonitoring, 0),
    cpuid_leaf!(CpuidFunction::ExtendedFeatures, 0),
    cpuid_leaf!(CpuidFunction::ExtendedTopologyEnumeration, 0),
    cpuid_leaf!(CpuidFunction::ExtendedTopologyEnumeration, 1),
    cpuid_leaf!(CpuidFunction::ExtendedVersionAndFeatures, 0),
    cpuid_leaf!(CpuidFunction::ExtendedL1CacheParameters, 0),
    cpuid_leaf!(CpuidFunction::ExtendedL2CacheParameters, 0),
    cpuid_leaf!(CpuidFunction::ExtendedPowerManagement, 0),
    cpuid_leaf!(CpuidFunction::ExtendedAddressSpaceSizes, 0),
    cpuid_leaf!(CpuidFunction::ExtendedSvmVersionAndFeatures, 0),
    cpuid_leaf!(CpuidFunction::ProcessorTopologyDefinition, 0),
    cpuid_leaf!(CpuidFunction::ExtendedStateEnumeration, 0),
    cpuid_leaf!(CpuidFunction::ExtendedStateEnumeration, 1),
    cpuid_leaf!(CpuidFunction::ExtendedStateEnumeration, 2),
    cpuid_leaf!(CpuidFunction::ExtendedStateEnumeration, 3),
    cpuid_leaf!(CpuidFunction::ExtendedStateEnumeration, 4),
    cpuid_leaf!(CpuidFunction::ExtendedStateEnumeration, 5),
    cpuid_leaf!(CpuidFunction::ExtendedStateEnumeration, 6),
    cpuid_leaf!(CpuidFunction::ExtendedStateEnumeration, 7),
    cpuid_leaf!(CpuidFunction::ExtendedStateEnumeration, 8),
    cpuid_leaf!(CpuidFunction::ExtendedBrandingString1, 0),
    cpuid_leaf!(CpuidFunction::ExtendedBrandingString2, 0),
    cpuid_leaf!(CpuidFunction::ExtendedBrandingString3, 0),
    cpuid_leaf!(CpuidFunction::ExtendedTlb1GBIdentifiers, 0),
    cpuid_leaf!(CpuidFunction::ExtendedOptimizationIdentifiers, 0),
    cpuid_leaf!(CpuidFunction::CacheTopologyDefinition, 0),
    cpuid_leaf!(CpuidFunction::CacheTopologyDefinition, 1),
    cpuid_leaf!(CpuidFunction::CacheTopologyDefinition, 2),
    cpuid_leaf!(CpuidFunction::CacheTopologyDefinition, 3),
];

#[repr(C)]
#[derive(Debug, IntoBytes, Immutable, KnownLayout, FromBytes, Clone, Copy)]
pub struct HV_PSP_CPUID_LEAF {
    pub eax_in: u32,
    pub ecx_in: u32,
    pub xfem_in: u64,
    pub xss_in: u64,
    pub eax_out: u32,
    pub ebx_out: u32,
    pub ecx_out: u32,
    pub edx_out: u32,
    pub reserved_z: u64,
}

pub const HV_PSP_CPUID_LEAF_COUNT_MAX: usize = 64;

#[repr(C)]
#[derive(Debug, IntoBytes, Immutable, KnownLayout, FromBytes)]
pub struct HV_PSP_CPUID_PAGE {
    pub count: u32,
    pub reserved_z1: u32,
    pub reserved_z2: u64,
    pub cpuid_leaf_info: [HV_PSP_CPUID_LEAF; HV_PSP_CPUID_LEAF_COUNT_MAX],
}

impl Default for HV_PSP_CPUID_PAGE {
    fn default() -> Self {
        HV_PSP_CPUID_PAGE {
            count: 0,
            reserved_z1: 0,
            reserved_z2: 0,
            cpuid_leaf_info: [HV_PSP_CPUID_LEAF::new_zeroed(); HV_PSP_CPUID_LEAF_COUNT_MAX],
        }
    }
}
