// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

#![cfg(unix)]
// UNSAFETY: Calls to libc functions to interact with low level primitives.
#![allow(unsafe_code)]

pub mod affinity;
pub mod pipe;
pub mod process;
pub mod pthread;

use std::fs::File;
use std::io;
use std::io::Error;
use std::os::unix::prelude::*;

/// A Linux error value.
#[derive(Copy, Clone, PartialEq, Eq)]
pub struct Errno(pub i32);

impl std::fmt::Debug for Errno {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        std::fmt::Debug::fmt(&Error::from(*self), f)
    }
}

impl From<Errno> for Error {
    fn from(code: Errno) -> Self {
        Self::from_raw_os_error(code.0)
    }
}

/// Trait for extracting a Unix error value from an error type.
pub trait TryAsErrno {
    /// Gets the Unix error value if there is one.
    fn try_as_errno(&self) -> Option<Errno>;
}

impl TryAsErrno for Errno {
    fn try_as_errno(&self) -> Option<Errno> {
        Some(*self)
    }
}

impl TryAsErrno for Error {
    fn try_as_errno(&self) -> Option<Errno> {
        self.raw_os_error().map(Errno)
    }
}

/// Returns the value of errno.
pub(crate) fn errno() -> Errno {
    Errno(Error::last_os_error().raw_os_error().unwrap())
}

/// A helper trait to convert from a libc return value to a `Result<_, Errno>`.
pub trait SyscallResult: Sized {
    /// Returns `Ok(self)` if `self >= 0`, otherwise `Err(errno())`.
    fn syscall_result(self) -> Result<Self, Errno>;
}

impl SyscallResult for i32 {
    fn syscall_result(self) -> Result<Self, Errno> {
        if self >= 0 {
            Ok(self)
        } else {
            Err(errno())
        }
    }
}

impl SyscallResult for isize {
    fn syscall_result(self) -> Result<Self, Errno> {
        if self >= 0 {
            Ok(self)
        } else {
            Err(errno())
        }
    }
}

/// Runs f() until it stop failing with EINTR (as indicated by errno).
pub fn while_eintr<F, R, E>(mut f: F) -> Result<R, E>
where
    F: FnMut() -> Result<R, E>,
    E: TryAsErrno,
{
    loop {
        match f() {
            Err(err) if err.try_as_errno() == Some(Errno(libc::EINTR)) => {}
            r => break r,
        }
    }
}

/// Closes stdout, replacing it the null device.
pub fn close_stdout() -> io::Result<()> {
    let new_stdout = File::open("/dev/null")?;
    // SAFETY: replacing stdout with an owned fd
    unsafe { libc::dup2(new_stdout.as_raw_fd(), 1) }.syscall_result()?;
    Ok(())
}
