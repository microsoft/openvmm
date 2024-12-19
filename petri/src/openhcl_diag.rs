// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use anyhow::Context;
use diag_client::DiagClient;
use diag_client::ExitStatus;
use futures::io::AllowStdIo;
use std::io::Read;
use std::path::PathBuf;

pub(crate) struct OpenHclDiagHandler {
    pub(crate) vtl2_vsock_path: PathBuf,
    pub(crate) client: DiagClient,
}

/// The result of running a VTL2 command.
#[derive(Debug)]
#[allow(dead_code)] // Fields output via Debug for debugging purposes.
pub(crate) struct Vtl2CommandResult {
    /// The stdout of the command.
    pub stdout: String,
    /// The stderr of the command.
    pub stderr: String,
    /// The raw stdout of the command.
    pub stdout_raw: Vec<u8>,
    /// The raw stderr of the command.
    pub stderr_raw: Vec<u8>,
    /// The exit status of the command.
    pub exit_status: ExitStatus,
}

impl OpenHclDiagHandler {
    pub(crate) async fn wait_for_vtl2(&self) -> anyhow::Result<()> {
        self.client.wait_for_server().await
    }

    pub(crate) async fn run_vtl2_command(
        &self,
        command: impl AsRef<str>,
        args: impl IntoIterator<Item = impl AsRef<str>>,
    ) -> anyhow::Result<Vtl2CommandResult> {
        let client = self.diag_client().await?;
        let mut proc = client
            .exec(command.as_ref())
            .args(args)
            .stdout(true)
            .stderr(true)
            .raw_socket_io(true)
            .spawn()
            .await?;

        let (mut stdout, mut stderr) = (proc.stdout.take().unwrap(), proc.stderr.take().unwrap());
        let exit_status = proc.wait().await?;

        let mut stdout_buf = Vec::new();
        stdout
            .read_to_end(&mut stdout_buf)
            .context("error reading stdout socket")?;
        let stdout_str = String::from_utf8_lossy(&stdout_buf);

        let mut stderr_buf = Vec::new();
        stderr
            .read_to_end(&mut stderr_buf)
            .context("error reading stderr socket")?;
        let stderr_str = String::from_utf8_lossy(&stderr_buf);

        Ok(Vtl2CommandResult {
            stdout: stdout_str.to_string(),
            stderr: stderr_str.to_string(),
            stdout_raw: stdout_buf,
            stderr_raw: stderr_buf,
            exit_status,
        })
    }

    pub(crate) async fn core_dump(&self, name: &str, path: &std::path::Path) -> anyhow::Result<()> {
        let client = self.diag_client().await?;
        let pid = client.get_pid(name).await?;
        client
            .core_dump(
                pid,
                AllowStdIo::new(fs_err::File::create(path)?),
                AllowStdIo::new(std::io::stderr()),
                true,
            )
            .await
    }

    pub(crate) async fn crash(&self, name: &str) -> anyhow::Result<()> {
        let client = self.diag_client().await?;
        let pid = client.get_pid(name).await?;
        client.crash(pid).await
    }

    pub(crate) async fn test_inspect(&self) -> anyhow::Result<()> {
        self.diag_client()
            .await?
            .inspect("", None, None)
            .await
            .map(|_| ())
    }

    async fn diag_client(&self) -> anyhow::Result<&DiagClient> {
        self.wait_for_vtl2().await?;
        Ok(&self.client)
    }
}
