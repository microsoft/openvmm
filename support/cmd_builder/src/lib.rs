// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Command Builder

#![forbid(unsafe_code)]

use std::ffi::OsStr;
use std::process::Command;
use std::process::Stdio;
use thiserror::Error;

pub mod ps;

/// Error running command
#[derive(Error, Debug)]
pub enum CommandError {
    /// failed to launch command
    #[error("failed to launch command")]
    Launch(#[from] std::io::Error),
    /// command exited with non-zero status
    #[error("command exited with non-zero status ({0}): {1}")]
    Command(std::process::ExitStatus, String),
    /// command output is not utf-8
    #[error("command output is not utf-8")]
    Utf8(#[from] std::string::FromUtf8Error),
}

/// Run the PowerShell script and return the output
pub fn run_cmd(mut cmd: Command, log_stdout: bool) -> Result<String, CommandError> {
    cmd.stderr(Stdio::piped()).stdin(Stdio::null());

    let cmd_str = cmd_to_string(&cmd);
    tracing::debug!(cmd_str, "executing command");

    let start = jiff::Timestamp::now();
    let output = cmd.output()?;
    let time_elapsed = jiff::Timestamp::now() - start;

    let stdout_str = (log_stdout || !output.status.success())
        .then(|| String::from_utf8_lossy(&output.stdout).to_string());
    let stderr_str = String::from_utf8_lossy(&output.stderr).to_string();
    tracing::debug!(
        cmd_str,
        stdout_str,
        stderr_str,
        "command exited in {:.3}s with status {}",
        time_elapsed.total(jiff::Unit::Second).unwrap_or(-1.0),
        output.status
    );

    if !output.status.success() {
        return Err(CommandError::Command(output.status, stderr_str));
    }

    Ok(String::from_utf8(output.stdout)?.trim().to_owned())
}

/// Get the command to be run
pub fn cmd_to_string(cmd: &Command) -> String {
    format!(
        "{} {}",
        cmd.get_program().to_string_lossy(),
        cmd.get_args()
            .collect::<Vec<_>>()
            .join(OsStr::new(" "))
            .to_string_lossy()
    )
}
