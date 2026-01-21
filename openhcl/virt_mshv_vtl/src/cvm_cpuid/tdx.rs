// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! CPUID definitions and implementation specific to Underhill in TDX CVMs.

use super::COMMON_REQUIRED_LEAVES;
use super::CpuidArchInitializer;
use super::CpuidResultMask;
use super::CpuidResults;
use super::CpuidResultsError;
use super::CpuidSubtable;
use super::ParsedCpuidEntry;
use super::TopologyError;
use core::arch::x86_64::CpuidResult;
use cvm_tracing::CVM_ALLOWED;
use vm_topology::processor::ProcessorTopology;
use vm_topology::processor::x86::X86Topology;
use x86defs::cpuid;
use x86defs::cpuid::CpuidFunction;
use x86defs::xsave;

pub const TDX_REQUIRED_LEAVES: &[(CpuidFunction, Option<u32>)] = &[
    (CpuidFunction::CoreCrystalClockInformation, None),
    (CpuidFunction::TileInformation, Some(0)),
    (CpuidFunction::TileInformation, Some(1)),
    (CpuidFunction::TmulInformation, Some(0)),
    // TODO TDX: The following aren't required from AMD. Need to double-check if
    // they're required for TDX
    (CpuidFunction::CacheAndTlbInformation, None),
    (CpuidFunction::ExtendedFeatures, Some(1)),
    (CpuidFunction::CacheParameters, Some(0)),
    (CpuidFunction::CacheParameters, Some(1)),
    (CpuidFunction::CacheParameters, Some(2)),
    (CpuidFunction::CacheParameters, Some(3)),
];

/// Implements [`CpuidArchSupport`] for TDX-isolation support
pub struct TdxCpuidInitializer<'a> {
    topology: &'a ProcessorTopology<X86Topology>,
    access_vsm: bool,
    vtom: u64,
}

impl<'a> TdxCpuidInitializer<'a> {
    pub fn new(topology: &'a ProcessorTopology<X86Topology>, access_vsm: bool, vtom: u64) -> Self {
        Self {
            topology,
            access_vsm,
            vtom,
        }
    }

    fn cpuid(leaf: u32, subleaf: u32) -> CpuidResult {
        safe_intrinsics::cpuid(leaf, subleaf)
    }
}

