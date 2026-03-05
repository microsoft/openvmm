// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! RPC service for diagnostics.

use crate::grpc_result;
use crate::new_pty;
use anyhow::Context;
use azure_profiler_proto::AzureProfiler;
use azure_profiler_proto::ProfileRequest;
use diag_proto::ExecRequest;
use diag_proto::ExecResponse;
use diag_proto::FILE_LINE_MAX;
use diag_proto::FileRequest;
use diag_proto::KmsgRequest;
use diag_proto::MemoryProfileTraceRequest;
use diag_proto::NetworkPacketCaptureRequest;
use diag_proto::NetworkPacketCaptureResponse;
use diag_proto::OpenhclDiag;
use diag_proto::StartRequest;
use diag_proto::UnderhillDiag;
use diag_proto::WaitRequest;
use diag_proto::WaitResponse;
use diag_proto::network_packet_capture_request::Operation;
use futures::AsyncRead;
use futures::AsyncReadExt;
use futures::AsyncWrite;
use futures::AsyncWriteExt;
use futures::FutureExt;
use futures::StreamExt;
use futures::future::join_all;
use futures::io::AllowStdIo;
use futures_concurrency::stream::Merge;
use inspect::InspectionBuilder;
use inspect_proto::InspectRequest;
use inspect_proto::InspectResponse2;
use inspect_proto::InspectService;
use inspect_proto::UpdateRequest;
use inspect_proto::UpdateResponse2;
use mesh::CancelContext;
use mesh::rpc::FailableRpc;
use mesh::rpc::RpcSend;
use mesh_rpc::server::RpcReceiver;
use net_packet_capture::OperationData;
use net_packet_capture::PacketCaptureOperation;
use net_packet_capture::PacketCaptureParams;
use net_packet_capture::StartData;
use pal::unix::process::Stdio;
use pal_async::driver::Driver;
use pal_async::interest::InterestSlot;
use pal_async::interest::PollEvents;
use pal_async::pipe::PolledPipe;
use pal_async::socket::AsSockRef;
use pal_async::socket::PollReady;
use pal_async::socket::PollReadyExt;
use pal_async::socket::PolledSocket;
use pal_async::task::Spawn;
use pal_async::task::Task;
use parking_lot::Mutex;
use socket2::Socket;
use std::collections::HashMap;
use std::fs::File;
use std::future::poll_fn;
use std::io;
use std::io::Read;
use std::os::unix::fs::FileTypeExt;
use std::os::unix::prelude::*;
use std::process::ExitStatus;
use std::sync::Arc;

/// A diagnostics request.
#[derive(Debug, mesh::MeshPayload)]
pub enum DiagRequest {
    /// Start the VM, if it has not already been started.
    Start(FailableRpc<StartParams, ()>),
    /// Inspect the VM.
    Inspect(inspect::Deferred),
    /// Crash the VM
    Crash(i32),
    /// Restart the worker.
    Restart(FailableRpc<(), ()>),
    /// Pause VTL0
    Pause(FailableRpc<(), ()>),
    /// Resume VTL0
    Resume(FailableRpc<(), ()>),
    /// Save VTL2 state
    Save(FailableRpc<(), Vec<u8>>),
    /// Setup network trace
    PacketCapture(FailableRpc<PacketCaptureParams<Socket>, PacketCaptureParams<Socket>>),
    /// Profile VTL2
    #[cfg(feature = "profiler")]
    Profile(FailableRpc<profiler_worker::ProfilerRequest, ()>),
    /// Get memory profile trace
    #[cfg(feature = "mem-profile-tracing")]
    MemoryProfileTrace(FailableRpc<i32, Vec<u8>>),
}

/// Additional parameters provided as part of a delayed start request.
#[derive(Debug, mesh::MeshPayload)]
pub struct StartParams {
    /// Environment variables to set or remove.
    pub env: Vec<(String, Option<String>)>,
    /// Command line arguments to append.
    pub args: Vec<String>,
}

