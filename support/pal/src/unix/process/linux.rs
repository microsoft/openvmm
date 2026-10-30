// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Linux process spawning.

use super::Builder;
use super::Child;
use super::FdOp;
use crate::unix::SyscallResult;
use crate::unix::errno;
use seccompiler::BpfProgram;
use std::ffi::CStr;
use std::ffi::CString;
use std::io;
use std::os::unix::prelude::*;

const ID_MAP_CAPACITY: usize = 32;

#[derive(Clone, Copy)]
struct IdMap {
    bytes: [u8; ID_MAP_CAPACITY],
    len: usize,
}

impl IdMap {
    fn new(outer_id: u32) -> Self {
        let mut bytes = [0; ID_MAP_CAPACITY];
        bytes[0] = b'0';
        bytes[1] = b' ';
        let mut index = 2;
        let mut divisor = 1_000_000_000;
        let mut started = false;
        while divisor != 0 {
            let digit = (outer_id / divisor) % 10;
            if digit != 0 || started || divisor == 1 {
                bytes[index] = b'0' + digit as u8;
                index += 1;
                started = true;
            }
            divisor /= 10;
        }
        bytes[index..index + 3].copy_from_slice(b" 1\n");
        Self {
            bytes,
            len: index + 3,
        }
    }
}

struct CloneContext<'a> {
    executable: &'a CStr,
    argv: &'a [*const libc::c_char],
    envp: &'a [*const libc::c_char],
    result: Option<i32>,
    // TODO: refactor this to contain BorrowedFds
    fd_ops: &'a mut [(i32, FdOp)],
    setsid: bool,
    controlling_terminal: Option<BorrowedFd<'a>>,
    user_namespace_maps: Option<(IdMap, IdMap)>,
    fd_close_ranges: &'a [(u32, u32)],
    uid: Option<libc::uid_t>,
    gid: Option<libc::uid_t>,
    trace_seccomp_program: Option<&'a BpfProgram>,
}

