// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! CPUID definitions and implementation specific to Underhill in SNP CVMs.

use super::CpuidArchInitializer;
use super::CpuidResultMask;
use super::CpuidResults;
use super::CpuidResultsError;
use super::CpuidSubtable;
use super::ExtendedTopologyResult;
use super::ParsedCpuidEntry;
use super::TopologyError;
use core::arch::x86_64::CpuidResult;
use x86defs::cpuid;
use x86defs::cpuid::CpuidFunction;
use x86defs::snp::HvPspCpuidPage;
use x86defs::xsave;
use zerocopy::FromZeros;
use zerocopy::IntoBytes;

enum CpuidPageIndexErr {
    OutOfBounds,
}

/// Information needed to index the individual cpuid leaf info in a list of
/// [`PspCpuidPage`]
struct CpuidPageIndex {
    page_counts: Vec<usize>,
    page_index: usize,
    function_index: usize,
}

impl CpuidPageIndex {
    fn new(cpuid_pages: &'_ [HvPspCpuidPage]) -> Self {
        let page_counts = cpuid_pages.iter().map(|page| page.count as usize).collect();
        let mut index = Self {
            page_counts,
            page_index: 0,
            function_index: 0,
        };

        let _ = index.next_valid_index();

        index
    }

    /// If function_index has reached the limit of the current page's
    /// counts, updates the index until it finds the next valid entry in the
    /// cpuid pages or reaches the end
    fn next_valid_index(&mut self) -> Result<(), CpuidPageIndexErr> {
        loop {
            if self.function_index >= self.page_counts[self.page_index] {
                self.page_index += 1;
                self.function_index = 0;
                self.in_bounds()?;
            } else {
                break;
            }
        }

        Ok(())
    }

    /// Increments the index of cpuid leaf info in a list of [`PspCpuidPage`],
    /// appropriately adjusting when crossing a page boundary.
    fn increment(&mut self) -> Result<(), CpuidPageIndexErr> {
        self.in_bounds()?;
        self.function_index += 1;
        self.next_valid_index()
    }

    fn in_bounds(&self) -> Result<(), CpuidPageIndexErr> {
        if self.page_index >= self.page_counts.len() {
            return Err(CpuidPageIndexErr::OutOfBounds);
        }

        Ok(())
    }
}

pub const SNP_REQUIRED_LEAVES: &[(CpuidFunction, Option<u32>)] = &[
    (CpuidFunction::ExtendedSevFeatures, None),
    (CpuidFunction::ExtendedSvmVersionAndFeatures, None),
    (CpuidFunction::ExtendedTlb1GBIdentifiers, None),
    (CpuidFunction::ExtendedOptimizationIdentifiers, None),
    (CpuidFunction::CacheTopologyDefinition, Some(0)),
    (CpuidFunction::CacheTopologyDefinition, Some(1)),
    (CpuidFunction::CacheTopologyDefinition, Some(2)),
    (CpuidFunction::CacheTopologyDefinition, Some(3)),
    (CpuidFunction::ProcessorTopologyDefinition, None),
];

/// Implements [`CpuidArchSupport`] for SNP-isolation support
pub struct SnpCpuidInitializer {
    cpuid_pages: Vec<HvPspCpuidPage>,
    access_vsm: bool,
    vtom: u64,
}

impl SnpCpuidInitializer {
    pub fn new(cpuid_pages_data: &[u8], access_vsm: bool, vtom: u64) -> Self {
        let mut cpuid_pages = vec![
            HvPspCpuidPage::new_zeroed();
            cpuid_pages_data.len() / size_of::<HvPspCpuidPage>()
        ];
        cpuid_pages
            .as_mut_slice()
            .as_mut_bytes()
            .copy_from_slice(cpuid_pages_data);

        Self {
            cpuid_pages,
            access_vsm,
            vtom,
        }
    }
}

impl CpuidArchInitializer for SnpCpuidInitializer {
    fn vendor(&self) -> cpuid::Vendor {
        cpuid::Vendor::AMD
    }

    fn max_function(&self) -> u32 {
        CpuidFunction::AmdMaximum.0
    }

