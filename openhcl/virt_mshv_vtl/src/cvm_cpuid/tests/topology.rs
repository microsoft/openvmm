// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Tests for the cpu topology-related subleaves.
use super::super::*;
use super::*;
use zerocopy::FromZeros;

#[test]
fn real_topology() {
    let mut pages = vec![HvPspCpuidPage::new_zeroed(), HvPspCpuidPage::new_zeroed()];

    pages[0].count += 1;
    pages[0].cpuid_leaf_info[0] = HvPspCpuidLeaf {
        eax_in: CpuidFunction::VersionAndFeatures.0,
        ecx_in: 0,
        xfem_in: 0,
        xss_in: 0,
        eax_out: 0xa00f11,
        ebx_out: 0x100800,
        ecx_out: 0x76fa3203,
        edx_out: 0x178bfbff,
        reserved_z: 0,
    };

    pages[0].cpuid_leaf_info[1] = HvPspCpuidLeaf {
        eax_in: CpuidFunction::ExtendedAddressSpaceSizes.0,
        ecx_in: 0,
        xfem_in: 0,
        xss_in: 0,
        eax_out: 0x302f,
        ebx_out: 0x1112d01d,
        ecx_out: 0x400f,
        edx_out: 0x10007,
        reserved_z: 0,
    };

    pages[0].cpuid_leaf_info[2] = HvPspCpuidLeaf {
        eax_in: CpuidFunction::ProcessorTopologyDefinition.0,
        ecx_in: 0,
        xfem_in: 0,
        xss_in: 0,
        eax_out: 0,
        ebx_out: 0x100,
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
        cpuid.registered_result(CpuidFunction::ExtendedTopologyEnumeration, 0),
        CpuidResult {
            eax: 0x1,
            ebx: 0x2,
            ecx: 0x100,
            edx: 0
        }
    );

    assert_eq!(
        cpuid.registered_result(CpuidFunction::ExtendedTopologyEnumeration, 1),
        CpuidResult {
            eax: 0x4,
            ebx: 0x10,
            ecx: 0x201,
            edx: 0
        }
    );

    assert_eq!(
        cpuid.registered_result(CpuidFunction::ExtendedTopologyEnumeration, 2),
        ZERO_CPUID_RESULT
    );
}

fn initialize_topology(
    apic_core_id_size: u8,
    mt_per_socket: bool,
    nc: u8,
    lps_per_package: u8,
    threads_per_compute_unit: u8,
) -> Result<CpuidResults, CpuidResultsError> {
    let mut pages = vec![HvPspCpuidPage::new_zeroed(), HvPspCpuidPage::new_zeroed()];

    pages[0].count += 1;
    pages[0].cpuid_leaf_info[0] = HvPspCpuidLeaf {
        eax_in: CpuidFunction::VersionAndFeatures.0,
        ecx_in: 0,
        xfem_in: 0,
        xss_in: 0,
        eax_out: 0xffffffff,
        ebx_out: cpuid::VersionAndFeaturesEbx::from(0xffffffff)
            .with_lps_per_package(lps_per_package)
            .into(),
        ecx_out: 0xffffffff,
        edx_out: cpuid::VersionAndFeaturesEdx::from(0xffffffff)
            .with_mt_per_socket(mt_per_socket)
            .into(),
        reserved_z: 0,
    };

    pages[0].count += 1;
    pages[0].cpuid_leaf_info[1] = HvPspCpuidLeaf {
        eax_in: CpuidFunction::ExtendedAddressSpaceSizes.0,
        ecx_in: 0,
        xfem_in: 0,
        xss_in: 0,
        eax_out: 0xffffffff,
        ebx_out: 0xffffffff,
        ecx_out: cpuid::ExtendedAddressSpaceSizesEcx::from(0xffffffff)
            .with_apic_core_id_size(apic_core_id_size)
            .with_nc(nc)
            .into(),
        edx_out: 0xffffffff,
        reserved_z: 0,
    };

    pages[0].count += 1;
    pages[0].cpuid_leaf_info[2] = HvPspCpuidLeaf {
        eax_in: CpuidFunction::ProcessorTopologyDefinition.0,
        ecx_in: 0,
        xfem_in: 0,
        xss_in: 0,
        eax_out: 0xffffffff,
        ebx_out: cpuid::ProcessorTopologyDefinitionEbx::from(0xffffffff)
            .with_threads_per_compute_unit(threads_per_compute_unit)
            .into(),
        ecx_out: 0xffffffff,
        edx_out: 0xffffffff,
        reserved_z: 0,
    };

    pages[0].count += 1;
    pages[0].cpuid_leaf_info[3] = HvPspCpuidLeaf {
        eax_in: CpuidFunction::ExtendedTopologyEnumeration.0,
        ecx_in: 0,
        xfem_in: 0,
        xss_in: 0,
        eax_out: 0xffffffff,
        ebx_out: 0xffffffff,
        ecx_out: 0xffffffff,
        edx_out: 0xffffffff,
        reserved_z: 0,
    };

    fill_required_leaves(&mut pages, None);

    CpuidResults::new(CpuidResultsIsolationType::Snp {
        cpuid_pages: pages.as_slice().as_bytes(),
    })
}

