// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Helper methods and general tests for CPUID handling for hardware-isolated VMs.

mod topology;
mod xfem;

use super::*;
use x86defs::cpuid::Vendor;
use x86defs::snp::HvPspCpuidLeaf;
use x86defs::snp::HvPspCpuidPage;
use zerocopy::FromZeros;
use zerocopy::IntoBytes;

// Because of the filtering logic, besides subleaves 0 and 1, these are the
// extended state enumeration subleaves that can be tested on.
const XSAVE_ADDITIONAL_SUBLEAF_MASK: cpuid::ExtendedStateEnumerationSubleaf0Eax =
    cpuid::ExtendedStateEnumerationSubleaf0Eax::new()
        .with_avx(true)
        .with_opmask(true)
        .with_zmmhi(true)
        .with_zmm16_31(true);

fn fill_result(leaf: CpuidFunction, subleaf: Option<u32>) -> HvPspCpuidLeaf {
    HvPspCpuidLeaf {
        eax_in: leaf.0,
        ecx_in: subleaf.unwrap_or_default(),
        xfem_in: 0,
        xss_in: 0,
        eax_out: 0xffffffff,
        ebx_out: 0xffffffff,
        ecx_out: 0xffffffff,
        edx_out: 0xffffffff,
        reserved_z: 0,
    }
}

/// If the leaf needs extra handling for test values, fills in a value for the
/// leaf. Returns true if a value was filled.
fn fill_special_leaf(
    leaf: CpuidFunction,
    subleaf: Option<u32>,
    pages: &mut [HvPspCpuidPage],
) -> bool {
    let page0_index = pages[0].count as usize;
    let page1_index = pages[1].count as usize;

    if leaf != CpuidFunction::ExtendedStateEnumeration {
        assert!(page0_index < pages[0].cpuid_leaf_info.len());

        let filled = match leaf {
            CpuidFunction::VersionAndFeatures => {
                pages[0].cpuid_leaf_info[page0_index] = HvPspCpuidLeaf {
                    eax_in: CpuidFunction::VersionAndFeatures.0,
                    ecx_in: 0,
                    xfem_in: 0,
                    xss_in: 0,
                    eax_out: 0xffffffff,
                    ebx_out: cpuid::VersionAndFeaturesEbx::from(0xffffffff)
                        .with_lps_per_package(0x10)
                        .into(),
                    ecx_out: 0xffffffff,
                    edx_out: cpuid::VersionAndFeaturesEdx::from(0xffffffff)
                        .with_mt_per_socket(true)
                        .into(),
                    reserved_z: 0,
                };
                true
            }
            CpuidFunction::ExtendedAddressSpaceSizes => {
                pages[0].cpuid_leaf_info[page0_index] = HvPspCpuidLeaf {
                    eax_in: CpuidFunction::ExtendedAddressSpaceSizes.0,
                    ecx_in: 0,
                    xfem_in: 0,
                    xss_in: 0,
                    eax_out: 0xffffffff,
                    ebx_out: 0xffffffff,
                    ecx_out: cpuid::ExtendedAddressSpaceSizesEcx::new()
                        .with_nc(15)
                        .into(),
                    edx_out: 0xffffffff,
                    reserved_z: 0,
                };
                true
            }
            CpuidFunction::ProcessorTopologyDefinition => {
                pages[0].cpuid_leaf_info[page0_index] = HvPspCpuidLeaf {
                    eax_in: CpuidFunction::ProcessorTopologyDefinition.0,
                    ecx_in: 0,
                    xfem_in: 0,
                    xss_in: 0,
                    eax_out: 0xffffffff,
                    ebx_out: cpuid::ProcessorTopologyDefinitionEbx::from(0xffffffff)
                        .with_threads_per_compute_unit(0x1)
                        .into(),
                    ecx_out: 0xffffffff,
                    edx_out: 0xffffffff,
                    reserved_z: 0,
                };
                true
            }
            _ => false,
        };
        if filled {
            pages[0].count += 1;
        }

        filled
    } else {
        assert!(page1_index < pages[1].cpuid_leaf_info.len());
        pages[1].cpuid_leaf_info[page1_index] = fill_result(leaf, subleaf);
        pages[1].count += 1;
        true
    }
}

/// Fills in the extended state enumeration subleaves 2+ based on the masks in
/// subleafs 0 and 1
fn fill_additional_extended_state_enum_subleaves(pages: &mut [HvPspCpuidPage]) {
    let mut page1_index = pages[1].count as usize;

    let mask = {
        let mut subleaf0_mask: Option<u64> = None;
        let mut subleaf1_mask: Option<u64> = None;
        for leaf in pages[1].cpuid_leaf_info {
            if leaf.eax_in == CpuidFunction::ExtendedStateEnumeration.0 {
                if subleaf0_mask.is_none() && leaf.ecx_in == 0 {
                    subleaf0_mask = Some(leaf.eax_out as u64 | ((leaf.edx_out as u64) << 32));
                } else if subleaf1_mask.is_none() && leaf.ecx_in == 1 {
                    let subleaf1_eax =
                        cpuid::ExtendedStateEnumerationSubleaf1Eax::from(leaf.eax_out);
                    if subleaf1_eax.xsave_c() && subleaf1_eax.xsave_s() {
                        subleaf1_mask = Some(leaf.ecx_out as u64 | ((leaf.edx_out as u64) << 32));
                    } else {
                        subleaf1_mask = Some(0);
                    }
                }
            }
        }
        subleaf0_mask.unwrap_or_default() | subleaf1_mask.unwrap_or_default()
    };

    let additional_extended_state_leaves = ((u32::from(XSAVE_ADDITIONAL_SUBLEAF_MASK) as u64)
        | xsave::XSAVE_SUPERVISOR_FEATURE_CET)
        & mask;

    for i in 0..(MAX_EXTENDED_STATE_ENUMERATION_SUBLEAF + 1) {
        if (1 << i) & additional_extended_state_leaves != 0 {
            let result = HvPspCpuidLeaf {
                eax_in: CpuidFunction::ExtendedStateEnumeration.0,
                ecx_in: i,
                xfem_in: 0,
                xss_in: 0,
                eax_out: 0x88,
                ebx_out: 0x88,
                ecx_out: 0xffffffff,
                edx_out: 0xffffffff,
                reserved_z: 0,
            };

            assert!(page1_index < pages[1].cpuid_leaf_info.len());
            pages[1].cpuid_leaf_info[page1_index] = result;
            pages[1].count += 1;
            page1_index += 1;
        }
    }
}

/// Fills in the leaves that are required into the CPUID pages. Some of them
/// may have already been filled in by the caller, so leaves that the caller
/// wishes to have skipped can be provided.
fn fill_required_leaves(pages: &mut [HvPspCpuidPage], skip_leaves: Option<&[CpuidFunction]>) {
    // This could end up duplicating values, depending on what the caller
    // has already put into the buffer. As a result, this will test
    // filtering of duplicates.
    assert!(pages.len() >= 2);
    for (leaf, subleaf) in [COMMON_REQUIRED_LEAVES, snp::SNP_REQUIRED_LEAVES].concat() {
        if let Some(skip) = skip_leaves {
            if skip.contains(&leaf) {
                continue;
            }
        }

        if !fill_special_leaf(leaf, subleaf, pages) {
            // Extended State enumeration leaves are the only ones that use page index 1, and those should be handled by fill_special_leaf
            let page0_index = pages[0].count as usize;
            assert!(page0_index < pages[0].cpuid_leaf_info.len());
            pages[0].cpuid_leaf_info[page0_index] = fill_result(leaf, subleaf);
            pages[0].count += 1;
        }
    }

    let add_additional_extended_state = {
        if let Some(skip) = skip_leaves {
            !skip.contains(&CpuidFunction::ExtendedStateEnumeration)
        } else {
            true
        }
    };

    if add_additional_extended_state {
        fill_additional_extended_state_enum_subleaves(pages);
    }
}

