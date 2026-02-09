// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Linux process launching support.

#![warn(missing_docs)]

#[cfg(target_os = "linux")]
mod linux;
#[cfg(target_os = "macos")]
mod posix_spawn;

use super::SyscallResult;
use super::while_eintr;
use std::collections::BTreeMap;
use std::ffi::CString;
use std::ffi::OsString;
use std::io;
use std::os::unix::prelude::*;
use std::path::PathBuf;
use std::process::ExitStatus;

#[cfg(target_os = "linux")]
use caps::CapsHashSet;
#[cfg(target_os = "linux")]
use landlock::RulesetCreated;
#[cfg(target_os = "linux")]
use seccompiler::SeccompFilter;

/// The different failure modes for sandbox related syscalls.
#[derive(Copy, Clone, Default)]
pub enum SandboxFailureMode {
    /// When a sandbox related syscall fails during process started,
    /// report no error and continue.
    Silent,
    /// When a sandbox related syscall fails during process started,
    /// report a trace::warn, and continue.
    Warn,
    /// When a sandbox related syscall fails during process started,
    /// report no trace::error, and fail.
    #[default]
    Error,
}

/// A container for linux specific builder options.
#[cfg(target_os = "linux")]
#[derive(Default)]
pub struct LinuxBuilder<'a> {
    clone_flags: libc::c_int,
    vfork: bool,
    setsid: bool,
    sandbox_failure_mode: SandboxFailureMode,
    controlling_terminal: Option<BorrowedFd<'a>>,
    permitted_capabilities: Option<CapsHashSet>,
    effective_capabilities: Option<CapsHashSet>,
    ambient_capabilities: Option<CapsHashSet>,
    bounding_capabilities: Option<CapsHashSet>,
    inheritable_capabilities: Option<CapsHashSet>,
    landlock_rules: Option<RulesetCreated>,
    seccomp_filter: Option<SeccompFilter>,
}

/// A builder for a child process.
pub struct Builder<'a> {
    executable: CString,
    argv: Vec<CString>,
    env: BTreeMap<OsString, Option<OsString>>,
    clear_env: bool,
    saw_nul: bool,
    stdin: Stdio<'a>,
    stdout: Stdio<'a>,
    stderr: Stdio<'a>,
    fd_ops: Vec<(i32, FdOp)>,
    uid: Option<libc::uid_t>,
    gid: Option<libc::uid_t>,
    #[cfg(target_os = "linux")]
    linux_builder: LinuxBuilder<'a>,
}

/// A stdio option.
#[derive(Debug)]
pub enum Stdio<'a> {
    /// Inherit the current process's stdio fd.
    Inherit,
    /// Open /dev/null for the child process.
    Null,
    /// Use the provided fd.
    Fd(BorrowedFd<'a>),
}

impl Stdio<'_> {
    fn op(&self, null: &mut Option<std::fs::File>) -> io::Result<Option<FdOp>> {
        Ok(match self {
            Stdio::Inherit => None,
            Stdio::Null => {
                let null = if let Some(null) = null.as_ref() {
                    null
                } else {
                    let f = std::fs::OpenOptions::new()
                        .read(true)
                        .write(true)
                        .open("/dev/null")?;
                    null.get_or_insert(f)
                };
                Some(FdOp::Dup(null.as_raw_fd()))
            }
            Stdio::Fd(oldfd) => Some(FdOp::Dup(oldfd.as_raw_fd())),
        })
    }
}

#[derive(Debug, Copy, Clone)]
enum FdOp {
    Close,
    Dup(i32),
}

fn os2c(s: OsString, saw_nul: &mut bool) -> CString {
    CString::new(s.into_vec()).unwrap_or_else(|_| {
        *saw_nul = true;
        CString::new("xxx").unwrap()
    })
}

fn c_slice_to_pointers(s: &[CString]) -> Vec<*const libc::c_char> {
    s.iter()
        .map(|x| x.as_ptr())
        .chain(std::iter::once(std::ptr::null()))
        .collect()
}

