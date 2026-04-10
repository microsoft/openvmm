// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! The client for connecting to the Underhill diagnostics server.

#![forbid(unsafe_code)]

pub mod kmsg_stream;

use anyhow::Context;
use diag_proto::ExecRequest;
use diag_proto::WaitRequest;
use diag_proto::WaitResponse;
use diag_proto::network_packet_capture_request::OpData;
use diag_proto::network_packet_capture_request::Operation;
use futures::AsyncReadExt;
use futures::AsyncWrite;
use futures::AsyncWriteExt;
use inspect::Node;
use inspect::ValueKind;
use kmsg_stream::KmsgStream;
use mesh_rpc::service::Status;
use pal_async::driver::Driver;
use pal_async::socket::PolledSocket;
use pal_async::task::Spawn;
use std::io::ErrorKind;
use std::path::Path;
use std::path::PathBuf;
use std::time::Duration;
use thiserror::Error;

#[cfg(windows)]
/// Functions for Hyper-V
pub mod hyperv {
    use super::ConnectError;
    use anyhow::Context;
    use guid::Guid;
    use pal_async::driver::Driver;
    use pal_async::socket::PolledSocket;
    use pal_async::timer::PolledTimer;
    use std::fs::File;
    use std::io::Write;
    use std::process::Command;
    use std::time::Duration;
    use vmsocket::VmAddress;
    use vmsocket::VmSocket;
    use vmsocket::VmStream;

    /// Defines how to access the serial port
    pub enum ComPortAccessInfo<'a> {
        /// Access by number
        NameAndPortNumber(&'a str, u32),
        /// Access through a named pipe
        PortPipePath(&'a str),
    }

    /// Get ID from name
    pub fn vm_id_from_name(name: &str) -> anyhow::Result<Guid> {
        let output = Command::new("hvc.exe")
            .arg("id")
            .arg(name)
            .output()
            .context("failed to launch hvc")?;

        if output.status.success() {
            let stdout = std::str::from_utf8(&output.stdout)
                .context("failed to parse hvc output")?
                .trim();
            Ok(stdout
                .parse()
                .with_context(|| format!("failed to parse VM ID '{}'", &stdout))?)
        } else {
            anyhow::bail!(
                "{}",
                std::str::from_utf8(&output.stderr).context("failed to parse hvc error output")?
            )
        }
    }

    /// Connect to Hyper-V socket
    pub async fn connect_vsock(
        driver: &(impl Driver + ?Sized),
        vm_id: Guid,
        port: u32,
    ) -> Result<VmStream, ConnectError> {
        let socket = VmSocket::new()
            .context("failed to create AF_HYPERV socket")
            .map_err(ConnectError::other)?;

        socket
            .set_connect_timeout(Duration::from_secs(1))
            .context("failed to set connect timeout")
            .map_err(ConnectError::other)?;

        socket
            .set_high_vtl(true)
            .context("failed to set socket for VTL2")
            .map_err(ConnectError::other)?;

        let mut socket: PolledSocket<socket2::Socket> = PolledSocket::new(driver, socket.into())
            .context("failed to create polled socket")
            .map_err(ConnectError::other)?;

        socket
            .connect(&VmAddress::hyperv_vsock(vm_id, port).into())
            .await
            .map_err(ConnectError::connect)?;

        Ok(socket.convert().into_inner())
    }

    /// Opens a serial port on a Hyper-V VM.
    ///
    /// If the VM is not running, it will periodically try to connect to the
    /// pipe until the VM starts running. In theory, we could instead create a
    /// named pipe server, which Hyper-V would connect to when the VM starts.
    /// However, in this mode, once the named pipe is disconnected, Hyper-V
    /// stops trying to reconnect until the VM is powered off and powered on
    /// again, so don't do that.
    pub async fn open_serial_port(
        driver: &(impl Driver + ?Sized),
        port: ComPortAccessInfo<'_>,
    ) -> anyhow::Result<File> {
        let path = match port {
            ComPortAccessInfo::NameAndPortNumber(vm, num) => {
                let output = Command::new("powershell.exe")
                    .arg("-NoProfile")
                    .arg(format!(
                        r#"$x = Get-VMComPort "{vm}" -Number {num} -ErrorAction Stop; $x.Path"#,
                    ))
                    .output()
                    .context("failed to query VM com port")?;

                if !output.status.success() {
                    let _ = std::io::stderr().write_all(&output.stderr);
                    anyhow::bail!(
                        "failed to query VM com port: exit status {}",
                        output.status.code().unwrap()
                    );
                }
                &String::from_utf8(output.stdout)?
            }
            ComPortAccessInfo::PortPipePath(path) => path,
        };

        let path = path.trim();
        if path.is_empty() {
            anyhow::bail!("Requested VM COM port is not configured");
        }

        let mut timer = None;
        let pipe = loop {
            match fs_err::OpenOptions::new().read(true).write(true).open(path) {
                Ok(pipe) => break pipe.into(),
                Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                    // The VM is not running. Wait a bit and try again.
                    timer
                        .get_or_insert_with(|| PolledTimer::new(driver))
                        .sleep(Duration::from_millis(100))
                        .await;
                }
                Err(err) => Err(err)?,
            }
        };

        Ok(pipe)
    }
}

