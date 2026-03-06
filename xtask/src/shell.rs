// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Wrapper around [`xshell::Shell`] that centralizes the `clippy::disallowed_methods`
//! allow to a single place, since xtask is not a flowey node and legitimately
//! needs to spawn subprocesses.

/// Thin wrapper around [`xshell::Shell`] for use in xtask code.
pub struct XtaskShell(xshell::Shell);

impl XtaskShell {
    /// Create a new shell for spawning subprocesses.
    #[expect(
        clippy::disallowed_methods,
        reason = "xtask runs outside of a flowey runtime context and needs to spawn subprocesses"
    )]
    pub fn new() -> anyhow::Result<Self> {
        Ok(Self(xshell::Shell::new()?))
    }

    /// Build a command with `program` as the executable.
    ///
    /// Returns an [`xshell::Cmd`] builder; chain `.arg()`/`.args()`/`.run()`/
    /// `.output()` etc. on the result.
    pub fn cmd(&self, program: impl AsRef<std::path::Path>) -> xshell::Cmd<'_> {
        self.0.cmd(program)
    }

    /// Read the value of an environment variable.
    pub fn var(&self, key: &str) -> Result<String, xshell::Error> {
        self.0.var(key)
    }

    /// Set an environment variable for commands spawned from this shell.
    pub fn set_var(&self, key: impl AsRef<std::ffi::OsStr>, val: impl AsRef<std::ffi::OsStr>) {
        self.0.set_var(key, val)
    }
}
