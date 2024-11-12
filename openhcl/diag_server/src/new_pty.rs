// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

// UNSAFETY: Calling openpty.
#![allow(unsafe_code)]

use std::fs::File;
use std::io;
use std::os::fd::FromRawFd;
use std::ptr::null_mut;

pub(crate) fn new_pty() -> io::Result<(File, File)> {
    // SAFETY: calling openpty as documented
    unsafe {
        let mut primary = 0;
        let mut secondary = 0;
        if libc::openpty(
            &mut primary,
            &mut secondary,
            null_mut(),
            null_mut(),
            null_mut(),
        ) < 0
        {
            return Err(io::Error::last_os_error());
        }
        let primary = File::from_raw_fd(primary);
        let secondary = File::from_raw_fd(secondary);
        Ok((primary, secondary))
    }
}
