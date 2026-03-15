// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! PID 1 init mode for pipette.
//!
//! When pipette runs as PID 1 (e.g., via `rdinit=/pipette` in the kernel
//! cmdline), it performs minimal init duties (mount filesystems), then
//! forks. The child runs the normal pipette agent. PID 1 stays in a
//! simple `wait()` loop, reaping all children (including orphans adopted
//! by PID 1). When the pipette child exits, PID 1 calls `reboot(2)`.
//!
//! This fork-based design avoids conflicts between PID 1's reaping
//! duties and the pipette execute handler's `Child::wait()` calls —
//! PID 1 never runs pipette logic, so there's no race between
//! `waitpid(-1)` and `waitpid(specific_pid)`.

// UNSAFETY: Required for libc calls (fork, mount, reboot, waitpid).
#![expect(unsafe_code)]

use std::ffi::CString;
use std::io;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;

static FORKED_FROM_PID1: AtomicBool = AtomicBool::new(false);

/// Returns `true` if this process is running as PID 1.
pub fn is_pid1() -> bool {
    std::process::id() == 1
}

/// Returns `true` if this process was forked from PID 1 and should
/// use `reboot(2)` directly for shutdown.
pub fn was_forked_from_pid1() -> bool {
    FORKED_FROM_PID1.load(Ordering::Relaxed)
}

/// Perform minimal init duties and fork.
///
/// Mounts `/dev`, `/proc`, `/sys`, then forks. Returns normally in the
/// child (which should continue to run the pipette agent). Never returns
/// in the parent (PID 1 stays in a reap loop until the child exits,
/// then calls `reboot(2)` to power off).
pub fn init_as_pid1() {
    eprintln!("Pipette running as PID 1, performing init duties");

    // Mount essential filesystems
    mount_or_warn("devtmpfs", "/dev", "devtmpfs");
    mount_or_warn("proc", "/proc", "proc");
    mount_or_warn("sysfs", "/sys", "sysfs");

    // Fork: child runs pipette, parent stays as PID 1 reaper.
    let pid = unsafe { libc::fork() };
    match pid {
        -1 => {
            eprintln!("fatal: fork() failed: {}", io::Error::last_os_error());
            // Can't fork — just run pipette as PID 1 and hope for the best.
        }
        0 => {
            // Child: set flag so shutdown handler uses reboot(2),
            // then return to caller to run the normal pipette agent.
            FORKED_FROM_PID1.store(true, Ordering::Relaxed);
        }
        child_pid => {
            // Parent (PID 1): reap loop — never returns.
            eprintln!("Pipette PID 1: forked child {child_pid}, entering reap loop");
            pid1_reap_loop(child_pid);
        }
    }
}

/// PID 1 reap loop: wait for children forever, power off when the
/// pipette child exits.
fn pid1_reap_loop(pipette_pid: i32) -> ! {
    loop {
        let mut status: i32 = 0;
        let pid = unsafe { libc::wait(&mut status) };
        if pid == pipette_pid {
            // Pipette child exited — shut down the VM.
            let exit_code = if libc::WIFEXITED(status) {
                libc::WEXITSTATUS(status)
            } else {
                -1
            };
            eprintln!("Pipette child {pipette_pid} exited (status {exit_code}), powering off");
            // reboot(2) doesn't return, but if it somehow does:
            std::process::exit(exit_code);
        }
        // Any other child — just reaped an orphan, continue looping.
    }
}

fn mount_or_warn(source: &str, target: &str, fstype: &str) {
    if let Err(e) = mount(source, target, fstype) {
        eprintln!("warning: failed to mount {fstype} on {target}: {e}");
    }
}

fn mount(source: &str, target: &str, fstype: &str) -> io::Result<()> {
    // Create mount point if it doesn't exist
    let _ = std::fs::create_dir_all(target);

    let source = CString::new(source).unwrap();
    let target = CString::new(target).unwrap();
    let fstype = CString::new(fstype).unwrap();

    let ret = unsafe {
        libc::mount(
            source.as_ptr(),
            target.as_ptr(),
            fstype.as_ptr(),
            0,
            std::ptr::null(),
        )
    };

    if ret == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}
