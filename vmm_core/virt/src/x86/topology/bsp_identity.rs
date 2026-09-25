// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Reports the BSP's APIC identity in the partition-wide topology CPUID
//! leaves. Backends replace these values for each VP.

use crate::CpuidLeaf;
use vm_topology::processor::ProcessorTopology;
use x86defs::cpuid::CpuidFunction;
use x86defs::cpuid::ProcessorTopologyDefinitionEax;
use x86defs::cpuid::ProcessorTopologyDefinitionEbx;
use x86defs::cpuid::ProcessorTopologyDefinitionEcx;
use x86defs::cpuid::VersionAndFeaturesEbx;

/// Adds the BSP identity to the topology `leaves` built by
/// [`super::topology_cpuid`]: the initial APIC ID in leaf 01h, the x2APIC ID
/// in leaves 0Bh and 1Fh, and the extended APIC, compute unit, and node IDs
/// in leaf 8000001Eh, each included in the leaf's mask.
pub(super) fn apply(topology: &ProcessorTopology, leaves: &mut [CpuidLeaf]) {
    let bsp_apic_id = topology.vp_arch(crate::VpIndex::BSP).apic_id;
    for leaf in leaves {
        match CpuidFunction(leaf.function) {
            CpuidFunction::VersionAndFeatures => {
                leaf.result[1] = VersionAndFeaturesEbx::from(leaf.result[1])
                    .with_initial_apic_id(bsp_apic_id as u8)
                    .into();
                leaf.mask[1] = VersionAndFeaturesEbx::from(leaf.mask[1])
                    .with_initial_apic_id(0xff)
                    .into();
            }
            CpuidFunction::ExtendedTopologyEnumeration
            | CpuidFunction::V2ExtendedTopologyEnumeration => {
                leaf.result[3] = bsp_apic_id;
                leaf.mask[3] = !0;
            }
            CpuidFunction::ProcessorTopologyDefinition => {
                let threads_per_compute_unit: u8 = if topology.smt_enabled() { 1 } else { 0 };
                leaf.result[0] = ProcessorTopologyDefinitionEax::from(leaf.result[0])
                    .with_extended_apic_id(bsp_apic_id)
                    .into();
                leaf.mask[0] = ProcessorTopologyDefinitionEax::from(leaf.mask[0])
                    .with_extended_apic_id(!0)
                    .into();
                leaf.result[1] = ProcessorTopologyDefinitionEbx::from(leaf.result[1])
                    .with_compute_unit_id(
                        (bsp_apic_id % topology.reserved_vps_per_socket()
                            / (u32::from(threads_per_compute_unit) + 1))
                            as u8,
                    )
                    .into();
                leaf.mask[1] = ProcessorTopologyDefinitionEbx::from(leaf.mask[1])
                    .with_compute_unit_id(!0)
                    .into();
                leaf.result[2] = ProcessorTopologyDefinitionEcx::from(leaf.result[2])
                    .with_node_id((bsp_apic_id / topology.reserved_vps_per_socket()) as u8)
                    .into();
                leaf.mask[2] = ProcessorTopologyDefinitionEcx::from(leaf.mask[2])
                    .with_node_id(!0)
                    .into();
            }
            _ => {}
        }
    }
}
