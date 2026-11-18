// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! VM CPUID support.

use hvdef::HV_CPUID_FUNCTION_MS_HV_HARDWARE_FEATURES;
use hvdef::VIRTUALIZATION_STACK_CPUID_INTERFACE;
use hvdef::VIRTUALIZATION_STACK_CPUID_PROPERTIES;
use hvdef::VIRTUALIZATION_STACK_CPUID_VENDOR;
use hvdef::VS1_PARTITION_PROPERTIES_EAX_CONFIDENTIAL_VMBUS_AVAILABLE;
use hvdef::VS1_PARTITION_PROPERTIES_EAX_EXTENDED_IOAPIC_RTE;
use hvdef::VS1_PARTITION_PROPERTIES_EAX_IS_PORTABLE;
use virt::CpuidLeaf;
use x86defs::cpuid::CpuidFunction;

// EAX bits 4 and 5 report hypervisor-managed DMA and interrupt remapping.
const HYPERV_HARDWARE_FEATURES_IOMMU_IN_USE_MASK: u32 = (1 << 4) | (1 << 5);

/// Returns CPUID leaves for Hyper-V-style VMs.
///
/// `extended_ioapic_rte` indicates that MSIs and the IOAPIC can reference a
/// 15-bit APIC ID instead of the architectural 8-bit value. To match Hyper-V
/// behavior, this should be enabled for non-PCAT VMs.
pub fn hyperv_cpuid_leaves(
    extended_ioapic_rte: bool,
    confidential_vmbus: bool,
) -> impl Iterator<Item = CpuidLeaf> {
    [
        // Enable the virtualization bit.
        //
        // Not all hypervisors (e.g. KVM) enable this automatically.
        CpuidLeaf::new(CpuidFunction::VersionAndFeatures.0, [0, 0, 1 << 31, 0]).masked([
            0,
            0,
            1 << 31,
            0,
        ]),
        CpuidLeaf::new(
            VIRTUALIZATION_STACK_CPUID_VENDOR,
            [
                VIRTUALIZATION_STACK_CPUID_PROPERTIES,
                u32::from_le_bytes(*b"Micr"),
                u32::from_le_bytes(*b"osof"),
                u32::from_le_bytes(*b"t VS"),
            ],
        ),
        CpuidLeaf::new(
            VIRTUALIZATION_STACK_CPUID_INTERFACE,
            [u32::from_le_bytes(*b"VS#1"), 0, 0, 0],
        ),
        CpuidLeaf::new(
            VIRTUALIZATION_STACK_CPUID_PROPERTIES,
            [
                VS1_PARTITION_PROPERTIES_EAX_IS_PORTABLE
                    | if extended_ioapic_rte {
                        VS1_PARTITION_PROPERTIES_EAX_EXTENDED_IOAPIC_RTE
                    } else {
                        0
                    }
                    | if confidential_vmbus {
                        VS1_PARTITION_PROPERTIES_EAX_CONFIDENTIAL_VMBUS_AVAILABLE
                    } else {
                        0
                    },
                0,
                0,
                0,
            ],
        ),
    ]
    .into_iter()
}

/// Returns a CPUID override indicating that the guest, not the hypervisor,
/// manages DMA and interrupt remapping.
pub fn hyperv_guest_managed_iommu_leaf() -> CpuidLeaf {
    CpuidLeaf::new(HV_CPUID_FUNCTION_MS_HV_HARDWARE_FEATURES, [0; 4]).masked([
        HYPERV_HARDWARE_FEATURES_IOMMU_IN_USE_MASK,
        0,
        0,
        0,
    ])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn guest_managed_iommu_leaf_clears_only_iommu_bits() {
        let leaf = hyperv_guest_managed_iommu_leaf();
        let mut result = [u32::MAX; 4];

        leaf.apply(&mut result);

        assert_eq!(leaf.function, HV_CPUID_FUNCTION_MS_HV_HARDWARE_FEATURES);
        assert_eq!(
            leaf.mask,
            [HYPERV_HARDWARE_FEATURES_IOMMU_IN_USE_MASK, 0, 0, 0]
        );
        assert_eq!(
            result,
            [
                u32::MAX & !HYPERV_HARDWARE_FEATURES_IOMMU_IN_USE_MASK,
                u32::MAX,
                u32::MAX,
                u32::MAX,
            ]
        );
    }
}
