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
use seccompiler::SeccompFilter;

/// A container for linux specific builder options.
#[cfg(target_os = "linux")]
#[derive(Default)]
pub struct LinuxBuilder<'a> {
    sandbox: Option<sandbox::SandboxProcessConfig>,
    vfork: bool,
    trace_seccomp_filter: Option<SeccompFilter>,
    setsid: bool,
    controlling_terminal: Option<BorrowedFd<'a>>,
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

    /// Applies prepared sandbox launch requirements to the new process.
    ///
    /// With user namespace self-mapping, explicit [`Self::setuid`] or
    /// [`Self::setgid`] requests are unsupported and cause spawning to fail.
    #[cfg(target_os = "linux")]
    pub fn apply_sandbox(
        &mut self,
        config: sandbox::SandboxProcessConfig,
    ) -> io::Result<&mut Self> {
        if self.linux_builder.sandbox.is_some() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "sandbox configuration has already been applied",
            ));
        }
        if config.windows.is_some() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "Windows sandbox configuration cannot be applied on Linux",
            ));
        }
        self.linux_builder.sandbox = Some(config);
        Ok(self)
    }

    /// Applies prepared sandbox launch requirements to the new process.
    #[cfg(not(target_os = "linux"))]
    pub fn apply_sandbox(
        &mut self,
        _config: sandbox::SandboxProcessConfig,
    ) -> io::Result<&mut Self> {
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "sandboxed process launch is not implemented on this platform",
        ))
    }

    /// Stops the child before installing a tracing seccomp filter and
    /// executing the new image so a ptrace supervisor can attach.
    ///
    /// This disables `vfork` because a stopped `vfork` child would indefinitely
    /// block the spawning thread.
    #[cfg(target_os = "linux")]
    pub fn set_trace_seccomp_filter(&mut self, seccomp_filter: SeccompFilter) -> &mut Self {
        self.linux_builder.trace_seccomp_filter = Some(seccomp_filter);
        self
    }

    /// Gets whether the new process will vfork or not.
    #[cfg(target_os = "linux")]
    pub fn vfork(&mut self) -> bool {
        self.linux_builder.vfork
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
    #[cfg(all(target_os = "linux", feature = "sandbox_tests"))]
    fn test_user_namespace_self_map_with_vfork() {
        use super::Stdio;
        use std::io::BufRead;
        use std::io::BufReader;
        use std::os::fd::AsFd;
        use std::os::unix::net::UnixStream;

        let (output, child_output) = UnixStream::pair().unwrap();
        let mut cmd = Builder::new("/bin/sh");
        let sandbox = sandbox::SandboxProcessConfig {
            clone_flags: libc::CLONE_NEWUSER as u64,
            map_current_user: true,
            ..Default::default()
        };
        cmd.arg("-c")
            .arg("id -u; id -g; sleep 30")
            .stdout(Stdio::Fd(child_output.as_fd()))
            .apply_sandbox(sandbox)
            .unwrap();

        let mut child = cmd.spawn().unwrap();
        drop(child_output);

        let mut output = BufReader::new(output);
        let mut line = String::new();
        output.read_line(&mut line).unwrap();
        assert_eq!(line, "0\n");
        line.clear();
        output.read_line(&mut line).unwrap();
        assert_eq!(line, "0\n");

        // SAFETY: geteuid and getegid have no safety requirements.
        let (uid, gid) = unsafe { (libc::geteuid(), libc::getegid()) };
        assert_eq!(
            std::fs::read_to_string(format!("/proc/{}/uid_map", child.id())).unwrap(),
            format!("         0 {uid:10}          1\n")
        );
        assert_eq!(
            std::fs::read_to_string(format!("/proc/{}/gid_map", child.id())).unwrap(),
            format!("         0 {gid:10}          1\n")
        );

        // SAFETY: child.id() names the live process spawned above.
        assert_eq!(unsafe { libc::kill(child.id(), libc::SIGKILL) }, 0);
        child.wait().unwrap();
    }

    #[test]
    #[cfg(all(target_os = "linux", feature = "sandbox_tests"))]
    fn test_sandbox_ipc_namespace_isolation() {
        // A private fixture namespace ensures the kernel cleans up all IPC
        // objects even if a test assertion fails.
        let output = std::process::Command::new("unshare")
            .args(["--user", "--map-root-user", "--ipc"])
            .arg(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "unix::process::tests::helper_sandbox_ipc_parent",
                "--ignored",
                "--nocapture",
            ])
            .env_remove(sandbox::DISABLE_ENV)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "IPC namespace helper failed:\nstdout: {}\nstderr: {}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr),
        );
    }

    #[test]
    #[ignore]
    #[cfg(all(target_os = "linux", feature = "sandbox_tests"))]
    fn helper_sandbox_ipc_parent() {
        use sandbox::profiles::SandboxRole;

        let objects = sysv_ipc_objects(libc::IPC_CREAT | libc::IPC_EXCL | 0o600)
            .map(|result| result.unwrap());
        for role in [SandboxRole::Vm, SandboxRole::Tpm] {
            for vfork in [true, false] {
                let preparation =
                    sandbox::prepare(&role.profile(), &sandbox::Identity::default(), &[]).unwrap();
                let mut cmd = Builder::new(std::env::current_exe().unwrap());
                cmd.args([
                    "--exact",
                    "unix::process::tests::helper_sandbox_ipc_child",
                    "--ignored",
                    "--nocapture",
                ])
                .set_vfork(vfork)
                .apply_sandbox(preparation)
                .unwrap();
                assert!(
                    cmd.spawn().unwrap().wait().unwrap().success(),
                    "{} worker launch (vfork={vfork}) failed",
                    role.name(),
                );
                assert_eq!(
                    sysv_ipc_objects(0).map(|result| result.unwrap()),
                    objects,
                    "worker changed the parent's IPC objects",
                );
            }
        }
    }

    #[test]
    #[ignore]
    #[cfg(all(target_os = "linux", feature = "sandbox_tests"))]
    fn helper_sandbox_ipc_child() {
        for (name, result) in ["shared memory", "semaphore", "message queue"]
            .into_iter()
            .zip(sysv_ipc_objects(0))
        {
            let error = result.expect_err("parent IPC object is visible");
            assert_eq!(error.raw_os_error(), Some(libc::ENOENT), "{name}: {error}");
        }
        for result in sysv_ipc_objects(libc::IPC_CREAT | libc::IPC_EXCL | 0o600) {
            result.unwrap();
        }
    }

    #[cfg(all(target_os = "linux", feature = "sandbox_tests"))]
    fn sysv_ipc_objects(flags: libc::c_int) -> [std::io::Result<libc::c_int>; 3] {
        use crate::sys::SyscallResult;

        const KEY: libc::key_t = 0x5342;
        // SAFETY: these calls take only integer arguments. The fixed key is
        // used inside a private fixture or worker IPC namespace.
        unsafe {
            [
                libc::shmget(KEY, 4096, flags).syscall_result(),
                libc::semget(KEY, 1, flags).syscall_result(),
                libc::msgget(KEY, flags).syscall_result(),
            ]
        }
        .map(|result| result.map_err(Into::into))
    }

    #[test]
    #[cfg(target_os = "linux")]
    fn test_user_namespace_self_map_rejects_explicit_identity() {
        for (uid, gid) in [
            (Some(0), None),
            (None, Some(0)),
            (Some(1000), None),
            (None, Some(1000)),
            (Some(1000), Some(1000)),
        ] {
            let mut cmd = Builder::new("/usr/bin/true");
            if let Some(uid) = uid {
                cmd.setuid(uid);
            }
            if let Some(gid) = gid {
                cmd.setgid(gid);
            }
            cmd.apply_sandbox(sandbox::SandboxProcessConfig {
                clone_flags: libc::CLONE_NEWUSER as u64,
                map_current_user: true,
                ..Default::default()
            })
            .unwrap();
            let error = cmd.spawn().expect_err("explicit identity was accepted");
            assert_eq!(error.kind(), std::io::ErrorKind::Unsupported);
        }
    }

    #[test]
    #[cfg(target_os = "linux")]
    fn test_explicit_identity_without_user_namespace() {
        // SAFETY: geteuid and getegid have no safety requirements.
        let (uid, gid) = unsafe { (libc::geteuid(), libc::getegid()) };
        let mut cmd = Builder::new("/usr/bin/true");
        cmd.setuid(uid).setgid(gid);
        assert!(cmd.spawn().unwrap().wait().unwrap().success());
    }

    #[test]
    #[cfg(target_os = "linux")]
    fn test_inherited_fd_allowlist() {
        use std::io::Write;
        use std::os::fd::AsFd;
        use std::os::fd::AsRawFd;
        use std::os::fd::FromRawFd;
        use std::os::fd::OwnedFd;
        use std::os::fd::RawFd;
        use std::os::unix::net::UnixStream;

        const ALLOWED_FD: RawFd = 10;

        let (mut send, receive) = UnixStream::pair().unwrap();
        let null = std::fs::File::open("/dev/null").unwrap();
        // SAFETY: null is a valid descriptor and the returned descriptor is
        // uniquely owned.
        let leaked_fd = unsafe {
            let fd = libc::fcntl(null.as_fd().as_raw_fd(), libc::F_DUPFD, 100);
            assert!(fd >= 100);
            OwnedFd::from_raw_fd(fd)
        };

        let mut cmd = Builder::new(std::env::current_exe().unwrap());
        let sandbox = sandbox::SandboxProcessConfig {
            inherit_handles: vec![(sandbox::HandleTag(0), sandbox::RawHandle(ALLOWED_FD as u64))],
            ..Default::default()
        };
        cmd.args([
            "--exact",
            "unix::process::tests::helper_verify_inherited_fd_allowlist",
            "--ignored",
            "--nocapture",
        ])
        .env("PAL_ALLOWED_FD", ALLOWED_FD.to_string())
        .env("PAL_LEAKED_FD", leaked_fd.as_raw_fd().to_string())
        .dup_fd(receive.as_fd(), ALLOWED_FD)
        .apply_sandbox(sandbox)
        .unwrap();

        let mut child = cmd.spawn().unwrap();
        drop(receive);
        send.write_all(b"x").unwrap();
        assert!(child.wait().unwrap().success());
        // SAFETY: leaked_fd remains owned by this process.
        assert!(unsafe { libc::fcntl(leaked_fd.as_raw_fd(), libc::F_GETFD) } >= 0);
    }

    #[test]
    #[ignore]
    #[cfg(target_os = "linux")]
    fn helper_verify_inherited_fd_allowlist() {
        use std::io::Read;
        use std::os::fd::FromRawFd;
        use std::os::fd::OwnedFd;
        use std::os::fd::RawFd;

        let allowed_fd: RawFd = std::env::var("PAL_ALLOWED_FD").unwrap().parse().unwrap();
        let leaked_fd: RawFd = std::env::var("PAL_LEAKED_FD").unwrap().parse().unwrap();

        // SAFETY: the parent intentionally transferred ownership of this
        // descriptor to the child at the configured target number.
        let allowed = unsafe { OwnedFd::from_raw_fd(allowed_fd) };
        let mut allowed = std::fs::File::from(allowed);
        let mut byte = [0];
        allowed.read_exact(&mut byte).unwrap();
        assert_eq!(byte, [b'x']);

        // SAFETY: F_GETFD only inspects the numeric descriptor.
        let result = unsafe { libc::fcntl(leaked_fd, libc::F_GETFD) };
        assert_eq!(result, -1);
        assert_eq!(
            std::io::Error::last_os_error().raw_os_error(),
            Some(libc::EBADF)
        );
    }
}
