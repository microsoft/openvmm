// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Definitions and implementation related to masking CPUID results.

use super::CPUID_LEAF_B_MAX_SUBLEAF_INDEX;
use super::CpuidArchInitializer;
use super::CpuidResults;
use super::MAX_EXTENDED_STATE_ENUMERATION_SUBLEAF;
use core::arch::x86_64::CpuidResult;
use x86defs::cpuid;
use x86defs::cpuid::CpuidFunction;
use x86defs::xsave;

/// Return value for [`CpuidArchSupport::additional_leaf_mask`] (see that method for more
/// details). Supplies the mask that should be applied to a cpuid leaf result so
/// that only supported results are returned.
#[derive(Clone, Copy)]
pub(crate) struct CpuidResultMask {
    /// eax mask for a given leaf/subleaf result
    mask_eax: u32,
    /// ebx mask for a given leaf/subleaf result
    mask_ebx: u32,
    /// ecx mask for a given leaf/subleaf result
    mask_ecx: u32,
    /// edx mask for a given leaf/subleaf result
    mask_edx: u32,
    /// True if the leaf has a valid subleaf and the result is for that subleaf
    is_subleaf: bool,
}

impl CpuidResultMask {
    pub(crate) fn new(
        mask_eax: u32,
        mask_ebx: u32,
        mask_ecx: u32,
        mask_edx: u32,
        is_subleaf: bool,
    ) -> Self {
        Self {
            mask_eax,
            mask_ebx,
            mask_ecx,
            mask_edx,
            is_subleaf,
        }
    }

    pub(crate) fn apply_mask(&self, result: &CpuidResult) -> CpuidResult {
        CpuidResult {
            eax: result.eax & self.mask_eax,
            ebx: result.ebx & self.mask_ebx,
            ecx: result.ecx & self.mask_ecx,
            edx: result.edx & self.mask_edx,
        }
    }

    pub(crate) fn combine(&self, with_mask: &CpuidResultMask) -> CpuidResultMask {
        assert!(self.is_subleaf == with_mask.is_subleaf);

        CpuidResultMask {
            mask_eax: with_mask.mask_eax | self.mask_eax,
            mask_ebx: with_mask.mask_ebx | self.mask_ebx,
            mask_ecx: with_mask.mask_ecx | self.mask_ecx,
            mask_edx: with_mask.mask_edx | self.mask_edx,
            is_subleaf: self.is_subleaf,
        }
    }

    pub(crate) fn is_subleaf(&self) -> bool {
        self.is_subleaf
    }
}