/// Test basic leaf parsing and the vendor
#[test]
fn populate_and_filter() {
    let mut pages = vec![HvPspCpuidPage::new_zeroed(), HvPspCpuidPage::new_zeroed()];

    pages[0].count += 1;
    pages[0].cpuid_leaf_info[0] = HvPspCpuidLeaf {
        eax_in: CpuidFunction::ExtendedAddressSpaceSizes.0,
        ecx_in: 0,
        xfem_in: 0,
        xss_in: 0,
        eax_out: 0xffffffff,
        ebx_out: 0xffffffff,
        ecx_out: 0xffffffff,
        edx_out: 0xffffffff,
        reserved_z: 0,
    };

    // Adding the same leaf a second time doesn't fail or overwrite
    pages[0].count += 1;
    pages[0].cpuid_leaf_info[1] = HvPspCpuidLeaf {
        eax_in: CpuidFunction::ExtendedAddressSpaceSizes.0,
        ecx_in: 0,
        xfem_in: 0,
        xss_in: 0,
        eax_out: 0,
        ebx_out: 0,
        ecx_out: 0,
        edx_out: 0,
        reserved_z: 0,
    };

    fill_required_leaves(&mut pages, None);
    let cpuid = CpuidResults::new(CpuidResultsIsolationType::Snp {
        cpuid_pages: pages.as_slice().as_bytes(),
    })
    .unwrap();

    assert_eq!(
        cpuid.registered_result(CpuidFunction::ExtendedAddressSpaceSizes, 0),
        CpuidResult {
            eax: cpuid::ExtendedAddressSpaceSizesEax::new()
                .with_physical_address_size(0xff)
                .with_virtual_address_size(0xff)
                .into(),
            ebx: cpuid::ExtendedAddressSpaceSizesEbx::new()
                .with_wbnoinvd(true)
                .with_cl_zero(true)
                .with_inst_ret_cnt_msr(true)
                .with_x_save_er_ptr(true)
                .with_invlpgb(true)
                .with_rdpru(true)
                .with_ibpb(true)
                .with_ibrs(true)
                .with_stibp(true)
                .with_ssbd(true)
                .with_stibp_always_on(true)
                .with_efer_lmsle_unsupported(true)
                .with_psfd(true)
                .with_btc_no(true)
                .into(),
            ecx: cpuid::ExtendedAddressSpaceSizesEcx::new()
                .with_nc(0xff)
                .with_apic_core_id_size(0xf)
                .into(),
            edx: cpuid::ExtendedAddressSpaceSizesEdx::new()
                .with_invlpgb_count_max(0xffff)
                .with_rdpru_max_ecx(0xffff)
                .into(),
        }
    );

    // Check vendor and max function
    let CpuidResult {
        eax: max_function,
        ebx,
        ecx,
        edx,
    } = cpuid.registered_result(CpuidFunction::VendorAndMaxFunction, 0);

    assert!(Vendor::from_ebx_ecx_edx(ebx, ecx, edx).is_amd_compatible());
    assert_eq!(max_function, CpuidFunction::AmdMaximum.0);

    // Check vendor and extended max function
    let CpuidResult {
        eax: extended_max_function,
        ebx,
        ecx,
        edx,
    } = cpuid.registered_result(CpuidFunction::ExtendedMaxFunction, 0);

    assert!(Vendor::from_ebx_ecx_edx(ebx, ecx, edx).is_amd_compatible());
    assert_eq!(extended_max_function, CpuidFunction::ExtendedAmdMaximum.0);
}

/// Test adding a subtable with some subleaves
#[test]
fn subleaf() {
    let mut pages = vec![HvPspCpuidPage::new_zeroed(), HvPspCpuidPage::new_zeroed()];

    pages[0].count += 1;
    pages[0].cpuid_leaf_info[0] = HvPspCpuidLeaf {
        eax_in: CpuidFunction::CacheTopologyDefinition.0,
        ecx_in: 0,
        xfem_in: 0,
        xss_in: 0,
        eax_out: 0xffffffff,
        ebx_out: 0xffffffff,
        ecx_out: 0xffffffff,
        edx_out: 0xffffffff,
        reserved_z: 0,
    };

    // only first entry accepted
    pages[0].count += 1;
    pages[0].cpuid_leaf_info[1] = HvPspCpuidLeaf {
        eax_in: CpuidFunction::CacheTopologyDefinition.0,
        ecx_in: 0,
        xfem_in: 0,
        xss_in: 0,
        eax_out: 0,
        ebx_out: 0,
        ecx_out: 0,
        edx_out: 0,
        reserved_z: 0,
    };

    // multiple subleaves
    pages[0].count += 1;
    pages[0].cpuid_leaf_info[2] = HvPspCpuidLeaf {
        eax_in: CpuidFunction::CacheTopologyDefinition.0,
        ecx_in: 1,
        xfem_in: 0,
        xss_in: 0,
        eax_out: 0x12345678,
        ebx_out: 0x12345678,
        ecx_out: 0x12345678,
        edx_out: 0x12345678,
        reserved_z: 0,
    };

    fill_required_leaves(&mut pages, None);

    let cpuid = CpuidResults::new(CpuidResultsIsolationType::Snp {
        cpuid_pages: pages.as_slice().as_bytes(),
    })
    .unwrap();

    assert_eq!(
        cpuid.registered_result(CpuidFunction::CacheTopologyDefinition, 0),
        CpuidResult {
            eax: 0xffffffff,
            ebx: 0xffffffff,
            ecx: 0xffffffff,
            edx: 0xffffffff
        }
    );

    assert_eq!(
        cpuid.registered_result(CpuidFunction::CacheTopologyDefinition, 1),
        CpuidResult {
            eax: 0x12345678,
            ebx: 0x12345678,
            ecx: 0x12345678,
            edx: 0x12345678
        }
    );
}

// TODO: this is SNP-specific
#[test]
fn invlpgb() {
    let mut pages = vec![HvPspCpuidPage::new_zeroed(), HvPspCpuidPage::new_zeroed()];

    pages[0].count += 1;
    pages[0].cpuid_leaf_info[0] = HvPspCpuidLeaf {
        eax_in: CpuidFunction::ExtendedAddressSpaceSizes.0,
        ecx_in: 0,
        xfem_in: 0,
        xss_in: 0,
        eax_out: 0xffffffff,
        ebx_out: cpuid::ExtendedAddressSpaceSizesEbx::from(0xffffffff)
            .with_invlpgb(false)
            .into(),
        ecx_out: cpuid::ExtendedAddressSpaceSizesEcx::new()
            .with_nc(15)
            .into(),
        edx_out: 0xffffffff,
        reserved_z: 0,
    };

    fill_required_leaves(&mut pages, None);

    assert!(matches!(
        CpuidResults::new(CpuidResultsIsolationType::Snp {
            cpuid_pages: pages.as_slice().as_bytes(),
        }),
        Err(CpuidResultsError::InvlpgbUnavailable)
    ));
}