    fn extended_max_function(&self) -> u32 {
        CpuidFunction::ExtendedAmdMaximum.0
    }

    fn additional_leaf_mask(&self, leaf: CpuidFunction, subleaf: u32) -> Option<CpuidResultMask> {
        match leaf {
            CpuidFunction::ExtendedVersionAndFeatures => Some(CpuidResultMask::new(
                0,
                0,
                0,
                cpuid::ExtendedVersionAndFeaturesEdx::new()
                    .with_fast_fxsr(true)
                    .into(),
                false,
            )),
            CpuidFunction::ExtendedAddressSpaceSizes => Some(CpuidResultMask::new(
                0,
                cpuid::ExtendedAddressSpaceSizesEbx::new()
                    .with_cl_zero(true)
                    .with_inst_ret_cnt_msr(true)
                    .with_x_save_er_ptr(true)
                    .with_invlpgb(true)
                    .with_rdpru(true)
                    .with_wbnoinvd(true)
                    .with_ibpb(true)
                    .with_ibrs(true)
                    .with_stibp(true)
                    .with_ssbd(true)
                    .with_stibp_always_on(true)
                    .with_efer_lmsle_unsupported(true)
                    .with_psfd(true)
                    .with_btc_no(true)
                    .into(),
                cpuid::ExtendedAddressSpaceSizesEcx::new()
                    .with_nc(0xff)
                    .with_apic_core_id_size(0xf)
                    .into(),
                cpuid::ExtendedAddressSpaceSizesEdx::new()
                    .with_invlpgb_count_max(0xffff)
                    .with_rdpru_max_ecx(0xffff)
                    .into(),
                false,
            )),
            CpuidFunction::ExtendedSvmVersionAndFeatures => Some(CpuidResultMask::new(
                cpuid::ExtendedSvmVersionAndFeaturesEax::new()
                    .with_svm_rev(0xff)
                    .into(),
                0,
                0,
                0,
                false,
            )),
            CpuidFunction::ExtendedTlb1GBIdentifiers => {
                Some(CpuidResultMask::new(0xffffffff, 0xffffffff, 0, 0, false))
            }
            CpuidFunction::ExtendedOptimizationIdentifiers => Some(CpuidResultMask::new(
                cpuid::ExtendedOptimizationIdentifiersEax::new()
                    .with_fp128(true)
                    .with_mov_u(true)
                    .with_fp256(true)
                    .into(),
                0,
                0,
                0,
                false,
            )),
            CpuidFunction::CacheTopologyDefinition => {
                if subleaf <= 3 {
                    Some(CpuidResultMask::new(
                        0xffffffff, 0xffffffff, 0xffffffff, 0xffffffff, true,
                    ))
                } else {
                    None
                }
            }
            CpuidFunction::ProcessorTopologyDefinition => Some(CpuidResultMask::new(
                0xffffffff, 0xffffffff, 0xffffffff, 0, false,
            )),
            CpuidFunction::ExtendedSevFeatures => Some(CpuidResultMask::new(
                cpuid::ExtendedSevFeaturesEax::new()
                    .with_sme(true)
                    .with_sev(true)
                    .with_vmpage_flush_msr_available(true)
                    .with_sev_es(true)
                    .with_sev_snp(true)
                    .with_vmpl(true)
                    .with_rmp_query(true)
                    .with_tsc_aux_virtualization(true)
                    .into(),
                cpuid::ExtendedSevFeaturesEbx::new()
                    .with_cbit_position(0x3f)
                    .with_encryption_physical_bits_used(0x3f)
                    .with_number_of_vmpls(0xf)
                    .into(),
                0xffffffff, // MaximumEncryptedGuests
                0xffffffff, // MinimumNonEsAsid
                false,
            )),
            _ => None,
        }
    }

