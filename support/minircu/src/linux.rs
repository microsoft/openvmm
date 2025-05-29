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
    let r = match membarrier_syscall(libc::MEMBARRIER_CMD_PRIVATE_EXPEDITED) {
        Err(err) if err.raw_os_error() == Some(libc::EPERM) => {
            membarrier_syscall(libc::MEMBARRIER_CMD_REGISTER_PRIVATE_EXPEDITED)
                .expect("failed to register for membarrier use");
            membarrier_syscall(libc::MEMBARRIER_CMD_PRIVATE_EXPEDITED)
        }
        r => r,
    };
    r.expect("failed to issue membarrier syscall");
}

fn membarrier_syscall(cmd: libc::c_int) -> std::io::Result<()> {
    // SAFETY: no special requirements for the syscall.
    let r = unsafe { syscall(SYS_membarrier, cmd, 0, 0) };
    if r < 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(())
}