pub(crate) struct DiagServiceHandler {
    request_send: mesh::Sender<DiagRequest>,
    children: Mutex<HashMap<i32, Task<ExitStatus>>>,
    inspect_sensitivity_level: Option<inspect::SensitivityLevel>,
    inner: Arc<crate::Inner>,
}

impl DiagServiceHandler {
    pub fn new(request_send: mesh::Sender<DiagRequest>, inner: Arc<crate::Inner>) -> Self {
        Self {
            children: Default::default(),
            request_send,
            // On CVMs only allow inspecting nodes defined as safe.
            inspect_sensitivity_level: if underhill_confidentiality::confidential_filtering_enabled(
            ) {
                Some(inspect::SensitivityLevel::Safe)
            } else {
                None
            },
            // TODO: use a remotable type for `Inner`, which is just used to get
            // data connection sockets.
            inner,
        }
    }

    pub async fn process_requests(
        self: &Arc<Self>,
        driver: &(impl Driver + Spawn + Clone),
        diag_recv: RpcReceiver<UnderhillDiag>,
        diag2_recv: RpcReceiver<OpenhclDiag>,
        inspect_recv: RpcReceiver<InspectService>,
        profile_recv: RpcReceiver<AzureProfiler>,
    ) -> anyhow::Result<()> {
        enum Event {
            Diag(UnderhillDiag),
            Diag2(OpenhclDiag),
            Inspect(InspectService),
            Profile(AzureProfiler),
        }
        let mut s = (
            diag_recv.map(|(ctx, req)| (ctx, Event::Diag(req))),
            diag2_recv.map(|(ctx, req)| (ctx, Event::Diag2(req))),
            inspect_recv.map(|(ctx, req)| (ctx, Event::Inspect(req))),
            profile_recv.map(|(ctx, req)| (ctx, Event::Profile(req))),
        )
            .merge();

        while let Some((ctx, req)) = s.next().await {
            driver
                .spawn("diag request", {
                    let driver = driver.clone();
                    let this = self.clone();
                    async move {
                        match req {
                            Event::Diag(req) => this.handle_diag_request(&driver, req, ctx).await,
                            Event::Diag2(req) => this.handle_diag2_request(&driver, req, ctx).await,
                            Event::Inspect(req) => this.handle_inspect_request(req, ctx).await,
                            Event::Profile(req) => this.handle_profile_request(req, ctx).await,
                        }
                    }
                })
                .detach();
        }
        Ok(())
    }

    async fn take_connection(&self, id: u64) -> anyhow::Result<PolledSocket<Socket>> {
        self.inner.take_connection(id).await
    }

    async fn handle_inspect_request(&self, req: InspectService, mut ctx: CancelContext) {
        match req {
            InspectService::Inspect(request, response) => {
                let inspect_response = self.handle_inspect(&request, ctx).await;
                response.send(grpc_result(Ok(Ok(inspect_response))));
            }
            InspectService::Update(request, response) => {
                response.send(grpc_result(
                    ctx.until_cancelled(self.handle_update(&request)).await,
                ));
            }
        }
    }

    async fn handle_profile_request(&self, req: AzureProfiler, mut ctx: CancelContext) {
        match req {
            AzureProfiler::Profile(request, response) => response.send(grpc_result(
                ctx.until_cancelled(self.handle_profile(request)).await,
            )),
        }
    }

