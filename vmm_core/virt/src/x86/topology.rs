// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Provides processor topology related cpuid leaves.

mod bsp_identity;

use crate::CpuidLeaf;
use std::cmp::min;
use thiserror::Error;
use vm_topology::processor::ProcessorTopology;
use vm_topology::processor::VpIndex;
use vm_topology::processor::x86::X86Topology;
use x86defs::cpuid::CacheParametersEax;
use x86defs::cpuid::CpuidFunction;
use x86defs::cpuid::ExtendedAddressSpaceSizesEcx;
use x86defs::cpuid::ExtendedTopologyEax;
use x86defs::cpuid::ExtendedTopologyEbx;
use x86defs::cpuid::ExtendedTopologyEcx;
use x86defs::cpuid::ProcessorTopologyDefinitionEax;
use x86defs::cpuid::ProcessorTopologyDefinitionEbx;
use x86defs::cpuid::ProcessorTopologyDefinitionEcx;
use x86defs::cpuid::TopologyLevelType;
use x86defs::cpuid::Vendor;
use x86defs::cpuid::VendorAndMaxFunctionEax;
use x86defs::cpuid::VersionAndFeaturesEbx;

/// A function used to query the cpuid result for a given input value (`eax`,
/// `ecx`).
pub type CpuidFn<'a> = &'a dyn Fn(u32, u32) -> [u32; 4];

#[derive(Debug, Error)]
#[error("unknown processor vendor {0}")]
pub struct UnknownVendor(Vendor);

/// Returns the bits of a CPUID result that hold a VP's own APIC identity,
/// which backends set for each VP: the initial APIC ID in leaf 01h, the
/// x2APIC ID in leaves 0Bh and 1Fh, and the extended APIC, compute unit, and
/// node IDs in leaf 8000001Eh. The partition-wide results that
/// [`topology_cpuid`] builds carry the BSP's identity in these bits.
pub fn per_vp_cpuid_bits(function: u32) -> [u32; 4] {
    match CpuidFunction(function) {
        CpuidFunction::VersionAndFeatures => [
            0,
            VersionAndFeaturesEbx::new()
                .with_initial_apic_id(0xff)
                .into(),
            0,
            0,
        ],
        CpuidFunction::ExtendedTopologyEnumeration
        | CpuidFunction::V2ExtendedTopologyEnumeration => [0, 0, 0, !0],
        CpuidFunction::ProcessorTopologyDefinition => [
            ProcessorTopologyDefinitionEax::new()
                .with_extended_apic_id(!0)
                .into(),
            ProcessorTopologyDefinitionEbx::new()
                .with_compute_unit_id(!0)
                .into(),
            ProcessorTopologyDefinitionEcx::new()
                .with_node_id(!0)
                .into(),
            0,
        ],
        _ => [0; 4],
    }
}

/// Adds appropriately masked leaves for reporting processor topology.
///
/// This includes some bits of leaves 01h and 04h, plus all of leaves 0Bh and
/// 1Fh, and on AMD processors the core count of leaf 80000008h, the
/// cache-sharing counts of leaf 8000001Dh, and leaf 8000001Eh.
pub fn topology_cpuid<'a>(
    topology: &'a ProcessorTopology<X86Topology>,
    cpuid: CpuidFn<'a>,
    leaves: &mut Vec<CpuidLeaf>,
) -> Result<(), UnknownVendor> {
    let result = cpuid(CpuidFunction::VendorAndMaxFunction.0, 0);
    let max = VendorAndMaxFunctionEax::from(result[0]).max_function();
    let vendor = Vendor::from_ebx_ecx_edx(result[1], result[2], result[3]);
    if !vendor.is_intel_compatible() && !vendor.is_amd_compatible() {
        return Err(UnknownVendor(vendor));
    };

    let first_leaf = leaves.len();
    // Set the number of VPs per socket in leaf 01h.
    leaves.push(
        CpuidLeaf::new(
            CpuidFunction::VersionAndFeatures.0,
            [
                0,
                VersionAndFeaturesEbx::new()
                    .with_lps_per_package(topology.reserved_vps_per_socket() as u8)
                    .into(),
                0,
                0,
            ],
        )
        .masked([
            0,
            VersionAndFeaturesEbx::new()
                .with_lps_per_package(0xff)
                .into(),
            0,
            0,
        ]),
    );

    // Set leaf 04h for Intel processors.
    if vendor.is_intel_compatible() {
        cache_parameters_cpuid(topology, cpuid, leaves);
    }

    // Set leaf 0bh.
    extended_topology_cpuid(topology, CpuidFunction::ExtendedTopologyEnumeration, leaves);

    // Set leaf 1fh if requested.
    if max >= CpuidFunction::V2ExtendedTopologyEnumeration.0 {
        extended_topology_cpuid(
            topology,
            CpuidFunction::V2ExtendedTopologyEnumeration,
            leaves,
        );
    }

    if vendor.is_amd_compatible() {
        // Add AMD-specific topology leaves here.
        amd_extended_address_space_sizes_cpuid(topology, leaves);
        amd_cache_topology_cpuid(topology, cpuid, leaves);
        amd_processor_topology_definition_cpuid(topology, leaves);
    }

    bsp_identity::apply(topology, &mut leaves[first_leaf..]);
    Ok(())
}

