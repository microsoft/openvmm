// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! The pipette protocol used for host-to-guest agent communications. It is
//! defined as messages over a mesh point-to-point connection.

#![forbid(unsafe_code)]

use mesh::MeshPayload;
use mesh::payload::Timestamp;
use mesh::pipe::ReadPipe;
use mesh::pipe::WritePipe;
use mesh::rpc::FailableRpc;
use mesh::rpc::Rpc;

/// The port used for the pipette connection over AF_VSOCK.
pub const PIPETTE_VSOCK_PORT: u32 = 0x1337;

/// The bootstrap message sent from the agent to the host.
#[derive(MeshPayload)]
pub struct PipetteBootstrap {
    /// The sender for requests to the agent.
    pub requests: mesh::Sender<PipetteRequest>,
    /// The receiver for diagnostics files from the agent.
    pub diag_file_recv: mesh::Receiver<DiagnosticFile>,
    /// The receiver on a channel closed when the agent exits.
    pub watch: mesh::OneshotReceiver<()>,
    /// The log channel.
    pub log: ReadPipe,
}

/// A request to the agent.
#[derive(MeshPayload)]
pub enum PipetteRequest {
    /// Pings the agent to check if it's alive.
    Ping(Rpc<(), ()>),
    /// Executes a command inside the guest.
    Execute(FailableRpc<ExecuteRequest, ExecuteResponse>),
    /// Powers off or reboots the guest.
    ///
    /// A successful response to this request may be lost depending on when
    /// pipette is terminated during the shutdown process.
    Shutdown(FailableRpc<ShutdownRequest, ()>),
    /// Reads the full contents of a file.
    ReadFile(FailableRpc<ReadFileRequest, u64>),
    /// Writes a file
    WriteFile(FailableRpc<WriteFileRequest, u64>),
    /// Get the current time in the guest.
    GetTime(Rpc<(), Timestamp>),
    /// Crash the agent.
    Crash(FailableRpc<(), ()>),
    /// Crash the kernel.
    KernelCrash(FailableRpc<(), ()>),
}

/// A request to execute a command inside the guest.
#[derive(MeshPayload, Default)]
pub struct ExecuteRequest {
    /// The program to execute.
    pub program: String,
    /// The arguments to the program.
    pub args: Vec<String>,
    /// The current working directory for the program.
    pub current_dir: Option<String>,
    /// The stdin for the program.
    pub stdin: Option<ReadPipe>,
    /// The stdout for the program.
    pub stdout: Option<WritePipe>,
    /// The stderr for the program.
    pub stderr: Option<WritePipe>,
    /// The environment variables for the program.
    pub env: Vec<EnvPair>,
    /// Whether to clear the environment before setting the new environment.
    pub clear_env: bool,
}

impl std::fmt::Debug for ExecuteRequest {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ExecuteRequest")
            .field("program", &self.program)
            .field("args", &self.args)
            .field("current_dir", &self.current_dir)
            .field("stdin", &self.stdin.is_some())
            .field("stdout", &self.stdout.is_some())
            .field("stderr", &self.stderr.is_some())
            .field("env", &self.env)
            .field("clear_env", &self.clear_env)
            .finish()
    }
}

/// A pair of environment variable name and value.
#[derive(MeshPayload, Clone, Debug)]
pub struct EnvPair {
    /// The name of the environment variable.
    pub name: String,
    /// The value of the environment variable, or `None` to remove the variable.
    pub value: Option<String>,
}

/// The response to a request to execute a command inside the guest.
#[derive(MeshPayload)]
pub struct ExecuteResponse {
    /// The process ID of the executed command.
    pub pid: u32,
    /// The process result channel. Receives the exit status of the process.
    pub result: mesh::OneshotReceiver<ExitStatus>,
}

/// The exit status of a process.
#[derive(Debug, MeshPayload, Clone)]
pub enum ExitStatus {
    /// The process exited normally with the given exit code.
    Normal(i32),
    /// The process was terminated by the given signal.
    Signal(i32),
    /// The process exited with an unknown status.
    Unknown,
}

impl From<std::process::ExitStatus> for ExitStatus {
    fn from(status: std::process::ExitStatus) -> Self {
        if let Some(code) = status.code() {
            return Self::Normal(code);
        }
        #[cfg(unix)]
        if let Some(signal) = std::os::unix::process::ExitStatusExt::signal(&status) {
            return Self::Signal(signal);
        }
        Self::Unknown
    }
}

/// A request to power off or reboot the guest.
#[derive(Copy, Clone, MeshPayload)]
pub struct ShutdownRequest {
    /// The type of shutdown to perform.
    pub shutdown_type: ShutdownType,
}

/// The type of shutdown to perform.
#[derive(Copy, Clone, Debug, MeshPayload)]
pub enum ShutdownType {
    /// Powers off the guest.
    PowerOff,
    /// Reboots the guest.
    Reboot,
}

/// A request to read a file.
#[derive(MeshPayload)]
pub struct ReadFileRequest {
    /// The path to read the file from.
    pub path: String,
    /// The sender for the contents of the file.
    pub sender: WritePipe,
}

/// A request to write a file.
#[derive(MeshPayload)]
pub struct WriteFileRequest {
    /// The path to write the file to.
    pub path: String,
    /// The receiver of the contents of the file.
    pub receiver: ReadPipe,
}

/// A file that the guest client wishes to be logged on the host for diagnostic purposes.
#[derive(MeshPayload)]
pub struct DiagnosticFile {
    /// The name of the file.
    pub name: String,
    /// The receiver of the contents of the file.
    pub receiver: ReadPipe,
}
