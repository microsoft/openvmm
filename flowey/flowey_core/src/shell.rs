// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Shell abstraction for flowey command execution.
//!
//! Provides [`FloweyShell`] and [`FloweyCmd`] as thin wrappers around
//! [`xshell::Shell`] and [`xshell::Cmd`] that enable command
//! wrapping (e.g., running commands inside `nix-shell --pure --run`).

use std::ffi::OsStr;
use std::ffi::OsString;
use std::ops::Deref;
use std::process::Output;

use serde::Deserialize;
use serde::Serialize;

/// A wrapper around [`xshell::Shell`] that supports transparent command
/// wrapping via an optional [`CommandWrapperKind`].
///
/// Implements [`Deref<Target = xshell::Shell>`] so that existing usages like
/// `rt.sh.change_dir()` and `rt.sh.set_var()` continue to work unchanged.
pub struct FloweyShell {
    inner: xshell::Shell,
    wrapper: Option<CommandWrapperKind>,
}

impl FloweyShell {
    /// Create a new `FloweyShell` with no command wrapper.
    #[allow(clippy::disallowed_methods)]
    pub fn new() -> anyhow::Result<Self> {
        Ok(Self {
            inner: xshell::Shell::new()?,
            wrapper: None,
        })
    }

    /// Set (or clear) the command wrapper used for all commands created
    /// through this shell.
    pub fn set_wrapper(&mut self, wrapper: Option<CommandWrapperKind>) {
        self.wrapper = wrapper;
    }

    /// Access the underlying [`xshell::Shell`].
    ///
    /// This is primarily used by the [`shell_cmd!`](crate::shell_cmd)
    /// macro to pass the shell reference into [`xshell::cmd!`].
    pub fn xshell(&self) -> &xshell::Shell {
        &self.inner
    }

    /// Wrap an [`xshell::Cmd`] into a [`FloweyCmd`] that will apply
    /// this shell's [`CommandWrapperKind`] (if any) at execution time.
    pub fn wrap<'a>(&'a self, cmd: xshell::Cmd<'a>) -> FloweyCmd<'a> {
        FloweyCmd {
            inner: cmd,
            env_changes: Vec::new(),
            stdin_contents: None,
            ignore_status: false,
            quiet: false,
            secret: false,
            ignore_stdout: false,
            ignore_stderr: false,
            wrapper: self.wrapper.clone(),
            sh: &self.inner,
        }
    }
}

impl Deref for FloweyShell {
    type Target = xshell::Shell;

    fn deref(&self) -> &xshell::Shell {
        &self.inner
    }
}

/// Environment variable changes tracked by [`FloweyCmd`].
enum EnvChange {
    Set(OsString, OsString),
    Remove(OsString),
    Clear,
}

/// A wrapper around [`xshell::Cmd`] that applies a [`CommandWrapperKind`]
/// at execution time.
///
/// Builder methods (`.arg()`, `.env()`, etc.) are accumulated on the
/// inner [`xshell::Cmd`] (for args) or in shadow fields (for env, stdin,
/// and flags). Execution methods (`.run()`, `.read()`, etc.) consume
/// `self`, apply the wrapper transformation, re-apply the shadowed state,
/// and then execute.
pub struct FloweyCmd<'a> {
    /// The inner command accumulates program + arguments only.
    inner: xshell::Cmd<'a>,
    // Shadow fields for state that must survive wrapping.
    env_changes: Vec<EnvChange>,
    stdin_contents: Option<Vec<u8>>,
    ignore_status: bool,
    quiet: bool,
    secret: bool,
    ignore_stdout: bool,
    ignore_stderr: bool,
    wrapper: Option<CommandWrapperKind>,
    sh: &'a xshell::Shell,
}

// Mirrors xshell::Cmd's builder methods, but xshell doesn't export a common trait to implement
impl<'a> FloweyCmd<'a> {
    /// Adds an argument to this command.
    pub fn arg<P: AsRef<OsStr>>(mut self, arg: P) -> Self {
        self.inner = self.inner.arg(arg);
        self
    }