    async fn handle_diag_request(
        &self,
        driver: &(impl Driver + Spawn + Clone),
        req: UnderhillDiag,
        mut ctx: CancelContext,
    ) {
        match req {
            UnderhillDiag::Exec(request, response) => response.send(grpc_result(
                ctx.until_cancelled(self.handle_exec(driver, &request))
                    .await,
            )),
            UnderhillDiag::Wait(request, response) => response.send(grpc_result(
                ctx.until_cancelled(self.handle_wait(&request)).await,
            )),
            UnderhillDiag::Start(request, response) => {
                response.send(grpc_result(
                    ctx.until_cancelled(self.handle_start(request)).await,
                ));
            }
            UnderhillDiag::Kmsg(request, response) => {
                response.send(grpc_result(Ok(self.handle_kmsg(driver, &request).await)))
            }
            UnderhillDiag::Crash(request, response) => {
                response.send(grpc_result(
                    ctx.until_cancelled(self.handle_crash(request)).await,
                ));
            }
            UnderhillDiag::Restart(_, response) => {
                response.send(grpc_result(
                    ctx.until_cancelled(self.handle_restart()).await,
                ));
            }
            UnderhillDiag::ReadFile(request, response) => response.send(grpc_result(Ok(self
                .handle_read_file(driver, &request)
                .await))),
            UnderhillDiag::Pause(_, response) => {
                response.send(grpc_result(ctx.until_cancelled(self.handle_pause()).await))
            }
            UnderhillDiag::PacketCapture(request, response) => response.send(grpc_result(
                ctx.until_cancelled(self.handle_packet_capture(&request))
                    .await,
            )),
            UnderhillDiag::Resume(_, response) => {
                response.send(grpc_result(ctx.until_cancelled(self.handle_resume()).await))
            }
            UnderhillDiag::DumpSavedState((), response) => response.send(grpc_result(
                ctx.until_cancelled(self.handle_dump_saved_state()).await,
            )),
        }
    }

    async fn handle_diag2_request(
        &self,
        _driver: &(impl Driver + Spawn + Clone),
        req: OpenhclDiag,
        mut ctx: CancelContext,
    ) {
        match req {
            OpenhclDiag::Ping((), response) => {
                response.send(Ok(()));
            }
            OpenhclDiag::MemoryProfileTrace(request, response) => response.send(grpc_result(
                ctx.until_cancelled(self.handle_memory_profile_trace(&request))
                    .await,
            )),
        }
    }

    async fn handle_start(&self, request: StartRequest) -> anyhow::Result<()> {
        let params = StartParams {
            env: request
                .env
                .into_iter()
                .map(|pair| (pair.name, pair.value))
                .collect(),
            args: request.args,
        };
        self.request_send
            .call_failable(DiagRequest::Start, params)
            .await?;
        Ok(())
    }

    async fn handle_crash(&self, request: diag_proto::CrashRequest) -> anyhow::Result<()> {
        self.request_send.send(DiagRequest::Crash(request.pid));

        Ok(())
    }

