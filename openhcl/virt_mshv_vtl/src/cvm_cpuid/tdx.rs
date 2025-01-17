// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! CPUID definitions and implementation specific to Underhill in TDX CVMs.

use super::CpuidArchInitializer;
use super::CpuidArchSupport;
use super::CpuidResultMask;
use super::CpuidResults;
use super::CpuidResultsError;
use super::CpuidSubtable;
use super::ParsedCpuidEntry;
use super::TopologyError;
use super::COMMON_REQUIRED_LEAVES;
use core::arch::x86_64::CpuidResult;
use x86defs::cpuid;
use x86defs::cpuid::CpuidFunction;
use x86defs::xsave;

pub const TDX_REQUIRED_LEAVES: &[(CpuidFunction, Option<u32>)] = &[
    (CpuidFunction::CoreCrystalClockInformation, None),
    (CpuidFunction::TileInformation, Some(0)),
    (CpuidFunction::TileInformation, Some(1)),
    (CpuidFunction::TmulInformation, Some(0)),
    // Issue #562: The following aren't required from AMD. Need to double-check if
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
        // Issue #556: Check if this is the same value in the OS repo
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
                    // Issue #556: does this actually have subleaves? the spec says 1+ are reserved
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
        // Issue #556: See HvlpPopulateExtendedStateCpuid
        let xfd_supported = if let Some(support) = results.get(&1).map(
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
                let result_xfd = cpuid::ExtendedStateEnumerationSubleafNEcx::from(result.ecx).xfd();
                if xfd_supported && result_xfd {
                    // Issue #556: update some maximum xfd value; see HvlpMaximumXfd
                }

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
        // Issue #556: see HvlpInitializeCpuidTopologyIntel and fix returned errors
        let vps_per_socket;
        if !version_and_features_edx.mt_per_socket() {
            if version_and_features_ebx.lps_per_package() > 1 {
                return Err(CpuidResultsError::TopologyInconsistent(
                    TopologyError::ThreadsPerUnit,
                ));
            } else {
                vps_per_socket = 1;
            }
        } else {
            vps_per_socket = version_and_features_ebx.lps_per_package();
        }

        // Issue #556: validation of leaf 0xB
        Ok(super::ExtendedTopologyResult {
            subleaf0: None,
            subleaf1: None,
            vps_per_socket: vps_per_socket.into(),
        })
    }

    fn btc_no(&self) -> Option<bool> {
        None
    }

    fn supports_tsc_aux_virtualization(&self, _results: &CpuidResults) -> bool {
        true
    }
}

pub struct TdxCpuidSupport;

impl CpuidArchSupport for TdxCpuidSupport {
    fn process_guest_result(
        &self,
        _leaf: CpuidFunction,
        _subleaf: u32,
        _result: &mut CpuidResult,
        _guest_state: &super::CpuidGuestState,
        _vps_per_socket: u32,
    ) {
        // Nothing extra to do for TDX
    }
}