#[test]
fn extended_address_space_sizes() {
    let mut pages = vec![HvPspCpuidPage::new_zeroed(), HvPspCpuidPage::new_zeroed()];

    pages[0].count += 1;
    pages[0].cpuid_leaf_info[0] = HvPspCpuidLeaf {
        eax_in: CpuidFunction::ExtendedAddressSpaceSizes.0,
        ecx_in: 0,
        xfem_in: 0,
        xss_in: 0,
        eax_out: 0xffffffff,
        ebx_out: cpuid::ExtendedAddressSpaceSizesEbx::new()
            .with_invlpgb(true)
            .with_ibrs(true)
            .with_stibp(true)
            .into(),
        ecx_out: cpuid::ExtendedAddressSpaceSizesEcx::new()
            .with_nc(15)
            .into(),
        edx_out: 0xffffffff,
        reserved_z: 0,
    };

    fill_required_leaves(&mut pages, None);

    let cpuid = CpuidResults::new(CpuidResultsIsolationType::Snp {
        cpuid_pages: pages.as_slice().as_bytes(),
    })
    .unwrap();

    let address_space_sizes_ebx = cpuid::ExtendedAddressSpaceSizesEbx::from(
        cpuid
            .registered_result(CpuidFunction::ExtendedAddressSpaceSizes, 0)
            .ebx,
    );

    assert!(address_space_sizes_ebx.btc_no());
    assert!(!address_space_sizes_ebx.ibrs());
    assert!(!address_space_sizes_ebx.stibp());
}

#[test]
fn hypervisor_present() {
    let mut pages = vec![HvPspCpuidPage::new_zeroed(), HvPspCpuidPage::new_zeroed()];

    fill_required_leaves(&mut pages, None);

    let cpuid = CpuidResults::new(CpuidResultsIsolationType::Snp {
        cpuid_pages: pages.as_slice().as_bytes(),
    })
    .unwrap();

    assert!(cpuid::VersionAndFeaturesEcx::from(
        cpuid
            .registered_result(CpuidFunction::VersionAndFeatures, 0)
            .ecx,
    )
    .hypervisor_present());
}

#[test]
fn guest_results() {
    let mut pages = vec![HvPspCpuidPage::new_zeroed(), HvPspCpuidPage::new_zeroed()];

    fill_required_leaves(&mut pages, None);

    let cpuid = CpuidResults::new(CpuidResultsIsolationType::Snp {
        cpuid_pages: pages.as_slice().as_bytes(),
    })
    .unwrap();

    // Returning something non-zero, to make it obvious. The fill_required_leaves also fills
    // in 8 as the default value, so choose something else.
    let apic_id = 0xd;
    let test_results = [
        (
            CpuidFunction::VersionAndFeatures,
            0,
            CpuidGuestState {
                xfem: 0,
                xss: 0,
                cr4: x86defs::X64_CR4_OSXSAVE,
                apic_id,
            },
            CpuidResult {
                eax: 0xfff3fff,
                ebx: 0xd10ffff,
                ecx: 0xfefa3203,
                edx: 0x178bfbff,
            },
        ),
        (
            CpuidFunction::VersionAndFeatures,
            0,
            CpuidGuestState {
                xfem: 0,
                xss: 0,
                cr4: 0,
                apic_id,
            },
            CpuidResult {
                eax: 0xfff3fff,
                ebx: 0xd10ffff,
                ecx: 0xf6fa3203,
                edx: 0x178bfbff,
            },
        ),
        (
            CpuidFunction::ExtendedTopologyEnumeration,
            0,
            CpuidGuestState {
                xfem: 0,
                xss: 0,
                cr4: 0,
                apic_id,
            },
            CpuidResult {
                eax: 0x1,
                ebx: 0x2,
                ecx: 0x100,
                edx: 0xd,
            },
        ),
        (
            CpuidFunction::ExtendedStateEnumeration,
            0,
            CpuidGuestState {
                xfem: 0,
                xss: 0,
                cr4: 0,
                apic_id,
            },
            CpuidResult {
                eax: 0xe7,
                ebx: 0x240,
                ecx: 0x240,
                edx: 0x0,
            },
        ),
        (
            CpuidFunction::ExtendedStateEnumeration,
            1,
            CpuidGuestState {
                xfem: 0x3,
                xss: 0x1800,
                cr4: 0,
                apic_id,
            },
            CpuidResult {
                eax: 0xb,
                ebx: 0x388,
                ecx: 0x1800,
                edx: 0x0,
            },
        ),
        (
            CpuidFunction::ProcessorTopologyDefinition,
            0,
            CpuidGuestState {
                xfem: 0,
                xss: 0,
                cr4: 0,
                apic_id,
            },
            CpuidResult {
                eax: 0xd,
                ebx: 0xffff0106,
                ecx: 0xfffff800,
                edx: 0x0,
            },
        ),
        (
            CpuidFunction::ExtendedSevFeatures,
            0,
            CpuidGuestState {
                xfem: 0,
                xss: 0,
                cr4: 0,
                apic_id,
            },
            ZERO_CPUID_RESULT,
        ),
    ];

    for (leaf, subleaf, guest_state, result) in test_results {
        assert_eq!(cpuid.guest_result(leaf, subleaf, &guest_state), result);
    }
}

// Note: this will not test whether the list of required results is correct, but
// will test that the code enforces the existence of each of the required
// results.
#[test]
fn validate_required_snp() {
    let mut pages = vec![HvPspCpuidPage::new_zeroed(), HvPspCpuidPage::new_zeroed()];

    let mut filled_d0 = false;
    let mut filled_d1 = false;

    for (leaf, subleaf) in [COMMON_REQUIRED_LEAVES, snp::SNP_REQUIRED_LEAVES].concat() {
        assert!(matches!(
            CpuidResults::new(CpuidResultsIsolationType::Snp {
                cpuid_pages: pages.as_slice().as_bytes().as_bytes(),
            }),
            Err(CpuidResultsError::MissingRequiredResult(err_leaf, err_subleaf)) if (err_leaf == leaf && err_subleaf == subleaf)
        ));

        if !fill_special_leaf(leaf, subleaf, &mut pages) {
            // Extended State enumeration leaves are the only ones that use page index 1, and those should be handled by fill_special_leaf
            let page0_index = pages[0].count as usize;
            assert!(page0_index < pages[0].cpuid_leaf_info.len());
            pages[0].cpuid_leaf_info[page0_index] = fill_result(leaf, subleaf);
            pages[0].count += 1;
        }

        if leaf == CpuidFunction::ExtendedStateEnumeration {
            match subleaf.unwrap() {
                0 => filled_d0 = true,
                1 => filled_d1 = true,
                _ => (),
            }
        }

        if filled_d0 && filled_d1 {
            // Missing additional subleaves gets tested by the xfem tests, so
            // just fill in the additional leaves here
            fill_additional_extended_state_enum_subleaves(&mut pages);
        }
    }
}

