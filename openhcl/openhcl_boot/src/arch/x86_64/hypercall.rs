// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use core::ptr::addr_of;
use hvdef::HV_PAGE_SIZE;
use minimal_rt::arch::hypercall::HYPERCALL_PAGE;
use minimal_rt::arch::msr::read_msr;
use minimal_rt::arch::msr::write_msr;

/// 2MB-aligned, large page sized buffer for use with hypercalls
///
/// The hypercall page is 4KB in the standard setting, but we allocate a large page for
/// TDX compatibility. This is because the underlying static page is mapped in the
/// shim's virtual memory hieararchy as a large page, making 2-MB the minimum shareable
/// memory size between the TDX-enabled shim and hypervisor
#[repr(C, align(0x200000))]
pub struct HvcallPage {
    pub buffer: [u8; x86defs::X64_LARGE_PAGE_SIZE as usize],
}

impl HvcallPage {
    pub const fn new() -> Self {
        HvcallPage {
            buffer: [0; x86defs::X64_LARGE_PAGE_SIZE as usize],
        }
    }

    /// Address of the hypercall page.
    pub fn address(&self) -> u64 {
        let addr = self.buffer.as_ptr() as u64;

        // These should be page-aligned
        assert!(addr % x86defs::X64_LARGE_PAGE_SIZE == 0);

        addr
    }
}

fn report_os_id(guest_os_id: u64) {
    // SAFETY: Using the contract established in the Hyper-V TLFS.
    unsafe {
        write_msr(hvdef::HV_X64_MSR_GUEST_OS_ID, guest_os_id);
    };
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
pub(crate) fn initialize(guest_os_id: u64) {
    // We are assuming we are running under a Microsoft hypervisor, so there is
    // no need to check any cpuid leaves.
    report_os_id(guest_os_id);

    write_hypercall_msr(true);
}

/// Call before jumping to kernel.
pub(crate) fn uninitialize() {
    write_hypercall_msr(false);

    report_os_id(0);
}