impl<'a> Builder<'a> {
    /// Creates a new process builder for `program`.
    pub fn new(program: impl Into<PathBuf>) -> Self {
        let mut saw_nul = false;
        let executable = os2c(program.into().into_os_string(), &mut saw_nul);
        let argv = vec![executable.clone()];
        Self {
            executable,
            argv,
            saw_nul,
            env: BTreeMap::new(),
            clear_env: false,
            stdin: Stdio::Inherit,
            stdout: Stdio::Inherit,
            stderr: Stdio::Inherit,
            fd_ops: vec![],
            uid: None,
            gid: None,
            #[cfg(target_os = "linux")]
            linux_builder: LinuxBuilder {
                clone_flags: 0,
                vfork: true,
                ..Default::default()
            },
        }
    }

    /// Sets argv\[0\].
    pub fn arg0(&mut self, arg: impl Into<OsString>) -> &mut Self {
        self.argv[0] = os2c(arg.into(), &mut self.saw_nul);
        self
    }

    /// Appends a command-line argument.
    pub fn arg(&mut self, arg: impl Into<OsString>) -> &mut Self {
        self.argv.push(os2c(arg.into(), &mut self.saw_nul));
        self
    }

    /// Appends a list of command-line arguments.
    pub fn args<I, S>(&mut self, args: I) -> &mut Self
    where
        I: IntoIterator<Item = S>,
        S: Into<OsString>,
    {
        for arg in args {
            self.arg(arg);
        }
        self
    }

    /// Sets the environment variable `key` to `val`.
    pub fn env<K, V>(&mut self, key: K, val: V) -> &mut Self
    where
        K: Into<OsString>,
        V: Into<OsString>,
    {
        self.env.insert(key.into(), Some(val.into()));
        self
    }

    /// Removes the environment variable `key`.
    pub fn env_remove<K: Into<OsString>>(&mut self, key: K) -> &mut Self {
        self.env.insert(key.into(), None);
        self
    }

    /// Clears all environment variables.
    pub fn env_clear(&mut self) -> &mut Self {
        self.env.clear();
        self.clear_env = true;
        self
    }

    /// Sets the policy for stdin.
    pub fn stdin(&mut self, stdin: Stdio<'a>) -> &mut Self {
        self.stdin = stdin;
        self
    }

    /// Sets the policy for stdout.
    pub fn stdout(&mut self, stdout: Stdio<'a>) -> &mut Self {
        self.stdout = stdout;
        self
    }

    /// Sets the policy for stderr.
    pub fn stderr(&mut self, stderr: Stdio<'a>) -> &mut Self {
        self.stderr = stderr;
        self
    }

    /// Closes `fd` in the new process.
    pub fn close_fd(&mut self, fd: i32) -> &mut Self {
        self.fd_ops.push((fd, FdOp::Close));
        self
    }

    /// Duplicates `oldfd` to `newfd` in the new process.
    pub fn dup_fd(&mut self, oldfd: BorrowedFd<'a>, newfd: i32) -> &mut Self {
        self.fd_ops.push((newfd, FdOp::Dup(oldfd.as_raw_fd())));
        self
    }

    /// Sets the real and effective user id of the new process.
    pub fn setuid(&mut self, uid: u32) -> &mut Self {
        self.uid = Some(uid);
        self
    }

    /// Gets the real and effective user id of the new process.
    pub fn uid(&self) -> Option<u32> {
        self.uid
    }

    /// Sets the real and effective group id of the new process.
    pub fn setgid(&mut self, gid: u32) -> &mut Self {
        self.gid = Some(gid);
        self
    }

    /// Gets the real and effective user id of the new process.
    pub fn gid(&self) -> Option<u32> {
        self.gid
    }

    /// Sets whether the new process will vfork or not.
    #[cfg(target_os = "linux")]
    pub fn set_vfork(&mut self, vfork: bool) -> &mut Self {
        self.linux_builder.vfork = vfork;
        self
    }

    /// Gets whether the new process will vfork or not.
    #[cfg(target_os = "linux")]
    pub fn vfork(&mut self) -> bool {
        self.linux_builder.vfork
    }

