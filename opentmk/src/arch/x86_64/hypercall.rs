// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Hypercall interface for x86_64 architecture.

// UNSAFETY: This module contains unsafe code to perform low-level operations such as managing the hypercall page access
#![expect(unsafe_code)]

use core::ptr::addr_of;

use hvdef::HV_PAGE_SIZE;
use minimal_rt::arch::hypercall::HYPERCALL_PAGE;
use minimal_rt::arch::msr::read_msr;
use minimal_rt::arch::msr::write_msr;

/// Writes an MSR to tell the hypervisor the OS ID.
fn report_os_id(guest_os_id: u64) {
    // SAFETY: Using the contract established in the Hyper-V TLFS.
    unsafe {
        write_msr(hvdef::HV_X64_MSR_GUEST_OS_ID, guest_os_id);
    };
}

/// Writes an MSR to tell the hypervisor where the hypercall page is
pub fn write_hypercall_msr(enable: bool) {
    // SAFETY: Using the contract established in the Hyper-V TLFS.
    let hypercall_contents = hvdef::hypercall::MsrHypercallContents::from(unsafe {
        read_msr(hvdef::HV_X64_MSR_HYPERCALL)
    });

    let hypercall_page_num = addr_of!(HYPERCALL_PAGE) as u64 / HV_PAGE_SIZE;

    if !(!enable || !hypercall_contents.enable()) {
        return;
    }
    let new_hv_contents: hvdef::hypercall::MsrHypercallContents = hypercall_contents
        .with_enable(enable)
        .with_gpn(if enable { hypercall_page_num } else { 0 });

    // SAFETY: Using the contract established in the Hyper-V TLFS.
    unsafe { write_msr(hvdef::HV_X64_MSR_HYPERCALL, new_hv_contents.into()) };
}

/// Has to be called before using hypercalls.
pub fn initialize(guest_os_id: u64) {
    // We are assuming we are running under a Microsoft hypervisor, so there is
    // no need to check any cpuid leaves.
    report_os_id(guest_os_id);
    write_hypercall_msr(true);
}

/// Call to uninitialize hypercalL page overlay
pub fn uninitialize() {
    write_hypercall_msr(false);
    report_os_id(0);
}
