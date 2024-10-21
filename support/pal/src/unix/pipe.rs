// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use super::SyscallResult;
use std::fs::File;
use std::io::Result;
use std::os::unix::prelude::*;

/// Creates a connected pair of pipes, returning (read, write).
pub fn pair() -> Result<(File, File)> {
    // SAFETY: calling C APIs as documented, with no special requirements.
    unsafe {
        let mut fds = [0; 2];
        #[cfg(target_os = "linux")]
        {
            // Use pipe2 to set O_CLOEXEC atomically with pipe creation.
            libc::pipe2(fds.as_mut_ptr(), libc::O_CLOEXEC).syscall_result()?;
        }
        #[cfg(not(target_os = "linux"))]
        {
            // Create the pipes and then set O_CLOEXEC as a separate step since
            // pipe2 is not available.
            //
            // N.B. There is a race window here where the pipe will be inherited
            //      by a child process if the fork occurs right after this call
            //      before O_CLOEXEC is set.
            libc::pipe(fds.as_mut_ptr()).syscall_result()?;
            libc::fcntl(fds[0], libc::F_SETFL, libc::O_CLOEXEC)
                .syscall_result()
                .unwrap();
            libc::fcntl(fds[1], libc::F_SETFL, libc::O_CLOEXEC)
                .syscall_result()
                .unwrap();
        }
        Ok((File::from_raw_fd(fds[0]), File::from_raw_fd(fds[1])))
    }
}

/// Sets a file's nonblocking state.
pub fn set_nonblocking(file: &File, nonblock: bool) -> Result<()> {
    // SAFETY: the fd is owned, and changing the nonblocking state should not
    // result in any memory safety issues since it just changes the conditions
    // under which an IO will fail.
    unsafe {
        let mut flags = libc::fcntl(file.as_raw_fd(), libc::F_GETFL).syscall_result()?;
        if nonblock {
            flags |= libc::O_NONBLOCK;
        } else {
            flags &= !libc::O_NONBLOCK;
        }
        libc::fcntl(file.as_raw_fd(), libc::F_SETFL, flags).syscall_result()?;
    }
    Ok(())
}