    async fn handle_exec(
        &self,
        driver: &(impl Driver + Spawn + Clone),
        request: &ExecRequest,
    ) -> anyhow::Result<ExecResponse> {
        tracing::info!(
            command = %request.command,
            stdin = request.stdin,
            stdout = request.stdout,
            stderr = request.stderr,
            tty = request.tty,
            "exec request"
        );

        let stdin = if request.stdin != 0 {
            Some(
                self.take_connection(request.stdin)
                    .await
                    .context("failed to get stdin conn")?,
            )
        } else {
            None
        };
        let stdout = if request.stdout != 0 {
            Some(
                self.take_connection(request.stdout)
                    .await
                    .context("failed to get stdout conn")?,
            )
        } else {
            None
        };
        let stderr = if request.stderr != 0 {
            Some(
                self.take_connection(request.stderr)
                    .await
                    .context("failed to get stderr conn")?,
            )
        } else {
            None
        };

        let mut builder = pal::unix::process::Builder::new(&request.command);
        builder.args(&request.args);
        if request.clear_env {
            builder.env_clear();
        }
        for diag_proto::EnvPair { name, value } in &request.env {
            if let Some(value) = value {
                builder.env(name, value);
            } else {
                builder.env_remove(name);
            }
        }

        // HACK: A hack to fix segfault caused by glibc bug in L1 TDX VMM.
        // Should be removed after glibc update or a clean CPUID virtualization solution.
        // Please refer to https://github.com/microsoft/openvmm-deps/issues/21 for more information.
        // xtask-fmt allow-target-arch cpu-intrinsic
        #[cfg(target_arch = "x86_64")]
        {
            let result =
                safe_intrinsics::cpuid(hvdef::HV_CPUID_FUNCTION_MS_HV_ISOLATION_CONFIGURATION, 0);
            // Value 3 means TDX.
            let tdx_isolated = (result.ebx & 0xF) == 3;
            if tdx_isolated {
                builder.env("GLIBC_TUNABLES", "glibc.cpu.x86_non_temporal_threshold=0x11a000:glibc.cpu.x86_rep_movsb_threshold=0x4000");
            }
        };

        let mut stdin_relay = None;
        let mut stdout_relay = None;
        let mut stderr_relay = None;
        let mut raw_stdout = None;
        let mut raw_stderr = None;
        let mut child = {
            let (stdin_pipes, stdout_pipes, stderr_pipes);
            let stdin_socket;
            let stdout_socket;
            let stderr_socket;
            let pty;
            if request.tty {
                pty = new_pty::new_pty().context("failed to create pty")?;

                let primary = PolledPipe::new(driver, pty.0)
                    .context("failed to create polled pty primary")?;

                let secondary = &pty.1;

                let (primary_read, primary_write) = primary.split();
                if let Some(stdin) = stdin {
                    stdin_relay = Some(driver.spawn("pty stdin relay", async move {
                        relay(stdin, primary_write).await;
                    }));
                }
                if let Some(stdout) = stdout {
                    stdout_relay =
                        Some(driver.spawn("pty stdout relay", relay(primary_read, stdout)));
                }

                builder
                    .setsid(true)
                    .controlling_terminal(secondary.as_fd())
                    .stdin(Stdio::Fd(secondary.as_fd()))
                    .stdout(Stdio::Fd(secondary.as_fd()))
                    .stderr(Stdio::Fd(secondary.as_fd()));
            } else if request.raw_socket_io {
                if let Some(stdin) = stdin {
                    stdin_socket = stdin.into_inner();
                    builder.stdin(Stdio::Fd(stdin_socket.as_fd()));
                }
                if let Some(stdout) = stdout {
                    stdout_socket = raw_stdout.insert(stdout.into_inner());
                    builder.stdout(Stdio::Fd(stdout_socket.as_fd()));
                    if request.combine_stderr {
                        builder.stderr(Stdio::Fd(stdout_socket.as_fd()));
                    }
                }
                if let Some(stderr) = stderr {
                    stderr_socket = raw_stderr.insert(stderr.into_inner());
                    builder.stderr(Stdio::Fd(stderr_socket.as_fd()));
                }
            } else {
                if let Some(stdin) = stdin {
                    stdin_pipes = pal::unix::pipe::pair().context("failed to create pipe pair")?;
                    let pipe = PolledPipe::new(driver, stdin_pipes.1)
                        .context("failed to create polled pipe")?;
                    stdin_relay = Some(driver.spawn("stdin relay", async move {
                        relay(stdin, pipe).await;
                    }));
                    builder.stdin(Stdio::Fd(stdin_pipes.0.as_fd()));
                }
                if let Some(stdout) = stdout {
                    stdout_pipes = pal::unix::pipe::pair().context("failed to create pipe pair")?;
                    let pipe = PolledPipe::new(driver, stdout_pipes.0)
                        .context("failed to create polled pipe")?;
                    stdout_relay = Some(driver.spawn("stdout relay", relay(pipe, stdout)));
                    builder.stdout(Stdio::Fd(stdout_pipes.1.as_fd()));
                    if request.combine_stderr {
                        builder.stderr(Stdio::Fd(stdout_pipes.1.as_fd()));
                    }
                }
                if let Some(stderr) = stderr {
                    stderr_pipes = pal::unix::pipe::pair().context("failed to create pipe pair")?;
                    let pipe = PolledPipe::new(driver, stderr_pipes.0)
                        .context("failed to create polled pipe")?;
                    stderr_relay = Some(driver.spawn("stderr relay", relay(pipe, stderr)));
                    builder.stderr(Stdio::Fd(stderr_pipes.1.as_fd()));
                }
            }

            builder
                .spawn()
                .with_context(|| format!("failed to launch {}", &request.command))?
        };

        let pid = child.id();

        tracing::info!(pid, "spawned child");

        let mut child_ready = driver
            .new_dyn_fd_ready(child.as_fd().as_raw_fd())
            .expect("failed creating child poll");

        let status = driver.spawn("diag child wait", {
            let driver = driver.clone();
            async move {
                poll_fn(|cx| child_ready.poll_fd_ready(cx, InterestSlot::Read, PollEvents::IN))
                    .await;
                let status = child.try_wait().unwrap().unwrap();
                tracing::info!(pid, ?status, "child exited");

                // The process is gone, so the stdin relay's job is done.
                drop(stdin_relay);

                // Shut down raw stdout and stderr to notify the host that there
                // is no more data.
                let finish_raw = |raw: Option<Socket>| {
                    raw.and_then(|raw| {
                        let _ = raw.as_sock_ref().shutdown(std::net::Shutdown::Write);
                        PolledSocket::new(&driver, raw).ok()
                    })
                };
                let raw_stdout = finish_raw(raw_stdout);
                let raw_stderr = finish_raw(raw_stderr);

                // Wait for the host to finish with the stdout and stderr
                // sockets, but don't block the process exit notification.
                driver
                    .spawn("socket-wait", async move {
                        let await_output_relay = async |task, raw| {
                            let socket = if let Some(task) = task {
                                Some(task.await)
                            } else {
                                raw
                            };
                            if let Some(socket) = socket {
                                // Wait for the host to close the socket to ensure that all
                                // the data is written.
                                let _ = futures::io::copy(socket, &mut futures::io::sink()).await;
                            }
                        };

                        await_output_relay(stdout_relay, raw_stdout).await;
                        await_output_relay(stderr_relay, raw_stderr).await;
                    })
                    .detach();

                status
            }
        });

        self.children.lock().insert(pid, status);
        Ok(ExecResponse { pid })
    }