impl Builder<'_> {
    pub(super) fn spawn_internal(
        &self,
        envp: &[CString],
        fd_ops: &mut [(i32, FdOp)],
    ) -> io::Result<Child> {
        let sandbox = self.linux_builder.sandbox.as_ref();
        let clone_flags: libc::c_int = sandbox
            .map(|config| {
                config.clone_flags.try_into().map_err(|_| {
                    io::Error::new(
                        io::ErrorKind::InvalidInput,
                        "sandbox clone flags do not fit in Linux clone flags",
                    )
                })
            })
            .transpose()?
            .unwrap_or_default();
        let map_current_user = sandbox.is_some_and(|config| config.map_current_user);
        if map_current_user && clone_flags & libc::CLONE_NEWUSER == 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "user namespace self-mapping requires CLONE_NEWUSER",
            ));
        }
        if map_current_user && (self.uid.is_some() || self.gid.is_some()) {
            return Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "explicit UID/GID changes are not supported with user namespace self-mapping",
            ));
        }

        // Build the null-terminated arrays for exec.
        let argv = super::c_slice_to_pointers(&self.argv);
        let envp = super::c_slice_to_pointers(envp);
        let inherited_fds = sandbox.map(inherited_fds).transpose()?;
        let fd_close_ranges = inherited_fds
            .as_deref()
            .map(fd_close_ranges)
            .transpose()?
            .unwrap_or_default();

        // The clone callback must not compile or drop heap-owned filter state.
        let trace_seccomp_program = self
            .linux_builder
            .trace_seccomp_filter
            .as_ref()
            .map(|filter| {
                BpfProgram::try_from(filter.clone()).map_err(|error| {
                    io::Error::new(
                        io::ErrorKind::InvalidInput,
                        format!("failed to compile tracing seccomp filter: {error}"),
                    )
                })
            })
            .transpose()?;

        let mut context = CloneContext {
            executable: &self.executable,
            argv: &argv,
            envp: &envp,
            result: None,
            fd_ops: &mut *fd_ops,
            setsid: self.linux_builder.setsid,
            controlling_terminal: self.linux_builder.controlling_terminal,
            user_namespace_maps: map_current_user.then(|| {
                // SAFETY: geteuid and getegid have no safety requirements.
                let (uid, gid) = unsafe { (libc::geteuid(), libc::getegid()) };
                (IdMap::new(uid), IdMap::new(gid))
            }),
            fd_close_ranges: &fd_close_ranges,
            uid: self.uid,
            gid: self.gid,
            trace_seccomp_program: trace_seccomp_program.as_ref(),
        };

        // Use CLONE_VM and CLONE_VFORK so that the new process will share the
        // current address space and will block this thread until it either
        // exits or calls exec.
        //
        // Use CLONE_PIDFD to get an fd back to use for polling.
        let mut flags = clone_flags | libc::CLONE_PIDFD | libc::SIGCHLD;

        // Tracing stops the child before exec so the parent can attach ptrace.
        // Using vfork would deadlock: the parent waits for exec while the child
        // waits for the parent to resume it.
        let using_vfork = self.linux_builder.vfork && trace_seccomp_program.is_none();
        if using_vfork {
            flags |= libc::CLONE_VM | libc::CLONE_VFORK;
        }

        // SAFETY: sysconf has no safety requirements.
        let page_size = unsafe { libc::sysconf(libc::_SC_PAGESIZE) } as usize;

        // Common page sizes are 4KiB, 16KiB, and 64KiB. The stack size must be a multiple
        // of the page size.
        let stack_len: usize = std::cmp::max(16 * 1024, page_size);
        assert!(stack_len.is_multiple_of(page_size));

        // Create a stack with one guard page.
        let stack_len = stack_len + page_size;
        // SAFETY: creating a new mapping, which has no safety requirements.
        let stack = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                stack_len,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_PRIVATE | libc::MAP_ANONYMOUS,
                -1,
                0,
            )
        };
        if stack == libc::MAP_FAILED {
            return Err(errno().into());
        }
        let mmap_guard = ChildStackGuard(stack, stack_len);
        // SAFETY: The stack has been checked to be valid, and its length is more than one page.
        unsafe { libc::mprotect(stack, page_size, libc::PROT_NONE) }.syscall_result()?;
        let mut pidfd: libc::pid_t = -1;

        // SAFETY: The stack is valid for stack len, if the child goes off the
        // stack they'll hit our guard page, the flags include PIDFD so passing
        // pidfd is valid, and clone_cb takes a CloneContext pointer as its only
        // argument.
        let pid = unsafe {
            libc::clone(
                clone_cb,
                stack.add(stack_len),
                flags,
                std::ptr::from_mut(&mut context).cast(),
                &mut pidfd,
            )
        }
        .syscall_result()?;
        drop(mmap_guard);

        // SAFETY: We set the PIDFD flag, and clone returned successfully, so pidfd is now valid.
        let pidfd = unsafe { OwnedFd::from_raw_fd(pidfd) };
        let mut child = Child {
            pid,
            pidfd,
            status: None,
        };

        // This can only be done if we are vforking, without sharing another
        // type of status object we can't determine if the execve failed or
        // the process failed during early initialization.
        if using_vfork && context.result != Some(0) {
            // The new process failed without successfully calling execve. Reap
            // it and return the associated error code (which may come from
            // context or from the exit code).
            let status = child.wait().unwrap();
            let ec = context.result.unwrap_or_else(|| {
                status
                    .code()
                    .expect("child should not have failed with a signal")
            });
            return Err(io::Error::from_raw_os_error(ec));
        }

        Ok(child)
    }
}

fn inherited_fds(config: &sandbox::SandboxProcessConfig) -> io::Result<Vec<i32>> {
    let mut fds = vec![0, 1, 2];
    for (_, raw_handle) in &config.inherit_handles {
        fds.push(raw_handle.0.try_into().map_err(|_| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                "sandbox inherited handle does not fit in a Linux file descriptor",
            )
        })?);
    }
    fds.sort_unstable();
    fds.dedup();
    Ok(fds)
}

struct ChildStackGuard(*mut libc::c_void, usize);

impl Drop for ChildStackGuard {
    fn drop(&mut self) {
        // SAFETY: We know the pointer is valid and the length is correct at
        // construction, and we know the child is not running anymore, so it's
        // safe to unmap the stack.
        unsafe { libc::munmap(self.0, self.1) }
            .syscall_result()
            .unwrap();
    }
}

