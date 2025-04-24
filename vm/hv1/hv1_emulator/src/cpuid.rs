// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Provides the synthetic hypervisor cpuid leaves matching this hv1 emulator's
//! capabilities.

#![cfg(guest_arch = "x86_64")]

use hvdef::HvEnlightenmentInformation;
use hvdef::HvFeatures;
use hvdef::HvHardwareFeatures;
use hvdef::HvIsolationConfiguration;
use hvdef::HvPartitionPrivilege;
use std::arch::x86_64::CpuidResult;
use virt::CpuidLeaf;
use x86defs::cpuid::CpuidFunction;

/// The partition privileges that this emulator supports.
pub const SUPPORTED_PRIVILEGES: HvPartitionPrivilege = HvPartitionPrivilege::new()
    .with_access_partition_reference_counter(true)
    .with_access_hypercall_msrs(true)
    .with_access_vp_index(true)
    .with_access_synic_msrs(true)
    .with_access_synthetic_timer_msrs(true)
    .with_access_partition_reference_tsc(true);

/// The hypervisor features that this emulator supports.
pub const SUPPORTED_FEATURES: HvFeatures = HvFeatures::new()
    .with_privileges(SUPPORTED_PRIVILEGES)
    .with_direct_synthetic_timers(true);

const fn split_u128(x: u128) -> CpuidResult {
    let bytes: [u32; 4] = zerocopy::transmute!(x);
    CpuidResult {
        eax: bytes[0],
        ebx: bytes[1],
        ecx: bytes[2],
        edx: bytes[3],
    }
}

/// Converts the given features and enlightenment information into a set of
/// synthetic cpuid leaves.
pub fn make_hv_cpuid_leaves(
    features: HvFeatures,
    enlightenments: HvEnlightenmentInformation,
    max_cpus: u32,
) -> [(CpuidFunction, CpuidResult); 3] {
    const fn split_u128(x: u128) -> CpuidResult {
        let bytes: [u32; 4] = zerocopy::transmute!(x);
        CpuidResult {
            eax: bytes[0],
            ebx: bytes[1],
            ecx: bytes[2],
            edx: bytes[3],
        }
    }

    [
        (CpuidFunction(hvdef::HV_CPUID_FUNCTION_MS_HV_FEATURES), {
            split_u128(features.into_bits())
        }),
        (
            CpuidFunction(hvdef::HV_CPUID_FUNCTION_MS_HV_ENLIGHTENMENT_INFORMATION),
            split_u128(enlightenments.into_bits()),
        ),
        (
            CpuidFunction(hvdef::HV_CPUID_FUNCTION_MS_HV_IMPLEMENTATION_LIMITS),
            CpuidResult {
                eax: max_cpus,
                ebx: max_cpus,
                ecx: 0,
                edx: 0,
            },
        ),
    ]
}

/// Converts the given features and enlightenment information into a set of
/// synthetic cpuid leaves for isolated VMs.
pub fn make_isolated_hv_cpuid_leaves(
    hardware_features: HvHardwareFeatures,
    isolation_config: HvIsolationConfiguration,
) -> [(CpuidFunction, CpuidResult); 2] {
    [
        (
            CpuidFunction(hvdef::HV_CPUID_FUNCTION_MS_HV_HARDWARE_FEATURES),
            split_u128(hardware_features.into_bits()),
        ),
        (
            CpuidFunction(hvdef::HV_CPUID_FUNCTION_MS_HV_ISOLATION_CONFIGURATION),
            split_u128(isolation_config.into_bits()),
        ),
    ]
}

/// Adds the standard hypervisor leaves and version information, and filters out
/// information we shouldn't expose if we're hiding isolation.
pub fn process_hv_cpuid_leaves(
    leaves: &mut Vec<CpuidLeaf>,
    hide_isolation: bool,
    hv_version: [u32; 4],
) {
    // Add the standard leaves.
    leaves.push(CpuidLeaf::new(
        hvdef::HV_CPUID_FUNCTION_HV_VENDOR_AND_MAX_FUNCTION,
        [
            if hide_isolation {
                hvdef::HV_CPUID_FUNCTION_MS_HV_IMPLEMENTATION_LIMITS
            } else {
                hvdef::HV_CPUID_FUNCTION_MS_HV_ISOLATION_CONFIGURATION
            },
            u32::from_le_bytes(*b"Micr"),
            u32::from_le_bytes(*b"osof"),
            u32::from_le_bytes(*b"t Hv"),
        ],
    ));
    leaves.push(CpuidLeaf::new(
        hvdef::HV_CPUID_FUNCTION_HV_INTERFACE,
        [u32::from_le_bytes(*b"Hv#1"), 0, 0, 0],
    ));
    leaves.push(CpuidLeaf::new(
        hvdef::HV_CPUID_FUNCTION_MS_HV_VERSION,
        hv_version,
    ));

    // If we're hiding isolation, remove any HV leaves above the lowered limit.
    if hide_isolation {
        leaves.retain(|leaf| {
            if leaf.function & 0xF0000000 == hvdef::HV_CPUID_FUNCTION_HV_VENDOR_AND_MAX_FUNCTION {
                leaf.function <= hvdef::HV_CPUID_FUNCTION_MS_HV_IMPLEMENTATION_LIMITS
            } else {
                true
            }
        });

        // And don't report that we're isolated.
        let isolated_mask =
            HvFeatures::new().with_privileges(HvPartitionPrivilege::new().with_isolation(true));
        leaves.push(
            CpuidLeaf::new(hvdef::HV_CPUID_FUNCTION_MS_HV_FEATURES, [0, 0, 0, 0])
                .masked(zerocopy::transmute!(isolated_mask)),
        );
    }
}