    /// Sets the sandbox failure mode.
    #[cfg(target_os = "linux")]
    pub fn set_sandbox_failure_mode(&mut self, mode: SandboxFailureMode) -> &mut Self {
        self.linux_builder.sandbox_failure_mode = mode;
        self
    }

    /// Gets the sandbox failure mode.
    #[cfg(target_os = "linux")]
    pub fn sandbox_failure_mode(&mut self) -> SandboxFailureMode {
        self.linux_builder.sandbox_failure_mode
    }

    /// Sets the permitted and inheritable capabilities of the new process.
    #[cfg(target_os = "linux")]
    pub fn set_permitted_caps(&mut self, caps: CapsHashSet) -> &mut Self {
        self.linux_builder.permitted_capabilities = Some(caps);
        self
    }

    /// Gets the permitted and inheritable capabilities of the new process.
    #[cfg(target_os = "linux")]
    pub fn permitted_caps(&mut self) -> Option<CapsHashSet> {
        self.linux_builder.permitted_capabilities.clone()
    }

    /// Sets the effective capabilities of the new process.
    #[cfg(target_os = "linux")]
    pub fn set_effective_caps(&mut self, caps: CapsHashSet) -> &mut Self {
        self.linux_builder.effective_capabilities = Some(caps);
        self
    }

    /// Gets the effective capabilities of the new process.
    #[cfg(target_os = "linux")]
    pub fn effective_caps(&mut self) -> Option<CapsHashSet> {
        self.linux_builder.effective_capabilities.clone()
    }

    /// Sets the ambient capabilities of the new process.
    #[cfg(target_os = "linux")]
    pub fn set_ambient_caps(&mut self, caps: CapsHashSet) -> &mut Self {
        self.linux_builder.ambient_capabilities = Some(caps);
        self
    }

    /// Gets the ambient capabilities of the new process.
    #[cfg(target_os = "linux")]
    pub fn ambient_caps(&mut self) -> Option<CapsHashSet> {
        self.linux_builder.ambient_capabilities.clone()
    }

    /// Sets the inheritable capabilities of the new process.
    #[cfg(target_os = "linux")]
    pub fn set_inheritable_caps(&mut self, caps: CapsHashSet) -> &mut Self {
        self.linux_builder.inheritable_capabilities = Some(caps);
        self
    }

    /// Gets the inheritable capabilities of the new process.
    #[cfg(target_os = "linux")]
    pub fn inheritable_caps(&mut self) -> Option<CapsHashSet> {
        self.linux_builder.inheritable_capabilities.clone()
    }

    /// Sets the bounding capabilities of the new process.
    #[cfg(target_os = "linux")]
    pub fn set_bounding_caps(&mut self, caps: CapsHashSet) -> &mut Self {
        self.linux_builder.bounding_capabilities = Some(caps);
        self
    }

    /// Gets the bounding capabilities of the new process.
    #[cfg(target_os = "linux")]
    pub fn bounding_caps(&mut self) -> Option<CapsHashSet> {
        self.linux_builder.bounding_capabilities.clone()
    }

    /// Sets the landlock ruleset of the new process.
    #[cfg(target_os = "linux")]
    pub fn set_landlock_rules(&mut self, landlock_rules: RulesetCreated) -> &mut Self {
        self.linux_builder.landlock_rules = Some(landlock_rules);
        self
    }

    /// Gets the landlock ruleset of the new process.
    #[cfg(target_os = "linux")]
    pub fn landlock_rules(&mut self) -> Option<RulesetCreated> {
        self.linux_builder
            .landlock_rules
            .as_ref()
            .map(|ruleset_created| ruleset_created.try_clone().unwrap())
    }

    /// Sets the seccomp filter for the new process.
    #[cfg(target_os = "linux")]
    pub fn set_seccomp_filter(&mut self, seccomp_filter: SeccompFilter) -> &mut Self {
        self.linux_builder.seccomp_filter = Some(seccomp_filter);
        self
    }

    /// Gets the seccomp filter for the new process.
    #[cfg(target_os = "linux")]
    pub fn seccomp_filter(&mut self) -> Option<SeccompFilter> {
        self.linux_builder.seccomp_filter.clone()
    }

