// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use hvdef::HV_PAGE_SIZE;

/// Page-aligned, page-sized buffer for use with hypercalls
#[repr(C, align(4096))]
pub struct HvcallPage {
    pub buffer: [u8; HV_PAGE_SIZE as usize],
}

impl HvcallPage {
    pub const fn new() -> Self {
        HvcallPage {
            buffer: [0; HV_PAGE_SIZE as usize],
        }
    }

    /// Address of the hypercall page.
    pub fn address(&self) -> u64 {
        let addr = self.buffer.as_ptr() as u64;

        // These should be page-aligned
        assert!(addr % HV_PAGE_SIZE == 0);

        addr
    }
}

/// Writes a synthehtic register to tell the hypervisor the OS ID for the boot shim.
fn report_os_id(guest_os_id: u64) {
    // On ARM64, to be able to make hypercalls, one needs first to set the Guest OS ID
    // synthetic register using a hypercall. Can't use `Hvcall::set_register` at that will
    // lead to the infinite recursion as that function will first try initializing hypercalls
    // with setting a register.
    //
    // Only one very specific HvSetVpRegisters hypercall is allowed to set the Guest OS ID
    // (this is TLFS section 17.4.4.1.1 and 5.3), and that must be the fast hypercall.
    let _ = minimal_rt::arch::hypercall::set_register_fast(
        hvdef::HvArm64RegisterName::GuestOsId.into(),
        guest_os_id.into(),
    );
}

pub(crate) fn initialize(guest_os_id: u64) {
    // We are assuming we are running under a Microsoft hypervisor.
    report_os_id(guest_os_id);
}

/// Call before jumping to kernel.
pub(crate) fn uninitialize() {
    report_os_id(0);
}
