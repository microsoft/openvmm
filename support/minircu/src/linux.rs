// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use libc::SYS_membarrier;
use libc::syscall;

// Use a compiler fence on the read side since we have a working membarrier
// implementation.
pub use std::sync::atomic::compiler_fence as access_fence;

pub fn membarrier() {
    // Use the membarrier syscall to ensure that all other threads in the
    // process have observed the writes made by this thread.
    //
    // This could be quite expensive with lots of threads, but most of the
    // threads in a VMM should be idle most of the time. However, In OpenVMM on
    // a host, this could be problematic--KVM and MSHV VP threads will probably
    // not be considered idle by the membarrier implementation.
    //
    // Luckily, in the OpenHCL environment VP threads are usually idle (to
    // prevent unnecessary scheduler ticks), so this should be a non-issue.

    // SAFETY: no special requirements for the syscall.
    let r = unsafe { syscall(SYS_membarrier, libc::MEMBARRIER_CMD_PRIVATE_EXPEDITED, 0, 0) };
    if r < 0 {
        panic!(
            "membarrier syscall failed: {}",
            std::io::Error::last_os_error()
        );
    }
}