/// Adds the subleaf that ends each extended topology leaf in `leaves` (0Bh,
/// and 1Fh where present) after its levels: an invalid level with its own
/// number in `ECX[7:0]` and the BSP's x2APIC ID in `EDX`, as Intel defines
/// it. Linux reads it to end its topology enumeration.
///
/// [`topology_cpuid`] does not add it, so partitions without the NVX time ABI
/// keep their backend's own answer there. The time ABI's effective CPUID
/// lists it; backend tests that mirror the effective CPUID call this function
/// instead of copying it.
pub fn terminate_extended_topology(
    topology: &ProcessorTopology<X86Topology>,
    leaves: &mut Vec<CpuidLeaf>,
) {
    let bsp_apic_id = topology.vp_arch(VpIndex::BSP).apic_id;
    for function in [
        CpuidFunction::ExtendedTopologyEnumeration,
        CpuidFunction::V2ExtendedTopologyEnumeration,
    ] {
        let levels = leaves
            .iter()
            .filter(|leaf| leaf.function == function.0)
            .count() as u32;
        if levels == 0 {
            continue;
        }
        let ecx = ExtendedTopologyEcx::new().with_level_number(levels as u8);
        leaves.push(
            CpuidLeaf::new(function.0, [0, 0, ecx.into(), bsp_apic_id])
                .indexed(levels)
                .masked([!0; 4]),
        );
    }
}

/// Adds subleaves for leaf 04h.
///
/// Only valid for Intel processors.
fn cache_parameters_cpuid(
    topology: &ProcessorTopology<X86Topology>,
    cpuid: CpuidFn<'_>,
    leaves: &mut Vec<CpuidLeaf>,
) {
    for i in 0..=255 {
        let result = cpuid(CpuidFunction::CacheParameters.0, i);
        if result == [0; 4] {
            break;
        }
        let mut eax = CacheParametersEax::new();
        // Only 6 bits are available in the cache parameters CPUID leaf (04H)
        // so use a saturated value here as the maximum to avoid a panic later.
        const MAX_CORES_PER_SOCKET_MINUS_ONE: u32 = 0b111111;
        if topology.smt_enabled() {
            eax.set_cores_per_socket_minus_one(min(
                MAX_CORES_PER_SOCKET_MINUS_ONE,
                topology.reserved_vps_per_socket() / 2 - 1,
            ));
            eax.set_threads_sharing_cache_minus_one(1);
        } else {
            eax.set_cores_per_socket_minus_one(min(
                MAX_CORES_PER_SOCKET_MINUS_ONE,
                topology.reserved_vps_per_socket() - 1,
            ));
            eax.set_threads_sharing_cache_minus_one(0);
        }

        // The level 3 cache is not per-VP; indicate that it is per-socket.
        // The level comes from the cache's own descriptor: `eax` carries only
        // the topology fields.
        if CacheParametersEax::from(result[0]).cache_level() == 3 {
            eax.set_threads_sharing_cache_minus_one(topology.reserved_vps_per_socket() - 1);
        }

        let eax_mask = CacheParametersEax::new()
            .with_cores_per_socket_minus_one(0x3f)
            .with_threads_sharing_cache_minus_one(0xfff);

        // Each subleaf describes one cache, so the result applies to subleaf
        // `i` only. An unindexed result would apply to every subleaf, and a
        // backend that looks up the first matching result, such as KVM,
        // would then report it for every cache.
        leaves.push(
            CpuidLeaf::new(CpuidFunction::CacheParameters.0, [eax.into(), 0, 0, 0])
                .indexed(i)
                .masked([eax_mask.into(), 0, 0, 0]),
        )
    }
}

