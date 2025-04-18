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
pub struct TdxCpuidInitializer {}

impl TdxCpuidInitializer {
    fn cpuid(leaf: u32, subleaf: u32) -> CpuidResult {
        safe_intrinsics::cpuid(leaf, subleaf)
    }
}

impl CpuidArchInitializer for TdxCpuidInitializer {
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
                let result = Self::cpuid(leaf.0, subleaf);

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
}
