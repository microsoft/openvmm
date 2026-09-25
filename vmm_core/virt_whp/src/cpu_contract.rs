// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! CPUID policy for the CPU compatibility contract: the fixed TSC frequency
//! and CPUID exits of a versioned CPU contract, the per-VP topology results
//! returned on CPUID exits, and hiding the L0 GPA pinning enlightenment.

use crate::Error;
use crate::WhpProcessor;
use crate::WhpResultExt;
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

/// Sets the fixed TSC frequency of a versioned CPU contract and routes the
/// topology and clock CPUID leaves to the VMM.
pub(crate) fn configure_versioned_contract(
    whp_config: &mut whp::PartitionConfig,
    extended_exits: &mut whp::abi::WHV_EXTENDED_VM_EXITS,
) -> Result<(), Error> {
    const VERSIONED_TSC_FREQUENCY_HZ: u64 = 1_000_000_000;

    match whp_config.set_property(whp::PartitionProperty::ProcessorClockFrequency(
        VERSIONED_TSC_FREQUENCY_HZ,
    )) {
        Ok(_) => {}
        Err(err @ (whp::WHvError::ERROR_NOT_SUPPORTED | whp::WHvError::WHV_E_UNKNOWN_PROPERTY)) => {
            tracing::warn!(
                error = %err,
                "WHP cannot set the versioned TSC frequency; using the host frequency"
            );
        }
        Err(err) => {
            return Err(err).for_op("set versioned CPU contract TSC frequency");
        }
    }
    *extended_exits |= whp::abi::WHV_EXTENDED_VM_EXITS::X64CpuidExit;
    let cpuid_exit_list = [
        CpuidFunction::VendorAndMaxFunction.0,
        CpuidFunction::VersionAndFeatures.0,
        CpuidFunction::CacheParameters.0,
        CpuidFunction::ExtendedTopologyEnumeration.0,
        CpuidFunction::V2ExtendedTopologyEnumeration.0,
        CpuidFunction::ExtendedAddressSpaceSizes.0,
        CpuidFunction::ProcessorTopologyDefinition.0,
        CpuidFunction::CoreCrystalClockInformation.0,
    ];
    whp_config
        .set_property(whp::PartitionProperty::CpuidExitList(&cpuid_exit_list))
        .for_op("set versioned CPU contract CPUID exits")?;
    Ok(())
}

/// Processor topology parameters of the per-VP topology CPUID results.
#[derive(Inspect)]
pub(crate) struct CpuidTopology {
    reserved_vps_per_socket: u32,
    smt_enabled: bool,
}

impl CpuidTopology {
    pub(crate) fn new(topology: &vm_topology::processor::ProcessorTopology) -> Self {
        Self {
            reserved_vps_per_socket: topology.reserved_vps_per_socket(),
            smt_enabled: topology.smt_enabled(),
        }
    }
}

/// Reports `apic_id` for the SMT and core levels of the extended topology
/// leaves, and terminates the enumeration after them.
fn fixup_extended_topology(index: u32, apic_id: u32, result: &mut [u32; 4]) {
    if index >= 2 {
        *result = [0; 4];
    } else {
        result[3] = apic_id;
    }
}

impl WhpProcessor<'_> {
    /// Replaces the BSP identity in the partition's topology CPUID results
    /// with this VP's APIC-derived identity.
    pub(crate) fn fixup_topology_cpuid(&self, function: u32, index: u32, default: &mut [u32; 4]) {
        match CpuidFunction(function) {
            CpuidFunction::VersionAndFeatures => {
                let ebx = x86defs::cpuid::VersionAndFeaturesEbx::from(default[1]);
                default[1] = ebx
                    .with_initial_apic_id(self.inner.vp_info.apic_id as u8)
                    .into();
            }
            CpuidFunction::ExtendedTopologyEnumeration
            | CpuidFunction::V2ExtendedTopologyEnumeration => {
                fixup_extended_topology(index, self.inner.vp_info.apic_id, default);
            }
            CpuidFunction::ProcessorTopologyDefinition => {
                let topology = &self.vp.partition.cpuid_topology;
                let apic_id = self.inner.vp_info.apic_id;
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
}

#[cfg(test)]
mod tests {
    use super::fixup_extended_topology;
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
            let mut result = [1, 2, 3, 99];
            fixup_extended_topology(index, 7, &mut result);
            assert_eq!(result, [1, 2, 3, 7]);
        }

        for index in [2, 3, u32::MAX] {
            let mut result = [1, 2, 3, 99];
            fixup_extended_topology(index, 7, &mut result);
            assert_eq!(result, [0; 4]);
        }
    }
}