/// Returns topology information in cpuid format (0Bh and 1Fh leaves).
///
/// The x2APIC values in edx will be zero. The caller will need to ensure
/// these are set correctly for each VP.
fn extended_topology_cpuid(
    topology: &ProcessorTopology<X86Topology>,
    function: CpuidFunction,
    leaves: &mut Vec<CpuidLeaf>,
) {
    assert!(
        function == CpuidFunction::ExtendedTopologyEnumeration
            || function == CpuidFunction::V2ExtendedTopologyEnumeration
    );
    for (index, (level_type, num_lps)) in [
        (
            TopologyLevelType::SMT,
            if topology.smt_enabled() { 2 } else { 1 },
        ),
        (TopologyLevelType::CORE, topology.reserved_vps_per_socket()),
    ]
    .into_iter()
    .enumerate()
    {
        if level_type <= TopologyLevelType::CORE
            || function == CpuidFunction::V2ExtendedTopologyEnumeration
        {
            let eax = ExtendedTopologyEax::new().with_x2_apic_shift(num_lps.trailing_zeros());
            let ebx = ExtendedTopologyEbx::new().with_num_lps(num_lps as u16);
            let ecx = ExtendedTopologyEcx::new()
                .with_level_number(index as u8)
                .with_level_type(level_type.0);

            // Don't include edx in the mask: it is the x2APIC ID, which
            // must be filled in by the caller separately for each VP.
            leaves.push(
                CpuidLeaf::new(function.0, [eax.into(), ebx.into(), ecx.into(), 0])
                    .indexed(index as u32)
                    .masked([!0, !0, !0, 0]),
            );
        }
    }
}

/// Adds leaf 80000008h (Extended Address Space Sizes) for AMD processors.
///
/// This leaf contains core count and APIC ID size information.
fn amd_extended_address_space_sizes_cpuid(
    topology: &ProcessorTopology<X86Topology>,
    leaves: &mut Vec<CpuidLeaf>,
) {
    let nc = (topology.reserved_vps_per_socket() - 1) as u8;
    let apic_core_id_size = topology.reserved_vps_per_socket().trailing_zeros() as u8;
    let ecx = ExtendedAddressSpaceSizesEcx::new()
        .with_nc(nc)
        .with_apic_core_id_size(apic_core_id_size);

    let ecx_mask = ExtendedAddressSpaceSizesEcx::new()
        .with_nc(0xff)
        .with_apic_core_id_size(0xf);

    leaves.push(
        CpuidLeaf::new(
            CpuidFunction::ExtendedAddressSpaceSizes.0,
            [0, 0, ecx.into(), 0],
        )
        .masked([0, 0, ecx_mask.into(), 0]),
    );
}

