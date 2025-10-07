// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Provides a shell abstraction to interact with the guest, similar to
//! `xshell::Shell`.

// This is a bit of a hack, since `__cmd` is an internal detail of xshell. But
// it does exactly what we need, so let's use it for now. If the internal
// details change, we can fork it.
#[doc(hidden)]
pub use xshell_macros::__cmd;

use crate::PipetteClient;
use crate::process::Command;
use crate::process::Output;
use crate::process::Stdio;
use anyhow::Context;
use futures::AsyncWriteExt;
use futures_concurrency::future::Join;
use std::collections::HashMap;
use typed_path::Utf8Encoding;
use typed_path::Utf8Path;
use typed_path::Utf8PathBuf;
use typed_path::Utf8UnixEncoding;
use typed_path::Utf8WindowsEncoding;

/// A stateful shell abstraction for interacting with the guest.
///
/// This is modeled after `xshell::Shell`.
pub struct Shell<'a, T: Utf8Encoding> {
    client: &'a PipetteClient,
    cwd: Utf8PathBuf<T>,
    env: HashMap<String, String>,
}

/// A shell for a Windows guest.
pub type WindowsShell<'a> = Shell<'a, Utf8WindowsEncoding>;

/// A shell for a Linux guest.
pub type UnixShell<'a> = Shell<'a, Utf8UnixEncoding>;

impl<'a> UnixShell<'a> {
    pub(crate) fn new(client: &'a PipetteClient) -> Self {
        Self {
            client,
            cwd: Utf8PathBuf::from("/"),
            env: HashMap::new(),
        }
    }
}

impl<'a> WindowsShell<'a> {
    pub(crate) fn new(client: &'a PipetteClient) -> Self {
        Self {
            client,
            cwd: Utf8PathBuf::from("C:/"),
            env: HashMap::new(),
        }
    }
}

impl<T> Shell<'_, T>
where
    T: Utf8Encoding,
{
    fn path(&self, path: impl AsRef<Utf8Path<T>>) -> Utf8PathBuf<T> {
        self.cwd.join(path)
    }

    /// Change the effective working directory of the shell.
    ///
    /// Other paths will be resolved relative to this directory.
    pub fn change_dir(&mut self, path: impl AsRef<Utf8Path<T>>) {
        self.cwd = self.path(path);
    }

    /// Reads a file from the guest into a string.
    pub async fn read_file(&self, path: impl AsRef<Utf8Path<T>>) -> anyhow::Result<String> {
        let path = self.path(path);
        let v = self.client.read_file(path.as_str()).await?;
        String::from_utf8(v).with_context(|| format!("file '{}' is not valid utf-8", path.as_str()))
    }

    /// Creates a builder to execute a command inside the guest.
    ///
    /// Consider using the [`cmd!`](crate::cmd!) macro.
    pub fn cmd(&self, program: impl AsRef<Utf8Path<T>>) -> Cmd<'_, T> {
        Cmd {
            shell: self,
            prog: program.as_ref().to_owned(),
            args: Vec::new(),
            env_changes: Vec::new(),
            ignore_status: false,
            stdin_contents: Vec::new(),
            ignore_stdout: false,
            ignore_stderr: false,
        }
    }
}

/// A command builder.
pub struct Cmd<'a, T: Utf8Encoding> {
    shell: &'a Shell<'a, T>,
    prog: Utf8PathBuf<T>,
    args: Vec<String>,
    env_changes: Vec<EnvChange>,
    ignore_status: bool,
    stdin_contents: Vec<u8>,
    ignore_stdout: bool,
    ignore_stderr: bool,
}

enum EnvChange {
    Set(String, String),
    Remove(String),
    Clear,
}

impl<'a, T: Utf8Encoding> Cmd<'a, T> {
    /// Adds an argument to the command.
    pub fn arg<P: AsRef<str>>(mut self, arg: P) -> Self {
        self.args.push(arg.as_ref().to_owned());
        self
    }

    /// Adds multiple arguments to the command.
    pub fn args<I>(mut self, args: I) -> Self
    where
        I: IntoIterator,
        I::Item: AsRef<str>,
    {
        for it in args.into_iter() {
            self = self.arg(it.as_ref());
        }
        self
    }

    // Used by xshell_macros::__cmd
    #[doc(hidden)]
    pub fn __extend_arg(mut self, arg_fragment: impl AsRef<str>) -> Self {
        match self.args.last_mut() {
            Some(last_arg) => last_arg.push_str(arg_fragment.as_ref()),
            None => {
                let mut prog = std::mem::take(&mut self.prog).into_string();
                prog.push_str(arg_fragment.as_ref());
                self.prog = prog.into();
            }
        }
        self
    }

    /// Sets an environment variable for the command.
    pub fn env(mut self, key: impl AsRef<str>, val: impl AsRef<str>) -> Self {
        self.env_changes.push(EnvChange::Set(
            key.as_ref().to_owned(),
            val.as_ref().to_owned(),
        ));
        self
    }

    /// Sets multiple environment variables for the command.
    pub fn envs<I, K, V>(mut self, vars: I) -> Self
    where
        I: IntoIterator<Item = (K, V)>,
        K: AsRef<str>,
        V: AsRef<str>,
    {
        for (k, v) in vars.into_iter() {
            self = self.env(k.as_ref(), v.as_ref());
        }
        self
    }

