// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Provides the synthetic hypervisor cpuid leaves matching this hv1 emulator's
//! capabilities.

use virt::CpuidLeaf;

/// Modify the given set of cpuid leaves to match the given parameters.
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
        let isolated_mask = hvdef::HvFeatures::new()
            .with_privileges(hvdef::HvPartitionPrivilege::new().with_isolation(true));
        leaves.push(
            CpuidLeaf::new(hvdef::HV_CPUID_FUNCTION_MS_HV_FEATURES, [0, 0, 0, 0])
                .masked(zerocopy::transmute!(isolated_mask)),
        );
    }
}
