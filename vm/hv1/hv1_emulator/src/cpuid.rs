// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Provides the synthetic hypervisor cpuid leaves matching this hv1 emulator's
//! capabilities.

use virt::CpuidLeaf;
use virt::IsolationType;
use vm_topology::processor::x86::X86Topology;
use vm_topology::processor::ProcessorTopology;

const MAX_CPUS: usize = 2048;

/// Provides the values for the synthetic hypervisor cpuid leaves.
pub fn hv_cpuid_leaves(
    topology: &ProcessorTopology<X86Topology>,
    emulate_apic: bool,
    isolation: IsolationType,
    access_vsm: bool,
    hv_version: [u32; 4],
    vtom: Option<u64>,
) -> Vec<CpuidLeaf> {
    let hardware_isolated = isolation.is_hardware_isolated();
    let split_u128 = |x: u128| -> [u32; 4] {
        let bytes = x.to_le_bytes();
        [
            u32::from_le_bytes(bytes[0..4].try_into().unwrap()),
            u32::from_le_bytes(bytes[4..8].try_into().unwrap()),
            u32::from_le_bytes(bytes[8..12].try_into().unwrap()),
            u32::from_le_bytes(bytes[12..16].try_into().unwrap()),
        ]
    };

    let privileges = {
        let mut privileges = hvdef::HvPartitionPrivilege::new()
            .with_access_partition_reference_counter(true)
            .with_access_hypercall_msrs(true)
            .with_access_vp_index(true)
            .with_access_frequency_msrs(true)
            .with_access_synic_msrs(true)
            .with_access_synthetic_timer_msrs(true)
            .with_access_apic_msrs(true)
            .with_access_vp_runtime_msr(true)
            .with_access_partition_reference_tsc(true)
            .with_start_virtual_processor(true)
            .with_access_vsm(access_vsm)
            // TODO GUEST_VSM: Not actually implemented yet, but this is
            // needed for guest vsm bringup
            .with_enable_extended_gva_ranges_flush_va_list(access_vsm);

        if hardware_isolated {
            privileges = privileges.with_isolation(true)

            // TODO SNP:
            //     .with_fast_hypercall_output(true);
        }

        u64::from(privileges)
    };

    let mut hv_cpuid = vec![
        CpuidLeaf::new(
            hvdef::HV_CPUID_FUNCTION_HV_VENDOR_AND_MAX_FUNCTION,
            [
                if hardware_isolated {
                    hvdef::HV_CPUID_FUNCTION_MS_HV_ISOLATION_CONFIGURATION
                } else {
                    hvdef::HV_CPUID_FUNCTION_MS_HV_IMPLEMENTATION_LIMITS
                },
                u32::from_le_bytes(*b"Micr"),
                u32::from_le_bytes(*b"osof"),
                u32::from_le_bytes(*b"t Hv"),
            ],
        ),
        CpuidLeaf::new(
            hvdef::HV_CPUID_FUNCTION_HV_INTERFACE,
            [u32::from_le_bytes(*b"Hv#1"), 0, 0, 0],
        ),
        CpuidLeaf::new(hvdef::HV_CPUID_FUNCTION_MS_HV_VERSION, hv_version),
        CpuidLeaf::new(hvdef::HV_CPUID_FUNCTION_MS_HV_FEATURES, {
            let mut features = hvdef::HvFeatures::new()
                .with_privileges(privileges)
                .with_frequency_regs_available(true)
                .with_direct_synthetic_timers(true)
                // TODO GUEST_VSM: flush virtual address list is not
                // actually implemented yet, but this is needed for guest
                // vsm bringup
                .with_extended_gva_ranges_for_flush_virtual_address_list_available(access_vsm);

            // TODO SNP
            //    .with_fast_hypercall_output_available(true);

            if cfg!(guest_arch = "x86_64") {
                features = features.with_xmm_registers_for_fast_hypercall_available(true);
            }

            split_u128(features.into())
        }),
        CpuidLeaf::new(hvdef::HV_CPUID_FUNCTION_MS_HV_ENLIGHTENMENT_INFORMATION, {
            let use_apic_msrs = match topology.apic_mode() {
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
                    // However, Lazy EOI on SNP is beneficial and requires the
                    // Hyper-V MSRs to function. Enable it there regardless.
                    isolation == IsolationType::Snp
                }
            };

            let mut enlightenments = hvdef::HvEnlightenmentInformation::new()
                .with_deprecate_auto_eoi(true)
                .with_use_relaxed_timing(true)
                .with_use_ex_processor_masks(true)
                .with_use_apic_msrs(use_apic_msrs);

            if hardware_isolated {
                // TODO TDX too when it's ready
                if isolation == IsolationType::Snp {
                    enlightenments = enlightenments
                        .with_use_hypercall_for_remote_flush_and_local_flush_entire(true);
                }
                // TODO HCVM:
                //    .with_use_synthetic_cluster_ipi(true);

                if emulate_apic {
                    enlightenments.set_long_spin_wait_count(0xffffffff); // no spin wait notifications
                }
            };
            split_u128(enlightenments.into())
        }),
        CpuidLeaf::new(
            hvdef::HV_CPUID_FUNCTION_MS_HV_IMPLEMENTATION_LIMITS,
            [MAX_CPUS as u32, MAX_CPUS as u32, 0, 0],
        ),
    ];

    if hardware_isolated {
        hv_cpuid.append(&mut vec![
            CpuidLeaf::new(
                hvdef::HV_CPUID_FUNCTION_MS_HV_HARDWARE_FEATURES,
                split_u128(
                    hvdef::HvHardwareFeatures::new()
                        .with_apic_overlay_assist_in_use(true)
                        .with_msr_bitmaps_in_use(true)
                        .with_second_level_address_translation_in_use(true)
                        .with_dma_remapping_in_use(false)
                        .with_interrupt_remapping_in_use(false)
                        .into(),
                ),
            ),
            CpuidLeaf::new(
                hvdef::HV_CPUID_FUNCTION_MS_HV_ISOLATION_CONFIGURATION,
                split_u128(
                    hvdef::HvIsolationConfiguration::new()
                        .with_paravisor_present(true)
                        .with_isolation_type(isolation.to_hv().0)
                        .with_shared_gpa_boundary_active(true)
                        .with_shared_gpa_boundary_bits(
                            vtom.expect("cvm requires vtom").trailing_zeros() as u8,
                        )
                        .into(),
                ),
            ),
        ]);
    }

    hv_cpuid
}