    /// Adds all of the arguments to this command.
    pub fn args<I>(mut self, args: I) -> Self
    where
        I: IntoIterator,
        I::Item: AsRef<OsStr>,
    {
        self.inner = self.inner.args(args);
        self
    }

    /// Overrides the value of an environmental variable for this command.
    pub fn env<K: AsRef<OsStr>, V: AsRef<OsStr>>(mut self, key: K, val: V) -> Self {
        self.env_changes.push(EnvChange::Set(
            key.as_ref().to_owned(),
            val.as_ref().to_owned(),
        ));
        self
    }

    /// Overrides the values of specified environmental variables for this
    /// command.
    pub fn envs<I, K, V>(mut self, vars: I) -> Self
    where
        I: IntoIterator<Item = (K, V)>,
        K: AsRef<OsStr>,
        V: AsRef<OsStr>,
    {
        for (k, v) in vars {
            self.env_changes
                .push(EnvChange::Set(k.as_ref().to_owned(), v.as_ref().to_owned()));
        }
        self
    }

    /// Removes an environment variable from this command.
    pub fn env_remove<K: AsRef<OsStr>>(mut self, key: K) -> Self {
        self.env_changes
            .push(EnvChange::Remove(key.as_ref().to_owned()));
        self
    }

    /// Removes all environment variables from this command.
    pub fn env_clear(mut self) -> Self {
        self.env_changes.push(EnvChange::Clear);
        self
    }

    /// If set, the command's status code will not be checked, and
    /// non-zero exit codes will not produce an error.
    pub fn ignore_status(mut self) -> Self {
        self.ignore_status = true;
        self
    }

    /// Mutating variant of [`ignore_status`](Self::ignore_status).
    pub fn set_ignore_status(&mut self, yes: bool) {
        self.ignore_status = yes;
    }

    /// If set, the command's output will not be echoed to stdout.
    pub fn quiet(mut self) -> Self {
        self.quiet = true;
        self
    }

    /// Mutating variant of [`quiet`](Self::quiet).
    pub fn set_quiet(&mut self, yes: bool) {
        self.quiet = yes;
    }

    /// If set, the command is treated as containing a secret and its
    /// display will be redacted.
    pub fn secret(mut self) -> Self {
        self.secret = true;
        self
    }

    /// Mutating variant of [`secret`](Self::secret).
    pub fn set_secret(&mut self, yes: bool) {
        self.secret = yes;
    }

    /// Passes data to the command's stdin.
    pub fn stdin(mut self, stdin: impl AsRef<[u8]>) -> Self {
        self.stdin_contents = Some(stdin.as_ref().to_vec());
        self
    }

    /// If set, stdout is not captured.
    pub fn ignore_stdout(mut self) -> Self {
        self.ignore_stdout = true;
        self
    }

    /// Mutating variant of [`ignore_stdout`](Self::ignore_stdout).
    pub fn set_ignore_stdout(&mut self, yes: bool) {
        self.ignore_stdout = yes;
    }

    /// If set, stderr is not captured.
    pub fn ignore_stderr(mut self) -> Self {
        self.ignore_stderr = true;
        self
    }

    /// Mutating variant of [`ignore_stderr`](Self::ignore_stderr).
    pub fn set_ignore_stderr(&mut self, yes: bool) {
        self.ignore_stderr = yes;
    }

    /// Consume this command, apply the wrapper (if any), re-apply
    /// shadowed state (env, stdin, flags), and return the final
    /// [`xshell::Cmd`] ready for execution.
    fn into_resolved(self) -> xshell::Cmd<'a> {
        let mut cmd = match self.wrapper {
            Some(wrapper) => wrapper.wrap_cmd(self.sh, self.inner),
            None => self.inner,
        };