#[test]
fn legacy_topology() {
    let apic_core_id_size = 0;

    let mt_per_socket = false;
    let nc = 1;
    let max_lps = 0;
    let threads_per_compute_unit = 0;
    assert!(matches!(
        initialize_topology(
            apic_core_id_size,
            mt_per_socket,
            nc,
            max_lps,
            threads_per_compute_unit
        ),
        Err(CpuidResultsError::TopologyInconsistent(
            TopologyError::Hyperthreading(_)
        ))
    ));

    let mt_per_socket = false;
    let nc = 0;
    let max_lps = 2;
    let threads_per_compute_unit = 0;
    assert!(matches!(
        initialize_topology(
            apic_core_id_size,
            mt_per_socket,
            nc,
            max_lps,
            threads_per_compute_unit
        ),
        Err(CpuidResultsError::TopologyInconsistent(
            TopologyError::Hyperthreading(_)
        ))
    ));

    let mt_per_socket = false;
    let nc = 0;
    let max_lps = 1;
    let threads_per_compute_unit = 2;
    assert!(matches!(
        initialize_topology(
            apic_core_id_size,
            mt_per_socket,
            nc,
            max_lps,
            threads_per_compute_unit
        ),
        Err(CpuidResultsError::TopologyInconsistent(
            TopologyError::ThreadsPerUnit
        ))
    ));

    let mt_per_socket = false;
    let nc = 0;
    let max_lps = 1;
    let threads_per_compute_unit = 1;
    assert!(matches!(
        initialize_topology(
            apic_core_id_size,
            mt_per_socket,
            nc,
            max_lps,
            threads_per_compute_unit
        ),
        Err(CpuidResultsError::TopologyInconsistent(
            TopologyError::ThreadsPerUnit
        ))
    ));

    let mt_per_socket = false;
    let nc = 0;
    let max_lps = 1;
    let threads_per_compute_unit = 0;
    let cpuid = initialize_topology(
        apic_core_id_size,
        mt_per_socket,
        nc,
        max_lps,
        threads_per_compute_unit,
    )
    .unwrap();

    assert_eq!(
        cpuid.registered_result(CpuidFunction::ExtendedTopologyEnumeration, 0),
        CpuidResult {
            eax: 0x0,
            ebx: 0x1,
            ecx: 0x100,
            edx: 0
        }
    );

    assert_eq!(
        cpuid.registered_result(CpuidFunction::ExtendedTopologyEnumeration, 1),
        CpuidResult {
            eax: 0x0,
            ebx: 0x1,
            ecx: 0x201,
            edx: 0
        }
    );

    let mt_per_socket = true;
    let nc = 6;
    let max_lps = 8;
    let threads_per_compute_unit = 1;
    assert!(matches!(
        initialize_topology(
            apic_core_id_size,
            mt_per_socket,
            nc,
            max_lps,
            threads_per_compute_unit
        ),
        Err(CpuidResultsError::TopologyInconsistent(
            TopologyError::ProcessorCount(_)
        ))
    ));

    let mt_per_socket = true;
    let nc = 7;
    let max_lps = 8;
    let threads_per_compute_unit = 0;
    let cpuid = initialize_topology(
        apic_core_id_size,
        mt_per_socket,
        nc,
        max_lps,
        threads_per_compute_unit,
    )
    .unwrap();

    assert_eq!(
        cpuid.registered_result(CpuidFunction::ExtendedTopologyEnumeration, 0),
        CpuidResult {
            eax: 0x0,
            ebx: 0x1,
            ecx: 0x100,
            edx: 0
        }
    );

    assert_eq!(
        cpuid.registered_result(CpuidFunction::ExtendedTopologyEnumeration, 1),
        CpuidResult {
            eax: 0x3,
            ebx: 0x8,
            ecx: 0x201,
            edx: 0
        }
    );
}