    fn validate_results(&self, results: &CpuidResults) -> Result<(), CpuidResultsError> {
        for &(leaf, subleaf) in SNP_REQUIRED_LEAVES {
            if results.leaf_result_ref(leaf, subleaf, true).is_none() {
                return Err(CpuidResultsError::MissingRequiredResult(leaf, subleaf));
            }
        }

        let extended_address_space_sizes = results
            .leaf_result_ref(CpuidFunction::ExtendedAddressSpaceSizes, None, true)
            .expect("validated this exists");

        if !cpuid::ExtendedAddressSpaceSizesEbx::from(extended_address_space_sizes.ebx).invlpgb() {
            return Err(CpuidResultsError::InvlpgbUnavailable);
        }

        Ok(())
    }

    fn cpuid_info(&self) -> Vec<ParsedCpuidEntry> {
        SnpCpuidIterator {
            cpuid_pages: self.cpuid_pages.as_slice(),
            index: CpuidPageIndex::new(self.cpuid_pages.as_slice()),
        }
        .collect()
    }

    fn process_extended_state_subleaves(
        &self,
        results: &mut CpuidSubtable,
        extended_state_mask: u64,
    ) -> Result<(), CpuidResultsError> {
        let summary_mask = extended_state_mask & !xsave::X86X_XSAVE_LEGACY_FEATURES;

        for i in 0..=super::MAX_EXTENDED_STATE_ENUMERATION_SUBLEAF {
            if ((1 << i) & summary_mask != 0) && results.get(&i).is_none() {
                return Err(CpuidResultsError::MissingRequiredResult(
                    CpuidFunction::ExtendedStateEnumeration,
                    Some(i),
                ));
            }
        }

        Ok(())
    }

    fn extended_topology(
        &self,
        version_and_features_ebx: cpuid::VersionAndFeaturesEbx,
        version_and_features_edx: cpuid::VersionAndFeaturesEdx,
        address_space_sizes_ecx: cpuid::ExtendedAddressSpaceSizesEcx,
        processor_topology_ebx: Option<cpuid::ProcessorTopologyDefinitionEbx>,
    ) -> Result<ExtendedTopologyResult, CpuidResultsError> {
        let vps_per_socket;

        let apic_core_id_size = address_space_sizes_ecx.apic_core_id_size();

        if apic_core_id_size == 0 {
            // Legacy method
            if !version_and_features_edx.mt_per_socket() {
                // verify the package contains one logical processor
                if (address_space_sizes_ecx.nc() != 0)
                    || (version_and_features_ebx.lps_per_package() > 1)
                {
                    return Err(CpuidResultsError::TopologyInconsistent(
                        TopologyError::Hyperthreading(apic_core_id_size),
                    ));
                }
                vps_per_socket = 1;
            } else {
                vps_per_socket = version_and_features_ebx.lps_per_package() as u32;

                if vps_per_socket != (address_space_sizes_ecx.nc() + 1) as u32 {
                    return Err(CpuidResultsError::TopologyInconsistent(
                        TopologyError::ProcessorCount(apic_core_id_size),
                    ));
                }
            }
        } else {
            vps_per_socket = 1 << apic_core_id_size;

            // If there are < 256 processors per socket, make sure that the information exposed via the
            // legacy method fields is consistent.

            if vps_per_socket < 256 {
                if ((address_space_sizes_ecx.nc() + 1) as u32 > vps_per_socket)
                    || (vps_per_socket != version_and_features_ebx.lps_per_package() as u32)
                {
                    return Err(CpuidResultsError::TopologyInconsistent(
                        TopologyError::ProcessorCount(apic_core_id_size),
                    ));
                }

                if (vps_per_socket > 1) != version_and_features_edx.mt_per_socket() {
                    return Err(CpuidResultsError::TopologyInconsistent(
                        TopologyError::Hyperthreading(apic_core_id_size),
                    ));
                }
            }
        }

        let mut smt_enabled = false;

        let processor_topology_ebx = processor_topology_ebx.expect("must exist on SNP");

        if (processor_topology_ebx.threads_per_compute_unit() > 1)
            || (processor_topology_ebx.threads_per_compute_unit() as u32 >= vps_per_socket)
        {
            return Err(CpuidResultsError::TopologyInconsistent(
                TopologyError::ThreadsPerUnit,
            ));
        }

        if processor_topology_ebx.threads_per_compute_unit() > 0 {
            smt_enabled = true;
        }

        let topology_subleaf0 = CpuidResult {
            eax: cpuid::ExtendedTopologyEax::new()
                .with_x2_apic_shift(if smt_enabled { 1 } else { 0 })
                .into(),
            ebx: cpuid::ExtendedTopologyEbx::new()
                .with_num_lps(if smt_enabled { 2 } else { 1 })
                .into(),
            ecx: cpuid::ExtendedTopologyEcx::new()
                .with_level_number(super::CPUID_LEAF_B_LEVEL_NUMBER_SMT)
                .with_level_type(super::CPUID_LEAF_B_LEVEL_TYPE_SMT)
                .into(),
            edx: 0,
        };

        let x2_apic_shift = 31 - vps_per_socket.leading_zeros().min(31);

        let topology_subleaf1 = CpuidResult {
            eax: cpuid::ExtendedTopologyEax::new()
                .with_x2_apic_shift(x2_apic_shift)
                .into(),
            ebx: cpuid::ExtendedTopologyEbx::new()
                .with_num_lps(vps_per_socket as u16)
                .into(),
            ecx: cpuid::ExtendedTopologyEcx::new()
                .with_level_number(super::CPUID_LEAF_B_LEVEL_NUMBER_CORE)
                .with_level_type(super::CPUID_LEAF_B_LEVEL_TYPE_CORE)
                .into(),
            edx: 0,
        };

        Ok(ExtendedTopologyResult {
            subleaf0: Some(topology_subleaf0),
            subleaf1: Some(topology_subleaf1),
        })
    }