    async fn handle_wait(&self, request: &WaitRequest) -> anyhow::Result<WaitResponse> {
        tracing::debug!(pid = request.pid, "wait request");
        let channel = self
            .children
            .lock()
            .remove(&request.pid)
            .context("pid not found")?;

        let status = channel.await;
        let exit_code = status.code().unwrap_or(255);

        tracing::debug!(pid = request.pid, exit_code, "wait complete");

        Ok(WaitResponse { exit_code })
    }

    async fn handle_inspect(
        &self,
        request: &InspectRequest,
        mut ctx: CancelContext,
    ) -> InspectResponse2 {
        tracing::debug!(
            path = request.path.as_str(),
            depth = request.depth,
            "inspect request"
        );
        let mut inspection = InspectionBuilder::new(&request.path)
            .depth(Some(request.depth as usize))
            .sensitivity(self.inspect_sensitivity_level)
            .inspect(inspect::send(&self.request_send, DiagRequest::Inspect));

        // Don't return early on cancel, as we want to return the partial
        // inspection results.
        let _ = ctx.until_cancelled(inspection.resolve()).await;

        let result = inspection.results();
        InspectResponse2 { result }
    }

    async fn handle_update(&self, request: &UpdateRequest) -> anyhow::Result<UpdateResponse2> {
        tracing::debug!(
            path = request.path.as_str(),
            value = request.value.as_str(),
            "update request"
        );
        let new_value = InspectionBuilder::new(&request.path)
            .sensitivity(self.inspect_sensitivity_level)
            .update(
                &request.value,
                inspect::send(&self.request_send, DiagRequest::Inspect),
            )
            .await?;
        Ok(UpdateResponse2 { new_value })
    }

    async fn handle_kmsg(
        &self,
        driver: &(impl Driver + Spawn + Clone),
        request: &KmsgRequest,
    ) -> anyhow::Result<()> {
        self.handle_read_file_request(driver, request.conn, request.follow, "/dev/kmsg")
            .await
    }

    async fn handle_read_file(
        &self,
        driver: &(impl Driver + Spawn + Clone),
        request: &FileRequest,
    ) -> anyhow::Result<()> {
        self.handle_read_file_request(driver, request.conn, request.follow, &request.file_path)
            .await
    }

