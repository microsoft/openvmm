// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! VM CPUID support.

pub mod topology;

use hvdef::VIRTUALIZATION_STACK_CPUID_INTERFACE;
use hvdef::VIRTUALIZATION_STACK_CPUID_PROPERTIES;
use hvdef::VIRTUALIZATION_STACK_CPUID_VENDOR;
use hvdef::VS1_PARTITION_PROPERTIES_EAX_CONFIDENTIAL_VMBUS_AVAILABLE;
use hvdef::VS1_PARTITION_PROPERTIES_EAX_EXTENDED_IOAPIC_RTE;
use hvdef::VS1_PARTITION_PROPERTIES_EAX_IS_PORTABLE;
use virt::CpuidLeaf;
use x86defs::cpuid::CpuidFunction;

/// A function used to query the cpuid result for a given input value (`eax`,
/// `ecx`).
pub type CpuidFn<'a> = &'a dyn Fn(u32, u32) -> [u32; 4];

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