    fn btc_no(&self) -> Option<bool> {
        // Advertise BTC_NO since it is available on all SNP-capable parts
        // regardless of what the processor reports.

        Some(true)
    }

    fn supports_tsc_aux_virtualization(&self, results: &CpuidResults) -> bool {
        let sev_features_eax = cpuid::ExtendedSevFeaturesEax::from(
            results
                .leaf_result_ref(CpuidFunction::ExtendedSevFeatures, None, true)
                .expect("this leaf was validated to exist")
                .eax,
        );

        sev_features_eax.tsc_aux_virtualization()
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
            .with_direct_synthetic_timers(true)
            .with_extended_gva_ranges_for_flush_virtual_address_list_available(true)
            .with_guest_idle_available(true)
            .with_xmm_registers_for_fast_hypercall_available(true)
            .with_register_pat_available(true)
            .with_fast_hypercall_output_available(true)
            .with_translate_gva_flags_available(true)
            .with_guest_crash_regs_available(true);

        let enlightenments = hvdef::HvEnlightenmentInformation::new()
            .with_deprecate_auto_eoi(true)
            .with_use_relaxed_timing(true)
            .with_use_ex_processor_masks(true)
            // If only xAPIC is supported, then the Hyper-V MSRs are
            // more efficient for EOIs.
            // If X2APIC is supported, then we can use the X2APIC MSRs. These
            // are as efficient as the Hyper-V MSRs, and they are
            // compatible with APIC hardware offloads.
            // However, Lazy EOI on SNP is beneficial and requires the
            // Hyper-V MSRs to function. Enable it here always.
            .with_use_apic_msrs(true)
            .with_long_spin_wait_count(!0)
            .with_use_hypercall_for_remote_flush_and_local_flush_entire(true)
            .with_use_synthetic_cluster_ipi(true);

        let hardware_features = hvdef::HvHardwareFeatures::new()
            .with_apic_overlay_assist_in_use(true)
            .with_msr_bitmaps_in_use(true)
            .with_second_level_address_translation_in_use(true)
            .with_dma_remapping_in_use(false)
            .with_interrupt_remapping_in_use(false);

        let isolation_config = hvdef::HvIsolationConfiguration::new()
            .with_paravisor_present(true)
            .with_isolation_type(virt::IsolationType::Snp.to_hv().0)
            .with_shared_gpa_boundary_active(true)
            .with_shared_gpa_boundary_bits(self.vtom.trailing_zeros() as u8);

        let [l0, l1, l2] =
            hv1_emulator::cpuid::make_hv_cpuid_leaves(features, enlightenments, MAX_CPUS);
        let [l3, l4] =
            hv1_emulator::cpuid::make_isolated_hv_cpuid_leaves(hardware_features, isolation_config);

        [l0, l1, l2, l3, l4]
    }