/// Runs in the cloned process to set up the process environment and exec the
/// new binary.
///
/// This function must not use the heap or call any functions that might. It
/// also has only a small amount of stack space available. It should avoid using
/// OS functionality via the std crate and should use libc directly.
///
/// Returns the exit code for the new process. If this function does not update
/// context's result, then the exit code will be the Linux errno value
/// associated with the error.
//
// N.B. this should be unsafe but the libc crate neglected to mark the clone
// callback appropriately.
extern "C" fn clone_cb(context: *mut libc::c_void) -> libc::c_int {
    // SAFETY: Context is temporarily owned by this function, and we know
    // we were passed a valid pointer.
    let context = unsafe { &mut *(context.cast::<CloneContext<'_>>()) };

    if let Some((uid_map, gid_map)) = context.user_namespace_maps {
        if write_proc_file(c"/proc/self/setgroups", b"deny\n") < 0 {
            return errno().0;
        }
        if write_proc_file(c"/proc/self/uid_map", &uid_map.bytes[..uid_map.len]) < 0 {
            return errno().0;
        }
        if write_proc_file(c"/proc/self/gid_map", &gid_map.bytes[..gid_map.len]) < 0 {
            return errno().0;
        }
    }

    if context.setsid {
        // SAFETY: setsid has no safety requirements.
        if unsafe { libc::setsid() } < 0 {
            return errno().0;
        }
    }

    if let Some(fd) = context.controlling_terminal {
        // SAFETY: fd is guaranteed to be valid.
        if unsafe { libc::ioctl(fd.as_raw_fd(), libc::TIOCSCTTY, 0) } < 0 {
            return errno().0;
        }
    }

    // Find the maximum newfd, needed below.
    let maxfd = context.fd_ops.iter().map(|(fd, _)| *fd).max();

    if let Some(maxfd) = maxfd {
        for (newfd, op) in &mut *context.fd_ops {
            match op {
                FdOp::Close => {}
                FdOp::Dup(oldfd) => {
                    // Ensure oldfd is above the maximum newfd. This is
                    // necessary to ensure that another operation does not close
                    // an oldfd targeted by this operation.
                    if oldfd != newfd && *oldfd < maxfd {
                        // SAFETY: fd is guaranteed to be valid
                        let new_oldfd =
                            unsafe { libc::fcntl(*oldfd, libc::F_DUPFD_CLOEXEC, maxfd) };
                        if new_oldfd < 0 {
                            return errno().0;
                        }
                        *oldfd = new_oldfd;
                    }
                }
            }
        }

        for (newfd, op) in &*context.fd_ops {
            match op {
                FdOp::Close => {
                    // SAFETY: fd is guaranteed to be valid
                    if unsafe { libc::close(*newfd) } < 0 {
                        return errno().0;
                    }
                }
                FdOp::Dup(oldfd) => {
                    if *newfd == *oldfd {
                        // SAFETY: fd is guaranteed to be valid
                        if unsafe {
                            libc::fcntl(
                                *oldfd,
                                libc::F_SETFD,
                                libc::fcntl(*oldfd, libc::F_GETFD) & !libc::FD_CLOEXEC,
                            )
                        } < 0
                        {
                            return errno().0;
                        }
                    } else {
                        // SAFETY: fds are guaranteed to be valid
                        if unsafe { libc::dup2(*oldfd, *newfd) } < 0 {
                            return errno().0;
                        }
                    }
                }
            }
        }
    }

    for &(first, last) in context.fd_close_ranges {
        // SAFETY: close_range takes integer bounds and affects only the
        // calling process's file descriptor table.
        if unsafe { libc::syscall(libc::SYS_close_range, first, last, 0) } < 0 {
            return errno().0;
        }
    }

    if let Some(gid) = context.gid {
        // SAFETY: setresgid has no safety requirements.
        if unsafe { libc::setresgid(gid, gid, gid) } < 0 {
            return errno().0;
        }
    }

    if let Some(uid) = context.uid {
        // SAFETY: setresuid has no safety requirements.
        if unsafe { libc::setresuid(uid, uid, uid) } < 0 {
            return errno().0;
        }
    }

    // Stop before installing the tracing filter and executing the worker so the
    // parent can attach ptrace and capture syscalls from the start of execution.
    if context.trace_seccomp_program.is_some() {
        // SAFETY: raise has no memory-safety requirements.
        if unsafe { libc::raise(libc::SIGSTOP) } < 0 {
            return errno().0;
        }
    }

    if let Some(bpf_program) = context.trace_seccomp_program {
        if seccompiler::apply_filter(bpf_program).is_err() {
            return libc::ENOTSUP;
        }
    }

    // Update the result indicating success in case execvpe does not return.
    context.result = Some(0);
    // N.B. This will only return on error.
    // SAFETY: Arguments in the context are valid CStrings, and the two arrays
    // are properly null terminated.
    unsafe {
        libc::execvpe(
            context.executable.as_ptr(),
            context.argv.as_ptr(),
            context.envp.as_ptr(),
        )
    };
    // Update the result with the failure code.
    context.result = Some(errno().0);
    255
}

fn fd_close_ranges(allowlist: &[i32]) -> io::Result<Vec<(u32, u32)>> {
    let mut allowlist = allowlist.to_vec();
    allowlist.sort_unstable();

    let mut ranges = Vec::new();
    let mut first = 0;
    let mut previous = None;
    for fd in allowlist {
        if fd < 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "inherited file descriptor allowlist contains a negative descriptor",
            ));
        }
        if previous == Some(fd) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "inherited file descriptor allowlist contains a duplicate descriptor",
            ));
        }

        let fd = fd as u32;
        if first < fd {
            ranges.push((first, fd - 1));
        }
        first = fd + 1;
        previous = Some(fd as i32);
    }
    ranges.push((first, u32::MAX));
    Ok(ranges)
}

