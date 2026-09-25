// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Persistence of TAP interfaces.

// UNSAFETY: Calling an ioctl.
#![expect(unsafe_code)]

use super::Error;
use linux_net_bindings::tun_set_persist;
use std::io;
use std::os::fd::OwnedFd;
use std::os::raw::c_int;
use std::os::unix::prelude::AsRawFd;

/// Sets whether a TAP interface persists after its last fd is closed.
pub fn set_persistent(fd: &OwnedFd, persistent: bool) -> Result<(), Error> {
    // SAFETY: calling the ioctl according to implementation requirements.
    unsafe {
        tun_set_persist(fd.as_raw_fd(), c_int::from(persistent))
            .map_err(|_e| Error::SetPersistent(io::Error::last_os_error()))?;
    }
    Ok(())
}