    async fn handle_packet_capture(
        &self,
        request: &NetworkPacketCaptureRequest,
    ) -> anyhow::Result<NetworkPacketCaptureResponse> {
        let operation = if request.operation == Operation::Query as i32 {
            PacketCaptureOperation::Query
        } else if request.operation == Operation::Start as i32 {
            PacketCaptureOperation::Start
        } else if request.operation == Operation::Stop as i32 {
            PacketCaptureOperation::Stop
        } else {
            anyhow::bail!("unsupported request type {}", request.operation);
        };

        let op_data = match operation {
            // Query the number of streams needed, starting with a value of 0.
            PacketCaptureOperation::Query => Some(OperationData::OpQueryData(0)),
            PacketCaptureOperation::Start => {
                let Some(op_data) = &request.op_data else {
                    anyhow::bail!("missing start operation parameters");
                };

                match op_data {
                    diag_proto::network_packet_capture_request::OpData::StartData(start_data) => {
                        let writers = join_all(start_data.conns.iter().map(async |c| {
                            let conn = self.take_connection(*c).await?;
                            Ok(conn.into_inner())
                        }))
                        .await
                        .into_iter()
                        .collect::<anyhow::Result<Vec<Socket>>>()?;
                        Some(OperationData::OpStartData(StartData {
                            writers,
                            snaplen: start_data.snaplen,
                        }))
                    }
                }
            }
            _ => None,
        };

        let params = PacketCaptureParams { operation, op_data };
        let params = self
            .request_send
            .call_failable(DiagRequest::PacketCapture, params)
            .await?;
        let num_streams = match params.op_data {
            Some(OperationData::OpQueryData(num_streams)) => num_streams,
            _ => 0,
        };
        Ok(NetworkPacketCaptureResponse { num_streams })
    }

    async fn handle_profile(&self, request: ProfileRequest) -> anyhow::Result<()> {
        let conn = self.take_connection(request.conn).await?;
        #[cfg(feature = "profiler")]
        {
            let profiler_request = profiler_worker::ProfilerRequest {
                profiler_args: request.profiler_args,
                duration: request.duration,
                conn: conn.into_inner(),
            };

            self.request_send
                .call_failable(DiagRequest::Profile, profiler_request)
                .await?;
        }
        #[cfg(not(feature = "profiler"))]
        {
            // Profiler feature disabled, drop the connection.
            drop(conn);
            tracing::error!("Profiler feature disabled");
        }
        Ok(())
    }

    async fn handle_read_file_request(
        &self,
        driver: &(impl Driver + Spawn + Clone),
        conn: u64,
        follow: bool,
        file_path: &str,
    ) -> anyhow::Result<()> {
        let mut conn = self.take_connection(conn).await?;
        let file = fs_err::File::open(file_path).context("failed to open file")?;

        let file_meta = file.metadata()?;

        if file_meta.file_type().is_char_device() {
            let file =
                PolledPipe::new(driver, file.into()).context("failed to create polled pipe")?;

            driver
                .spawn("read file relay", async move {
                    if let Err(err) = relay_read_file(file, conn, follow).await {
                        tracing::warn!(
                            error = &*err as &dyn std::error::Error,
                            "read file relay failed"
                        );
                    }
                })
                .detach();
        } else if file_meta.file_type().is_file() {
            driver
                .spawn("read file relay", async move {
                    // Since this is a file, and in Underhill files are backed
                    // by RAM, allow blocking reads directly on this thread,
                    // since the reads should be satisfied instantly.
                    //
                    // (If this becomes a problem, we can spawn a thread to do
                    // this, or use io-uring.)
                    if let Err(err) =
                        futures::io::copy(AllowStdIo::new(File::from(file)), &mut conn).await
                    {
                        tracing::warn!(
                            error = &err as &dyn std::error::Error,
                            "read file relay failed"
                        );
                    }
                })
                .detach();
        } else {
            anyhow::bail!("cannot read directory");
        }

        Ok(())
    }

    async fn handle_restart(&self) -> anyhow::Result<()> {
        self.request_send
            .call_failable(DiagRequest::Restart, ())
            .await?;
        Ok(())
    }