/// Adds the cache-sharing counts of leaf 8000001Dh (Cache Topology
/// Information) for AMD processors, as [`cache_parameters_cpuid`] sets them
/// in leaf 04h for Intel processors: each VP has its own L1 and L2 caches,
/// shared only with its SMT sibling, and the socket shares the level 3
/// cache.
///
/// Each subleaf describes one cache, up to the first with a null cache type,
/// and only its `EAX[25:14]`, the number of logical processors sharing the
/// cache less one, changes. Linux derives each CPU's last-level cache from
/// the last cache's count, so without it a guest would see the host's L3
/// grouping, such as one L3 per core complex.
fn amd_cache_topology_cpuid(
    topology: &ProcessorTopology<X86Topology>,
    cpuid: CpuidFn<'_>,
    leaves: &mut Vec<CpuidLeaf>,
) {
    // AMD's EAX matches leaf 04h's below bit 26, where leaf 04h counts the
    // cores per socket and AMD reserves the bits.
    const MAX_SHARING_MINUS_ONE: u32 = 0xfff;
    let threads_per_core = if topology.smt_enabled() { 2 } else { 1 };
    for i in 0..=255 {
        let descriptor =
            CacheParametersEax::from(cpuid(CpuidFunction::CacheTopologyDefinition.0, i)[0]);
        if descriptor.cache_type() == 0 {
            break;
        }
        let sharing = if descriptor.cache_level() == 3 {
            topology.reserved_vps_per_socket()
        } else {
            threads_per_core
        };
        let eax = CacheParametersEax::new()
            .with_threads_sharing_cache_minus_one(min(MAX_SHARING_MINUS_ONE, sharing - 1));
        let eax_mask =
            CacheParametersEax::new().with_threads_sharing_cache_minus_one(MAX_SHARING_MINUS_ONE);
        leaves.push(
            CpuidLeaf::new(
                CpuidFunction::CacheTopologyDefinition.0,
                [eax.into(), 0, 0, 0],
            )
            .indexed(i)
            .masked([eax_mask.into(), 0, 0, 0]),
        );
    }
}

