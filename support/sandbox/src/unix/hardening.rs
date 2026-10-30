// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Small `prctl` knobs that shrink a sandbox's blast radius.

use crate::unix::SyscallResult;
use std::io;

/// `PR_SET_SECUREBITS` — not always exported by `libc`, so pinned here. See
/// `include/uapi/linux/prctl.h`.
const PR_SET_SECUREBITS: libc::c_int = 28;

// Securebit flags
//
// For every dimension we set both the behavior bit and its `_LOCKED` partner:
// the behavior bit changes policy now, the locked bit prevents any later code
// (even a UID-0 worker) from clearing it.
const SECBIT_NOROOT: libc::c_ulong = 1 << 0;
const SECBIT_NOROOT_LOCKED: libc::c_ulong = 1 << 1;
const SECBIT_NO_SETUID_FIXUP: libc::c_ulong = 1 << 2;
const SECBIT_NO_SETUID_FIXUP_LOCKED: libc::c_ulong = 1 << 3;
const SECBIT_NO_CAP_AMBIENT_RAISE: libc::c_ulong = 1 << 6;
const SECBIT_NO_CAP_AMBIENT_RAISE_LOCKED: libc::c_ulong = 1 << 7;

/// Clear the "dumpable" flag (`PR_SET_DUMPABLE = 0`).
///
/// A non-dumpable process produces no core dump (so guest memory cannot spill
/// to disk on a crash) and cannot be `ptrace`-attached or have its
/// `/proc/<pid>/mem` opened by another process of the same UID — the R-S6
/// lateral-movement defense.
pub fn set_dumpable_off() -> io::Result<()> {
    // SAFETY: prctl(PR_SET_DUMPABLE, 0, 0, 0, 0) affects only the calling
    // process and takes no pointer arguments.
    unsafe { libc::prctl(libc::PR_SET_DUMPABLE, 0, 0, 0, 0) }.syscall_result()?;
    Ok(())
}

/// Lock securebits so sandbox cannot re-acquire root-like authority across an
/// `execve`.
///
/// Sets `SECBIT_NOROOT`, `SECBIT_NO_SETUID_FIXUP`, and
/// `SECBIT_NO_CAP_AMBIENT_RAISE`, each with its `_LOCKED` partner. Without the
/// locked bits, a UID-0 worker regains capabilities across `execve`.
///
/// This requires `CAP_SETPCAP`, so it must run *before* the capability drop.
/// Callers treat it as opportunistic hardening — a failure degrades rather
/// than aborts.
pub fn lock_securebits() -> io::Result<()> {
    let bits = SECBIT_NOROOT
        | SECBIT_NOROOT_LOCKED
        | SECBIT_NO_SETUID_FIXUP
        | SECBIT_NO_SETUID_FIXUP_LOCKED
        | SECBIT_NO_CAP_AMBIENT_RAISE
        | SECBIT_NO_CAP_AMBIENT_RAISE_LOCKED;
    // SAFETY: prctl(PR_SET_SECUREBITS, bits, 0, 0, 0) affects only the calling
    // process and takes no pointer arguments.
    unsafe { libc::prctl(PR_SET_SECUREBITS, bits, 0, 0, 0) }.syscall_result()?;
    Ok(())
}

/// Apply the `PR_SET_NO_NEW_PRIVS` flag to the calling process.
///
/// Once set, this is irreversible for the process and all its descendants.
/// It prevents gaining new privileges through execve (e.g., setuid binaries
/// are executed but without elevated privileges).
pub fn set_no_new_privs() -> io::Result<()> {
    // SAFETY: prctl(PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0) affects only the
    // calling thread/process and requires no pointer arguments.
    unsafe { libc::prctl(libc::PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0) }.syscall_result()?;
    Ok(())
}

/// Check whether `PR_SET_NO_NEW_PRIVS` is currently set on the calling process.
#[cfg(test)]
pub fn get_no_new_privs() -> io::Result<bool> {
    // SAFETY: prctl(PR_GET_NO_NEW_PRIVS, 0, 0, 0, 0) is a read-only query.
    let ret = unsafe { libc::prctl(libc::PR_GET_NO_NEW_PRIVS, 0, 0, 0, 0) }.syscall_result()?;
    Ok(ret != 0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command;
    use test_with_tracing::test;

    // no_new_privs is irreversible, so we test it in a subprocess to avoid
    // contaminating the test runner process.

    #[test]
    fn not_set_by_default() {
        let is_set = get_no_new_privs().expect("prctl GET_NO_NEW_PRIVS failed");
        // The test runner itself shouldn't have no_new_privs set (unless
        // something external set it). This is a sanity check.
        assert!(!is_set, "no_new_privs unexpectedly set on test runner");
    }

    #[test]
    fn set_in_subprocess() {
        let exe = std::env::current_exe().expect("failed to get test executable path");

        // Run a helper test in a subprocess so the irreversible flag
        // doesn't affect other tests.
        let output = Command::new(&exe)
            .arg("--exact")
            .arg("unix::hardening::tests::helper_set_and_verify")
            .arg("--ignored")
            .arg("--nocapture")
            .output()
            .expect("failed to spawn subprocess");

        assert!(
            output.status.success(),
            "subprocess failed:\nstdout: {}\nstderr: {}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr),
        );
    }

    /// Helper test that actually sets no_new_privs. Runs only when invoked
    /// explicitly by `set_in_subprocess` (marked `#[ignore]`).
    #[test]
    #[ignore]
    fn helper_set_and_verify() {
        assert!(!get_no_new_privs().unwrap(), "should not be set initially");
        set_no_new_privs().expect("failed to set no_new_privs");
        assert!(get_no_new_privs().unwrap(), "should be set after prctl");
    }
}
