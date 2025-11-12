// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! The client for `pipette`.

#![forbid(unsafe_code)]

pub mod process;
mod send;
pub mod shell;

pub use pipette_protocol::PIPETTE_VSOCK_PORT;

use crate::send::PipetteSender;
use anyhow::Context;
use futures::AsyncBufReadExt;
use futures::AsyncRead;
use futures::AsyncWrite;
use futures::AsyncWriteExt;
use futures::FutureExt as _;
use futures::StreamExt;
use futures::io::BufReader;
use futures_concurrency::future::TryJoin;
use mesh::error::RemoteError;
use mesh::payload::Timestamp;
use mesh::rpc::RpcError;
use mesh_remote::PointToPointMesh;
use pal_async::task::Spawn;
use pal_async::task::Task;
use pipette_protocol::DiagnosticFile;
use pipette_protocol::PipetteBootstrap;
use pipette_protocol::PipetteRequest;
use pipette_protocol::ReadFileRequest;
use pipette_protocol::WriteFileRequest;
use shell::UnixShell;
use shell::WindowsShell;
use std::path::Path;
use std::path::PathBuf;

/// A client to a running `pipette` instance inside a VM.
pub struct PipetteClient {
    send: PipetteSender,
    watch: mesh::OneshotReceiver<()>,
    _mesh: PointToPointMesh,
    _log_task: Task<()>,
    _diag_task: Task<()>,
}

impl PipetteClient {
    /// Connects to a `pipette` instance inside a VM.
    ///
    /// `conn` must be an established connection over some byte stream (e.g., a
    /// socket).
    pub async fn new(
        spawner: impl Spawn,
        conn: impl 'static + AsyncRead + AsyncWrite + Send + Unpin,
        output_dir: &Path,
    ) -> Result<Self, mesh::RecvError> {
        let (bootstrap_send, bootstrap_recv) = mesh::oneshot::<PipetteBootstrap>();
        let mesh = PointToPointMesh::new(&spawner, conn, bootstrap_send.into());
        let bootstrap = bootstrap_recv.await?;

        let PipetteBootstrap {
            requests,
            diag_file_recv,
            watch,
            log,
        } = bootstrap;

        let log_task = spawner.spawn("pipette-log", replay_logs(log));
        let diag_task = spawner.spawn(
            "diagnostics-recv",
            recv_diag_files(output_dir.to_owned(), diag_file_recv),
        );

        Ok(Self {
            send: PipetteSender::new(requests),
            watch,
            _mesh: mesh,
            _log_task: log_task,
            _diag_task: diag_task,
        })
    }

    /// Pings the agent to check if it's alive.
    pub async fn ping(&self) -> Result<(), RpcError> {
        self.send.call(PipetteRequest::Ping, ()).await
    }