/// Adds leaf 8000001Eh (Processor Topology Definition) for AMD processors.
fn amd_processor_topology_definition_cpuid(
    topology: &ProcessorTopology<X86Topology>,
    leaves: &mut Vec<CpuidLeaf>,
) {
    // threads_per_compute_unit is (threads per core - 1).
    let threads_per_compute_unit = if topology.smt_enabled() { 1 } else { 0 };
    let ebx = ProcessorTopologyDefinitionEbx::new()
        .with_threads_per_compute_unit(threads_per_compute_unit);

    let ebx_mask = ProcessorTopologyDefinitionEbx::new().with_threads_per_compute_unit(!0);

    // TODO: support AMD's nodes per socket concept.
    let ecx = ProcessorTopologyDefinitionEcx::new().with_nodes_per_processor(0);
    let ecx_mask = ProcessorTopologyDefinitionEcx::new().with_nodes_per_processor(0x7);

    leaves.push(
        CpuidLeaf::new(
            CpuidFunction::ProcessorTopologyDefinition.0,
            [0, ebx.into(), ecx.into(), 0],
        )
        .masked([0, ebx_mask.into(), ecx_mask.into(), 0]),
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::CpuidLeafSet;
    use vm_topology::processor::TopologyBuilder;
    use vm_topology::processor::x86::X2ApicState;

    /// An Intel CPU with maximum basic leaf 0x1f and four caches in leaf 4:
    /// L1 data, L1 instruction, L2, and L3.
    fn intel_cpuid(leaf: u32, subleaf: u32) -> [u32; 4] {
        match (leaf, subleaf) {
            (0, _) => [
                0x1f,
                u32::from_le_bytes(*b"Genu"),
                u32::from_le_bytes(*b"ntel"),
                u32::from_le_bytes(*b"ineI"),
            ],
            (4, 0) => [0x21, 0x1c0_003f, 0x3f, 0],
            (4, 1) => [0x22, 0x1c0_003f, 0x3f, 0],
            (4, 2) => [0x43, 0x3c0_003f, 0x3ff, 0],
            (4, 3) => [0x63, 0x3c0_003f, 0xbfff, 4],
            _ => [0; 4],
        }
    }

    #[test]
    fn cache_results_apply_to_their_own_subleaf() {
        let topology = TopologyBuilder::new_x86()
            .vps_per_socket(4)
            .x2apic(X2ApicState::Supported)
            .build(4)
            .unwrap();
        let mut leaves = Vec::new();
        topology_cpuid(&topology, &intel_cpuid, &mut leaves).unwrap();

        let caches: Vec<_> = leaves
            .iter()
            .filter(|leaf| leaf.function == CpuidFunction::CacheParameters.0)
            .map(|leaf| leaf.index)
            .collect();
        assert_eq!(caches, [Some(0), Some(1), Some(2), Some(3)]);

        // Over each cache's own descriptor, only the topology fields change:
        // 4 cores per socket.
        let set = CpuidLeafSet::new(leaves);
        for subleaf in 0..4 {
            let native = intel_cpuid(4, subleaf);
            let result = set.result(4, subleaf, &native);
            assert_eq!(result[1..], native[1..], "subleaf {subleaf}");
            assert_eq!(result[0] & 0x3fff, native[0] & 0x3fff, "subleaf {subleaf}");
            assert_eq!(
                CacheParametersEax::from(result[0]).cores_per_socket_minus_one(),
                3
            );
        }
        // The null cache that ends the enumeration keeps its native value.
        assert_eq!(set.result(4, 4, &[0; 4]), [0; 4]);
    }

    /// The L1 and L2 caches belong to a core, and the L3 cache to the socket.
    /// Leaf 4 counts addressable IDs, so the socket's VP count is rounded up
    /// to a power of two.
    #[test]
    fn l3_cache_is_shared_by_the_socket() {
        for vps in [1, 2, 6, 8] {
            for smt in [false, true] {
                let topology = TopologyBuilder::new_x86()
                    .vps_per_socket(vps)
                    .smt_enabled(smt)
                    .x2apic(X2ApicState::Supported)
                    .build(vps)
                    .unwrap();
                let threads_per_core = if topology.smt_enabled() { 2 } else { 1 };
                let reserved = topology.reserved_vps_per_socket();
                let mut leaves = Vec::new();
                topology_cpuid(&topology, &intel_cpuid, &mut leaves).unwrap();
                let set = CpuidLeafSet::new(leaves);
                for subleaf in 0..4 {
                    let eax = CacheParametersEax::from(
                        set.result(4, subleaf, &intel_cpuid(4, subleaf))[0],
                    );
                    let sharing = if eax.cache_level() == 3 {
                        reserved
                    } else {
                        threads_per_core
                    };
                    let context = format!("{vps} VPs, smt {smt}, subleaf {subleaf}");
                    assert_eq!(
                        eax.threads_sharing_cache_minus_one() + 1,
                        sharing,
                        "{context}"
                    );
                    assert_eq!(
                        eax.cores_per_socket_minus_one() + 1,
                        reserved / threads_per_core,
                        "{context}"
                    );
                }
            }
        }
    }

    /// An AMD CPU with maximum basic leaf 0xd and four caches in leaf
    /// 0x8000001D, as an AMD EPYC 7763 reports them through WHP: L1 data, L1
    /// instruction, L2, and L3, with no cache-sharing counts.
    fn amd_cpuid(leaf: u32, subleaf: u32) -> [u32; 4] {
        match (leaf, subleaf) {
            (0, _) => [
                0xd,
                u32::from_le_bytes(*b"Auth"),
                u32::from_le_bytes(*b"cAMD"),
                u32::from_le_bytes(*b"enti"),
            ],
            (0x8000_001d, 0) => [0x121, 0x01c0_003f, 0x3f, 0],
            (0x8000_001d, 1) => [0x122, 0x01c0_003f, 0x3f, 0],
            (0x8000_001d, 2) => [0x143, 0x01c0_003f, 0x3ff, 2],
            (0x8000_001d, 3) => [0x163, 0x03c0_003f, 0x7fff, 1],
            _ => [0; 4],
        }
    }

    /// On AMD, the L1 and L2 caches belong to a core, and the L3 cache to
    /// the socket, as leaf 4 reports on Intel: in leaf 0x8000001D, whose
    /// subleaves each describe one cache.
    #[test]
    fn amd_l3_cache_is_shared_by_the_socket() {
        for vps in [1, 2, 6, 8] {
            for smt in [false, true] {
                let topology = TopologyBuilder::new_x86()
                    .vps_per_socket(vps)
                    .smt_enabled(smt)
                    .x2apic(X2ApicState::Supported)
                    .build(vps)
                    .unwrap();
                let threads_per_core = if topology.smt_enabled() { 2 } else { 1 };
                let reserved = topology.reserved_vps_per_socket();
                let mut leaves = Vec::new();
                topology_cpuid(&topology, &amd_cpuid, &mut leaves).unwrap();
                let caches: Vec<_> = leaves
                    .iter()
                    .filter(|leaf| leaf.function == CpuidFunction::CacheTopologyDefinition.0)
                    .map(|leaf| leaf.index)
                    .collect();
                assert_eq!(caches, [Some(0), Some(1), Some(2), Some(3)]);
                assert!(
                    !leaves
                        .iter()
                        .any(|leaf| leaf.function == CpuidFunction::CacheParameters.0)
                );
                let set = CpuidLeafSet::new(leaves);
                for subleaf in 0..4 {
                    let native = amd_cpuid(0x8000_001d, subleaf);
                    let result = set.result(0x8000_001d, subleaf, &native);
                    let context = format!("{vps} VPs, smt {smt}, subleaf {subleaf}");
                    // Only the sharing count changes.
                    assert_eq!(result[1..], native[1..], "{context}");
                    assert_eq!(
                        result[0] & !0x03ff_c000,
                        native[0] & !0x03ff_c000,
                        "{context}"
                    );
                    let eax = CacheParametersEax::from(result[0]);
                    let sharing = if eax.cache_level() == 3 {
                        reserved
                    } else {
                        threads_per_core
                    };
                    assert_eq!(
                        eax.threads_sharing_cache_minus_one() + 1,
                        sharing,
                        "{context}"
                    );
                }
                // The null cache that ends the enumeration keeps its native
                // value.
                assert_eq!(set.result(0x8000_001d, 4, &[0; 4]), [0; 4]);
            }
        }
    }

    #[test]
    fn per_vp_bits_cover_the_bsp_identity() {
        let topology = TopologyBuilder::new_x86()
            .vps_per_socket(2)
            .x2apic(X2ApicState::Supported)
            .build(2)
            .unwrap();
        let mut leaves = Vec::new();
        topology_cpuid(&topology, &intel_cpuid, &mut leaves).unwrap();
        for leaf in &leaves {
            let per_vp = per_vp_cpuid_bits(leaf.function);
            let expected = match CpuidFunction(leaf.function) {
                CpuidFunction::VersionAndFeatures => [0, 0xff00_0000, 0, 0],
                CpuidFunction::ExtendedTopologyEnumeration
                | CpuidFunction::V2ExtendedTopologyEnumeration => [0, 0, 0, !0],
                _ => [0; 4],
            };
            assert_eq!(per_vp, expected, "{leaf:?}");
            for (mask, bits) in leaf.mask.iter().zip(per_vp) {
                assert_eq!(mask & bits, bits);
            }
        }
        assert_ne!(
            per_vp_cpuid_bits(CpuidFunction::ProcessorTopologyDefinition.0),
            [0; 4]
        );
        assert_eq!(per_vp_cpuid_bits(7), [0; 4]);
    }

    #[test]
    fn every_extended_topology_leaf_gets_a_terminator() {
        let topology = TopologyBuilder::new_x86()
            .vps_per_socket(4)
            .x2apic(X2ApicState::Supported)
            .build(4)
            .unwrap();
        let mut leaves = Vec::new();
        for function in [0xb, 0x1f] {
            for index in 0..2 {
                leaves.push(
                    CpuidLeaf::new(function, [1, 2, 3, 0])
                        .indexed(index)
                        .masked([!0; 4]),
                );
            }
        }
        terminate_extended_topology(&topology, &mut leaves);
        let terminators: Vec<_> = leaves[4..]
            .iter()
            .map(|leaf| (leaf.function, leaf.index, leaf.result, leaf.mask))
            .collect();
        assert_eq!(
            terminators,
            [
                (0xb, Some(2), [0, 0, 2, 0], [!0; 4]),
                (0x1f, Some(2), [0, 0, 2, 0], [!0; 4]),
            ]
        );

        // Without extended topology leaves, nothing is added.
        let mut leaves = vec![CpuidLeaf::new(1, [0; 4])];
        terminate_extended_topology(&topology, &mut leaves);
        assert_eq!(leaves.len(), 1);
    }
}