/// Connect to a vsock with port and path
pub async fn connect_hybrid_vsock(
    driver: &(impl Driver + ?Sized),
    path: &Path,
    port: u32,
) -> Result<PolledSocket<socket2::Socket>, ConnectError> {
    let socket = unix_socket::UnixStream::connect(path).map_err(ConnectError::connect)?;
    let mut socket = PolledSocket::new(driver, socket).map_err(ConnectError::other)?;
    socket
        .write_all(format!("CONNECT {port}\n").as_bytes())
        .await
        .map_err(ConnectError::other)?;

    let mut ok = [0; 3];
    socket
        .read_exact(&mut ok)
        .await
        .map_err(ConnectError::other)?;
    if &ok != b"OK " {
        // FUTURE: consider returning an error that can be retried. This may
        // require some changes to the hybrid vsock protocol, unclear.
        return Err(ConnectError::other(anyhow::anyhow!(
            "missing hybrid vsock response"
        )));
    }

    for _ in 0.."4294967295\n".len() {
        let mut b = [0];
        socket
            .read_exact(&mut b)
            .await
            .map_err(ConnectError::other)?;
        if b[0] == b'\n' {
            // Don't need to parse the host port number.
            return Ok(socket.convert());
        }
    }
    Err(ConnectError::other(anyhow::anyhow!(
        "invalid hybrid vsock response"
    )))
}

enum SocketType<'a> {
    #[cfg(windows)]
    VmId {
        vm_id: guid::Guid,
        port: u32,
    },
    HybridVsock {
        path: &'a Path,
        port: u32,
    },
}

async fn new_data_connection(
    driver: &(impl Driver + ?Sized),
    typ: SocketType<'_>,
) -> anyhow::Result<(u64, PolledSocket<socket2::Socket>)> {
    let mut socket = match typ {
        #[cfg(windows)]
        SocketType::VmId { vm_id, port } => {
            let socket = hyperv::connect_vsock(driver, vm_id, port).await?;
            PolledSocket::new(driver, socket2::Socket::from(socket))?
        }
        SocketType::HybridVsock { path, port } => connect_hybrid_vsock(driver, path, port).await?,
    };

    // Read the 8 byte connection id which is always sent first on the connection.
    let mut id = [0; 8];
    socket
        .read_exact(&mut id)
        .await
        .context("reading connection id")?;
    let id = u64::from_ne_bytes(id);
    Ok((id, socket))
}

/// Represents different VM types.
#[derive(Clone)]
enum VmType {
    /// A Hyper-V VM represented by a VM ID GUID, which uses a VmSocket to connect.
    #[cfg(windows)]
    HyperV(guid::Guid),
    /// A VM which uses hybrid vsock over Unix sockets.
    HybridVsock(PathBuf),
    /// A VM that cannot be used for data connections.
    None,
}

/// The diagnostics client.
pub struct DiagClient {
    vm: VmType,
    ttrpc: mesh_rpc::Client,
    driver: Box<dyn Driver>,
}