    /// Creates a new session with the new process as the leader.
    #[cfg(target_os = "linux")]
    pub fn setsid(&mut self, setsid: bool) -> &mut Self {
        self.linux_builder.setsid = setsid;
        self
    }

    /// Sets the controlling terminal for the new process.
    #[cfg(target_os = "linux")]
    pub fn controlling_terminal(&mut self, controlling_terminal: BorrowedFd<'a>) -> &mut Self {
        self.linux_builder.controlling_terminal = Some(controlling_terminal);
        self
    }

    /// Spawns the process.
    pub fn spawn(&self) -> io::Result<Child> {
        let mut env = if self.clear_env {
            BTreeMap::new()
        } else {
            std::env::vars_os().collect()
        };
        for (key, value) in &self.env {
            if let Some(value) = value {
                env.insert(key.to_owned(), value.to_owned());
            } else {
                env.remove(key);
            }
        }

        let mut saw_nul = self.saw_nul;

        let envp: Vec<_> = env
            .into_iter()
            .map(|(mut key, value)| {
                key.push("=");
                key.push(&value);
                os2c(key, &mut saw_nul)
            })
            .collect();

        if saw_nul {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "null character in input string",
            ));
        }

        let mut null_file = None;
        let mut fd_ops = self.fd_ops.clone();

        if let Some(op) = self.stdin.op(&mut null_file)? {
            fd_ops.push((0, op));
        }
        if let Some(op) = self.stdout.op(&mut null_file)? {
            fd_ops.push((1, op));
        }
        if let Some(op) = self.stderr.op(&mut null_file)? {
            fd_ops.push((2, op));
        }

        self.spawn_internal(&envp, &mut fd_ops)
    }
}

/// A child process.
#[derive(Debug)]
pub struct Child {
    pid: i32,
    #[cfg(target_os = "linux")]
    pidfd: OwnedFd,
    status: Option<ExitStatus>,
}

impl Child {
    /// Synchronously waits for the child process to exit.
    pub fn wait(&mut self) -> io::Result<ExitStatus> {
        self.wait_internal(0).transpose().unwrap()
    }

    /// Tries to reap the child process it has exited. Otherwise returns `Ok(None)`.
    pub fn try_wait(&mut self) -> io::Result<Option<ExitStatus>> {
        self.wait_internal(libc::WNOHANG)
    }

    fn wait_internal(&mut self, options: i32) -> io::Result<Option<ExitStatus>> {
        if self.status.is_some() {
            return Ok(self.status);
        }

        let mut status = 0;
        // SAFETY: calling as documented.
        let n = unsafe {
            while_eintr(|| libc::waitpid(self.pid, &mut status, options).syscall_result())?
        };
        if n != 0 {
            self.status = Some(ExitStatus::from_raw(status));
        }
        Ok(self.status)
    }

    /// Returns the child process ID.
    pub fn id(&self) -> i32 {
        self.pid
    }
}

/// Terminates the process immediately.
pub(crate) fn terminate(exit_code: i32) -> ! {
    // SAFETY: there are no safety requirements for calling this function.
    unsafe {
        libc::_exit(exit_code);
    }
}

#[cfg(test)]
mod tests {
    use super::Builder;

    #[test]
    fn test_command() {
        let cmd = Builder::new("/usr/bin/true");
        let mut child = cmd.spawn().unwrap();

        #[cfg(target_os = "linux")]
        {
            use crate::sys::SyscallResult;
            use crate::sys::while_eintr;
            use std::os::unix::prelude::*;

            let mut pollfd = libc::pollfd {
                fd: child.as_fd().as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            };
            // SAFETY: pollfd holds a valid and open file descriptor.
            unsafe { while_eintr(|| libc::poll(&mut pollfd, 1, -1).syscall_result()).unwrap() };
            assert_eq!(pollfd.revents, libc::POLLIN);
            assert_eq!(child.try_wait().unwrap().unwrap().code().unwrap(), 0);
        }

        #[cfg(not(target_os = "linux"))]
        {
            assert_eq!(child.wait().unwrap().code().unwrap(), 0);
        }
    }