impl CpuidResults {
    /// Filters out unsupported features so that guests don't try to consume
    /// them. The paravisor should only expose CPUID features that are known and
    /// supported.
    pub(super) fn leaf_mask(
        leaf: CpuidFunction,
        subleaf: u32,
        arch_initializer: &dyn CpuidArchInitializer,
    ) -> Option<CpuidResultMask> {
        let arch_mask = arch_initializer.additional_leaf_mask(leaf, subleaf);

        // Note: HCL includes
        // - CacheAndTlbInformation
        // - CacheParameters
        // but these are listed as reserved in the AMD manual, so not including them here.
        let common_mask = match leaf {
            CpuidFunction::VersionAndFeatures => Some(CpuidResultMask {
                mask_eax: cpuid::VersionAndFeaturesEax::new()
                    .with_processor_stepping(0xf)
                    .with_processor_model(0xf)
                    .with_processor_family(0xf)
                    .with_processor_type(0x3)
                    .with_extended_model(0xf)
                    .with_extended_family(0xff)
                    .into(),
                mask_ebx: cpuid::VersionAndFeaturesEbx::new()
                    .with_brand_index(0xff)
                    .with_clflush_line_size(0xff)
                    .with_lps_per_package(0xff)
                    .with_initial_apic_id(0xff)
                    .into(),
                mask_ecx: cpuid::VersionAndFeaturesEcx::new()
                    .with_sse3(true)
                    .with_pclmulqdq(true)
                    .with_ssse3(true)
                    .with_fma(true)
                    .with_cx16(true)
                    .with_pcid(true)
                    .with_sse4_1(true)
                    .with_sse4_2(true)
                    .with_x2_apic(false)
                    .with_movbe(true)
                    .with_pop_cnt(true)
                    .with_aes(true)
                    .with_xsave(true)
                    .with_os_xsave(true)
                    .with_avx(true)
                    .with_f16c(true)
                    .with_rd_rand(true)
                    .into(),
                mask_edx: cpuid::VersionAndFeaturesEdx::new()
                    .with_fpu(true)
                    .with_vme(true)
                    .with_de(true)
                    .with_pse(true)
                    .with_tsc(true)
                    .with_msr(true)
                    .with_pae(true)
                    .with_mce(true)
                    .with_cx8(true)
                    .with_apic(true)
                    .with_sep(true)
                    .with_mtrr(true)
                    .with_pge(true)
                    .with_mca(true)
                    .with_cmov(true)
                    .with_pat(true)
                    .with_pse36(true)
                    .with_cl_fsh(true)
                    .with_mmx(true)
                    .with_fxsr(true)
                    .with_sse(true)
                    .with_sse2(true)
                    .with_mt_per_socket(true)
                    .into(),
                is_subleaf: false,
            }),
            CpuidFunction::MonitorMwait => Some(CpuidResultMask {
                mask_eax: 0xffffffff,
                mask_ebx: 0xffffffff,
                mask_ecx: 0xffffffff,
                mask_edx: 0xffffffff,
                is_subleaf: false,
            }),
            CpuidFunction::ExtendedFeatures => {
                if subleaf == 0 {
                    Some(CpuidResultMask {
                        mask_eax: 0xffffffff, // MaxSubleaf
                        mask_ebx: cpuid::ExtendedFeatureSubleaf0Ebx::new()
                            .with_rd_wr_fs_gs(true)
                            .with_bmi1(true)
                            .with_avx2(true)
                            .with_smep(true)
                            .with_bmi2(true)
                            .with_enhanced_fast_string(true)
                            .with_inv_pcid(true)
                            .with_dep_x87_fpu_save(true)
                            .with_avx512f(true)
                            .with_avx512dq(true)
                            .with_rd_seed(true)
                            .with_adx(true)
                            .with_smap(true)
                            .with_avx512_ifma(true)
                            .with_clflushopt(true)
                            .with_clwb(true)
                            .with_avx512cd(true)
                            .with_sha(true)
                            .with_avx512bw(true)
                            .with_avx512vl(true)
                            .into(),
                        mask_ecx: cpuid::ExtendedFeatureSubleaf0Ecx::new()
                            .with_avx512_vbmi(true)
                            .with_umip(true)
                            .with_umwait_tpause(true)
                            .with_avx512_vbmi2(true)
                            .with_cet_ss(true)
                            .with_gfni(true)
                            .with_vaes(true)
                            .with_vpclmulqdq(true)
                            .with_avx512_vnni(true)
                            .with_avx512_bitalg(true)
                            .with_avx512_vpopcntdq(true)
                            .with_rd_pid(true)
                            .with_cldemote(true)
                            .with_movdiri(true)
                            .with_movdir64b(true)
                            .into(),
                        mask_edx: cpuid::ExtendedFeatureSubleaf0Edx::new()
                            .with_fast_short_rep_move(true)
                            .with_avx512_vp2_intersect(true)
                            .with_serialize(true)
                            .with_avx512_fp16(true)
                            .into(),
                        is_subleaf: true,
                    })
                } else {
                    None
                }
            }
            CpuidFunction::ExtendedStateEnumeration => {
                match subleaf {
                    0 => {
                        Some(CpuidResultMask {
                            mask_eax: (cpuid::ExtendedStateEnumerationSubleaf0Eax::new()
                                .with_x87(true)
                                .with_sse(true)
                                .with_avx(true)
                                .with_opmask(true)
                                .with_zmmhi(true)
                                .with_zmm16_31(true))
                            .into(),
                            mask_ebx: 0xffffffff, // XSaveMaxSizeEnabled
                            mask_ecx: 0xffffffff, // XSaveMaxSizeHw
                            mask_edx: 0,
                            is_subleaf: true,
                        })
                    }
                    1 => {
                        Some(CpuidResultMask {
                            mask_eax: (cpuid::ExtendedStateEnumerationSubleaf1Eax::new()
                                .with_xsave_opt(true)
                                .with_xsave_c(true)
                                .with_xsave_s(true))
                            .into(),
                            mask_ebx: 0xffffffff, // XsavesMaxSizeEnabled
                            mask_ecx: xsave::XSAVE_SUPERVISOR_FEATURE_CET as u32, // XssMaskLow, CetU + CetS
                            mask_edx: 0,
                            is_subleaf: true,
                        })
                    }
                    x if 1 < x && x <= MAX_EXTENDED_STATE_ENUMERATION_SUBLEAF => {
                        Some(CpuidResultMask {
                            mask_eax: 0xffffffff, // XFeatureSupportedMask
                            mask_ebx: 0xffffffff, // XFeatureEnabledSizeMax
                            mask_ecx: 0xffffffff, // XFeatureSupportedSizeMax
                            mask_edx: 0xffffffff, // XFeatureSupportedMask
                            is_subleaf: true,
                        })
                    }
                    _ => None, // Entries beyond the limit are ignored
                }
            }
            CpuidFunction::ExtendedTopologyEnumeration => {
                if subleaf <= CPUID_LEAF_B_MAX_SUBLEAF_INDEX {
                    Some(CpuidResultMask {
                        mask_eax: 0xffffffff,
                        mask_ebx: 0xffffffff,
                        mask_ecx: 0xffffffff,
                        mask_edx: 0xffffffff,
                        is_subleaf: true,
                    })
                } else {
                    None
                }
            }
            CpuidFunction::ExtendedVersionAndFeatures => Some(CpuidResultMask {
                mask_eax: cpuid::ExtendedVersionAndFeaturesEax::new()
                    .with_processor_stepping(0xf)
                    .with_processor_model(0xf)
                    .with_processor_family(0xf)
                    .with_processor_type(0x3)
                    .with_extended_model(0xf)
                    .with_extended_family(0xff)
                    .into(),
                mask_ebx: cpuid::ExtendedVersionAndFeaturesEbx::new()
                    .with_brand_id(0xffff)
                    .with_pkg_type(0xf)
                    .into(),
                mask_ecx: cpuid::ExtendedVersionAndFeaturesEcx::new()
                    .with_lahf_sahf_available(true)
                    .with_cmp_legacy(true)
                    .with_alt_mov_cr8(true)
                    .with_abm(true)
                    .with_sse4_a(true)
                    .with_mis_align_sse(true)
                    .with_prefetch(true)
                    .with_ibs(true)
                    .with_xop(true)
                    .with_fma4(true)
                    .with_topology_extensions(true)
                    .into(),
                mask_edx: cpuid::ExtendedVersionAndFeaturesEdx::new()
                    .with_fpu(true)
                    .with_vme(true)
                    .with_de(true)
                    .with_pse(true)
                    .with_tsc(true)
                    .with_msr(true)
                    .with_pae(true)
                    .with_mce(true)
                    .with_cx8(true)
                    .with_apic(true)
                    .with_syscall(true)
                    .with_mtrr(true)
                    .with_pge(true)
                    .with_mca(true)
                    .with_cmov(true)
                    .with_pat(true)
                    .with_pse36(true)
                    .with_no_execute(true)
                    .with_amd_mmx(true)
                    .with_mmx(true)
                    .with_fxsr(true)
                    .with_page_1gb(true)
                    .with_rdtscp(true)
                    .with_long_mode(true)
                    .with_extended3d_now(true)
                    .with_amd3d_now(true)
                    .into(),
                is_subleaf: false,
            }),
            CpuidFunction::ExtendedBrandingString1 => Some(CpuidResultMask {
                mask_eax: 0xffffffff,
                mask_ebx: 0xffffffff,
                mask_ecx: 0xffffffff,
                mask_edx: 0xffffffff,
                is_subleaf: false,
            }),
            CpuidFunction::ExtendedBrandingString2 => Some(CpuidResultMask {
                mask_eax: 0xffffffff,
                mask_ebx: 0xffffffff,
                mask_ecx: 0xffffffff,
                mask_edx: 0xffffffff,
                is_subleaf: false,
            }),
            CpuidFunction::ExtendedBrandingString3 => Some(CpuidResultMask {
                mask_eax: 0xffffffff,
                mask_ebx: 0xffffffff,
                mask_ecx: 0xffffffff,
                mask_edx: 0xffffffff,
                is_subleaf: false,
            }),
            CpuidFunction::ExtendedPowerManagement => Some(CpuidResultMask {
                mask_eax: 0xffffffff,
                mask_ebx: 0xffffffff,
                mask_ecx: 0xffffffff,
                mask_edx: 0xffffffff,
                is_subleaf: false,
            }),
            CpuidFunction::ExtendedL1CacheParameters => Some(CpuidResultMask {
                mask_eax: 0xffffffff,
                mask_ebx: 0xffffffff,
                mask_ecx: 0xffffffff,
                mask_edx: 0xffffffff,
                is_subleaf: false,
            }),
            CpuidFunction::ExtendedL2CacheParameters => Some(CpuidResultMask {
                mask_eax: 0xffffffff,
                mask_ebx: 0xffffffff,
                mask_ecx: 0xffffffff,
                mask_edx: 0xffffffff,
                is_subleaf: false,
            }),
            CpuidFunction::ExtendedAddressSpaceSizes => Some(CpuidResultMask {
                mask_eax: cpuid::ExtendedAddressSpaceSizesEax::new()
                    .with_physical_address_size(0xff)
                    .with_virtual_address_size(0xff)
                    .into(),
                mask_ebx: 0,
                mask_ecx: 0,
                mask_edx: 0,
                is_subleaf: false,
            }),
            _ => None,
        };

        common_mask.map_or(arch_mask, |common| {
            arch_mask.map_or(common_mask, |arch| Some(common.combine(&arch)))
        })
    }
}
