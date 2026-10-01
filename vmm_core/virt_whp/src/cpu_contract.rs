// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! CPUID policy for the CPU compatibility contract: the per-VP topology
//! results returned on CPUID exits, and hiding the L0 GPA pinning
//! enlightenment.

use crate::WhpProcessor;
use inspect::Inspect;
use x86defs::cpuid::CpuidFunction;

/// The L0 pinning requirement describes memory owned by OpenVMM, not memory
/// owned by OpenVMM's guest. OpenVMM does not expose the GPA pin/unpin
/// hypercalls, so this leaf keeps the enlightenment from being passed through
/// to the guest.
pub(crate) fn mask_gpa_pinning_enlightenment() -> virt::CpuidLeaf {
    let mask = hvdef::HvEnlightenmentInformation::new()
        .with_use_gpa_pinning_hypercall(true)
        .into_bits();
    virt::CpuidLeaf::new(
        hvdef::HV_CPUID_FUNCTION_MS_HV_ENLIGHTENMENT_INFORMATION,
        [0; 4],
    )
    .masked([
        mask as u32,
        (mask >> 32) as u32,
        (mask >> 64) as u32,
        (mask >> 96) as u32,
    ])
}

/// Processor topology parameters of the per-VP topology CPUID results.
#[derive(Inspect)]
pub(crate) struct CpuidTopology {
    reserved_vps_per_socket: u32,
    smt_enabled: bool,
    /// The extended topology subleaves past the core level (leaves 0Bh and
    /// 1Fh, subleaf 2 and up) that a time ABI partition's effective CPUID
    /// lists, such as the terminating subleaf, sorted. Other partitions list
    /// none.
    #[inspect(debug)]
    listed_extended_topology: Vec<(u32, u32)>,
}

impl CpuidTopology {
    pub(crate) fn new(topology: &vm_topology::processor::ProcessorTopology) -> Self {
        Self {
            reserved_vps_per_socket: topology.reserved_vps_per_socket(),
            smt_enabled: topology.smt_enabled(),
            listed_extended_topology: Vec::new(),
        }
    }

    /// Keeps the extended topology subleaves past the core level that
    /// `cpuid`, a time ABI partition's effective CPUID, lists: each VP sees
    /// them with its own x2APIC ID, instead of the zeros that end the
    /// enumeration on other partitions.
    pub(crate) fn with_listed_extended_topology(mut self, cpuid: &virt::CpuidLeafSet) -> Self {
        let mut listed: Vec<(u32, u32)> = cpuid
            .leaves()
            .iter()
            .filter(|leaf| is_extended_topology(leaf.function))
            .filter_map(|leaf| Some((leaf.function, leaf.index.filter(|&index| index >= 2)?)))
            .collect();
        listed.sort_unstable();
        listed.dedup();
        self.listed_extended_topology = listed;
        self
    }

    fn lists(&self, function: u32, index: u32) -> bool {
        self.listed_extended_topology
            .binary_search(&(function, index))
            .is_ok()
    }
}

fn is_extended_topology(function: u32) -> bool {
    matches!(
        CpuidFunction(function),
        CpuidFunction::ExtendedTopologyEnumeration | CpuidFunction::V2ExtendedTopologyEnumeration
    )
}

/// Reports `apic_id` for the SMT and core levels of the extended topology
/// leaves and for each further subleaf that is `listed`, and terminates the
/// enumeration after the core level otherwise.
fn fixup_extended_topology(index: u32, apic_id: u32, listed: bool, result: &mut [u32; 4]) {
    if index >= 2 && !listed {
        *result = [0; 4];
    } else {
        result[3] = apic_id;
    }
}

impl WhpProcessor<'_> {
    /// Replaces the BSP identity in the partition's topology CPUID results
    /// with this VP's APIC-derived identity.
    pub(crate) fn fixup_topology_cpuid(&self, function: u32, index: u32, default: &mut [u32; 4]) {
        fixup_vp_topology_cpuid(
            &self.vp.partition.cpuid_topology,
            self.inner.vp_info.apic_id,
            function,
            index,
            default,
        );
    }
}