/// Defines packet capture operations.
#[derive(PartialEq)]
pub enum PacketCaptureOperation {
    /// Query details.
    Query,
    /// Start packet capture.
    Start,
    /// Stop packet capture.
    Stop,
}

/// An error connecting to the diagnostics server.
#[derive(Debug, Error)]
#[error("failed to connect")]
pub struct ConnectError {
    #[source]
    err: anyhow::Error,
    kind: ConnectErrorKind,
}

#[derive(Debug)]
enum ConnectErrorKind {
    Other,
    VmNotStarted,
    ServerTimedOut,
}

impl ConnectError {
    /// Returns the time to wait before retrying the connection. If `None`, the
    /// connection should not be retried.
    pub fn retry_timeout(&self) -> Option<Duration> {
        match self.kind {
            ConnectErrorKind::VmNotStarted => Some(Duration::from_secs(1)),
            ConnectErrorKind::ServerTimedOut => {
                // The socket infrastructure has an internal timeout.
                Some(Duration::ZERO)
            }
            _ => None,
        }
    }

    fn other(err: impl Into<anyhow::Error>) -> Self {
        Self {
            err: err.into(),
            kind: ConnectErrorKind::Other,
        }
    }

    fn connect(err: std::io::Error) -> Self {
        let kind = match err.kind() {
            ErrorKind::AddrNotAvailable => ConnectErrorKind::VmNotStarted,
            ErrorKind::TimedOut => ConnectErrorKind::ServerTimedOut,
            _ => match err.raw_os_error() {
                #[cfg(windows)]
                Some(windows_sys::Win32::Networking::WinSock::WSAENETUNREACH) => {
                    ConnectErrorKind::VmNotStarted
                }
                _ => ConnectErrorKind::Other,
            },
        };
        Self {
            err: anyhow::Error::from(err).context("failed to connect"),
            kind,
        }
    }
}

struct VmConnector {
    vm: VmType,
    driver: Box<dyn Driver>,
}

impl mesh_rpc::client::Dial for VmConnector {
    type Stream = PolledSocket<socket2::Socket>;

    async fn dial(&mut self) -> std::io::Result<Self::Stream> {
        match &self.vm {
            #[cfg(windows)]
            VmType::HyperV(guid) => {
                let socket = hyperv::connect_vsock(
                    self.driver.as_ref(),
                    *guid,
                    diag_proto::VSOCK_CONTROL_PORT,
                )
                .await
                .map_err(std::io::Error::other)?;
                Ok(PolledSocket::new(&self.driver, socket.into())?)
            }
            VmType::HybridVsock(path) => {
                let socket = connect_hybrid_vsock(
                    self.driver.as_ref(),
                    path,
                    diag_proto::VSOCK_CONTROL_PORT,
                )
                .await
                .map_err(std::io::Error::other)?;
                Ok(socket)
            }
            VmType::None => unreachable!(),
        }
    }
}

impl DiagClient {
    /// Creates a client from Hyper-V VM name.
    #[cfg(windows)]
    pub fn from_hyperv_name(
        driver: impl Driver + Spawn + Clone,
        name: &str,
    ) -> anyhow::Result<Self> {
        Ok(Self::from_hyperv_id(
            driver,
            hyperv::vm_id_from_name(name).map_err(ConnectError::other)?,
        ))
    }

    /// Creates a client from a Hyper-V or HCS VM ID.
    #[cfg(windows)]
    pub fn from_hyperv_id(driver: impl Driver + Spawn + Clone, vm_id: guid::Guid) -> Self {
        let vm = VmType::HyperV(vm_id);
        Self::new(
            driver.clone(),
            vm.clone(),
            VmConnector {
                vm,
                driver: Box::new(driver),
            },
        )
    }

    /// Creates a client from a hybrid vsock Unix socket path.
    pub fn from_hybrid_vsock(driver: impl Driver + Spawn + Clone, path: &Path) -> Self {
        let vm = VmType::HybridVsock(path.into());
        Self::new(
            driver.clone(),
            vm.clone(),
            VmConnector {
                vm,
                driver: Box::new(driver.clone()),
            },
        )
    }