    #[test]
    #[cfg(target_os = "linux")]
    fn test_landlock_sandbox() {
        use crate::sys::SyscallResult;
        use crate::sys::while_eintr;
        use landlock::AccessFs;
        use landlock::PathBeneath;
        use landlock::PathFd;
        use landlock::Ruleset;
        use landlock::RulesetAttr;
        use landlock::RulesetCreatedAttr;
        use std::os::unix::prelude::*;

        let landlock_rules = Ruleset::default()
            .handle_access(AccessFs::Execute)
            .unwrap()
            .create()
            .unwrap()
            .add_rule(PathBeneath::new(
                PathFd::new("/").unwrap(),
                AccessFs::Execute,
            ))
            .unwrap();

        let mut cmd = Builder::new("/usr/bin/true");
        cmd.set_vfork(false);
        cmd.set_landlock_rules(landlock_rules);

        let mut child = cmd.spawn().unwrap();

        let mut pollfd = libc::pollfd {
            fd: child.as_fd().as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        };
        // SAFETY: pollfd holds a valid and open file descriptor.
        unsafe { while_eintr(|| libc::poll(&mut pollfd, 1, -1).syscall_result()).unwrap() };
        assert_eq!(pollfd.revents, libc::POLLIN);
        assert_eq!(child.wait().unwrap().code().unwrap(), 0);
    }

    #[test]
    #[cfg(target_os = "linux")]
    #[cfg(target_arch = "x86_64")]
    fn test_seccomp_sandbox() {
        use crate::sys::SyscallResult;
        use crate::sys::while_eintr;
        use seccompiler::SeccompAction;
        use seccompiler::SeccompFilter;
        use seccompiler::TargetArch;
        use std::os::unix::prelude::*;

        // This isn't defined in libc MUSL yet.
        const SYS_RSEQ: libc::c_long = 334;

        // This filter should work for both a dynamically linked `true`
        // or for a busybox statically linked `true`.
        let seccomp_filter = SeccompFilter::new(
            vec![
                (libc::SYS_execve, vec![]),
                (libc::SYS_brk, vec![]),
                (libc::SYS_arch_prctl, vec![]),
                (libc::SYS_mmap, vec![]),
                (libc::SYS_access, vec![]),
                (libc::SYS_openat, vec![]),
                (libc::SYS_newfstatat, vec![]),
                (libc::SYS_fstat, vec![]),
                (libc::SYS_close, vec![]),
                (libc::SYS_read, vec![]),
                (libc::SYS_pread64, vec![]),
                (libc::SYS_set_tid_address, vec![]),
                (libc::SYS_set_robust_list, vec![]),
                (SYS_RSEQ, vec![]),
                (libc::SYS_mprotect, vec![]),
                (libc::SYS_prlimit64, vec![]),
                (libc::SYS_munmap, vec![]),
                (libc::SYS_getrandom, vec![]),
                (libc::SYS_futex, vec![]),
                (libc::SYS_write, vec![]),
                (libc::SYS_exit_group, vec![]),
                (libc::SYS_readlink, vec![]),
                (libc::SYS_uname, vec![]),
                (libc::SYS_getgid, vec![]),
                (libc::SYS_getuid, vec![]),
                (libc::SYS_setgid, vec![]),
                (libc::SYS_setuid, vec![]),
                (libc::SYS_prctl, vec![]),
            ]
            .into_iter()
            .collect(),
            // mismatch_action
            SeccompAction::Log,
            // match_action
            SeccompAction::Allow,
            // target architecture of filter
            TargetArch::x86_64,
        )
        .unwrap();

        let mut cmd = Builder::new("/usr/bin/true");
        cmd.set_vfork(false);
        cmd.set_seccomp_filter(seccomp_filter);

        let mut child = cmd.spawn().unwrap();

        let mut pollfd = libc::pollfd {
            fd: child.as_fd().as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        };
        // SAFETY: pollfd holds a valid and open file descriptor.
        unsafe { while_eintr(|| libc::poll(&mut pollfd, 1, -1).syscall_result()).unwrap() };
        assert_eq!(pollfd.revents, libc::POLLIN);
        assert!([None, Some(0)].contains(&child.wait().unwrap().code()));
    }
}