#[test]
fn zeros_unsupported_leaf() {
    let mut pages = vec![HvPspCpuidPage::new_zeroed(), HvPspCpuidPage::new_zeroed()];

    fill_required_leaves(&mut pages, None);
    let cpuid = CpuidResults::new(CpuidResultsIsolationType::Snp {
        cpuid_pages: pages.as_slice().as_bytes(),
    })
    .unwrap();

    assert_eq!(
        cpuid.registered_result(CpuidFunction::SgxEnumeration, 0),
        ZERO_CPUID_RESULT
    );
}

/// Test effects of turning off tsc aux virtualization
#[test]
fn tsc_aux() {
    let mut pages = vec![HvPspCpuidPage::new_zeroed(), HvPspCpuidPage::new_zeroed()];

    pages[0].count += 1;
    pages[0].cpuid_leaf_info[0] = HvPspCpuidLeaf {
        eax_in: CpuidFunction::ExtendedSevFeatures.0,
        ecx_in: 0,
        xfem_in: 0,
        xss_in: 0,
        eax_out: cpuid::ExtendedSevFeaturesEax::from(0xffffffff)
            .with_tsc_aux_virtualization(false)
            .into(),
        ebx_out: 0xffffffff,
        ecx_out: 0xffffffff,
        edx_out: 0xffffffff,
        reserved_z: 0,
    };

    fill_required_leaves(&mut pages, Some(&[CpuidFunction::ExtendedSevFeatures]));
    let cpuid = CpuidResults::new(CpuidResultsIsolationType::Snp {
        cpuid_pages: pages.as_slice().as_bytes(),
    })
    .unwrap();

    assert!(!cpuid::ExtendedVersionAndFeaturesEdx::from(
        cpuid
            .registered_result(CpuidFunction::ExtendedVersionAndFeatures, 0)
            .edx
    )
    .rdtscp());

    assert!(!cpuid::ExtendedFeatureSubleaf0Ecx::from(
        cpuid
            .registered_result(CpuidFunction::ExtendedFeatures, 0)
            .ecx
    )
    .rd_pid());
}