/// Replaces the BSP identity in the partition's topology CPUID results with
/// the identity of the VP whose APIC ID is `apic_id`, as the CPUID exit
/// handler does for each VP.
pub(crate) fn fixup_vp_topology_cpuid(
    topology: &CpuidTopology,
    apic_id: u32,
    function: u32,
    index: u32,
    default: &mut [u32; 4],
) {
    match CpuidFunction(function) {
        CpuidFunction::VersionAndFeatures => {
            let ebx = x86defs::cpuid::VersionAndFeaturesEbx::from(default[1]);
            default[1] = ebx.with_initial_apic_id(apic_id as u8).into();
        }
        CpuidFunction::ExtendedTopologyEnumeration
        | CpuidFunction::V2ExtendedTopologyEnumeration => {
            fixup_extended_topology(index, apic_id, topology.lists(function, index), default);
        }
        CpuidFunction::ProcessorTopologyDefinition => {
            default[0] = x86defs::cpuid::ProcessorTopologyDefinitionEax::from(default[0])
                .with_extended_apic_id(apic_id)
                .into();
            let threads_per_core = if topology.smt_enabled { 2 } else { 1 };
            default[1] = x86defs::cpuid::ProcessorTopologyDefinitionEbx::from(default[1])
                .with_compute_unit_id(
                    ((apic_id % topology.reserved_vps_per_socket) / threads_per_core) as u8,
                )
                .with_threads_per_compute_unit((threads_per_core - 1) as u8)
                .into();
            default[2] = x86defs::cpuid::ProcessorTopologyDefinitionEcx::from(default[2])
                .with_node_id((apic_id / topology.reserved_vps_per_socket) as u8)
                .with_nodes_per_processor(0)
                .into();
        }
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::CpuidTopology;
    use super::fixup_extended_topology;
    use super::fixup_vp_topology_cpuid;
    use super::mask_gpa_pinning_enlightenment;

    #[test]
    fn l0_gpa_pinning_enlightenment_is_not_exposed() {
        let enlightenment = hvdef::HvEnlightenmentInformation::new()
            .with_use_gpa_pinning_hypercall(true)
            .with_nested(true);
        let bits = enlightenment.into_bits();
        let mut result = [
            bits as u32,
            (bits >> 32) as u32,
            (bits >> 64) as u32,
            (bits >> 96) as u32,
        ];

        mask_gpa_pinning_enlightenment().apply(&mut result);

        let result = hvdef::HvEnlightenmentInformation::from(
            result[0] as u128
                | (result[1] as u128) << 32
                | (result[2] as u128) << 64
                | (result[3] as u128) << 96,
        );
        assert!(!result.use_gpa_pinning_hypercall());
        assert!(result.nested());
    }

    #[test]
    fn extended_topology_uses_canonical_levels_and_terminates() {
        for index in [0, 1] {
            for listed in [false, true] {
                let mut result = [1, 2, 3, 99];
                fixup_extended_topology(index, 7, listed, &mut result);
                assert_eq!(result, [1, 2, 3, 7]);
            }
        }

        for index in [2, 3, u32::MAX] {
            let mut result = [1, 2, 3, 99];
            fixup_extended_topology(index, 7, false, &mut result);
            assert_eq!(result, [0; 4]);
        }
    }

    /// A time ABI partition's effective CPUID lists the subleaf that ends the
    /// enumeration (core's `terminate_extended_topology`): each VP sees it
    /// with its own x2APIC ID. Subleaves past it still read zero, and other
    /// partitions keep ending the enumeration with zeros.
    #[test]
    fn listed_extended_topology_subleaves_keep_their_values() {
        let topology = vm_topology::processor::TopologyBuilder::new_x86()
            .vps_per_socket(4)
            .build(4)
            .unwrap();
        let terminator = |function| {
            virt::CpuidLeaf::new(function, [0, 0, 2, 0])
                .indexed(2)
                .masked([!0, !0, !0, 0])
        };
        let cpuid = virt::CpuidLeafSet::new(vec![
            virt::CpuidLeaf::new(0xb, [1, 1, 0x100, 0])
                .indexed(0)
                .masked([!0, !0, !0, 0]),
            terminator(0xb),
            terminator(0x1f),
        ]);
        let time_abi = CpuidTopology::new(&topology).with_listed_extended_topology(&cpuid);
        assert_eq!(time_abi.listed_extended_topology, [(0xb, 2), (0x1f, 2)]);
        let legacy = CpuidTopology::new(&topology);
        for function in [0xb, 0x1f] {
            for apic_id in [0, 3] {
                let mut result = [0, 0, 2, 99];
                fixup_vp_topology_cpuid(&time_abi, apic_id, function, 2, &mut result);
                assert_eq!(result, [0, 0, 2, apic_id]);

                let mut result = [5, 6, 7, 8];
                fixup_vp_topology_cpuid(&time_abi, apic_id, function, 3, &mut result);
                assert_eq!(result, [0; 4]);

                let mut result = [0, 0, 2, 99];
                fixup_vp_topology_cpuid(&legacy, apic_id, function, 2, &mut result);
                assert_eq!(result, [0; 4]);
            }
        }
    }
}