    async fn handle_pause(&self) -> anyhow::Result<()> {
        self.request_send
            .call_failable(DiagRequest::Pause, ())
            .await?;
        Ok(())
    }

    async fn handle_resume(&self) -> anyhow::Result<()> {
        self.request_send
            .call_failable(DiagRequest::Resume, ())
            .await?;
        Ok(())
    }

    async fn handle_dump_saved_state(&self) -> anyhow::Result<diag_proto::DumpSavedStateResponse> {
        let data = self
            .request_send
            .call_failable(DiagRequest::Save, ())
            .await?;

        Ok(diag_proto::DumpSavedStateResponse { data })
    }

    async fn handle_memory_profile_trace(
        &self,
        request: &MemoryProfileTraceRequest,
    ) -> anyhow::Result<diag_proto::MemoryProfileTraceResponse> {
        cfg_if::cfg_if! {
            if #[cfg(feature = "mem-profile-tracing")]
            {
                let data = self
                    .request_send
                    .call_failable(DiagRequest::MemoryProfileTrace, request.pid)
                    .await?;

                Ok(diag_proto::MemoryProfileTraceResponse { data })
            } else {
                let _ = request;
                anyhow::bail!(
                    "Profiler tracing feature disabled: rebuild with the `mem-profile-tracing` feature enabled"
                )
            }
        }
    }
}

async fn relay<
    R: 'static + AsyncRead + Unpin + Send,
    W: 'static + AsyncWrite + PollReady + Unpin + Send,
>(
    mut read: R,
    mut write: W,
) -> W {
    let mut buffer = [0; 1024];
    let result: anyhow::Result<_> = async {
        loop {
            let n = futures::select! { // merge semantics
                n = read.read(&mut buffer).fuse() => n.context("read failed")?,
                _ = write.wait_ready(PollEvents::RDHUP).fuse() => {
                    // RDHUP indicates the connection is closed or shut down.
                    // Although generically this does not indicate that the
                    // connection does not want to _read_ any more data, for our
                    // use cases it does (either we are using a unidirectional
                    // pipe/socket, or we are using a pty, which never returns
                    // RDHUP but does return HUP, which is just as good).
                    //
                    // Stop this relay to propagate the close notification to
                    // the other endpoint.
                    break;
                }
            };
            if n == 0 {
                break;
            }
            write
                .write_all(&buffer[..n])
                .await
                .context("write failed")?;
        }
        Ok(())
    }
    .await;
    let _ = write.close().await;
    if let Err(err) = result {
        tracing::warn!(error = &*err as &dyn std::error::Error, "relay error");
    }
    write
}

async fn relay_read_file(
    mut file: PolledPipe,
    mut conn: PolledSocket<Socket>,
    follow: bool,
) -> anyhow::Result<()> {
    let mut buffer = [0; FILE_LINE_MAX];
    loop {
        let n = if follow {
            futures::select! { // race semantics
                _ = conn.wait_ready(PollEvents::RDHUP).fuse() => break,
                n = file.read(&mut buffer[..FILE_LINE_MAX - 1]).fuse() => n
            }
        } else {
            // The caller just wants the current contents of file, so issue a
            // nonblocking, non-async read, and handle EAGAIN below.
            file.get().read(&mut buffer[..FILE_LINE_MAX - 1])
        };
        let n = match n {
            Ok(0) => break,
            Ok(count) => count,
            Err(e) => {
                match e.kind() {
                    io::ErrorKind::BrokenPipe => {
                        // The kmsg interface returns EPIPE if an entry has overwritten another in the ring.
                        // Retry the read which has the kernel move the seek position to the next available record.
                        continue;
                    }
                    io::ErrorKind::WouldBlock => {
                        // There are no more messages.
                        assert!(!follow);
                        break;
                    }
                    _ => return Err(e).context("file read failed"),
                }
            }
        };
        assert!(
            n < buffer.len(),
            "the file returned a line bigger than its maximum"
        );
        // Add a null terminator.
        buffer[n] = 0;
        // Write the message followed by a null terminator.
        conn.write_all(&buffer[..n + 1])
            .await
            .context("socket write failed")?;
    }
    Ok(())
}