#[test]
fn topology() {
    let apic_core_id_size = 6;
    let mt_per_socket = true;
    let nc = 64;
    let max_lps = 64;
    let threads_per_compute_unit = 0;
    assert!(matches!(
        initialize_topology(
            apic_core_id_size,
            mt_per_socket,
            nc,
            max_lps,
            threads_per_compute_unit
        ),
        Err(CpuidResultsError::TopologyInconsistent(
            TopologyError::ProcessorCount(_)
        ))
    ));

    let apic_core_id_size = 6;
    let mt_per_socket = true;
    let nc = 63;
    let max_lps = 128;
    let threads_per_compute_unit = 0;
    assert!(matches!(
        initialize_topology(
            apic_core_id_size,
            mt_per_socket,
            nc,
            max_lps,
            threads_per_compute_unit
        ),
        Err(CpuidResultsError::TopologyInconsistent(
            TopologyError::ProcessorCount(_)
        ))
    ));

    let apic_core_id_size = 6;
    let mt_per_socket = false;
    let nc = 63;
    let max_lps = 64;
    let threads_per_compute_unit = 0;
    assert!(matches!(
        initialize_topology(
            apic_core_id_size,
            mt_per_socket,
            nc,
            max_lps,
            threads_per_compute_unit
        ),
        Err(CpuidResultsError::TopologyInconsistent(
            TopologyError::Hyperthreading(_)
        ))
    ));

    let apic_core_id_size = 6;
    let mt_per_socket = true;
    let nc = 63;
    let max_lps = 64;
    let threads_per_compute_unit = 0;
    let cpuid = initialize_topology(
        apic_core_id_size,
        mt_per_socket,
        nc,
        max_lps,
        threads_per_compute_unit,
    )
    .unwrap();

    assert_eq!(
        cpuid.registered_result(CpuidFunction::ExtendedTopologyEnumeration, 0),
        CpuidResult {
            eax: 0x0,
            ebx: 0x1,
            ecx: 0x100,
            edx: 0
        }
    );

    assert_eq!(
        cpuid.registered_result(CpuidFunction::ExtendedTopologyEnumeration, 1),
        CpuidResult {
            eax: 0x6,
            ebx: 0x40,
            ecx: 0x201,
            edx: 0
        }
    );

    // legacy fields shouldn't matter
    let apic_core_id_size = 8;
    let mt_per_socket = true;
    let nc = 255;
    let max_lps = 255;
    let threads_per_compute_unit = 0;
    let cpuid = initialize_topology(
        apic_core_id_size,
        mt_per_socket,
        nc,
        max_lps,
        threads_per_compute_unit,
    )
    .unwrap();

    assert_eq!(
        cpuid.registered_result(CpuidFunction::ExtendedTopologyEnumeration, 0),
        CpuidResult {
            eax: 0x0,
            ebx: 0x1,
            ecx: 0x100,
            edx: 0
        }
    );

    assert_eq!(
        cpuid.registered_result(CpuidFunction::ExtendedTopologyEnumeration, 1),
        CpuidResult {
            eax: 0x8,
            ebx: 0x100,
            ecx: 0x201,
            edx: 0
        }
    );

    let apic_core_id_size = 8;
    let mt_per_socket = true;
    let nc = 255;
    let max_lps = 255;
    let threads_per_compute_unit = 1;
    let cpuid = initialize_topology(
        apic_core_id_size,
        mt_per_socket,
        nc,
        max_lps,
        threads_per_compute_unit,
    )
    .unwrap();

    assert_eq!(
        cpuid.registered_result(CpuidFunction::ExtendedTopologyEnumeration, 0),
        CpuidResult {
            eax: 0x1,
            ebx: 0x2,
            ecx: 0x100,
            edx: 0
        }
    );

    assert_eq!(
        cpuid.registered_result(CpuidFunction::ExtendedTopologyEnumeration, 1),
        CpuidResult {
            eax: 0x8,
            ebx: 0x100,
            ecx: 0x201,
            edx: 0
        }
    );
}
