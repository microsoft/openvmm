// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Code to launch a command inside the guest.

use crate::PipetteClient;
use anyhow::Context;
use futures::executor::block_on;
use futures::io::AllowStdIo;
use futures::AsyncReadExt;
use futures_concurrency::future::Join;
use mesh::error::RemoteResultExt;
use mesh::pipe::ReadPipe;
use mesh::pipe::WritePipe;
use pipette_protocol::EnvPair;
use pipette_protocol::PipetteRequest;
use std::fmt;

/// A builder for launching a command inside the guest.
///
/// This has a similar API to [`std::process::Command`].
pub struct Command<'a> {
    client: &'a PipetteClient,
    program: String,
    args: Vec<String>,
    current_dir: Option<String>,
    stdin: Option<Stdio>,
    stdout: Option<Stdio>,
    stderr: Option<Stdio>,
    env: Vec<EnvPair>,
    clear_env: bool,
}

impl<'a> Command<'a> {
    pub(super) fn new(client: &'a PipetteClient, program: impl AsRef<str>) -> Self {
        Self {
            client,
            program: program.as_ref().to_owned(),
            args: Vec::new(),
            current_dir: None,
            stdin: None,
            stdout: None,
            stderr: None,
            env: Vec::new(),
            clear_env: false,
        }
    }

    /// Adds an argument to the command.
    pub fn arg(&mut self, arg: impl AsRef<str>) -> &mut Self {
        self.args.push(arg.as_ref().to_owned());
        self
    }

    /// Adds multiple arguments to the command.
    pub fn args<I: IntoIterator>(&mut self, args: I) -> &mut Self
    where
        I::Item: AsRef<str>,
    {
        self.args
            .extend(args.into_iter().map(|item| item.as_ref().to_owned()));
        self
    }

    /// Sets the current working directory for the command.
    pub fn current_dir(&mut self, dir: impl AsRef<str>) -> &mut Self {
        self.current_dir = Some(dir.as_ref().to_owned());
        self
    }

    /// Clears the environment before setting the new environment.
    pub fn env_clear(&mut self) -> &mut Self {
        self.clear_env = true;
        self.env.clear();
        self
    }

    /// Sets an environment variable for the command.
    pub fn env(&mut self, name: impl AsRef<str>, value: impl AsRef<str>) -> &mut Self {
        self.env.push(EnvPair {
            name: name.as_ref().to_owned(),
            value: Some(value.as_ref().to_owned()),
        });
        self
    }

    /// Removes an environment variable for the command.
    pub fn env_remove(&mut self, name: impl AsRef<str>) -> &mut Self {
        self.env.push(EnvPair {
            name: name.as_ref().to_owned(),
            value: None,
        });
        self
    }

    /// Sets the stdin for the command.
    pub fn stdin(&mut self, stdin: impl Into<Stdio>) -> &mut Self {
        self.stdin = Some(stdin.into());
        self
    }

    /// Sets the stdout for the command.
    pub fn stdout(&mut self, stdout: impl Into<Stdio>) -> &mut Self {
        self.stdout = Some(stdout.into());
        self
    }

    /// Sets the stderr for the command.
    pub fn stderr(&mut self, stderr: impl Into<Stdio>) -> &mut Self {
        self.stderr = Some(stderr.into());
        self
    }

    /// Spawns the command, defaulting to inheriting (relaying, really) the
    /// current process for stdin, stdout, and stderr.
    pub async fn spawn(&self) -> anyhow::Result<Child> {
        self.spawn_inner(&StdioInner::Inherit, true).await
    }

    /// Spawns the command, capturing the standard output and standard error
    /// (if they are not already set).
    pub async fn output(&self) -> anyhow::Result<Output> {
        let child = self.spawn_inner(&StdioInner::Piped, false).await?;
        child.wait_with_output().await
    }

    async fn spawn_inner(
        &self,
        default_stdio: &StdioInner,
        default_stdin: bool,
    ) -> anyhow::Result<Child> {
        let (stdin_read, stdin_write) = self
            .stdin
            .as_ref()
            .map_or(
                if default_stdin {
                    default_stdio
                } else {
                    &StdioInner::Null
                },
                |x| &x.0,
            )
            .pipes(StdioFd::Stdin);

        let (stdout_read, stdout_write) = self
            .stdout
            .as_ref()
            .map_or(default_stdio, |x| &x.0)
            .pipes(StdioFd::Stdout);
        let (stderr_read, stderr_write) = self
            .stderr
            .as_ref()
            .map_or(default_stdio, |x| &x.0)
            .pipes(StdioFd::Stderr);

        let request = pipette_protocol::ExecuteRequest {
            program: self.program.clone(),
            args: self.args.clone(),
            current_dir: self.current_dir.clone(),
            stdin: stdin_read,
            stdout: stdout_write,
            stderr: stderr_write,
            env: self.env.clone(),
            clear_env: self.clear_env,
        };

        let response = self
            .client
            .send
            .call(PipetteRequest::Execute, request)
            .await
            .flatten()
            .with_context(|| format!("failed to execute {}", self.program))?;

        Ok(Child {
            stdin: stdin_write,
            stdout: stdout_read,
            stderr: stderr_read,
            pid: response.pid,
            result: Ok(response.result),
        })
    }
}

/// Describes what to do with a standard I/O stream for a child process.
pub struct Stdio(StdioInner);

enum StdioInner {
    Inherit,
    Null,
    Piped,
}

impl Stdio {
    /// This stream will be "inherited" from the parent process.
    ///
    /// Internally, this will relay the standard input, output, or error of the
    /// current process to the guest process.
    pub fn inherit() -> Self {
        Self(StdioInner::Inherit)
    }

