// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use anyhow::Context;
use diag_client::DiagClient;
use diag_client::ExitStatus;
use diag_client::kmsg_stream::KmsgStream;
use futures::io::AllowStdIo;
use std::io::Read;

pub struct OpenHclDiagHandler(DiagClient);

/// The result of running a VTL2 command.
#[derive(Debug)]
pub struct Vtl2CommandResult {
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
    pub(crate) fn new(client: DiagClient) -> Self {
        Self(client)
    }

    pub(crate) async fn wait_for_vtl2(&self) -> anyhow::Result<()> {
        self.0.wait_for_server().await
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

    pub(crate) async fn run_detached_vtl2_command(
        &self,
        command: impl AsRef<str>,
        args: impl IntoIterator<Item = impl AsRef<str>>,
    ) -> anyhow::Result<ExitStatus> {
        let client = self.diag_client().await?;
        let proc = client.exec(command.as_ref()).args(args).spawn().await?;
        let exit_status = proc.wait().await?;
        Ok(exit_status)
    }

    pub async fn core_dump(&self, name: &str, path: &std::path::Path) -> anyhow::Result<()> {
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

    pub async fn crash(&self, name: &str) -> anyhow::Result<()> {
        let client = self.diag_client().await?;
        let pid = client.get_pid(name).await?;
        client.crash(pid).await
    }

    pub async fn inspect(
        &self,
        path: impl Into<String>,
        depth: Option<usize>,
        timeout: Option<std::time::Duration>,
    ) -> anyhow::Result<inspect::Node> {
        self.diag_client()
            .await?
            .inspect(path, depth, timeout)
            .await
    }

    pub async fn inspect_update(
        &self,
        path: impl Into<String>,
        value: impl Into<String>,
    ) -> anyhow::Result<inspect::Value> {
        self.diag_client().await?.update(path, value).await
    }

    pub async fn kmsg(&self) -> anyhow::Result<KmsgStream> {
        self.diag_client().await?.kmsg(false).await
    }

    async fn diag_client(&self) -> anyhow::Result<&DiagClient> {
        self.wait_for_vtl2().await?;
        Ok(&self.0)
    }
}