    /// Creates a client from a dialer.
    ///
    /// This client won't be usable with operations that require additional connections.
    pub fn from_dialer(driver: impl Driver + Spawn, conn: impl mesh_rpc::client::Dial) -> Self {
        Self::new(driver, VmType::None, conn)
    }

    fn new(driver: impl Driver + Spawn, vm: VmType, conn: impl mesh_rpc::client::Dial) -> Self {
        Self {
            vm,
            ttrpc: mesh_rpc::client::ClientBuilder::new()
                // Use a short reconnect timeout (compared to the normal 20
                // seconds) since the VM may start at any time.
                .retry_timeout(Duration::from_secs(1))
                .build(&driver, conn),
            driver: Box::new(driver),
        }
    }

    /// Waits for the paravisor to be ready for RPCs.
    pub async fn wait_for_server(&self) -> anyhow::Result<()> {
        match self
            .ttrpc
            .call()
            .wait_ready(true)
            .start(diag_proto::OpenhclDiag::Ping, ())
            .await
        {
            Ok(()) => {}
            Err(Status { code, .. }) if code == mesh_rpc::service::Code::Unimplemented as i32 => {
                // Older versions of the diag server don't support the ping
                // RPC, but an unimplemented failure is good enough to know
                // the server is ready.
            }
            Err(status) => return Err(grpc_status(status)),
        }
        Ok(())
    }

