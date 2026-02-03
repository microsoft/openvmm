// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Provides processor topology related cpuid leaves.

use super::CpuidFn;
use thiserror::Error;
use virt::CpuidLeaf;
use vm_topology::processor::ProcessorTopology;
use x86defs::cpuid::CacheParametersEax;
use x86defs::cpuid::CpuidFunction;
use x86defs::cpuid::ExtendedAddressSpaceSizesEcx;
use x86defs::cpuid::ExtendedTopologyEax;
use x86defs::cpuid::ExtendedTopologyEbx;
use x86defs::cpuid::ExtendedTopologyEcx;
use x86defs::cpuid::ProcessorTopologyDefinitionEbx;
use x86defs::cpuid::ProcessorTopologyDefinitionEcx;
use x86defs::cpuid::TopologyLevelType;
use x86defs::cpuid::Vendor;
use x86defs::cpuid::VendorAndMaxFunctionEax;
use x86defs::cpuid::VersionAndFeaturesEbx;

#[derive(Debug, Error)]
#[error("unknown processor vendor {0}")]
pub struct UnknownVendor(Vendor);

/// Adds appropriately masked leaves for reporting processor topology.
///
/// This includes some bits of leaves 01h and 04h, plus all of leaves 0Bh and
/// 1Fh
pub fn topology_cpuid<'a>(
    topology: &'a ProcessorTopology,
    cpuid: CpuidFn<'a>,
    leaves: &mut Vec<CpuidLeaf>,
) -> Result<(), UnknownVendor> {
    let result = cpuid(CpuidFunction::VendorAndMaxFunction.0, 0);
    let max = VendorAndMaxFunctionEax::from(result[0]).max_function();
    let vendor = Vendor::from_ebx_ecx_edx(result[1], result[2], result[3]);
    if !vendor.is_intel_compatible() && !vendor.is_amd_compatible() {
        return Err(UnknownVendor(vendor));
    };

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
        amd_processor_topology_definition_cpuid(topology, leaves);
    }

    Ok(())
}

/// Adds subleaves for leaf 04h.
///
/// Only valid for Intel processors.
fn cache_parameters_cpuid(
    topology: &ProcessorTopology,
    cpuid: CpuidFn<'_>,
    leaves: &mut Vec<CpuidLeaf>,
) {
    for i in 0..=255 {
        let result = cpuid(CpuidFunction::CacheParameters.0, i);
        if result == [0; 4] {
            break;
        }
        let mut eax = CacheParametersEax::new();
        if topology.smt_enabled() {
            eax.set_cores_per_socket_minus_one((topology.reserved_vps_per_socket() / 2) - 1);
            eax.set_threads_sharing_cache_minus_one(1);
        } else {
            eax.set_cores_per_socket_minus_one(topology.reserved_vps_per_socket() - 1);
            eax.set_threads_sharing_cache_minus_one(0);
        }

        // The level 3 cache is not per-VP; indicate that it is per-socket.
        if eax.cache_level() == 3 {
            eax.set_threads_sharing_cache_minus_one(topology.reserved_vps_per_socket() - 1);
        }

        let eax_mask = CacheParametersEax::new()
            .with_cores_per_socket_minus_one(0x3f)
            .with_threads_sharing_cache_minus_one(0xfff);

        leaves.push(
            CpuidLeaf::new(CpuidFunction::CacheParameters.0, [eax.into(), 0, 0, 0]).masked([
                eax_mask.into(),
                0,
                0,
                0,
            ]),
        )
    }
}

/// Returns topology information in cpuid format (0Bh and 1Fh leaves).
///
/// The x2APIC values in edx will be zero. The caller will need to ensure
/// these are set correctly for each VP.
fn extended_topology_cpuid(
    topology: &ProcessorTopology,
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
    topology: &ProcessorTopology,
    leaves: &mut Vec<CpuidLeaf>,
) {
    let nc = (topology.reserved_vps_per_socket() - 1) as u8;
    let apic_core_id_size = topology
        .reserved_vps_per_socket()
        .next_power_of_two()
        .trailing_zeros() as u8;

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

/// Adds leaf 8000001Eh (Processor Topology Definition) for AMD processors.
///
/// This leaf provides extended APIC ID, compute unit ID, and node information.
/// The extended APIC ID (eax) and compute unit ID (ebx) are per-VP values that
/// must be filled in by the caller.
fn amd_processor_topology_definition_cpuid(
    topology: &ProcessorTopology,
    leaves: &mut Vec<CpuidLeaf>,
) {
    // threads_per_compute_unit is (threads per core - 1).
    let threads_per_compute_unit = if topology.smt_enabled() { 1 } else { 0 };
    let ebx = ProcessorTopologyDefinitionEbx::new()
        .with_threads_per_compute_unit(threads_per_compute_unit);

    let ebx_mask = ProcessorTopologyDefinitionEbx::new().with_threads_per_compute_unit(!0);

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
