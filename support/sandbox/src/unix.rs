// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Unix-family sandbox primitives.
//!
//! Currently this module contains only Linux-specific primitives (namespaces,
//! seccomp, hardening `prctl`s, `no_new_privs`), so the whole module is gated
//! on `target_os = "linux"` at the crate root and is not compiled on other
//! Unix targets (macOS, BSDs).
//!
//! These are pure mechanism: they name libc flags and return [`io::Result`],
//! and never appear in the crate's public API (R-S11). Landlock is applied
//! from the Linux backend directly, where it can consult policy types.

// UNSAFETY: Calls to libc syscalls (unshare, mount, umount2, pivot_root,
// prctl) that expose kernel sandboxing primitives.
#![expect(unsafe_code)]
#![warn(missing_docs)]

use std::io;

pub mod hardening;
pub mod mount_namespace;
pub mod network_namespace;
pub mod seccomp;
pub(crate) mod syscall_denylist;

/// Helper trait to convert a libc return value into an [`io::Result`].
///
/// Mirrors the same-named helper in `pal::unix` but kept local to avoid
/// pulling `pal` into this crate.
pub(crate) trait SyscallResult: Sized {
    /// Returns `Ok(self)` when `self >= 0`, otherwise returns the current
    /// value of `errno` as an [`io::Error`].
    fn syscall_result(self) -> io::Result<Self>;
}

impl SyscallResult for i32 {
    fn syscall_result(self) -> io::Result<Self> {
        if self >= 0 {
            Ok(self)
        } else {
            Err(io::Error::last_os_error())
        }
    }
}

impl SyscallResult for isize {
    fn syscall_result(self) -> io::Result<Self> {
        if self >= 0 {
            Ok(self)
        } else {
            Err(io::Error::last_os_error())
        }
    }
}

impl SyscallResult for i64 {
    fn syscall_result(self) -> io::Result<Self> {
        if self >= 0 {
            Ok(self)
        } else {
            Err(io::Error::last_os_error())
        }
    }
}
