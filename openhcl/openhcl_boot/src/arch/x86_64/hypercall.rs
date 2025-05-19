// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use crate::MemoryRange;
use crate::arch::tdx::TdcallInstruction;
use crate::arch::x86_64::address_space::tdx_share_large_page;
use crate::arch::x86_64::address_space::tdx_unshare_large_page;
use crate::host_params::shim_params::IsolationType;
use core::arch::asm;
use core::ptr::addr_of;
use hvdef::HV_PAGE_SIZE;
use minimal_rt::arch::hypercall::HYPERCALL_PAGE;
use minimal_rt::arch::msr::read_msr;
use minimal_rt::arch::msr::write_msr;
use tdcall::tdcall_wrmsr;

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

    assert!(
        !enable || !hypercall_contents.enable(),
        "{:?}",
        hypercall_contents
    );
    let new_hv_contents = hypercall_contents.with_enable(enable).with_gpn(if enable {
        hypercall_page_num
    } else {
        0
    });

    // SAFETY: Using the contract established in the Hyper-V TLFS.
    unsafe { write_msr(hvdef::HV_X64_MSR_HYPERCALL, new_hv_contents.into()) };
}

/// Has to be called before using hypercalls.
pub(crate) fn initialize(
    guest_os_id: u64,
    isolation: IsolationType,
    input_page: Option<u64>,
    output_page: Option<u64>,
) {
    // We are assuming we are running under a Microsoft hypervisor, so there is
    // no need to check any cpuid leaves.
    report_os_id(guest_os_id, isolation);

    match isolation {
        IsolationType::Tdx => {
            // SAFETY: The hypercall i/o pages are valid virtual addresses owned by the caller
            unsafe {
                tdx_share_large_page(input_page.unwrap());
                tdx_share_large_page(output_page.unwrap());
            }

            //// Enable host visibility for hypercall page
            let input_page_range =
                MemoryRange::new(input_page.unwrap()..input_page.unwrap() + 4096);
            let output_page_range =
                MemoryRange::new(output_page.unwrap()..output_page.unwrap() + 4096);
            super::tdx::change_page_visibility(input_page_range, true);
            super::tdx::change_page_visibility(output_page_range, true);
        }

        _ => {
            write_hypercall_msr(true);
        }
    }
}

/// Call before jumping to kernel.
pub(crate) fn uninitialize(
    isolation: IsolationType,
    input_page: Option<u64>,
    output_page: Option<u64>,
) {
    report_os_id(0, isolation);

    match isolation {
        IsolationType::Tdx => {
            // SAFETY: The hypercall i/o pages are valid virtual addresses owned by the caller
            unsafe {
                tdx_unshare_large_page(input_page.unwrap());
                tdx_unshare_large_page(output_page.unwrap());
            }

            // Disable host visibility for hypercall page
            let input_page_range =
                MemoryRange::new(input_page.unwrap()..input_page.unwrap() + 4096);
            super::tdx::change_page_visibility(input_page_range, false);
            let output_page_range =
                MemoryRange::new(output_page.unwrap()..output_page.unwrap() + 4096);
            super::tdx::change_page_visibility(output_page_range, false);
            super::tdx::accept_pages(input_page_range)
                .expect("accepting vtl 2 memory must not fail");
            super::tdx::accept_pages(output_page_range)
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