// values obtained by running cpuid.exe  -G 0 -P 0 (from bin\idw, copied into
// the VM's VHD) inside the VM, and using a debugger to the HCL broken in on
// connection to get the cpuid page data ( dx -r3
// ((PHV_PSP_CPUID_PAGE)0xfffff80004116000)). Guest results required using a
// debugger to get the guest state as well.
#[test]
fn real_values() {
    let mut pages = vec![HvPspCpuidPage::new_zeroed(), HvPspCpuidPage::new_zeroed()];

    let mut page_index = 0;
    pages[0].cpuid_leaf_info[page_index] = HvPspCpuidLeaf {
        eax_in: 0x8000001f,
        ecx_in: 0x0,
        xfem_in: 0x0,
        xss_in: 0x0,
        eax_out: 0x100ff7b,
        ebx_out: 0x41b3,
        ecx_out: 0x0,
        edx_out: 0x0,
        reserved_z: 0x0,
    };
    pages[0].count += 1;
    page_index += 1;

    pages[0].cpuid_leaf_info[page_index] = HvPspCpuidLeaf {
        eax_in: 0x1,
        ecx_in: 0x0,
        xfem_in: 0x0,
        xss_in: 0x0,
        eax_out: 0xa10f11,
        ebx_out: 0x100800,
        ecx_out: 0x76fa3203,
        edx_out: 0x178bfbff,
        reserved_z: 0x0,
    };
    pages[0].count += 1;
    page_index += 1;

    pages[0].cpuid_leaf_info[page_index] = HvPspCpuidLeaf {
        eax_in: 0x2,
        ecx_in: 0x0,
        xfem_in: 0x0,
        xss_in: 0x0,
        eax_out: 0x0,
        ebx_out: 0x0,
        ecx_out: 0x0,
        edx_out: 0x0,
        reserved_z: 0x0,
    };
    pages[0].count += 1;
    page_index += 1;

    pages[0].cpuid_leaf_info[page_index] = HvPspCpuidLeaf {
        eax_in: 0x5,
        ecx_in: 0x0,
        xfem_in: 0x0,
        xss_in: 0x0,
        eax_out: 0x0,
        ebx_out: 0x0,
        ecx_out: 0x0,
        edx_out: 0x0,
        reserved_z: 0x0,
    };
    pages[0].count += 1;
    page_index += 1;

    pages[0].cpuid_leaf_info[page_index] = HvPspCpuidLeaf {
        eax_in: 0x6,
        ecx_in: 0x0,
        xfem_in: 0x0,
        xss_in: 0x0,
        eax_out: 0x0,
        ebx_out: 0x0,
        ecx_out: 0x1,
        edx_out: 0x0,
        reserved_z: 0x0,
    };
    pages[0].count += 1;
    page_index += 1;

    pages[0].cpuid_leaf_info[page_index] = HvPspCpuidLeaf {
        eax_in: 0x7,
        ecx_in: 0x0,
        xfem_in: 0x0,
        xss_in: 0x0,
        eax_out: 0x1,
        ebx_out: 0xf1bf07a9,
        ecx_out: 0x405fc6,
        edx_out: 0x10,
        reserved_z: 0x0,
    };
    pages[0].count += 1;
    page_index += 1;

    pages[0].cpuid_leaf_info[page_index] = HvPspCpuidLeaf {
        eax_in: 0x7,
        ecx_in: 0x1,
        xfem_in: 0x0,
        xss_in: 0x0,
        eax_out: 0x20,
        ebx_out: 0x0,
        ecx_out: 0x0,
        edx_out: 0x0,
        reserved_z: 0x0,
    };
    pages[0].count += 1;
    page_index += 1;

    pages[0].cpuid_leaf_info[page_index] = HvPspCpuidLeaf {
        eax_in: 0x4,
        ecx_in: 0x0,
        xfem_in: 0x0,
        xss_in: 0x0,
        eax_out: 0x0,
        ebx_out: 0x0,
        ecx_out: 0x0,
        edx_out: 0x0,
        reserved_z: 0x0,
    };
    pages[0].count += 1;
    page_index += 1;

    pages[0].cpuid_leaf_info[page_index] = HvPspCpuidLeaf {
        eax_in: 0x4,
        ecx_in: 0x1,
        xfem_in: 0x0,
        xss_in: 0x0,
        eax_out: 0x0,
        ebx_out: 0x0,
        ecx_out: 0x0,
        edx_out: 0x0,
        reserved_z: 0x0,
    };
    pages[0].count += 1;
    page_index += 1;

    pages[0].cpuid_leaf_info[page_index] = HvPspCpuidLeaf {
        eax_in: 0x4,
        ecx_in: 0x2,
        xfem_in: 0x0,
        xss_in: 0x0,
        eax_out: 0x0,
        ebx_out: 0x0,
        ecx_out: 0x0,
        edx_out: 0x0,
        reserved_z: 0x0,
    };
    pages[0].count += 1;
    page_index += 1;

    pages[0].cpuid_leaf_info[page_index] = HvPspCpuidLeaf {
        eax_in: 0x4,
        ecx_in: 0x3,
        xfem_in: 0x0,
        xss_in: 0x0,
        eax_out: 0x0,
        ebx_out: 0x0,
        ecx_out: 0x0,
        edx_out: 0x0,
        reserved_z: 0x0,
    };
    pages[0].count += 1;
    page_index += 1;

    pages[0].cpuid_leaf_info[page_index] = HvPspCpuidLeaf {
        eax_in: 0xb,
        ecx_in: 0x0,
        xfem_in: 0x0,
        xss_in: 0x0,
        eax_out: 0x1,
        ebx_out: 0x2,
        ecx_out: 0x100,
        edx_out: 0x0,
        reserved_z: 0x0,
    };
    pages[0].count += 1;
    page_index += 1;

    pages[0].cpuid_leaf_info[page_index] = HvPspCpuidLeaf {
        eax_in: 0xb,
        ecx_in: 0x1,
        xfem_in: 0x0,
        xss_in: 0x0,
        eax_out: 0x4,
        ebx_out: 0x10,
        ecx_out: 0x201,
        edx_out: 0x0,
        reserved_z: 0x0,
    };
    pages[0].count += 1;
    page_index += 1;

    pages[0].cpuid_leaf_info[page_index] = HvPspCpuidLeaf {
        eax_in: 0x80000001,
        ecx_in: 0x0,
        xfem_in: 0x0,
        xss_in: 0x0,
        eax_out: 0xa10f11,
        ebx_out: 0x40000000,
        ecx_out: 0x4003f3,
        edx_out: 0x27d3fbff,
        reserved_z: 0x0,
    };
    pages[0].count += 1;
    page_index += 1;

    pages[0].cpuid_leaf_info[page_index] = HvPspCpuidLeaf {
        eax_in: 0x80000005,
        ecx_in: 0x0,
        xfem_in: 0x0,
        xss_in: 0x0,
        eax_out: 0xff48ff40,
        ebx_out: 0xff48ff40,
        ecx_out: 0x20080140,
        edx_out: 0x20080140,
        reserved_z: 0x0,
    };
    pages[0].count += 1;
    page_index += 1;

    pages[0].cpuid_leaf_info[page_index] = HvPspCpuidLeaf {
        eax_in: 0x80000006,
        ecx_in: 0x0,
        xfem_in: 0x0,
        xss_in: 0x0,
        eax_out: 0x5c002200,
        ebx_out: 0x6c004200,
        ecx_out: 0x4006140,
        edx_out: 0xa009140,
        reserved_z: 0x0,
    };
    pages[0].count += 1;
    page_index += 1;

    pages[0].cpuid_leaf_info[page_index] = HvPspCpuidLeaf {
        eax_in: 0x80000007,
        ecx_in: 0x0,
        xfem_in: 0x0,
        xss_in: 0x0,
        eax_out: 0x0,
        ebx_out: 0x0,
        ecx_out: 0x0,
        edx_out: 0x0,
        reserved_z: 0x0,
    };
    pages[0].count += 1;
    page_index += 1;

    pages[0].cpuid_leaf_info[page_index] = HvPspCpuidLeaf {
        eax_in: 0x80000008,
        ecx_in: 0x0,
        xfem_in: 0x0,
        xss_in: 0x0,
        eax_out: 0x302f,
        ebx_out: 0x3112d01d,
        ecx_out: 0x400f,
        edx_out: 0x10007,
        reserved_z: 0x0,
    };
    pages[0].count += 1;
    page_index += 1;

    pages[0].cpuid_leaf_info[page_index] = HvPspCpuidLeaf {
        eax_in: 0x8000001e,
        ecx_in: 0x0,
        xfem_in: 0x0,
        xss_in: 0x0,
        eax_out: 0x0,
        ebx_out: 0x100,
        ecx_out: 0x0,
        edx_out: 0x0,
        reserved_z: 0x0,
    };
    pages[0].count += 1;
    page_index += 1;

    pages[0].cpuid_leaf_info[page_index] = HvPspCpuidLeaf {
        eax_in: 0x80000002,
        ecx_in: 0x0,
        xfem_in: 0x0,
        xss_in: 0x0,
        eax_out: 0x20444d41,
        ebx_out: 0x43595045,
        ecx_out: 0x37563920,
        edx_out: 0x30382034,
        reserved_z: 0x0,
    };
    pages[0].count += 1;
    page_index += 1;

    pages[0].cpuid_leaf_info[page_index] = HvPspCpuidLeaf {
        eax_in: 0x80000003,
        ecx_in: 0x0,
        xfem_in: 0x0,
        xss_in: 0x0,
        eax_out: 0x726f432d,
        ebx_out: 0x72502065,
        ecx_out: 0x7365636f,
        edx_out: 0x20726f73,
        reserved_z: 0x0,
    };
    pages[0].count += 1;
    page_index += 1;

    pages[0].cpuid_leaf_info[page_index] = HvPspCpuidLeaf {
        eax_in: 0x80000004,
        ecx_in: 0x0,
        xfem_in: 0x0,
        xss_in: 0x0,
        eax_out: 0x20202020,
        ebx_out: 0x20202020,
        ecx_out: 0x20202020,
        edx_out: 0x202020,
        reserved_z: 0x0,
    };
    pages[0].count += 1;
    page_index += 1;

    pages[0].cpuid_leaf_info[page_index] = HvPspCpuidLeaf {
        eax_in: 0x8000000a,
        ecx_in: 0x0,
        xfem_in: 0x0,
        xss_in: 0x0,
        eax_out: 0x0,
        ebx_out: 0x0,
        ecx_out: 0x0,
        edx_out: 0x0,
        reserved_z: 0x0,
    };
    pages[0].count += 1;
    page_index += 1;

    pages[0].cpuid_leaf_info[page_index] = HvPspCpuidLeaf {
        eax_in: 0x80000019,
        ecx_in: 0x0,
        xfem_in: 0x0,
        xss_in: 0x0,
        eax_out: 0x0,
        ebx_out: 0x0,
        ecx_out: 0x0,
        edx_out: 0x0,
        reserved_z: 0x0,
    };
    pages[0].count += 1;
    page_index += 1;

    pages[0].cpuid_leaf_info[page_index] = HvPspCpuidLeaf {
        eax_in: 0x8000001a,
        ecx_in: 0x0,
        xfem_in: 0x0,
        xss_in: 0x0,
        eax_out: 0x6,
        ebx_out: 0x0,
        ecx_out: 0x0,
        edx_out: 0x0,
        reserved_z: 0x0,
    };
    pages[0].count += 1;
    page_index += 1;

    pages[0].cpuid_leaf_info[page_index] = HvPspCpuidLeaf {
        eax_in: 0x8000001d,
        ecx_in: 0x0,
        xfem_in: 0x0,
        xss_in: 0x0,
        eax_out: 0x4121,
        ebx_out: 0x1c0003f,
        ecx_out: 0x3f,
        edx_out: 0x0,
        reserved_z: 0x0,
    };
    pages[0].count += 1;
    page_index += 1;

    pages[0].cpuid_leaf_info[page_index] = HvPspCpuidLeaf {
        eax_in: 0x8000001d,
        ecx_in: 0x1,
        xfem_in: 0x0,
        xss_in: 0x0,
        eax_out: 0x4122,
        ebx_out: 0x1c0003f,
        ecx_out: 0x3f,
        edx_out: 0x0,
        reserved_z: 0x0,
    };
    pages[0].count += 1;
    page_index += 1;

    pages[0].cpuid_leaf_info[page_index] = HvPspCpuidLeaf {
        eax_in: 0x8000001d,
        ecx_in: 0x2,
        xfem_in: 0x0,
        xss_in: 0x0,
        eax_out: 0x4143,
        ebx_out: 0x1c0003f,
        ecx_out: 0x7ff,
        edx_out: 0x2,
        reserved_z: 0x0,
    };
    pages[0].count += 1;
    page_index += 1;

    pages[0].cpuid_leaf_info[page_index] = HvPspCpuidLeaf {
        eax_in: 0x8000001d,
        ecx_in: 0x3,
        xfem_in: 0x0,
        xss_in: 0x0,
        eax_out: 0x3c163,
        ebx_out: 0x3c0003f,
        ecx_out: 0x7fff,
        edx_out: 0x1,
        reserved_z: 0x0,
    };
    pages[0].count += 1;

    // Extended state enumeration leaves
    page_index = 0;
    pages[1].cpuid_leaf_info[page_index] = HvPspCpuidLeaf {
        eax_in: 0xd,
        ecx_in: 0x0,
        xfem_in: 0x3,
        xss_in: 0x0,
        eax_out: 0xe7,
        ebx_out: 0x240,
        ecx_out: 0x980,
        edx_out: 0x0,
        reserved_z: 0x0,
    };
    pages[1].count += 1;
    page_index += 1;

    pages[1].cpuid_leaf_info[page_index] = HvPspCpuidLeaf {
        eax_in: 0xd,
        ecx_in: 0x1,
        xfem_in: 0x3,
        xss_in: 0x0,
        eax_out: 0xf,
        ebx_out: 0x240,
        ecx_out: 0x1800,
        edx_out: 0x0,
        reserved_z: 0x0,
    };
    pages[1].count += 1;
    page_index += 1;

    pages[1].cpuid_leaf_info[page_index] = HvPspCpuidLeaf {
        eax_in: 0xd,
        ecx_in: 0x2,
        xfem_in: 0x3,
        xss_in: 0x0,
        eax_out: 0x100,
        ebx_out: 0x240,
        ecx_out: 0x0,
        edx_out: 0x0,
        reserved_z: 0x0,
    };
    pages[1].count += 1;
    page_index += 1;

    pages[1].cpuid_leaf_info[page_index] = HvPspCpuidLeaf {
        eax_in: 0xd,
        ecx_in: 0x5,
        xfem_in: 0x3,
        xss_in: 0x0,
        eax_out: 0x40,
        ebx_out: 0x340,
        ecx_out: 0x0,
        edx_out: 0x0,
        reserved_z: 0x0,
    };
    pages[1].count += 1;
    page_index += 1;

    pages[1].cpuid_leaf_info[page_index] = HvPspCpuidLeaf {
        eax_in: 0xd,
        ecx_in: 0x6,
        xfem_in: 0x3,
        xss_in: 0x0,
        eax_out: 0x200,
        ebx_out: 0x380,
        ecx_out: 0x0,
        edx_out: 0x0,
        reserved_z: 0x0,
    };
    pages[1].count += 1;
    page_index += 1;

    pages[1].cpuid_leaf_info[page_index] = HvPspCpuidLeaf {
        eax_in: 0xd,
        ecx_in: 0x7,
        xfem_in: 0x3,
        xss_in: 0x0,
        eax_out: 0x400,
        ebx_out: 0x580,
        ecx_out: 0x0,
        edx_out: 0x0,
        reserved_z: 0x0,
    };
    pages[1].count += 1;
    page_index += 1;

    pages[1].cpuid_leaf_info[page_index] = HvPspCpuidLeaf {
        eax_in: 0xd,
        ecx_in: 0xb,
        xfem_in: 0x3,
        xss_in: 0x0,
        eax_out: 0x10,
        ebx_out: 0x0,
        ecx_out: 0x1,
        edx_out: 0x0,
        reserved_z: 0x0,
    };
    pages[1].count += 1;
    page_index += 1;

    pages[1].cpuid_leaf_info[page_index] = HvPspCpuidLeaf {
        eax_in: 0xd,
        ecx_in: 0xc,
        xfem_in: 0x3,
        xss_in: 0x0,
        eax_out: 0x18,
        ebx_out: 0x0,
        ecx_out: 0x1,
        edx_out: 0x0,
        reserved_z: 0x0,
    };
    pages[1].count += 1;

    let cpuid = CpuidResults::new(CpuidResultsIsolationType::Snp {
        cpuid_pages: pages.as_slice().as_bytes(),
    })
    .unwrap();

    let guest_state = CpuidGuestState {
        xfem: 0xe7,
        xss: 0x800,
        cr4: 0xb50ef8,
        apic_id: 0,
    };

    assert_eq!(
        cpuid.guest_result(CpuidFunction(0x00000000), 0x0, &guest_state),
        CpuidResult {
            eax: 0x0000000d,
            ebx: 0x68747541,
            ecx: 0x444d4163,
            edx: 0x69746e65,
        }
    );

    assert_eq!(
        cpuid.guest_result(CpuidFunction(0x00000001), 0x0, &guest_state),
        CpuidResult {
            eax: 0x00a10f11,
            ebx: 0x00100800,
            ecx: 0xfefa3203,
            edx: 0x178bfbff,
        }
    );

    assert_eq!(
        cpuid.guest_result(CpuidFunction(0x00000002), 0x0, &guest_state),
        CpuidResult {
            eax: 0x00000000,
            ebx: 0x00000000,
            ecx: 0x00000000,
            edx: 0x00000000,
        }
    );

    assert_eq!(
        cpuid.guest_result(CpuidFunction(0x00000003), 0x0, &guest_state),
        CpuidResult {
            eax: 0x00000000,
            ebx: 0x00000000,
            ecx: 0x00000000,
            edx: 0x00000000,
        }
    );

    assert_eq!(
        cpuid.guest_result(CpuidFunction(0x00000004), 0x0, &guest_state),
        CpuidResult {
            eax: 0x00000000,
            ebx: 0x00000000,
            ecx: 0x00000000,
            edx: 0x00000000,
        }
    );

    assert_eq!(
        cpuid.guest_result(CpuidFunction(0x00000005), 0x0, &guest_state),
        CpuidResult {
            eax: 0x00000000,
            ebx: 0x00000000,
            ecx: 0x00000000,
            edx: 0x00000000,
        }
    );

    assert_eq!(
        cpuid.guest_result(CpuidFunction(0x00000006), 0x0, &guest_state),
        CpuidResult {
            eax: 0x00000000,
            ebx: 0x00000000,
            ecx: 0x00000000,
            edx: 0x00000000,
        }
    );

    assert_eq!(
        cpuid.guest_result(CpuidFunction(0x00000007), 0x0, &guest_state),
        CpuidResult {
            eax: 0x00000001,
            ebx: 0xf1bf07a9,
            ecx: 0x00405fc6,
            edx: 0x00000010,
        }
    );

    assert_eq!(
        cpuid.guest_result(CpuidFunction(0x00000008), 0x0, &guest_state),
        CpuidResult {
            eax: 0x00000000,
            ebx: 0x00000000,
            ecx: 0x00000000,
            edx: 0x00000000,
        }
    );

    assert_eq!(
        cpuid.guest_result(CpuidFunction(0x00000009), 0x0, &guest_state),
        CpuidResult {
            eax: 0x00000000,
            ebx: 0x00000000,
            ecx: 0x00000000,
            edx: 0x00000000,
        }
    );

    assert_eq!(
        cpuid.registered_result(CpuidFunction(0x0000000a), 0x0),
        CpuidResult {
            eax: 0x00000000,
            ebx: 0x00000000,
            ecx: 0x00000000,
            edx: 0x00000000,
        }
    );

    assert_eq!(
        cpuid.guest_result(CpuidFunction(0x0000000b), 0x0, &guest_state),
        CpuidResult {
            eax: 0x00000001,
            ebx: 0x00000002,
            ecx: 0x00000100,
            edx: 0x00000000,
        }
    );

    assert_eq!(
        cpuid.guest_result(CpuidFunction(0x0000000b), 0x1, &guest_state),
        CpuidResult {
            eax: 0x00000004,
            ebx: 0x00000010,
            ecx: 0x00000201,
            edx: 0x00000000,
        }
    );

    assert_eq!(
        cpuid.guest_result(CpuidFunction(0x0000000b), 0x2, &guest_state),
        CpuidResult {
            eax: 0x00000000,
            ebx: 0x00000000,
            ecx: 0x00000000,
            edx: 0x00000000,
        }
    );

    assert_eq!(
        cpuid.guest_result(CpuidFunction(0x0000000c), 0x0, &guest_state),
        CpuidResult {
            eax: 0x00000000,
            ebx: 0x00000000,
            ecx: 0x00000000,
            edx: 0x00000000,
        }
    );

    assert_eq!(
        cpuid.guest_result(CpuidFunction(0x0000000d), 0x0, &guest_state),
        CpuidResult {
            eax: 0x000000e7,
            ebx: 0x00000980,
            ecx: 0x00000980,
            edx: 0x00000000,
        }
    );

    assert_eq!(
        cpuid.guest_result(CpuidFunction(0x0000000d), 0x1, &guest_state),
        CpuidResult {
            eax: 0x0000000b,
            ebx: 0x00000990,
            ecx: 0x00001800,
            edx: 0x00000000,
        }
    );

    assert_eq!(
        cpuid.guest_result(CpuidFunction(0x0000000d), 0x2, &guest_state),
        CpuidResult {
            eax: 0x00000100,
            ebx: 0x00000240,
            ecx: 0x00000000,
            edx: 0x00000000,
        }
    );

    assert_eq!(
        cpuid.guest_result(CpuidFunction(0x0000000d), 0x5, &guest_state),
        CpuidResult {
            eax: 0x00000040,
            ebx: 0x00000340,
            ecx: 0x00000000,
            edx: 0x00000000,
        }
    );

    assert_eq!(
        cpuid.guest_result(CpuidFunction(0x0000000d), 0x6, &guest_state),
        CpuidResult {
            eax: 0x00000200,
            ebx: 0x00000380,
            ecx: 0x00000000,
            edx: 0x00000000,
        }
    );

    assert_eq!(
        cpuid.guest_result(CpuidFunction(0x0000000d), 0x7, &guest_state),
        CpuidResult {
            eax: 0x00000400,
            ebx: 0x00000580,
            ecx: 0x00000000,
            edx: 0x00000000,
        }
    );

    assert_eq!(
        cpuid.guest_result(CpuidFunction(0x80000000), 0x0, &guest_state),
        CpuidResult {
            eax: 0x80000026,
            ebx: 0x68747541,
            ecx: 0x444d4163,
            edx: 0x69746e65,
        }
    );

    assert_eq!(
        cpuid.guest_result(CpuidFunction(0x80000001), 0x0, &guest_state),
        CpuidResult {
            eax: 0x00a10f11,
            ebx: 0x40000000,
            ecx: 0x004001f3,
            edx: 0x27d3fbff,
        }
    );

    assert_eq!(
        cpuid.guest_result(CpuidFunction(0x80000002), 0x0, &guest_state),
        CpuidResult {
            eax: 0x20444d41,
            ebx: 0x43595045,
            ecx: 0x37563920,
            edx: 0x30382034,
        }
    );

    assert_eq!(
        cpuid.guest_result(CpuidFunction(0x80000003), 0x0, &guest_state),
        CpuidResult {
            eax: 0x726f432d,
            ebx: 0x72502065,
            ecx: 0x7365636f,
            edx: 0x20726f73,
        }
    );

    assert_eq!(
        cpuid.guest_result(CpuidFunction(0x80000004), 0x0, &guest_state),
        CpuidResult {
            eax: 0x20202020,
            ebx: 0x20202020,
            ecx: 0x20202020,
            edx: 0x00202020,
        }
    );

    assert_eq!(
        cpuid.guest_result(CpuidFunction(0x80000005), 0x0, &guest_state),
        CpuidResult {
            eax: 0xff48ff40,
            ebx: 0xff48ff40,
            ecx: 0x20080140,
            edx: 0x20080140,
        }
    );

    assert_eq!(
        cpuid.guest_result(CpuidFunction(0x80000006), 0x0, &guest_state),
        CpuidResult {
            eax: 0x5c002200,
            ebx: 0x6c004200,
            ecx: 0x04006140,
            edx: 0x0a009140,
        }
    );

    assert_eq!(
        cpuid.guest_result(CpuidFunction(0x80000007), 0x0, &guest_state),
        CpuidResult {
            eax: 0x00000000,
            ebx: 0x00000000,
            ecx: 0x00000000,
            edx: 0x00000000,
        }
    );

    assert_eq!(
        cpuid.guest_result(CpuidFunction(0x80000008), 0x0, &guest_state),
        CpuidResult {
            eax: 0x0000302f,
            ebx: 0x3112d01d,
            ecx: 0x0000400f,
            edx: 0x00010007,
        }
    );

    assert_eq!(
        cpuid.guest_result(CpuidFunction(0x80000009), 0x0, &guest_state),
        CpuidResult {
            eax: 0x00000000,
            ebx: 0x00000000,
            ecx: 0x00000000,
            edx: 0x00000000,
        }
    );

    assert_eq!(
        cpuid.guest_result(CpuidFunction(0x8000000a), 0x0, &guest_state),
        CpuidResult {
            eax: 0x00000000,
            ebx: 0x00000000,
            ecx: 0x00000000,
            edx: 0x00000000,
        }
    );

    assert_eq!(
        cpuid.guest_result(CpuidFunction(0x8000000b), 0x0, &guest_state),
        CpuidResult {
            eax: 0x00000000,
            ebx: 0x00000000,
            ecx: 0x00000000,
            edx: 0x00000000,
        }
    );

    assert_eq!(
        cpuid.guest_result(CpuidFunction(0x8000000c), 0x0, &guest_state),
        CpuidResult {
            eax: 0x00000000,
            ebx: 0x00000000,
            ecx: 0x00000000,
            edx: 0x00000000,
        }
    );

    assert_eq!(
        cpuid.guest_result(CpuidFunction(0x8000000d), 0x0, &guest_state),
        CpuidResult {
            eax: 0x00000000,
            ebx: 0x00000000,
            ecx: 0x00000000,
            edx: 0x00000000,
        }
    );

    assert_eq!(
        cpuid.guest_result(CpuidFunction(0x8000000e), 0x0, &guest_state),
        CpuidResult {
            eax: 0x00000000,
            ebx: 0x00000000,
            ecx: 0x00000000,
            edx: 0x00000000,
        }
    );

    assert_eq!(
        cpuid.guest_result(CpuidFunction(0x8000000f), 0x0, &guest_state),
        CpuidResult {
            eax: 0x00000000,
            ebx: 0x00000000,
            ecx: 0x00000000,
            edx: 0x00000000,
        }
    );

    assert_eq!(
        cpuid.guest_result(CpuidFunction(0x80000010), 0x0, &guest_state),
        CpuidResult {
            eax: 0x00000000,
            ebx: 0x00000000,
            ecx: 0x00000000,
            edx: 0x00000000,
        }
    );

    assert_eq!(
        cpuid.guest_result(CpuidFunction(0x80000011), 0x0, &guest_state),
        CpuidResult {
            eax: 0x00000000,
            ebx: 0x00000000,
            ecx: 0x00000000,
            edx: 0x00000000,
        }
    );

    assert_eq!(
        cpuid.guest_result(CpuidFunction(0x80000012), 0x0, &guest_state),
        CpuidResult {
            eax: 0x00000000,
            ebx: 0x00000000,
            ecx: 0x00000000,
            edx: 0x00000000,
        }
    );

    assert_eq!(
        cpuid.guest_result(CpuidFunction(0x80000013), 0x0, &guest_state),
        CpuidResult {
            eax: 0x00000000,
            ebx: 0x00000000,
            ecx: 0x00000000,
            edx: 0x00000000,
        }
    );

    assert_eq!(
        cpuid.guest_result(CpuidFunction(0x80000014), 0x0, &guest_state),
        CpuidResult {
            eax: 0x00000000,
            ebx: 0x00000000,
            ecx: 0x00000000,
            edx: 0x00000000,
        }
    );

    assert_eq!(
        cpuid.guest_result(CpuidFunction(0x80000015), 0x0, &guest_state),
        CpuidResult {
            eax: 0x00000000,
            ebx: 0x00000000,
            ecx: 0x00000000,
            edx: 0x00000000,
        }
    );

    assert_eq!(
        cpuid.guest_result(CpuidFunction(0x80000016), 0x0, &guest_state),
        CpuidResult {
            eax: 0x00000000,
            ebx: 0x00000000,
            ecx: 0x00000000,
            edx: 0x00000000,
        }
    );

    assert_eq!(
        cpuid.guest_result(CpuidFunction(0x80000017), 0x0, &guest_state),
        CpuidResult {
            eax: 0x00000000,
            ebx: 0x00000000,
            ecx: 0x00000000,
            edx: 0x00000000,
        }
    );

    assert_eq!(
        cpuid.guest_result(CpuidFunction(0x80000018), 0x0, &guest_state),
        CpuidResult {
            eax: 0x00000000,
            ebx: 0x00000000,
            ecx: 0x00000000,
            edx: 0x00000000,
        }
    );

    assert_eq!(
        cpuid.guest_result(CpuidFunction(0x80000019), 0x0, &guest_state),
        CpuidResult {
            eax: 0x00000000,
            ebx: 0x00000000,
            ecx: 0x00000000,
            edx: 0x00000000,
        }
    );

    assert_eq!(
        cpuid.guest_result(CpuidFunction(0x8000001a), 0x0, &guest_state),
        CpuidResult {
            eax: 0x00000006,
            ebx: 0x00000000,
            ecx: 0x00000000,
            edx: 0x00000000,
        }
    );

    assert_eq!(
        cpuid.guest_result(CpuidFunction(0x8000001b), 0x0, &guest_state),
        CpuidResult {
            eax: 0x00000000,
            ebx: 0x00000000,
            ecx: 0x00000000,
            edx: 0x00000000,
        }
    );

    assert_eq!(
        cpuid.guest_result(CpuidFunction(0x8000001c), 0x0, &guest_state),
        CpuidResult {
            eax: 0x00000000,
            ebx: 0x00000000,
            ecx: 0x00000000,
            edx: 0x00000000,
        }
    );

    assert_eq!(
        cpuid.guest_result(CpuidFunction(0x8000001d), 0x0, &guest_state),
        CpuidResult {
            eax: 0x00004121,
            ebx: 0x01c0003f,
            ecx: 0x0000003f,
            edx: 0x00000000,
        }
    );

    assert_eq!(
        cpuid.guest_result(CpuidFunction(0x8000001e), 0x0, &guest_state),
        CpuidResult {
            eax: 0x00000000,
            ebx: 0x00000100,
            ecx: 0x00000000,
            edx: 0x00000000,
        }
    );

    assert_eq!(
        cpuid.guest_result(CpuidFunction(0x8000001f), 0x0, &guest_state),
        CpuidResult {
            eax: 0x00000000,
            ebx: 0x00000000,
            ecx: 0x00000000,
            edx: 0x00000000,
        }
    );

    assert_eq!(
        cpuid.guest_result(CpuidFunction(0x80000020), 0x0, &guest_state),
        CpuidResult {
            eax: 0x00000000,
            ebx: 0x00000000,
            ecx: 0x00000000,
            edx: 0x00000000,
        }
    );

    assert_eq!(
        cpuid.guest_result(CpuidFunction(0x80000021), 0x0, &guest_state),
        CpuidResult {
            eax: 0x00000000,
            ebx: 0x00000000,
            ecx: 0x00000000,
            edx: 0x00000000,
        }
    );

    assert_eq!(
        cpuid.guest_result(CpuidFunction(0x80000022), 0x0, &guest_state),
        CpuidResult {
            eax: 0x00000000,
            ebx: 0x00000000,
            ecx: 0x00000000,
            edx: 0x00000000,
        }
    );

    assert_eq!(
        cpuid.guest_result(CpuidFunction(0x80000023), 0x0, &guest_state),
        CpuidResult {
            eax: 0x00000000,
            ebx: 0x00000000,
            ecx: 0x00000000,
            edx: 0x00000000,
        }
    );

    assert_eq!(
        cpuid.guest_result(CpuidFunction(0x80000024), 0x0, &guest_state),
        CpuidResult {
            eax: 0x00000000,
            ebx: 0x00000000,
            ecx: 0x00000000,
            edx: 0x00000000,
        }
    );

    assert_eq!(
        cpuid.guest_result(CpuidFunction(0x80000025), 0x0, &guest_state),
        CpuidResult {
            eax: 0x00000000,
            ebx: 0x00000000,
            ecx: 0x00000000,
            edx: 0x00000000,
        }
    );

    assert_eq!(
        cpuid.guest_result(CpuidFunction(0x80000026), 0x0, &guest_state),
        CpuidResult {
            eax: 0x00000000,
            ebx: 0x00000000,
            ecx: 0x00000000,
            edx: 0x00000000,
        }
    );
}