    fn update_xsave_dependencies(&self, _xsave_support: u64, _results: &mut CpuidResults) {
        // No SNP-specific updates
    }
}

pub struct SnpCpuidIterator<'a> {
    cpuid_pages: &'a [HvPspCpuidPage],
    index: CpuidPageIndex,
}

impl Iterator for SnpCpuidIterator<'_> {
    type Item = ParsedCpuidEntry;

    fn next(&mut self) -> Option<Self::Item> {
        // The Linux kernel puts extended state enumeration into page 0. We don't
        // trust these values, so until this behavior is removed, skip
        // these values
        loop {
            if self.index.in_bounds().is_err() {
                return None;
            }

            let next_leaf =
                &self.cpuid_pages[self.index.page_index].cpuid_leaf_info[self.index.function_index];

            if !(self.index.page_index != 1
                && next_leaf.eax_in == CpuidFunction::ExtendedStateEnumeration.0)
            {
                break;
            }

            let _ = self.index.increment();
        }

        let leaf =
            &self.cpuid_pages[self.index.page_index].cpuid_leaf_info[self.index.function_index];

        let _ = self.index.increment();

        Some(ParsedCpuidEntry {
            leaf: CpuidFunction(leaf.eax_in),
            subleaf: leaf.ecx_in,
            result: CpuidResult {
                eax: leaf.eax_out,
                ebx: leaf.ebx_out,
                ecx: leaf.ecx_out,
                edx: leaf.edx_out,
            },
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use x86defs::snp::HvPspCpuidLeaf;

    // Tests the increment and validation implementations of CpuidPageIndex
    #[test]
    fn cpuid_index() {
        let mut pages = vec![
            HvPspCpuidPage::new_zeroed(),
            HvPspCpuidPage::new_zeroed(),
            HvPspCpuidPage::new_zeroed(),
            HvPspCpuidPage::new_zeroed(),
            HvPspCpuidPage::new_zeroed(),
            HvPspCpuidPage::new_zeroed(),
        ];

        pages[0].count = 1;
        pages[1].count = 5;
        pages[2].count = 0;
        pages[3].count = 0;
        pages[4].count = 1;
        pages[5].count = 0;

        let mut cpuid_index = CpuidPageIndex::new(pages.as_slice());

        assert_eq!(cpuid_index.function_index, 0);
        assert_eq!(cpuid_index.page_index, 0);
        assert!(cpuid_index.in_bounds().is_ok());

        assert!(cpuid_index.increment().is_ok());
        assert_eq!(cpuid_index.function_index, 0);
        assert_eq!(cpuid_index.page_index, 1);
        assert!(cpuid_index.in_bounds().is_ok());

        for i in 1..pages[1].count {
            assert!(cpuid_index.increment().is_ok());
            assert_eq!(cpuid_index.function_index, i as usize);
            assert_eq!(cpuid_index.page_index, 1);
            assert!(cpuid_index.in_bounds().is_ok());
        }

        assert!(cpuid_index.increment().is_ok());
        assert_eq!(cpuid_index.function_index, 0);
        assert_eq!(cpuid_index.page_index, 4);
        assert!(cpuid_index.in_bounds().is_ok());

        assert!(cpuid_index.increment().is_err());
        assert!(cpuid_index.in_bounds().is_err());
    }

    // Tests incrementing CpuidPageIndex over cpuid pages containing no results
    #[test]
    fn cpuid_index_no_results() {
        let pages = vec![
            HvPspCpuidPage::new_zeroed(),
            HvPspCpuidPage::new_zeroed(),
            HvPspCpuidPage::new_zeroed(),
        ];

        let mut cpuid_index = CpuidPageIndex::new(pages.as_slice());

        assert_eq!(cpuid_index.function_index, 0);
        assert_eq!(cpuid_index.page_index, 3);

        assert!(cpuid_index.in_bounds().is_err());
        assert!(cpuid_index.increment().is_err());
        assert!(cpuid_index.in_bounds().is_err());
    }

    // Tests the iterator over cpuid pages
    #[test]
    fn cpuid_iter() {
        let mut pages = vec![
            HvPspCpuidPage::new_zeroed(),
            HvPspCpuidPage::new_zeroed(),
            HvPspCpuidPage::new_zeroed(),
            HvPspCpuidPage::new_zeroed(),
        ];

        pages[0].count = 2;
        pages[0].cpuid_leaf_info[0] = HvPspCpuidLeaf {
            eax_in: CpuidFunction::ExtendedVersionAndFeatures.0,
            ecx_in: 0,
            xfem_in: 0,
            xss_in: 0,
            eax_out: 0x1,
            ebx_out: 0x0,
            ecx_out: 0x0,
            edx_out: 0x0,
            reserved_z: 0,
        };
        // This should be skipped
        pages[0].cpuid_leaf_info[1] = HvPspCpuidLeaf {
            eax_in: CpuidFunction::ExtendedStateEnumeration.0,
            ecx_in: 0,
            xfem_in: 0,
            xss_in: 0,
            eax_out: 0xffffffff,
            ebx_out: 0x0,
            ecx_out: 0x0,
            edx_out: 0x0,
            reserved_z: 0,
        };

        pages[1].count = 2;
        pages[1].cpuid_leaf_info[0] = HvPspCpuidLeaf {
            eax_in: CpuidFunction::ExtendedStateEnumeration.0,
            ecx_in: 0,
            xfem_in: 0,
            xss_in: 0,
            eax_out: 0x2,
            ebx_out: 0x0,
            ecx_out: 0x0,
            edx_out: 0x0,
            reserved_z: 0,
        };
        pages[1].cpuid_leaf_info[1] = HvPspCpuidLeaf {
            eax_in: CpuidFunction::ExtendedVersionAndFeatures.0,
            ecx_in: 0,
            xfem_in: 0,
            xss_in: 0,
            eax_out: 0x3,
            ebx_out: 0x0,
            ecx_out: 0x0,
            edx_out: 0x0,
            reserved_z: 0,
        };

        // 0 count, should be skipped
        pages[2].cpuid_leaf_info[1] = HvPspCpuidLeaf {
            eax_in: CpuidFunction::ExtendedVersionAndFeatures.0,
            ecx_in: 0,
            xfem_in: 0,
            xss_in: 0,
            eax_out: 0xc0ffee,
            ebx_out: 0x0,
            ecx_out: 0x0,
            edx_out: 0x0,
            reserved_z: 0,
        };

        pages[3].count = 3;
        // This should be skipped
        pages[3].cpuid_leaf_info[0] = HvPspCpuidLeaf {
            eax_in: CpuidFunction::ExtendedStateEnumeration.0,
            ecx_in: 0,
            xfem_in: 0,
            xss_in: 0,
            eax_out: 0xffffffff,
            ebx_out: 0x0,
            ecx_out: 0x0,
            edx_out: 0x0,
            reserved_z: 0,
        };
        pages[3].cpuid_leaf_info[1] = HvPspCpuidLeaf {
            eax_in: CpuidFunction::ExtendedVersionAndFeatures.0,
            ecx_in: 0,
            xfem_in: 0,
            xss_in: 0,
            eax_out: 0x4,
            ebx_out: 0x0,
            ecx_out: 0x0,
            edx_out: 0x0,
            reserved_z: 0,
        };
        pages[3].cpuid_leaf_info[2] = HvPspCpuidLeaf {
            eax_in: CpuidFunction::ExtendedVersionAndFeatures.0,
            ecx_in: 0,
            xfem_in: 0,
            xss_in: 0,
            eax_out: 0x5,
            ebx_out: 0x0,
            ecx_out: 0x0,
            edx_out: 0x0,
            reserved_z: 0,
        };

        let iter = SnpCpuidIterator {
            cpuid_pages: pages.as_slice(),
            index: CpuidPageIndex::new(pages.as_slice()),
        };

        let mut expected_value = 1;
        for entry in iter {
            assert_eq!(expected_value, entry.result.eax);
            expected_value += 1;
        }
    }
}