        // Re-apply env changes after wrapping to survive the wrapper's transformation
        for change in self.env_changes {
            match change {
                EnvChange::Set(k, v) => cmd = cmd.env(k, v),
                EnvChange::Remove(k) => cmd = cmd.env_remove(k),
                EnvChange::Clear => cmd = cmd.env_clear(),
            }
        }
        if let Some(stdin) = self.stdin_contents {
            cmd = cmd.stdin(stdin);
        }
        cmd.set_ignore_status(self.ignore_status);
        cmd.set_quiet(self.quiet);
        cmd.set_secret(self.secret);
        cmd.set_ignore_stdout(self.ignore_stdout);
        cmd.set_ignore_stderr(self.ignore_stderr);

        cmd
    }

    /// Run the command.
    pub fn run(self) -> xshell::Result<()> {
        self.into_resolved().run()
    }

    /// Run the command and return its stdout as a string, with leading
    /// and trailing whitespace trimmed.
    pub fn read(self) -> xshell::Result<String> {
        self.into_resolved().read()
    }

    /// Run the command and return its stderr as a string, with leading
    /// and trailing whitespace trimmed.
    pub fn read_stderr(self) -> xshell::Result<String> {
        self.into_resolved().read_stderr()
    }

    /// Run the command and return its full output.
    pub fn output(self) -> xshell::Result<Output> {
        self.into_resolved().output()
    }
}

impl std::fmt::Display for FloweyCmd<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if self.secret {
            return f.write_str("<secret>");
        }
        // Show the unwrapped command for user-facing logging.
        std::fmt::Display::fmt(&self.inner, f)
    }
}

/// Serializable description of a command wrapper.
///
/// This enum can be stored in `pipeline.json` so that CI backends can
/// reconstruct the appropriate wrapper at runtime. It is also used
/// directly by [`FloweyShell`] and [`FloweyCmd`] to transform commands.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum CommandWrapperKind {
    /// Wrap commands with `nix-shell --pure --run "..."`.
    NixShell {
        /// Optional path to a `shell.nix` file. If `None`, nix-shell
        /// uses its default discovery (looking for `shell.nix` /
        /// `default.nix` in the current directory).
        path: Option<std::path::PathBuf>,
    },
    /// Wrap commands with `sh -c "..."` (test-only).
    #[cfg(test)]
    ShCmd,
    /// Replace the command with `echo WRAPPED: <cmd>` (test-only).
    #[cfg(test)]
    Prefix,
}

impl CommandWrapperKind {
    /// Transform a command before execution.
    ///
    /// The `cmd` parameter contains only the program and arguments.
    /// Environment variables, stdin, and flags are applied by
    /// [`FloweyCmd`] after this method returns.
    fn wrap_cmd<'a>(self, sh: &'a xshell::Shell, cmd: xshell::Cmd<'a>) -> xshell::Cmd<'a> {
        let cmd_str = format!("{cmd}");
        match self {
            CommandWrapperKind::NixShell { path } => {
                let mut wrapped = sh.cmd("nix-shell");
                if let Some(path) = path {
                    wrapped = wrapped.arg(path);
                }
                wrapped.arg("--pure").arg("--run").arg(cmd_str)
            }
            #[cfg(test)]
            CommandWrapperKind::ShCmd => sh.cmd("sh").arg("-c").arg(cmd_str),
            #[cfg(test)]
            CommandWrapperKind::Prefix => sh.cmd("echo").arg(format!("WRAPPED: {cmd_str}")),
        }
    }
}

#[cfg(test)]
#[allow(
    clippy::disallowed_macros,
    clippy::disallowed_methods,
    reason = "test module"
)]
mod tests {
    use super::*;

    #[test]
    fn no_wrapper_runs_command_directly() {
        let sh = FloweyShell::new().unwrap();
        let cmd = sh.wrap(xshell::cmd!(sh.xshell(), "echo hello"));
        let output = cmd.read().unwrap();
        assert_eq!(output, "hello");
    }

    #[test]
    fn wrapper_transforms_command() {
        let mut sh = FloweyShell::new().unwrap();
        sh.set_wrapper(Some(CommandWrapperKind::Prefix));
        let cmd = sh.wrap(xshell::cmd!(sh.xshell(), "my-program --flag"));
        let output = cmd.read().unwrap();
        assert_eq!(output, "WRAPPED: my-program --flag");
    }

