// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use crate::arch::tdx::TdcallInstruction;
use crate::arch::x86_64::address_space::map_with_private_bit;
use crate::arch::x86_64::address_space::map_with_shared_bit;
use crate::host_params::shim_params::IsolationType;
use crate::MemoryRange;
use core::arch::asm;
use core::ptr::addr_of;
use hvdef::HV_PAGE_SIZE;
use minimal_rt::arch::hypercall::HYPERCALL_PAGE;
use minimal_rt::arch::msr::read_msr;
use minimal_rt::arch::msr::write_msr;
use tdcall::tdcall_wrmsr;
use x86defs::tdx::TDX_SHARED_GPA_BOUNDARY_ADDRESS_BIT;

const TWO_MB: u64 = 2 * 1024 * 1024;

/// Writes an MSR to tell the hypervisor the OS ID for the boot shim.
fn report_os_id(guest_os_id: u64, isolation: IsolationType) {
    match isolation {
        IsolationType::Tdx => {
            tdcall_wrmsr(
                &mut TdcallInstruction,
                hvdef::HV_X64_MSR_GUEST_OS_ID,
                guest_os_id,
            )
            .unwrap();
        }
        _ => {
            // SAFETY: Using the contract established in the Hyper-V TLFS.
            unsafe {
                write_msr(hvdef::HV_X64_MSR_GUEST_OS_ID, guest_os_id);
            };
        }
    }
}

/// Writes an MSR to tell the hypervisor where the hypercall page is
fn write_hypercall_msr(enable: bool) {
    // SAFETY: Using the contract established in the Hyper-V TLFS.
    let hypercall_contents = hvdef::hypercall::MsrHypercallContents::from(unsafe {
        read_msr(hvdef::HV_X64_MSR_HYPERCALL)
    });

    let hypercall_page_num = addr_of!(HYPERCALL_PAGE) as u64 / HV_PAGE_SIZE;

    assert!(!enable || !hypercall_contents.enable());
    let new_hv_contents = hypercall_contents.with_enable(enable).with_gpn(if enable {
        hypercall_page_num
    } else {
        0
    });

    // SAFETY: Using the contract established in the Hyper-V TLFS.
    unsafe { write_msr(hvdef::HV_X64_MSR_HYPERCALL, new_hv_contents.into()) };
}

/// Has to be called before using hypercalls.
pub(crate) fn initialize(guest_os_id: u64, input_page: Option<u64>, isolation: IsolationType) {
    // We are assuming we are running under a Microsoft hypervisor, so there is
    // no need to check any cpuid leaves.
    report_os_id(guest_os_id, isolation);

    match isolation {
        IsolationType::Tdx => {
            assert_eq!(input_page.unwrap() % TWO_MB, 0);

            // Set shared bit for the hypercall page entry in local mapping
            // SAFETY: input_page passed is taken from ram_buffer and ensured
            // that it is a valid large page
            unsafe {
                map_with_shared_bit(input_page.unwrap(), TDX_SHARED_GPA_BOUNDARY_ADDRESS_BIT);
            }

            // Enable host visibility for hypercall page
            let input_page_range =
                MemoryRange::new(input_page.unwrap()..input_page.unwrap() + TWO_MB);
            super::tdx::change_page_visibility(input_page_range, true);
        }

        _ => {
            write_hypercall_msr(true);
        }
    }
}

/// Call before jumping to kernel.
pub(crate) fn uninitialize(input_page: Option<u64>, isolation: IsolationType) {
    report_os_id(0, isolation);

    match isolation {
        IsolationType::Tdx => {
            assert_eq!(input_page.unwrap() % TWO_MB, 0);

            // Set private bit for the hypercall page entry in local mapping
            // SAFETY: input_page passed is taken from ram_buffer and ensured
            // that it is a valid large page
            unsafe {
                map_with_private_bit(input_page.unwrap(), TDX_SHARED_GPA_BOUNDARY_ADDRESS_BIT);
            }

            // Disable host visibility for hypercall page
            let input_page_range =
                MemoryRange::new(input_page.unwrap()..input_page.unwrap() + TWO_MB);
            super::tdx::change_page_visibility(input_page_range, false);
            super::tdx::accept_pages(input_page_range)
                .expect("accepting vtl 2 memory must not fail");

            // SAFETY: Flush TLB
            unsafe {
                asm! {
                    "mov rax, cr3",
                    "mov cr3, rax"
                }
            }
        }
        _ => {
            write_hypercall_msr(false);
        }
    }
}