fn write_proc_file(path: &CStr, value: &[u8]) -> libc::c_int {
    // SAFETY: path is NUL-terminated and points to a procfs control file.
    let fd = unsafe { libc::open(path.as_ptr(), libc::O_WRONLY | libc::O_CLOEXEC) };
    if fd < 0 {
        return -1;
    }

    let mut written = 0;
    while written < value.len() {
        // SAFETY: fd is open and the remaining slice is valid for reads.
        let result =
            unsafe { libc::write(fd, value[written..].as_ptr().cast(), value.len() - written) };
        if result <= 0 {
            let saved_errno = if result == 0 { libc::EIO } else { errno().0 };
            // SAFETY: fd is open and owned by this function.
            unsafe { libc::close(fd) };
            // SAFETY: assigning errno restores the write failure for the caller.
            unsafe { *libc::__errno_location() = saved_errno };
            return -1;
        }
        written += result as usize;
    }

    // SAFETY: fd is open and owned by this function.
    unsafe { libc::close(fd) }
}

impl AsFd for Child {
    fn as_fd(&self) -> BorrowedFd<'_> {
        self.pidfd.as_fd()
    }
}

#[cfg(test)]
mod tests {
    use super::Builder;
    use super::IdMap;
    use super::fd_close_ranges;
    use super::inherited_fds;
    use seccompiler::SeccompAction;
    use seccompiler::SeccompFilter;
    use seccompiler::TargetArch;

    #[test]
    fn rejects_uncompilable_trace_filter_before_spawning() {
        #[cfg(target_arch = "x86_64")]
        let arch = TargetArch::x86_64;
        #[cfg(target_arch = "aarch64")]
        let arch = TargetArch::aarch64;

        // The rules expand beyond seccomp's 4096-instruction limit.
        let filter = SeccompFilter::new(
            (0..4096).map(|nr| (nr, Vec::new())).collect(),
            SeccompAction::Allow,
            SeccompAction::Trace(0),
            arch,
        )
        .unwrap();
        let mut command = Builder::new("/usr/bin/true");
        command.set_trace_seccomp_filter(filter);
        let error = match command.spawn() {
            Err(error) => error,
            Ok(mut child) => {
                // SAFETY: child.id() names the process spawned above.
                assert_eq!(unsafe { libc::kill(child.id(), libc::SIGKILL) }, 0);
                child.wait().unwrap();
                panic!("uncompilable tracing filter spawned a child");
            }
        };
        assert_eq!(error.kind(), std::io::ErrorKind::InvalidInput);
    }

    #[test]
    fn computes_fd_close_ranges() {
        assert_eq!(fd_close_ranges(&[]).unwrap(), [(0, u32::MAX)]);
        assert_eq!(fd_close_ranges(&[0, 1, 2, 3]).unwrap(), [(4, u32::MAX)]);
        assert_eq!(
            fd_close_ranges(&[3, 0, 7]).unwrap(),
            [(1, 2), (4, 6), (8, u32::MAX)]
        );
    }

    #[test]
    fn rejects_invalid_fd_allowlists() {
        assert_eq!(
            fd_close_ranges(&[-1]).unwrap_err().kind(),
            std::io::ErrorKind::InvalidInput
        );
        assert_eq!(
            fd_close_ranges(&[3, 3]).unwrap_err().kind(),
            std::io::ErrorKind::InvalidInput
        );
    }

    #[test]
    fn sandbox_handles_become_inherited_fds() {
        let config = sandbox::SandboxProcessConfig {
            inherit_handles: vec![
                (sandbox::HandleTag(1), sandbox::RawHandle(7)),
                (sandbox::HandleTag(2), sandbox::RawHandle(3)),
            ],
            ..Default::default()
        };
        assert_eq!(inherited_fds(&config).unwrap(), [0, 1, 2, 3, 7]);
    }

    #[test]
    fn formats_single_id_map() {
        for (id, expected) in [
            (0, "0 0 1\n"),
            (1000, "0 1000 1\n"),
            (u32::MAX, "0 4294967295 1\n"),
        ] {
            let map = IdMap::new(id);
            assert_eq!(&map.bytes[..map.len], expected.as_bytes());
        }
    }
}
