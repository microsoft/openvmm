// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Linux process spawning.

use super::Builder;
use super::Child;
use super::FdOp;
use super::SandboxFailureMode;
use crate::unix::SyscallResult;
use crate::unix::errno;
use caps::CapsHashSet;
use landlock::RulesetCreated;
use seccompiler::SeccompFilter;
use std::ffi::CStr;
use std::ffi::CString;
use std::io;
use std::os::unix::prelude::*;

struct CloneContext<'a> {
    executable: &'a CStr,
    argv: &'a [*const libc::c_char],
    envp: &'a [*const libc::c_char],
    result: Option<i32>,
    // TODO: refactor this to contain BorrowedFds
    fd_ops: &'a mut [(i32, FdOp)],
    sandbox_failure_mode: SandboxFailureMode,
    setsid: bool,
    controlling_terminal: Option<BorrowedFd<'a>>,
    uid: Option<libc::uid_t>,
    gid: Option<libc::uid_t>,
    permitted_capabilities: Option<CapsHashSet>,
    effective_capabilities: Option<CapsHashSet>,
    ambient_capabilities: Option<CapsHashSet>,
    inheritable_capabilities: Option<CapsHashSet>,
    bounding_capabilities: Option<CapsHashSet>,
    landlock_rules: Option<RulesetCreated>,
    seccomp_filter: Option<SeccompFilter>,
}

impl Builder<'_> {
    pub(super) fn spawn_internal(
        &self,
        envp: &[CString],
        fd_ops: &mut [(i32, FdOp)],
    ) -> io::Result<Child> {
        let mut landlock_rules = None;
        if let Some(lr) = &self.linux_builder.landlock_rules {
            landlock_rules = Some(lr.try_clone()?);
        }

        // Build the null-terminated arrays for exec.
        let argv = super::c_slice_to_pointers(&self.argv);
        let envp = super::c_slice_to_pointers(envp);

        let mut context = CloneContext {
            executable: &self.executable,
            argv: &argv,
            envp: &envp,
            result: None,
            fd_ops: &mut *fd_ops,
            sandbox_failure_mode: self.linux_builder.sandbox_failure_mode,
            setsid: self.linux_builder.setsid,
            controlling_terminal: self.linux_builder.controlling_terminal,
            uid: self.uid,
            gid: self.gid,
            permitted_capabilities: self.linux_builder.permitted_capabilities.clone(),
            effective_capabilities: self.linux_builder.effective_capabilities.clone(),
            inheritable_capabilities: self.linux_builder.inheritable_capabilities.clone(),
            ambient_capabilities: self.linux_builder.ambient_capabilities.clone(),
            bounding_capabilities: self.linux_builder.bounding_capabilities.clone(),
            landlock_rules,
            seccomp_filter: self.linux_builder.seccomp_filter.clone(),
        };

        // Use CLONE_VM and CLONE_VFORK so that the new process will share the
        // current address space and will block this thread until it either
        // exits or calls exec.
        //
        // Use CLONE_PIDFD to get an fd back to use for polling.
        let mut flags = self.linux_builder.clone_flags | libc::CLONE_PIDFD | libc::SIGCHLD;

        if self.linux_builder.vfork {
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
        if self.linux_builder.vfork && context.result != Some(0) {
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

    macro_rules! handle_sandbox_failure {
        ($m:expr, $r:expr) => {
            match context.sandbox_failure_mode {
                SandboxFailureMode::Silent => {}
                SandboxFailureMode::Warn => {
                    tracing::warn!($m);
                }
                SandboxFailureMode::Error => {
                    tracing::error!($m);
                    return $r;
                }
            }
        };
    }

    if let Some(landlock_rules) = context.landlock_rules.take() {
        if landlock_rules.restrict_self().is_err() {
            handle_sandbox_failure!("failed to apply landlock ruleset", libc::ENOTSUP);
        }
    }

    if let Some(gid) = context.gid {
        // SAFETY: setresgid has no safety requirements.
        if unsafe { libc::setresgid(gid, gid, gid) } < 0 {
            handle_sandbox_failure!("failed to change group id", libc::ENOTSUP);
        }
    }

    if let Some(uid) = context.uid {
        // SAFETY: setresuid has no safety requirements.
        if unsafe { libc::setresuid(uid, uid, uid) } < 0 {
            handle_sandbox_failure!("failed to change user id", libc::ENOTSUP);
        }
    }

    macro_rules! set_capabilities {
        ($t:expr, $v:ident) => {
            if let Some($v) = &context.$v {
                if caps::set(None, $t, &$v).is_err() {
                    handle_sandbox_failure!(
                        std::concat!("failed to apply ", stringify!($t), " capabilities"),
                        libc::ENOTSUP
                    );
                }
            }
        };
    }

    set_capabilities!(caps::CapSet::Bounding, bounding_capabilities);
    set_capabilities!(caps::CapSet::Permitted, permitted_capabilities);
    set_capabilities!(caps::CapSet::Ambient, ambient_capabilities);
    set_capabilities!(caps::CapSet::Inheritable, inheritable_capabilities);
    set_capabilities!(caps::CapSet::Effective, effective_capabilities);

    if let Some(seccomp_filter) = context.seccomp_filter.take() {
        if let Ok(bpf_program) = TryInto::<seccompiler::BpfProgram>::try_into(seccomp_filter) {
            if seccompiler::apply_filter(&bpf_program).is_err() {
                handle_sandbox_failure!("failed to apply seccomp profile", libc::ENOTSUP);
            }
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

impl AsFd for Child {
    fn as_fd(&self) -> BorrowedFd<'_> {
        self.pidfd.as_fd()
    }
}