    #[test]
    fn env_vars_survive_with_wrapper() {
        let mut sh = FloweyShell::new().unwrap();
        sh.set_wrapper(Some(CommandWrapperKind::ShCmd));

        let cmd = sh
            .wrap(xshell::cmd!(sh.xshell(), "printenv MY_FLOWEY_WRAP_TEST"))
            .env("MY_FLOWEY_WRAP_TEST", "survived_wrapping");
        let output = cmd.read().unwrap();
        assert_eq!(output, "survived_wrapping");
    }

    #[test]
    fn stdin_survives_wrapping() {
        let sh = FloweyShell::new().unwrap();
        let cmd = sh
            .wrap(xshell::cmd!(sh.xshell(), "cat"))
            .stdin("test input");
        let output = cmd.read().unwrap();
        assert_eq!(output, "test input");
    }

    #[test]
    fn stdin_survives_with_wrapper() {
        let mut sh = FloweyShell::new().unwrap();
        sh.set_wrapper(Some(CommandWrapperKind::ShCmd));

        let cmd = sh
            .wrap(xshell::cmd!(sh.xshell(), "cat"))
            .stdin("wrapped stdin test");
        let output = cmd.read().unwrap();
        assert_eq!(output, "wrapped stdin test");
    }

    #[test]
    fn ignore_status_survives_wrapping() {
        let mut sh = FloweyShell::new().unwrap();
        sh.set_wrapper(Some(CommandWrapperKind::ShCmd));

        // `false` exits with status 1 — without ignore_status this would error.
        let cmd = sh.wrap(xshell::cmd!(sh.xshell(), "false")).ignore_status();
        assert!(cmd.run().is_ok());
    }

    #[test]
    fn display_shows_unwrapped_command() {
        let mut sh = FloweyShell::new().unwrap();
        sh.set_wrapper(Some(CommandWrapperKind::Prefix));
        let cmd = sh.wrap(xshell::cmd!(sh.xshell(), "my-program --flag value"));
        assert_eq!(format!("{cmd}"), "my-program --flag value");
    }

    #[test]
    fn nix_wrapper_display_without_path() {
        let sh = FloweyShell::new().unwrap();
        let cmd = CommandWrapperKind::NixShell { path: None }.wrap_cmd(
            sh.xshell(),
            xshell::cmd!(sh.xshell(), "cargo build --release"),
        );
        assert_eq!(
            format!("{cmd}"),
            "nix-shell --pure --run \"cargo build --release\""
        );
    }

    #[test]
    fn nix_wrapper_display_with_path() {
        let sh = FloweyShell::new().unwrap();
        let cmd = CommandWrapperKind::NixShell {
            path: Some("/my/shell.nix".into()),
        }
        .wrap_cmd(sh.xshell(), xshell::cmd!(sh.xshell(), "cargo build"));
        assert_eq!(
            format!("{cmd}"),
            "nix-shell /my/shell.nix --pure --run \"cargo build\""
        );
    }

    #[test]
    fn deref_exposes_shell_methods() {
        let sh = FloweyShell::new().unwrap();
        let _ = sh.current_dir();
    }

    #[test]
    fn set_wrapper_clears_wrapper() {
        let mut sh = FloweyShell::new().unwrap();
        sh.set_wrapper(Some(CommandWrapperKind::Prefix));
        sh.set_wrapper(None);
        // With wrapper cleared, command should run directly.
        let cmd = sh.wrap(xshell::cmd!(sh.xshell(), "echo direct"));
        let output = cmd.read().unwrap();
        assert_eq!(output, "direct");
    }

    #[test]
    fn quiet_flag_survives_wrapping() {
        let mut sh = FloweyShell::new().unwrap();
        sh.set_wrapper(Some(CommandWrapperKind::Prefix));
        // quiet() should not cause errors — just suppress echo to stderr.
        let cmd = sh.wrap(xshell::cmd!(sh.xshell(), "echo test")).quiet();
        let output = cmd.read().unwrap();
        assert_eq!(output, "WRAPPED: echo test");
    }

