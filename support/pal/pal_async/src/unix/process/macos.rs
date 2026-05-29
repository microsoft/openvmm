// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! macOS process wait implementations using kqueue `EVFILT_PROC`.

use crate::process::DynProcessWaitDriver;
use crate::process::PolledChild;
use std::io;

impl PolledChild<std::process::Child> {
    /// Creates a new `PolledChild` wrapping a [`std::process::Child`].
    ///
    /// Uses kqueue `EVFILT_PROC` to poll for exit. If the child has
    /// already exited, the wait completes without a kqueue registration.
    pub fn new(
        driver: &(impl ?Sized + DynProcessWaitDriver),
        mut child: std::process::Child,
    ) -> io::Result<Self> {
        // Check if the child has already exited before registering.
        if let Some(_status) = child.try_wait()? {
            return Ok(Self { wait: None, child });
        }
        let wait = driver.new_dyn_process_wait_pid(child.id() as libc::pid_t)?;
        Ok(Self {
            wait: Some(wait),
            child,
        })
    }
}

impl PolledChild<pal::unix::process::Child> {
    /// Creates a new `PolledChild` wrapping a [`pal::unix::process::Child`].
    ///
    /// Uses kqueue `EVFILT_PROC` to poll for exit. If the child has
    /// already exited, the wait completes without a kqueue registration.
    pub fn new(
        driver: &(impl ?Sized + DynProcessWaitDriver),
        mut child: pal::unix::process::Child,
    ) -> io::Result<Self> {
        if let Some(_status) = child.try_wait()? {
            return Ok(Self { wait: None, child });
        }
        let wait = driver.new_dyn_process_wait_pid(child.id() as libc::pid_t)?;
        Ok(Self {
            wait: Some(wait),
            child,
        })
    }
}