    /// Removes an environment variable for the command.
    pub fn env_remove(mut self, key: impl AsRef<str>) -> Self {
        self.env_changes
            .push(EnvChange::Remove(key.as_ref().to_owned()));
        self
    }

    /// Clears the environment for the command.
    pub fn env_clear(mut self) -> Self {
        self.env_changes.push(EnvChange::Clear);
        self
    }

    /// Ignores the status of the command.
    ///
    /// By default, the command will fail if the exit code is non-zero.
    pub fn ignore_status(mut self) -> Self {
        self.ignore_status = true;
        self
    }

    /// Ignores the stdout of the command.
    ///
    /// By default, the command's stdout will be captured or printed to stdout.
    pub fn ignore_stdout(mut self) -> Self {
        self.ignore_stdout = true;
        self
    }

    /// Ignores the stderr of the command.
    ///
    /// By default, the command's stderr will be captured or printed to stderr.
    pub fn ignore_stderr(mut self) -> Self {
        self.ignore_stderr = true;
        self
    }

    /// Sets contents to be written to the command's stdin.
    pub fn stdin(mut self, stdin: impl AsRef<[u8]>) -> Self {
        self.stdin_contents = stdin.as_ref().to_vec();
        self
    }

    /// Runs the command and waits for it to complete.
    ///
    /// By default, this will fail if the command's exit code is non-zero.
    ///
    /// By default, the command's stdout and stderr will be captured and traced.
    pub async fn run(&self) -> anyhow::Result<()> {
        self.read_output().await?;
        Ok(())
    }

    /// Runs the command and waits for it to complete, returning the stdout.
    ///
    /// By default, this will fail if the command's exit code is non-zero.
    ///
    /// By default, the command's stderr will be captured and traced.
    pub async fn read(&self) -> anyhow::Result<String> {
        self.read_stream(false).await
    }

    /// Runs the command and waits for it to complete, returning the stderr.
    ///
    /// By default, this will fail if the command's exit code is non-zero.
    ///
    /// By default, the command's stdout will be captured and traced.
    pub async fn read_stderr(&self) -> anyhow::Result<String> {
        self.read_stream(true).await
    }

    /// Runs the command and waits for it to complete, returning the stdout and
    /// stderr.
    ///
    /// By default, this will fail if the command's exit code is non-zero.
    pub async fn output(&self) -> anyhow::Result<Output> {
        self.read_output().await
    }

    fn command(&self) -> Command<'a> {
        let mut command = self.shell.client.command(&self.prog);
        command.args(&self.args);
        command.current_dir(&self.shell.cwd);
        for (name, value) in &self.shell.env {
            command.env(name, value);
        }
        for change in &self.env_changes {
            match change {
                EnvChange::Set(name, value) => {
                    command.env(name, value);
                }
                EnvChange::Remove(name) => {
                    command.env_remove(name);
                }
                EnvChange::Clear => {
                    command.env_clear();
                }
            }
        }
        if self.ignore_stdout {
            command.stdout(Stdio::null());
        }
        if self.ignore_stderr {
            command.stderr(Stdio::null());
        }
        command
    }

    async fn read_stream(&self, read_stderr: bool) -> anyhow::Result<String> {
        let output = self.read_output().await?;
        let stream = if read_stderr {
            output.stderr
        } else {
            output.stdout
        };
        let mut stream = String::from_utf8(stream).context("stream is not utf-8")?;
        if stream.ends_with('\n') {
            stream.pop();
        }
        if stream.ends_with('\r') {
            stream.pop();
        }
        Ok(stream)
    }

    async fn read_output(&self) -> anyhow::Result<Output> {
        let mut command = self.command();
        if !self.ignore_stdout {
            command.stdout(Stdio::piped());
        }
        if !self.ignore_stderr {
            command.stderr(Stdio::piped());
        }
        if !self.stdin_contents.is_empty() {
            command.stdin(Stdio::piped());
        }
        let mut child = command.spawn().await.context("failed to spawn child")?;

        // put in task
        let stdin = child.stdin.take();
        let copy_stdin = async move {
            if let Some(mut stdin) = stdin {
                stdin.write_all(&self.stdin_contents).await?;
            }
            anyhow::Ok(())
        };

        let wait = child.wait_with_output();

        let (copy_r, wait_r) = (copy_stdin, wait).join().await;
        let output = wait_r.context("failed to wait for child")?;
        copy_r.context("failed to write stdin")?;

        let out = String::from_utf8_lossy(&output.stdout);
        tracing::info!(?out, "command stdout");

        let err = String::from_utf8_lossy(&output.stderr);
        tracing::info!(?err, "command stderr");

        if !self.ignore_status && !output.status.success() {
            anyhow::bail!("command failed: {}", output.status);
        }

        Ok(output)
    }
}

/// Constructs a [`Cmd`] from the given string, with interpolation.
///
/// # Example
///
/// ```no_run
/// # use pipette_client::{cmd, shell::UnixShell};
/// async fn example(sh: &mut UnixShell<'_>) {
///     let args = ["hello", "world"];
///     assert_eq!(cmd!(sh, "echo {args...}").read().await.unwrap(), "hello world");
/// }
#[macro_export]
macro_rules! cmd {
    ($sh:expr, $cmd:literal) => {{
        let f = |prog| $sh.cmd(prog);
        let cmd: $crate::shell::Cmd<'_, _> = $crate::shell::__cmd!(f $cmd);
        cmd
    }};
}