    #[test]
    fn args_accumulate_before_wrapping() {
        let mut sh = FloweyShell::new().unwrap();
        sh.set_wrapper(Some(CommandWrapperKind::Prefix));
        let cmd = sh
            .wrap(xshell::cmd!(sh.xshell(), "echo"))
            .arg("one")
            .arg("two");
        let output = cmd.read().unwrap();
        assert_eq!(output, "WRAPPED: echo one two");
    }

    #[test]
    fn secret_display_is_redacted() {
        let sh = FloweyShell::new().unwrap();
        let cmd = sh
            .wrap(xshell::cmd!(sh.xshell(), "curl --header secret-token"))
            .secret();
        assert_eq!(format!("{cmd}"), "<secret>");
    }

    #[test]
    fn secret_display_redacted_with_wrapper() {
        let mut sh = FloweyShell::new().unwrap();
        sh.set_wrapper(Some(CommandWrapperKind::Prefix));
        let cmd = sh
            .wrap(xshell::cmd!(sh.xshell(), "curl --header secret-token"))
            .secret();
        assert_eq!(format!("{cmd}"), "<secret>");
    }

    #[test]
    fn env_remove_survives_wrapping() {
        let mut sh = FloweyShell::new().unwrap();
        sh.set_wrapper(Some(CommandWrapperKind::ShCmd));

        // Set a var via the shell, then remove it on the command.
        // printenv should fail (exit 1) because the var is removed.
        sh.set_var("FLOWEY_REMOVE_TEST", "present");
        let cmd = sh
            .wrap(xshell::cmd!(sh.xshell(), "printenv FLOWEY_REMOVE_TEST"))
            .env_remove("FLOWEY_REMOVE_TEST")
            .ignore_status();
        let output = cmd.output().unwrap();
        assert!(!output.status.success());
    }

    #[test]
    fn env_clear_survives_wrapping() {
        let mut sh = FloweyShell::new().unwrap();
        sh.set_wrapper(Some(CommandWrapperKind::ShCmd));

        // After env_clear, even PATH is gone. The wrapped command
        // should still run (sh is resolved before env_clear applies),
        // but the inner command won't find the var.
        sh.set_var("FLOWEY_CLEAR_TEST", "present");
        let cmd = sh
            .wrap(xshell::cmd!(sh.xshell(), "printenv FLOWEY_CLEAR_TEST"))
            .env_clear()
            .ignore_status();
        let output = cmd.output().unwrap();
        assert!(!output.status.success());
    }

    #[test]
    fn env_ordering_preserved_through_wrapping() {
        let mut sh = FloweyShell::new().unwrap();
        sh.set_wrapper(Some(CommandWrapperKind::ShCmd));

        // Set, clear, then set again — only the final value should survive.
        let cmd = sh
            .wrap(xshell::cmd!(sh.xshell(), "printenv FLOWEY_ORDER_TEST"))
            .env("FLOWEY_ORDER_TEST", "first")
            .env_clear()
            .env("FLOWEY_ORDER_TEST", "second");
        let output = cmd.read().unwrap();
        assert_eq!(output, "second");
    }

    #[test]
    fn envs_plural_survives_wrapping() {
        let mut sh = FloweyShell::new().unwrap();
        sh.set_wrapper(Some(CommandWrapperKind::ShCmd));

        let vars = vec![("FLOWEY_MULTI_A", "alpha"), ("FLOWEY_MULTI_B", "beta")];
        // Print both vars separated by a space.
        let cmd = sh
            .wrap(xshell::cmd!(sh.xshell(), "sh"))
            .arg("-c")
            .arg("echo $FLOWEY_MULTI_A $FLOWEY_MULTI_B")
            .envs(vars);
        let output = cmd.read().unwrap();
        assert_eq!(output, "alpha beta");
    }
}