impl CpuidArchInitializer for TdxCpuidInitializer<'_> {
    fn vendor(&self) -> cpuid::Vendor {
        cpuid::Vendor::INTEL
    }

    fn max_function(&self) -> u32 {
        CpuidFunction::IntelMaximum.0
    }

    fn extended_max_function(&self) -> u32 {
        CpuidFunction::ExtendedIntelMaximum.0
    }

    fn additional_leaf_mask(&self, leaf: CpuidFunction, subleaf: u32) -> Option<CpuidResultMask> {
        match leaf {
            CpuidFunction::ExtendedFeatures => {
                if subleaf == 0 {
                    Some(CpuidResultMask::new(
                        0,
                        0,
                        0,
                        cpuid::ExtendedFeatureSubleaf0Edx::new()
                            .with_amx_bf16(true)
                            .with_amx_tile(true)
                            .with_amx_int8(true)
                            .into(),
                        true,
                    ))
                } else {
                    None
                }
            }
            CpuidFunction::ExtendedStateEnumeration => {
                if subleaf == 0 {
                    Some(CpuidResultMask::new(
                        cpuid::ExtendedStateEnumerationSubleaf0Eax::new()
                            .with_xtile_cfg(true)
                            .with_xtile_dta(true)
                            .into(),
                        0,
                        0,
                        0,
                        true,
                    ))
                } else if subleaf == 1 {
                    Some(CpuidResultMask::new(
                        cpuid::ExtendedStateEnumerationSubleaf1Eax::new()
                            .with_xfd(true)
                            .into(),
                        0,
                        0,
                        0,
                        true,
                    ))
                } else {
                    None
                }
            }
            CpuidFunction::TileInformation => {
                if subleaf <= 1 {
                    Some(CpuidResultMask::new(
                        0xffffffff, 0xffffffff, 0xffffffff, 0xffffffff, true,
                    ))
                } else {
                    None
                }
            }
            CpuidFunction::TmulInformation => {
                if subleaf == 0 {
                    Some(CpuidResultMask::new(
                        0xffffffff, 0xffffffff, 0xffffffff, 0xffffffff, true,
                    ))
                } else {
                    None
                }
            }
            CpuidFunction::CoreCrystalClockInformation => Some(CpuidResultMask::new(
                0xffffffff, 0xffffffff, 0xffffffff, 0xffffffff, false,
            )),
            CpuidFunction::CacheAndTlbInformation => Some(CpuidResultMask::new(
                0xffffffff, 0xffffffff, 0xffffffff, 0xffffffff, false,
            )),
            CpuidFunction::CacheParameters if subleaf <= 3 => Some(CpuidResultMask::new(
                0xffffffff, 0xffffffff, 0xffffffff, 0xffffffff, true,
            )),
            _ => None,
        }
    }

    fn validate_results(&self, results: &CpuidResults) -> Result<(), CpuidResultsError> {
        for &(leaf, subleaf) in TDX_REQUIRED_LEAVES {
            if results.leaf_result_ref(leaf, subleaf, true).is_none() {
                return Err(CpuidResultsError::MissingRequiredResult(leaf, subleaf));
            }
        }

        Ok(())
    }

    fn cpuid_info(&self) -> Vec<ParsedCpuidEntry> {
        [TDX_REQUIRED_LEAVES, COMMON_REQUIRED_LEAVES]
            .concat()
            .into_iter()
            .map(|(leaf, subleaf)| {
                let subleaf = subleaf.unwrap_or(0);
                let mut result = Self::cpuid(leaf.0, subleaf);

                // Apply TDX specific fixups.
                #[expect(clippy::single_match)] // More may come later
                match leaf {
                    // Always tell the guest that MCE, MCA, and MTRR are supported.
                    CpuidFunction::VersionAndFeatures => {
                        let mut features = cpuid::VersionAndFeaturesEdx::from(result.edx);
                        features.set_mce(true);
                        features.set_mca(true);
                        features.set_mtrr(true);
                        result.edx = features.into();
                    }
                    _ => {}
                }

                ParsedCpuidEntry {
                    leaf,
                    subleaf,
                    result,
                }
            })
            .collect()
    }

    fn process_extended_state_subleaves(
        &self,
        results: &mut CpuidSubtable,
        extended_state_mask: u64,
    ) -> Result<(), CpuidResultsError> {
        if let Some(support) = results.get(&1).map(
            |CpuidResult {
                 eax,
                 ebx: _,
                 ecx: _,
                 edx: _,
             }| cpuid::ExtendedStateEnumerationSubleaf1Eax::from(*eax).xfd(),
        ) {
            support
        } else {
            return Err(CpuidResultsError::MissingRequiredResult(
                CpuidFunction::ExtendedStateEnumeration,
                Some(1),
            ));
        };

        let summary_mask = extended_state_mask & !xsave::X86X_XSAVE_LEGACY_FEATURES;

        for i in 0..=super::MAX_EXTENDED_STATE_ENUMERATION_SUBLEAF {
            if (1 << i) & summary_mask != 0 {
                let result = Self::cpuid(CpuidFunction::ExtendedStateEnumeration.0, i);

                results.insert(i, result);
            }
        }

        Ok(())
    }

    fn extended_topology(
        &self,
        version_and_features_ebx: cpuid::VersionAndFeaturesEbx,
        version_and_features_edx: cpuid::VersionAndFeaturesEdx,
        _address_space_sizes_ecx: cpuid::ExtendedAddressSpaceSizesEcx,
        _processor_topology_ebx: Option<cpuid::ProcessorTopologyDefinitionEbx>, // Will be None for Intel
    ) -> Result<super::ExtendedTopologyResult, CpuidResultsError> {
        // TODO TDX: see HvlpInitializeCpuidTopologyIntel
        // TODO TDX: fix returned errors
        if !version_and_features_edx.mt_per_socket() {
            if version_and_features_ebx.lps_per_package() > 1 {
                return Err(CpuidResultsError::TopologyInconsistent(
                    TopologyError::ThreadsPerUnit,
                ));
            }
        }

        // Validation for Leaf 0xB subleaf 0
        let extended_topology_ecx_0 = cpuid::ExtendedTopologyEcx::from(
            Self::cpuid(CpuidFunction::ExtendedTopologyEnumeration.0, 0).ecx,
        );

        if (extended_topology_ecx_0.level_number() != super::CPUID_LEAF_B_LEVEL_NUMBER_SMT)
            || (extended_topology_ecx_0.level_type() != super::CPUID_LEAF_B_LEVEL_TYPE_SMT)
        {
            tracing::error!(
                CVM_ALLOWED,
                "Incorrect values received: {:?}. Level Number should represent sub-leaf 0, while Level Type should represent domain type 1 for logical processor.",
                extended_topology_ecx_0
            );
        }

        // Validation for Leaf 0xB subleaf 1
        let extended_topology_ecx_1 = cpuid::ExtendedTopologyEcx::from(
            Self::cpuid(CpuidFunction::ExtendedTopologyEnumeration.0, 1).ecx,
        );

        if (extended_topology_ecx_1.level_number() != super::CPUID_LEAF_B_LEVEL_NUMBER_CORE)
            || (extended_topology_ecx_1.level_type() != super::CPUID_LEAF_B_LEVEL_TYPE_CORE)
        {
            tracing::error!(
                CVM_ALLOWED,
                "Incorrect values received: {:?}. Level Number should represent sub-leaf 1, while Level Type should represent domain type 2 for Core.",
                extended_topology_ecx_1
            );
        }

        Ok(super::ExtendedTopologyResult {
            subleaf0: None,
            subleaf1: None,
        })
    }

    fn btc_no(&self) -> Option<bool> {
        None
    }

    fn supports_tsc_aux_virtualization(&self, _results: &CpuidResults) -> bool {
        true
    }

    fn hv_cpuid_leaves(&self) -> [(CpuidFunction, CpuidResult); 5] {
        const MAX_CPUS: u32 = 2048;

        let privileges = hv1_emulator::cpuid::SUPPORTED_PRIVILEGES
            .with_access_frequency_msrs(true)
            .with_access_apic_msrs(true)
            .with_start_virtual_processor(true)
            .with_enable_extended_gva_ranges_flush_va_list(true)
            .with_access_guest_idle_msr(true)
            .with_access_vsm(self.access_vsm)
            .with_isolation(true)
            .with_fast_hypercall_output(true);

        let features = hv1_emulator::cpuid::SUPPORTED_FEATURES
            .with_privileges(privileges)
            .with_frequency_regs_available(true)
            .with_extended_gva_ranges_for_flush_virtual_address_list_available(true)
            .with_guest_idle_available(true)
            .with_xmm_registers_for_fast_hypercall_available(true)
            .with_register_pat_available(true)
            .with_fast_hypercall_output_available(true)
            .with_translate_gva_flags_available(true)
            .with_guest_crash_regs_available(true);

        let use_apic_msrs = match self.topology.apic_mode() {
            vm_topology::processor::x86::ApicMode::XApic => {
                // If only xAPIC is supported, then the Hyper-V MSRs are
                // more efficient for EOIs.
                true
            }
            vm_topology::processor::x86::ApicMode::X2ApicSupported
            | vm_topology::processor::x86::ApicMode::X2ApicEnabled => {
                // If X2APIC is supported, then use the X2APIC MSRs. These
                // are as efficient as the Hyper-V MSRs, and they are
                // compatible with APIC hardware offloads.
                false
            }
        };

        let enlightenments = hvdef::HvEnlightenmentInformation::new()
            .with_deprecate_auto_eoi(true)
            .with_use_relaxed_timing(true)
            .with_use_ex_processor_masks(true)
            .with_use_apic_msrs(use_apic_msrs)
            .with_long_spin_wait_count(!0)
            .with_use_hypercall_for_remote_flush_and_local_flush_entire(true);

        let hardware_features = hvdef::HvHardwareFeatures::new()
            .with_apic_overlay_assist_in_use(true)
            .with_msr_bitmaps_in_use(true)
            .with_second_level_address_translation_in_use(true)
            .with_dma_remapping_in_use(false)
            .with_interrupt_remapping_in_use(false);

        let isolation_config = hvdef::HvIsolationConfiguration::new()
            .with_paravisor_present(true)
            .with_isolation_type(virt::IsolationType::Tdx.to_hv().0)
            .with_shared_gpa_boundary_active(true)
            .with_shared_gpa_boundary_bits(self.vtom.trailing_zeros() as u8);

        let [l0, l1, l2] =
            hv1_emulator::cpuid::make_hv_cpuid_leaves(features, enlightenments, MAX_CPUS);
        let [l3, l4] =
            hv1_emulator::cpuid::make_isolated_hv_cpuid_leaves(hardware_features, isolation_config);

        [l0, l1, l2, l3, l4]
    }

    fn update_xsave_dependencies(&self, xsave_support: u64, results: &mut CpuidResults) {
        let mut clear_extended_features0_edx = cpuid::ExtendedFeatureSubleaf0Edx::new();

        if xsave_support & xsave::XFEATURE_AMX != xsave::XFEATURE_AMX {
            clear_extended_features0_edx.set_amx_tile(true);
            clear_extended_features0_edx.set_amx_int8(true);
            clear_extended_features0_edx.set_amx_bf16(true);
        }

        let extended_features0_entry = results
            .leaf_result_mut_ref(CpuidFunction::ExtendedFeatures, Some(0))
            .expect("validated this leaf exists");

        extended_features0_entry.edx &= !u32::from(clear_extended_features0_edx);
    }
}