    /// This stream will be ignored by the child process.
    pub fn null() -> Self {
        Self(StdioInner::Null)
    }

    /// A new pipe will be created to communicate with the child process.
    pub fn piped() -> Self {
        Self(StdioInner::Piped)
    }
}

enum StdioFd {
    Stdin,
    Stdout,
    Stderr,
}

impl StdioInner {
    fn pipes(&self, fd: StdioFd) -> (Option<ReadPipe>, Option<WritePipe>) {
        match self {
            StdioInner::Null => (None, None),
            StdioInner::Piped => {
                let (read, write) = mesh::pipe::pipe();
                (Some(read), Some(write))
            }
            StdioInner::Inherit => {
                let (read, mut write) = mesh::pipe::pipe();
                match fd {
                    StdioFd::Stdin => {
                        std::thread::Builder::new()
                            .name("stdin-relay".to_owned())
                            .spawn({
                                move || {
                                    block_on(futures::io::copy(
                                        AllowStdIo::new(std::io::stdin()),
                                        &mut write,
                                    ))
                                }
                            })
                            .unwrap();
                        (Some(read), None)
                    }
                    StdioFd::Stdout => {
                        std::thread::Builder::new()
                            .name("stdout-relay".to_owned())
                            .spawn({
                                move || {
                                    block_on(futures::io::copy(
                                        read,
                                        &mut AllowStdIo::new(std::io::stdout()),
                                    ))
                                }
                            })
                            .unwrap();
                        (None, Some(write))
                    }
                    StdioFd::Stderr => {
                        std::thread::Builder::new()
                            .name("stderr-relay".to_owned())
                            .spawn({
                                move || {
                                    block_on(futures::io::copy(
                                        read,
                                        &mut AllowStdIo::new(std::io::stderr()),
                                    ))
                                }
                            })
                            .unwrap();
                        (None, Some(write))
                    }
                }
            }
        }
    }
}

/// A spawned child process, similar to [`std::process::Child`].
pub struct Child {
    /// The standard input pipe of the process.
    pub stdin: Option<WritePipe>,
    /// The standard output pipe of the process.
    pub stdout: Option<ReadPipe>,
    /// The standard error pipe of the process.
    pub stderr: Option<ReadPipe>,
    pid: u32,
    result: Result<mesh::OneshotReceiver<pipette_protocol::ExitStatus>, ExitStatus>,
}

impl Child {
    /// Returns the process ID of the child within the guest.
    pub fn id(&self) -> u32 {
        self.pid
    }

    /// Waits for the child to exit, returning the exit status.
    pub async fn wait(&mut self) -> Result<ExitStatus, mesh::RecvError> {
        match &mut self.result {
            Ok(recv) => {
                let status = ExitStatus(recv.await?);
                self.result = Err(status.clone());
                Ok(status)
            }
            Err(status) => Ok(status.clone()),
        }
    }

    /// Waits for the child to exit, returning the exit status and the
    /// remaining data from standard output and standard error.
    pub async fn wait_with_output(mut self) -> anyhow::Result<Output> {
        self.stdin = None;
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        let stdout_pipe = self.stdout.take();
        let stderr_pipe = self.stderr.take();
        let stdout_task = async {
            if let Some(mut pipe) = stdout_pipe {
                let _ = pipe.read_to_end(&mut stdout).await;
            }
        };
        let stderr_task = async {
            if let Some(mut pipe) = stderr_pipe {
                let _ = pipe.read_to_end(&mut stderr).await;
            }
        };
        let wait_task = self.wait();
        let (status, (), ()) = (wait_task, stdout_task, stderr_task).join().await;
        let status = status?;
        Ok(Output {
            status,
            stdout,
            stderr,
        })
    }
}

/// The exit status of a process.
#[derive(Debug, Clone)]
pub struct ExitStatus(pipette_protocol::ExitStatus);

impl ExitStatus {
    /// Returns `true` if the process exited successfully.
    pub fn success(&self) -> bool {
        matches!(self.0, pipette_protocol::ExitStatus::Normal(0))
    }

    /// Returns the exit code of the process, if it exited normally.
    pub fn code(&self) -> Option<i32> {
        match self.0 {
            pipette_protocol::ExitStatus::Normal(code) => Some(code),
            pipette_protocol::ExitStatus::Signal(_) | pipette_protocol::ExitStatus::Unknown => None,
        }
    }

    /// Returns the signal that terminated the process, if it was terminated
    /// by a signal.
    pub fn signal(&self) -> Option<i32> {
        match self.0 {
            pipette_protocol::ExitStatus::Signal(signal) => Some(signal),
            pipette_protocol::ExitStatus::Normal(_) | pipette_protocol::ExitStatus::Unknown => None,
        }
    }
}

impl fmt::Display for ExitStatus {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.0 {
            pipette_protocol::ExitStatus::Normal(code) if code >= 0 => {
                write!(f, "exit code {}", code)
            }
            pipette_protocol::ExitStatus::Normal(code) => write!(f, "exit code {:#x}", code as u32),
            pipette_protocol::ExitStatus::Signal(signal) => {
                write!(f, "terminated by signal {}", signal)
            }
            pipette_protocol::ExitStatus::Unknown => write!(f, "unknown exit status"),
        }
    }
}

/// The result of a process execution.
pub struct Output {
    /// The exit status of the process.
    pub status: ExitStatus,
    /// The standard output of the process.
    pub stdout: Vec<u8>,
    /// The standard error of the process.
    pub stderr: Vec<u8>,
}
