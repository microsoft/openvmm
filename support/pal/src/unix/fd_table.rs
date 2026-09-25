// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Early expansion of the Linux file descriptor table.

use crate::unix::SyscallResult;
use std::fs::File;
use std::io;
use std::io::Error;
use std::os::unix::prelude::*;

const FIRST_DYNAMIC_FD: RawFd = usize::BITS as RawFd;

/// Expands the Linux file descriptor table before worker threads are started.
///
/// Growing a full descriptor table after other threads exist may wait for an
/// RCU grace period. Touching a high descriptor while startup is still
/// single-threaded avoids that restore-time latency.
pub fn expand_fd_table() -> io::Result<()> {
    let mut limits = libc::rlimit {
        rlim_cur: 0,
        rlim_max: 0,
    };

    // SAFETY: `limits` is valid writable memory for `getrlimit`.
    unsafe { libc::getrlimit(libc::RLIMIT_NOFILE, &mut limits) }.syscall_result()?;

    if limits.rlim_cur <= FIRST_DYNAMIC_FD as libc::rlim_t {
        return Ok(());
    }

    // Linux keeps BITS_PER_LONG descriptors inline. Allocate only the first
    // dynamic table: later growth caused the observed stall, while expanding
    // all the way toward RLIMIT_NOFILE adds unnecessary process-exit work.
    expand_fd_table_to(FIRST_DYNAMIC_FD)
}

fn expand_fd_table_to(target_fd: RawFd) -> io::Result<()> {
    // An existing descriptor proves that the table already reaches the target.
    // SAFETY: `F_GETFD` only inspects the descriptor number.
    if unsafe { libc::fcntl(target_fd, libc::F_GETFD) } >= 0 {
        return Ok(());
    }
    let error = Error::last_os_error();
    if error.raw_os_error() != Some(libc::EBADF) {
        return Err(error);
    }

    let source = File::open("/dev/null")?;
    if source.as_raw_fd() >= target_fd {
        return Ok(());
    }

    // Unlike dup2, F_DUPFD_CLOEXEC never replaces an inherited descriptor if
    // the target becomes occupied.
    // SAFETY: `source` is valid and `target_fd` is a nonnegative lower bound.
    let duplicate = unsafe { libc::fcntl(source.as_raw_fd(), libc::F_DUPFD_CLOEXEC, target_fd) }
        .syscall_result()?;
    // SAFETY: `duplicate` was returned as a new owned descriptor above.
    unsafe { libc::close(duplicate) }.syscall_result()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::FIRST_DYNAMIC_FD;
    use super::SyscallResult;
    use super::expand_fd_table_to;
    use std::io::Read;
    use std::io::Write;
    use std::os::fd::AsRawFd;
    use std::os::fd::FromRawFd;
    use std::os::unix::net::UnixStream;

    #[test]
    fn expand_fd_table_preserves_occupied_target() {
        let (source, mut peer) = UnixStream::pair().unwrap();
        // SAFETY: `source` is valid and the returned descriptor is checked.
        let inherited_fd =
            unsafe { libc::fcntl(source.as_raw_fd(), libc::F_DUPFD_CLOEXEC, FIRST_DYNAMIC_FD) }
                .syscall_result()
                .unwrap();
        // SAFETY: `inherited_fd` is a new owned descriptor.
        let mut inherited = unsafe { UnixStream::from_raw_fd(inherited_fd) };

        expand_fd_table_to(inherited.as_raw_fd()).unwrap();

        peer.write_all(b"x").unwrap();
        let mut byte = [0];
        inherited.read_exact(&mut byte).unwrap();
        assert_eq!(byte, [b'x']);
    }
}