    /// Return a shell object to interact with a Windows guest.
    pub fn windows_shell(&self) -> WindowsShell<'_> {
        WindowsShell::new(self)
    }

    /// Return a shell object to interact with a Linux guest.
    pub fn unix_shell(&self) -> UnixShell<'_> {
        UnixShell::new(self)
    }

    /// Returns an object used to launch a command inside the guest.
    ///
    /// TODO: this is a low-level interface. Make a high-level interface like
    /// `xshell::Shell` for manipulating the environment and launching
    /// processes.
    pub fn command(&self, program: impl AsRef<str>) -> process::Command<'_> {
        process::Command::new(self, program)
    }

    /// Sends a request to the guest to power off.
    pub async fn power_off(&self) -> anyhow::Result<()> {
        self.shutdown(pipette_protocol::ShutdownType::PowerOff)
            .await
    }

    /// Sends a request to the guest to reboot.
    pub async fn reboot(&self) -> anyhow::Result<()> {
        self.shutdown(pipette_protocol::ShutdownType::Reboot).await
    }

    async fn shutdown(&self, shutdown_type: pipette_protocol::ShutdownType) -> anyhow::Result<()> {
        tracing::debug!(?shutdown_type, "sending shutdown request to guest");
        let r = self.send.call(
            PipetteRequest::Shutdown,
            pipette_protocol::ShutdownRequest { shutdown_type },
        );
        match r.await {
            Ok(r) => r
                .map_err(anyhow::Error::from)
                .context("failed to shut down")?,
            Err(_) => {
                // Presumably this is an expected error due to the agent exiting
                // or the guest powering off.
            }
        }
        Ok(())
    }

    /// Reads the full contents of a file.
    pub async fn read_file(&self, path: impl AsRef<str>) -> anyhow::Result<Vec<u8>> {
        let (recv_pipe, send_pipe) = mesh::pipe::pipe();
        let req = ReadFileRequest {
            path: path.as_ref().to_string(),
            sender: send_pipe,
        };

        let request_future = self.send.call_failable(PipetteRequest::ReadFile, req);

        let mut contents = Vec::new();
        let transfer_future = async { futures::io::copy(recv_pipe, &mut contents).await };

        tracing::debug!(path = path.as_ref(), "beginning file read transfer");
        let (bytes_read, io_result) = (request_future, transfer_future.map(Ok))
            .try_join()
            .await
            .context("failed to read file")?;

        io_result.context("io failure")?;
        if bytes_read != contents.len() as u64 {
            anyhow::bail!("file truncated");
        }

        tracing::debug!("file read complete");
        Ok(contents)
    }

    /// Writes a file to the guest.
    /// Note: This may transfer the file in chunks. It is likely not suitable
    /// for writing to files that require all content to be written at once,
    /// e.g. files in /proc or /sys.
    pub async fn write_file(
        &self,
        path: impl AsRef<str>,
        contents: impl AsyncRead,
    ) -> anyhow::Result<()> {
        let (recv_pipe, mut send_pipe) = mesh::pipe::pipe();
        let req = WriteFileRequest {
            path: path.as_ref().to_string(),
            receiver: recv_pipe,
        };

        let request_future = self.send.call_failable(PipetteRequest::WriteFile, req);

        let transfer_future = async {
            let copy_result = futures::io::copy(contents, &mut send_pipe).await;
            send_pipe.close().await?;
            copy_result
        };

        tracing::debug!(path = path.as_ref(), "beginning file wurite transfer");
        let (bytes_written, io_result) = (request_future, transfer_future.map(Ok))
            .try_join()
            .await
            .context("failed to write file")?;
        if bytes_written != io_result.context("io failure")? {
            anyhow::bail!("file truncated");
        }

        tracing::debug!("file write complete");
        Ok(())
    }

    /// Waits for the agent to exit.
    pub async fn wait(self) -> Result<(), mesh::RecvError> {
        self.watch.await
    }

    /// Returns the current time in the guest.
    pub async fn get_time(&self) -> anyhow::Result<Timestamp> {
        self.send
            .call(PipetteRequest::GetTime, ())
            .await
            .context("failed to get time")
    }

    /// Tell the agent to crash itself.
    pub async fn crash(&self) -> Result<(), RemoteError> {
        Self::handle_crash_result(self.send.call_failable(PipetteRequest::Crash, ()).await)
    }

    /// Tell the agent to crash the kernel.
    pub async fn kernel_crash(&self) -> Result<(), RemoteError> {
        Self::handle_crash_result(
            self.send
                .call_failable(PipetteRequest::KernelCrash, ())
                .await,
        )
    }

    fn handle_crash_result(r: Result<(), RpcError<RemoteError>>) -> Result<(), RemoteError> {
        match r {
            Ok(()) => unreachable!(),
            Err(RpcError::Call(err)) => Err(err),
            Err(RpcError::Channel(_)) => {
                // Presumably this is an expected error due to the agent exiting
                // or the guest crashing.
                Ok(())
            }
        }
    }
}

async fn replay_logs(log: mesh::pipe::ReadPipe) {
    let mut lines = BufReader::new(log).lines();
    while let Some(line) = lines.next().await {
        match line {
            Ok(line) => tracing::info!(target: "pipette", "{}", line),
            Err(err) => {
                tracing::error!(
                    error = &err as &dyn std::error::Error,
                    "pipette log failure"
                );
                break;
            }
        }
    }
}

async fn recv_diag_files(output_dir: PathBuf, mut diag_file_recv: mesh::Receiver<DiagnosticFile>) {
    while let Some(diag_file) = diag_file_recv.next().await {
        let DiagnosticFile { name, mut receiver } = diag_file;
        tracing::debug!(name, "receiving diagnostic file");
        let path = output_dir.join(&name);
        let file = fs_err::File::create(&path).expect("failed to create diagnostic file {name}");
        futures::io::copy(&mut receiver, &mut futures::io::AllowStdIo::new(file))
            .await
            .expect("failed to write diagnostic file");
        tracing::debug!(name, "diagnostic file transfer complete");

        #[expect(
            clippy::disallowed_methods,
            reason = "ATTACHMENT is most reliable when using true canonicalized paths"
        )]
        let canonical_path = path
            .canonicalize()
            .expect("failed to canonicalize attachment path");
        // Use the inline junit syntax to attach the file to the test result.
        println!("[[ATTACHMENT|{}]]", canonical_path.display());
    }
}