    /// Creates a builder for execing a command.
    pub fn exec(&self, command: impl AsRef<str>) -> ExecBuilder<'_> {
        ExecBuilder {
            client: self,
            with_stdin: false,
            with_stdout: false,
            with_stderr: false,
            request: ExecRequest {
                command: command.as_ref().to_owned(),
                ..Default::default()
            },
        }
    }

    /// Creates a new data connection socket.
    ///
    /// This can be used with [`DiagClient::custom_call`].
    pub async fn connect_data(&self) -> anyhow::Result<(u64, PolledSocket<socket2::Socket>)> {
        let socket_type = match &self.vm {
            #[cfg(windows)]
            VmType::HyperV(guid) => SocketType::VmId {
                vm_id: *guid,
                port: diag_proto::VSOCK_DATA_PORT,
            },
            VmType::HybridVsock(path) => SocketType::HybridVsock {
                path,
                port: diag_proto::VSOCK_DATA_PORT,
            },
            VmType::None => {
                anyhow::bail!("cannot make additional connections with this client")
            }
        };
        new_data_connection(self.driver.as_ref(), socket_type).await
    }

    /// Sends an inspection request to the server.
    pub async fn inspect(
        &self,
        path: impl Into<String>,
        depth: Option<usize>,
        timeout: Option<Duration>,
    ) -> anyhow::Result<Node> {
        let response = self.ttrpc.call().timeout(timeout).start(
            inspect_proto::InspectService::Inspect,
            inspect_proto::InspectRequest {
                path: path.into(),
                // It would be better to pass an Option<u32> in the proto, but that would break backcompat.
                depth: depth.unwrap_or(u32::MAX as usize) as u32,
            },
        );

        let response = response.await.map_err(grpc_status)?;
        Ok(response.result)
    }

    /// Updates an inspectable value.
    pub async fn update(
        &self,
        path: impl Into<String>,
        value: impl Into<String>,
    ) -> anyhow::Result<inspect::Value> {
        let response = self.ttrpc.call().start(
            inspect_proto::InspectService::Update,
            inspect_proto::UpdateRequest {
                path: path.into(),
                value: value.into(),
            },
        );

        let response = response.await.map_err(grpc_status)?;

        Ok(response.new_value)
    }

    /// Get PID of a given process
    pub async fn get_pid(&self, name: &str) -> anyhow::Result<i32> {
        let hosts = self.inspect("mesh/hosts", Some(1), None).await?;
        let mut plist = Vec::new();

        let Node::Dir(processes) = hosts else {
            anyhow::bail!("Hosts node is not a dir");
        };
        for process in processes {
            let Node::Dir(pnode) = process.node else {
                anyhow::bail!("Process node is not a dir");
            };
            for entry in pnode {
                if entry.name == "name" {
                    let Node::Value(value) = entry.node else {
                        anyhow::bail!("Name node is not a value");
                    };
                    let ValueKind::String(strval) = value.kind else {
                        anyhow::bail!("Name node is not a string");
                    };
                    if strval == name {
                        return Ok(process.name.parse()?);
                    }
                    plist.push(strval);
                }
            }
        }

        anyhow::bail!("PID of {name} not found. Processes: {:?}", plist)
    }

    /// Starts the VM.
    pub async fn start(
        &self,
        env: impl IntoIterator<Item = (String, Option<String>)>,
        args: impl IntoIterator<Item = String>,
    ) -> anyhow::Result<()> {
        let request = diag_proto::StartRequest {
            env: env
                .into_iter()
                .map(|(name, value)| diag_proto::EnvPair { name, value })
                .collect(),
            args: args.into_iter().collect(),
        };
        self.ttrpc
            .call()
            .start(diag_proto::UnderhillDiag::Start, request)
            .await
            .map_err(grpc_status)?;

        Ok(())
    }

    /// Gets the contents of /dev/kmsg
    pub async fn kmsg(&self, follow: bool) -> anyhow::Result<KmsgStream> {
        let (conn, socket) = self.connect_data().await?;

        self.ttrpc
            .call()
            .start(
                diag_proto::UnderhillDiag::Kmsg,
                diag_proto::KmsgRequest { follow, conn },
            )
            .await
            .map_err(grpc_status)?;

        Ok(KmsgStream::new(socket))
    }

    /// Gets the contents of the file
    pub async fn read_file(
        &self,
        follow: bool,
        file_path: String,
    ) -> anyhow::Result<PolledSocket<socket2::Socket>> {
        let (conn, socket) = self.connect_data().await?;

        self.ttrpc
            .call()
            .start(
                diag_proto::UnderhillDiag::ReadFile,
                diag_proto::FileRequest {
                    follow,
                    conn,
                    file_path,
                },
            )
            .await
            .map_err(grpc_status)?;

        Ok(socket)
    }

    /// Issues a call to the server using a custom RPC.
    ///
    /// This can be used to support extension RPCs that are not part of the main
    /// diagnostics service.
    pub fn custom_call(&self) -> mesh_rpc::client::CallBuilder<'_> {
        self.ttrpc.call()
    }

    /// Crashes the VM.
    pub async fn crash(&self, pid: i32) -> anyhow::Result<()> {
        self.ttrpc
            .call()
            .start(
                diag_proto::UnderhillDiag::Crash,
                diag_proto::CrashRequest { pid },
            )
            .await
            .map_err(grpc_status)?;

        Ok(())
    }

    /// Sets up network packet capture trace.
    pub async fn packet_capture(
        &self,
        op: PacketCaptureOperation,
        num_streams: u32,
        snaplen: u16,
    ) -> anyhow::Result<(Vec<PolledSocket<socket2::Socket>>, u32)> {
        let mut sockets = Vec::new();
        let op_data = match op {
            PacketCaptureOperation::Start => {
                let mut conns = Vec::new();
                for _ in 0..num_streams {
                    let (conn, socket) = self.connect_data().await?;
                    conns.push(conn);
                    sockets.push(socket);
                }
                Some(OpData::StartData(diag_proto::StartPacketCaptureData {
                    snaplen: snaplen.into(),
                    conns,
                }))
            }
            _ => None,
        };

        let operation = match op {
            PacketCaptureOperation::Query => Operation::Query,
            PacketCaptureOperation::Start => Operation::Start,
            PacketCaptureOperation::Stop => Operation::Stop,
        };

        let response = self
            .ttrpc
            .call()
            .start(
                diag_proto::UnderhillDiag::PacketCapture,
                diag_proto::NetworkPacketCaptureRequest {
                    operation: operation.into(),
                    op_data,
                },
            )
            .await
            .map_err(grpc_status)?;

        Ok((sockets, response.num_streams))
    }

    /// Saves a core dump file being streamed from Underhill
    pub async fn core_dump(
        &self,
        pid: i32,
        mut writer: impl AsyncWrite + Unpin,
        mut stderr: impl AsyncWrite + Unpin,
        verbose: bool,
    ) -> anyhow::Result<()> {
        // Launch hcl-dump to dump the target process. Use raw_socket_io so that
        // the diagnostics process does not have to be running during the core
        // dump process; this ensures that we can dump the diagnostics process,
        // too.
        let mut process = self.exec("/bin/underhill-dump");
        if verbose {
            process.args(["-v"]);
        }
        let mut process = process
            .args([pid.to_string()])
            .stdin(false)
            .stdout(true)
            .stderr(true)
            .raw_socket_io(true)
            .spawn()
            .await
            .context("failed to launch underhill-dump")?;

        let process_stdout = PolledSocket::new(&self.driver, process.stdout.take().unwrap())?;
        let process_stderr = PolledSocket::new(&self.driver, process.stderr.take().unwrap())?;

        let out = futures::io::copy(process_stdout, &mut writer);
        let err = futures::io::copy(process_stderr, &mut stderr);

        futures::try_join!(out, err)?;

        let status = process
            .wait()
            .await
            .context("failed to wait for underhill-dump")?;

        if !status.success() {
            anyhow::bail!(
                "underhill-dump failed with exit code {}",
                status.exit_code()
            );
        }
        Ok(())
    }

    /// Saves the memory profile trace being streamed from Underhill
    pub async fn memory_profile_trace(&self, pid: i32) -> anyhow::Result<Vec<u8>> {
        let state = self
            .ttrpc
            .call()
            .start(
                diag_proto::OpenhclDiag::MemoryProfileTrace,
                diag_proto::MemoryProfileTraceRequest { pid },
            )
            .await
            .map_err(grpc_status)?;

        Ok(state.data)
    }

    /// Restarts the Underhill worker.
    pub async fn restart(&self) -> anyhow::Result<()> {
        self.ttrpc
            .call()
            .start(diag_proto::UnderhillDiag::Restart, ())
            .await
            .map_err(grpc_status)?;

        Ok(())
    }

    /// Pause the VM (including all devices).
    pub async fn pause(&self) -> anyhow::Result<()> {
        self.ttrpc
            .call()
            .start(diag_proto::UnderhillDiag::Pause, ())
            .await
            .map_err(grpc_status)?;

        Ok(())
    }

    /// Resume the VM.
    pub async fn resume(&self) -> anyhow::Result<()> {
        self.ttrpc
            .call()
            .start(diag_proto::UnderhillDiag::Resume, ())
            .await
            .map_err(grpc_status)?;

        Ok(())
    }

    /// Dumps the VM's VTL2 saved state.
    pub async fn dump_saved_state(&self) -> anyhow::Result<Vec<u8>> {
        let state = self
            .ttrpc
            .call()
            .start(diag_proto::UnderhillDiag::DumpSavedState, ())
            .await
            .map_err(grpc_status)?;

        Ok(state.data)
    }
}

fn grpc_status(status: Status) -> anyhow::Error {
    anyhow::anyhow!(status.message)
}

/// A builder for launching a command in VTL2.
pub struct ExecBuilder<'a> {
    client: &'a DiagClient,
    with_stdin: bool,
    with_stdout: bool,
    with_stderr: bool,
    request: ExecRequest,
}

impl ExecBuilder<'_> {
    /// Adds `args` to the argument list.
    pub fn args<T: AsRef<str>>(&mut self, args: impl IntoIterator<Item = T>) -> &mut Self {
        self.request
            .args
            .extend(args.into_iter().map(|s| s.as_ref().to_owned()));
        self
    }

    /// Sets whether the process is spawned with a TTY.
    pub fn tty(&mut self, tty: bool) -> &mut Self {
        self.request.tty = tty;
        self
    }

    /// Specifies whether a stdin socket should be opened.
    pub fn stdin(&mut self, stdin: bool) -> &mut Self {
        self.with_stdin = stdin;
        self
    }

    /// Specifies whether a stdout socket should be opened.
    pub fn stdout(&mut self, stdout: bool) -> &mut Self {
        self.with_stdout = stdout;
        self
    }

    /// Specifies whether a stderr socket should be opened.
    pub fn stderr(&mut self, stderr: bool) -> &mut Self {
        self.with_stderr = stderr;
        self
    }

    /// Specifies whether the processes's stdout and stderr should be combined
    /// into a single stream (the stdout socket).
    pub fn combine_stderr(&mut self, combine_stderr: bool) -> &mut Self {
        self.request.combine_stderr = combine_stderr;
        self
    }

    /// Specifies whether the vsock sockets used for stdio should be passed
    /// directly to the launched process instead of going through relays.
    pub fn raw_socket_io(&mut self, raw_socket_io: bool) -> &mut Self {
        self.request.raw_socket_io = raw_socket_io;
        self
    }

    /// Clears the default environment.
    pub fn env_clear(&mut self) -> &mut Self {
        self.request.clear_env = true;
        self
    }

    /// Removes an environment variable.
    pub fn env_remove(&mut self, name: impl AsRef<str>) -> &mut Self {
        self.request.env.push(diag_proto::EnvPair {
            name: name.as_ref().to_owned(),
            value: None,
        });
        self
    }

    /// Sets an environment variable.
    pub fn env(&mut self, name: impl AsRef<str>, value: impl AsRef<str>) -> &mut Self {
        self.request.env.push(diag_proto::EnvPair {
            name: name.as_ref().to_owned(),
            value: Some(value.as_ref().to_owned()),
        });
        self
    }

    /// Spawns the process.
    pub async fn spawn(&self) -> anyhow::Result<Process> {
        let mut request = self.request.clone();

        let stdin = if self.with_stdin {
            let (id, stdin) = self
                .client
                .connect_data()
                .await
                .context("failed to connect stdin")?;
            request.stdin = id;

            Some(stdin.into_inner())
        } else {
            None
        };

        let stdout = if self.with_stdout {
            let (id, stdout) = self
                .client
                .connect_data()
                .await
                .context("failed to connect stdout")?;
            request.stdout = id;

            Some(stdout.into_inner())
        } else {
            None
        };

        let stderr = if self.with_stdout {
            let (id, stderr) = self
                .client
                .connect_data()
                .await
                .context("failed to connect stderr")?;
            request.stderr = id;

            Some(stderr.into_inner())
        } else {
            None
        };

        let response = self
            .client
            .ttrpc
            .call()
            .start(diag_proto::UnderhillDiag::Exec, request)
            .await
            .map_err(grpc_status)?;

        let wait = self.client.ttrpc.call().start(
            diag_proto::UnderhillDiag::Wait,
            WaitRequest { pid: response.pid },
        );

        Ok(Process {
            stdin,
            stdout,
            stderr,
            wait,
            pid: response.pid,
        })
    }
}

/// A process running in VTL2.
#[derive(Debug)]
pub struct Process {
    /// The standard input stream.
    pub stdin: Option<socket2::Socket>,
    /// The standard output stream.
    pub stdout: Option<socket2::Socket>,
    /// The standard error stream.
    pub stderr: Option<socket2::Socket>,
    pid: i32,
    wait: mesh_rpc::client::Call<WaitResponse>,
}

impl Process {
    /// Returns the process ID.
    pub fn id(&self) -> i32 {
        self.pid
    }

    /// Waits for the process to exit.
    pub async fn wait(self) -> anyhow::Result<ExitStatus> {
        let response = self
            .wait
            .await
            .map_err(|err| anyhow::anyhow!("{}", err.message))?;

        Ok(ExitStatus { response })
    }
}

/// Process exit status.
#[derive(Debug)]
pub struct ExitStatus {
    response: WaitResponse,
}

impl ExitStatus {
    /// The exit code.
    pub fn exit_code(&self) -> i32 {
        self.response.exit_code
    }

    /// Whether the process successfully terminated.
    pub fn success(&self) -> bool {
        self.response.exit_code == 0
    }
}
