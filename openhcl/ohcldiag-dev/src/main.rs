// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! A "move fast, break things" tool, that provides no long-term CLI stability
//! guarantees.

#![expect(missing_docs)]
#![forbid(unsafe_code)]

mod completions;

use anyhow::Context;
use clap::ArgGroup;
use clap::Args;
use clap::Parser;
use clap::Subcommand;
use diag_client::DiagClient;
use diag_client::PacketCaptureOperation;
use futures::StreamExt;
use futures::io::AllowStdIo;
use futures_concurrency::future::Race;
use pal_async::DefaultPool;
use pal_async::driver::Driver;
use pal_async::socket::PolledSocket;
use pal_async::task::Spawn;
use pal_async::timer::PolledTimer;
use std::convert::Infallible;
use std::ffi::OsStr;
use std::io::ErrorKind;
use std::io::IsTerminal;
use std::io::Write;
use std::net::TcpListener;
use std::path::Path;
use std::path::PathBuf;
use std::str::FromStr;
use std::sync::Arc;
use std::time::Duration;
use thiserror::Error;
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;
use unicycle::FuturesUnordered;

#[derive(Parser)]
#[clap(about = "(dev) CLI to interact with the Underhill diagnostics server")]
#[clap(long_about = r#"
CLI to interact with the Underhill diagnostics server.

DISCLAIMER:
    `ohcldiag-dev` does not make ANY stability guarantees regarding the layout of
    the CLI, the syntax that is emitted via stdout/stderr, the location of nodes
    in the `inspect` graph, etc...

        !!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!
        !! ANY AUTOMATION THAT USES ohcldiag-dev WILL EVENTUALLY BREAK !!
        !!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!
"#)]
struct Options {
    #[clap(flatten)]
    vm: VmArg,

    #[clap(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    #[clap(hide = true)]
    Complete(clap_dyn_complete::Complete),
    Completions(completions::Completions),
    /// Starts an interactive terminal in VTL2.
    Shell {
        /// The shell process to start.
        #[clap(default_value = "/bin/sh")]
        shell: String,
        /// The arguments to pass to the shell process.
        args: Vec<String>,
    },
    /// Runs a process in VTL2.
    Run {
        /// The command to run.
        command: String,
        /// Arguments to pass to the command.
        args: Vec<String>,
    },
    /// Inspects the Underhill state.
    #[clap(visible_alias = "i")]
    Inspect {
        /// Recursively enumerate child nodes.
        #[clap(short)]
        recursive: bool,
        /// Limit the recursive inspection depth.
        #[clap(short, long, requires("recursive"))]
        limit: Option<usize>,
        /// Output in JSON format.
        #[clap(short, long)]
        json: bool,
        /// Poll periodically.
        #[clap(short)]
        poll: bool,
        /// The poll period in seconds.
        #[clap(long, default_value = "1", requires("poll"))]
        period: f64,
        /// The count of polls
        #[clap(long, requires("poll"))]
        count: Option<usize>,
        /// The path to inspect.
        path: Option<String>,
        /// Update the path with a new value.
        #[clap(short, long, conflicts_with("recursive"))]
        update: Option<String>,
        /// Timeout to wait for the inspection. 0 means no timeout.
        #[clap(short, default_value = "1", conflicts_with("update"))]
        timeout: u64,
    },
    /// Updates an inspectable value.
    #[clap(hide = true)]
    Update {
        /// The path.
        path: String,
        /// The new value.
        value: String,
    },
    /// Starts the VM if it's waiting for the signal to start.
    ///
    /// Underhill must have been started with --wait-for-start or
    /// OPENHCL_WAIT_FOR_START set.
    Start {
        /// Environment variables to set, in the form X=Y
        #[clap(short, long)]
        env: Vec<EnvString>,
        /// Environment variables to clear
        #[clap(short, long)]
        unset: Vec<String>,
        /// Extra command line arguments to append.
        args: Vec<String>,
    },
    /// Writes the contents of the kernel message buffer, /dev/kmsg.
    Kmsg {
        /// Keep waiting for and writing new data as its logged.
        #[clap(short, long)]
        follow: bool,
        /// Reconnect (retrying indefinitely) whenever the connection is lost.
        #[clap(short, long)]
        reconnect: bool,
        /// Write verbose information about the connection state.
        #[clap(short, long)]
        verbose: bool,
        /// Read kmsg from the VM's serial port.
        ///
        /// This only works on Hyper-V.
        #[cfg(windows)]
        #[clap(long, conflicts_with = "reconnect")]
        serial: bool,
        /// Pipe to read from for the serial port (or any other pipe)
        ///
        /// This only works on Hyper-V.
        #[cfg(windows)]
        #[clap(long, requires = "serial")]
        pipe_path: Option<String>,
    },
    /// Writes the contents of the file.
    File {
        /// Keep waiting for and writing new data as its logged.
        #[clap(short, long)]
        follow: bool,
        #[clap(short('p'), long)]
        file_path: String,
    },
    /// Starts GDB server on stdio.
    ///
    /// Use this with gdb's target command:
    ///
    ///     target remote |ohcldiag-dev.exe gdbserver my-vm
    ///
    /// Or for multi-process debugging:
    ///
    ///     target extended-remote |ohcldiag-dev.exe gdbserver --multi my-vm
    Gdbserver {
        /// The pid to attach to. Defaults to Underhill's.
        #[clap(long)]
        pid: Option<i32>,
        /// Use multi-process debugging, for use with gdb's extended-remote.
        #[clap(long, conflicts_with("pid"))]
        multi: bool,
    },
    /// Starts the GDB stub for debugging the guest on stdio.
    ///
    /// Use this with gdb's target command:
    ///
    ///     target remote |ohcldiag-dev.exe gdbstub my-vm
    ///
    Gdbstub {
        /// The vsock prot to connect to.
        #[clap(short, long, default_value = "4")]
        port: u32,
    },
    /// Crashes the VM.
    ///
    /// Must specify the VM name, as well as the crash type.
    #[clap(group(
        ArgGroup::new("process")
            .required(true)
            .args(&["pid", "name"]),
    ))]
    Crash {
        /// Type of crash.
        ///
        /// Current crash types supported: "panic"
        crash_type: CrashType,
        /// PID of underhill process to crash
        #[clap(short, long)]
        pid: Option<i32>,
        /// Name of underhill process to crash
        #[clap(short, long)]
        name: Option<String>,
    },
    /// Streams the ELF core dump file of a process to the host.
    ///
    /// Streams the core dump file of a process to the host where the file
    /// is saved as `dst`.
    #[clap(group(
        ArgGroup::new("process")
        .required(true)
        .args(&["pid", "name"]),
    ))]
    CoreDump {
        /// Enable verbose output.
        #[clap(short, long)]
        verbose: bool,
        /// PID of process to dump
        #[clap(short, long)]
        pid: Option<i32>,
        /// Name of underhill process to dump
        #[clap(short, long)]
        name: Option<String>,
        /// Destination file path. If omitted, the data is written to the standard
        /// output unless it is a terminal. In that case, an error is returned.
        dst: Option<PathBuf>,
    },
    /// Restarts the Underhill worker process, keeping VTL0 running.
    Restart,
    /// Get the current contents of the performance trace buffer, for use with
    /// <https://ui.perfetto.dev>.
    PerfTrace {
        /// The output file. Defaults to stdout.
        #[clap(short)]
        output: Option<PathBuf>,
    },
    /// Sets up a relay between a virtual socket and a TCP client on the host.
    VsockTcpRelay {
        vsock_port: u32,
        tcp_port: u16,
        #[clap(long)]
        allow_remote: bool,
        /// Reconnect (retrying indefinitely) whenever either side of the
        /// connection is lost.
        ///
        /// NOTE: Today, this does not handle the case where the vsock side is
        /// not ready to connect. That will cause the relay to terminate.
        #[clap(short, long)]
        reconnect: bool,
    },
    /// Pause the VM (including all devices)
    Pause,
    /// Resume the VM
    Resume,
    /// Dumps the VM's VTL2 state without servicing or tearing down Underhill.
    DumpSavedState {
        /// The output file. Defaults to stdout.
        #[clap(short)]
        output: Option<PathBuf>,
    },
    /// Starts a network packet capture trace.
    PacketCapture {
        /// Destination file path. nic index is appended to the file name.
        #[clap(short('w'), default_value = "nic")]
        output: PathBuf,
        /// Number of seconds for which to capture packets.
        #[clap(short('G'), long, default_value = "60", value_parser = |arg: &str| -> Result<Duration, std::num::ParseIntError> {Ok(Duration::from_secs(arg.parse()?))})]
        seconds: Duration,
        /// Length of the packet to capture.
        #[clap(short('s'), long, default_value = "65535", value_parser = clap::value_parser!(u16).range(1..))]
        snaplen: u16,
    },
    /// Memory usage profile tracing.
    MemoryProfileTrace {
        /// PID of process to collect the trace for
        #[clap(short, long)]
        pid: Option<i32>,
        /// Name of underhill process to dump
        #[clap(short, long)]
        name: Option<String>,
        /// The output file. Defaults to stdout.
        #[clap(short)]
        output: Option<PathBuf>,
    },
    /// Processes EFI diagnostics from guest memory and outputs the logs.
    ///
    /// The log level filter controls which UEFI log entries are emitted.
    /// The buffer already contains all log levels; this filter selects
    /// which ones to display.
    EfiDiagnostics {
        /// The log level filter to apply.
        ///
        /// Accepted values: "default" (errors+warnings), "info" (errors+warnings+info),
        /// "full" (all levels).
        #[clap(default_value = "default")]
        log_level: EfiDiagnosticsLogLevel,
        /// The output destination.
        #[clap(default_value = "stdout")]
        output: EfiDiagnosticsOutput,
    },
}

#[derive(Debug, Clone, Args)]
pub struct VmArg {
    #[doc = r#"VM identifier.

    This can be one of:

    * vsock:PATH - A path to a hybrid vsock Unix socket for a VM, as used by OpenVMM

    * unix:PATH - A path to a Unix socket for connecting to the control plane

    "#]
    #[cfg_attr(
        windows,
        doc = "* hyperv:NAME - A Hyper-V VM name

    "
    )]
    #[cfg_attr(
        windows,
        doc = "* NAME_OR_PATH - Either a Hyper-V VM name, or a path as in vsock:PATH>"
    )]
    #[cfg_attr(not(windows), doc = "* PATH - A path as in vsock:PATH")]
    #[clap(name = "VM")]
    id: VmId,
}

#[derive(Debug, Clone)]
enum VmId {
    #[cfg(windows)]
    HyperV(String),
    HybridVsock(PathBuf),
}

impl FromStr for VmId {
    type Err = Infallible;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        if let Some(s) = s.strip_prefix("vsock:") {
            Ok(Self::HybridVsock(Path::new(s).to_owned()))
        } else {
            #[cfg(windows)]
            if let Some(s) = s.strip_prefix("hyperv:") {
                return Ok(Self::HyperV(s.to_owned()));
            } else if !pal::windows::fs::is_unix_socket(s.as_ref()).unwrap_or(false) {
                return Ok(Self::HyperV(s.to_owned()));
            }
            // Default to hybrid vsock since this is what OpenVMM supports for
            // Underhill.
            Ok(Self::HybridVsock(Path::new(s).to_owned()))
        }
    }
}

#[derive(Clone)]
struct EnvString {
    name: String,
    value: String,
}

#[derive(Clone, clap::ValueEnum)]
enum CrashType {
    #[clap(name = "panic")]
    UhPanic,
}

#[derive(Clone, clap::ValueEnum)]
enum EfiDiagnosticsLogLevel {
    /// Errors and warnings only
    Default,
    /// Errors, warnings, and info
    Info,
    /// All log levels
    Full,
}

impl EfiDiagnosticsLogLevel {
    fn as_inspect_value(&self) -> &'static str {
        match self {
            EfiDiagnosticsLogLevel::Default => "default",
            EfiDiagnosticsLogLevel::Info => "info",
            EfiDiagnosticsLogLevel::Full => "full",
        }
    }
}

#[derive(Clone, clap::ValueEnum)]
enum EfiDiagnosticsOutput {
    /// Emit to stdout
    Stdout,
    /// Emit to tracing
    Tracing,
}

impl EfiDiagnosticsOutput {
    fn as_inspect_value(&self) -> &'static str {
        match self {
            EfiDiagnosticsOutput::Stdout => "stdout",
            EfiDiagnosticsOutput::Tracing => "tracing",
        }
    }
}

#[derive(Debug, Error)]
#[error("bad environment variable, expected VAR=value")]
struct BadEnvString;

impl FromStr for EnvString {
    type Err = BadEnvString;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let (name, value) = s.split_once('=').ok_or(BadEnvString)?;
        Ok(Self {
            name: name.to_owned(),
            value: value.to_owned(),
        })
    }
}

// N.B. this exits after a successful completion.
async fn run(
    client: &DiagClient,
    command: impl AsRef<str>,
    args: impl IntoIterator<Item = impl AsRef<str>>,
) -> anyhow::Result<()> {
    // TODO: if stdout and stderr of this process are backed by the
    // same thing, then pass combine_stderr instead.
    let mut process = client
        .exec(&command)
        .args(args)
        .stdin(true)
        .stdout(true)
        .stderr(true)
        .spawn()
        .await?;

    let mut stdin = process.stdin.take().unwrap();
    let mut stdout = process.stdout.take().unwrap();
    let mut stderr = process.stderr.take().unwrap();

    std::thread::spawn({
        move || {
            let _ = std::io::copy(&mut std::io::stdin(), &mut stdin);
        }
    });

    let stderr_thread =
        std::thread::spawn(move || std::io::copy(&mut stderr, &mut term::raw_stderr()));

    std::io::copy(&mut stdout, &mut term::raw_stdout()).context("failed stdout copy")?;

    stderr_thread
        .join()
        .unwrap()
        .context("failed stderr thread")?;

    let status = process.wait().await?;
    std::process::exit(status.exit_code());
}

fn new_client(driver: impl Driver + Spawn + Clone, input: &VmArg) -> anyhow::Result<DiagClient> {
    let client = match &input.id {
        #[cfg(windows)]
        VmId::HyperV(name) => DiagClient::from_hyperv_name(driver, name)?,
        VmId::HybridVsock(path) => DiagClient::from_hybrid_vsock(driver, path),
    };
    Ok(client)
}

pub fn main() -> anyhow::Result<()> {
    tracing_subscriber::registry()
        .with(tracing_subscriber::fmt::layer())
        .with(tracing_subscriber::EnvFilter::from_default_env())
        .init();

    term::enable_vt_and_utf8();
    DefaultPool::run_with(async |driver| {
        let Options { vm, command } = Options::parse();

        match command {
            Command::Complete(cmd) => {
                cmd.println_to_stub_script::<Options>(
                    None,
                    completions::OhcldiagDevCompleteFactory {
                        driver: driver.clone(),
                    },
                )
                .await
            }
            Command::Completions(cmd) => cmd.run()?,
            Command::Shell { shell, args } => {
                let client = new_client(driver.clone(), &vm)?;

                // Set TERM to ensure function keys and other characters work.
                let term = std::env::var("TERM");
                let term = term.as_deref().unwrap_or("xterm-256color");

                let mut process = client
                    .exec(&shell)
                    .args(&args)
                    .tty(true)
                    .stdin(true)
                    .stdout(true)
                    .env("TERM", term)
                    .spawn()
                    .await?;

                let mut stdin = process.stdin.take().unwrap();
                let mut stdout = process.stdout.take().unwrap();

                term::set_raw_console(true).expect("failed to set raw console mode");
                std::thread::spawn({
                    move || {
                        let _ = std::io::copy(&mut std::io::stdin(), &mut stdin);
                    }
                });

                std::io::copy(&mut stdout, &mut term::raw_stdout()).context("failed copy")?;

                let status = process.wait().await?;

                if !status.success() {
                    eprintln!(
                        "shell exited with non-zero exit code: {}",
                        status.exit_code()
                    );
                }
            }
            Command::Run { command, args } => {
                let client = new_client(driver.clone(), &vm)?;
                run(&client, command, &args).await?;
            }
            Command::Inspect {
                recursive,
                limit,
                json,
                poll,
                period,
                count,
                timeout,

                path,
                update,
            } => {
                let client = new_client(driver.clone(), &vm)?;

                if let Some(update) = update {
                    let Some(path) = path else {
                        anyhow::bail!("must provide path for update")
                    };

                    let value = client.update(path, update).await?;
                    match value.kind {
                        inspect::ValueKind::String(s) => println!("{s}"),
                        _ => println!("{value}"),
                    }
                } else {
                    let timeout = if timeout == 0 {
                        None
                    } else {
                        Some(Duration::from_secs(timeout))
                    };
                    let query = async || {
                        client
                            .inspect(
                                path.as_deref().unwrap_or(""),
                                if recursive { limit } else { Some(0) },
                                timeout,
                            )
                            .await
                    };

                    if poll {
                        let mut timer = PolledTimer::new(&driver);
                        let period = Duration::from_secs_f64(period);
                        let mut last_time = pal_async::timer::Instant::now();
                        let mut last = query().await?;
                        let mut count = count;

                        loop {
                            match count.as_mut() {
                                Some(count) if *count == 0 => break,
                                Some(count) => *count -= 1,
                                None => {}
                            }
                            timer.sleep_until(last_time + period).await;
                            let now = pal_async::timer::Instant::now();
                            let this = query().await?;
                            let diff = this.since(&last, now - last_time);
                            if json {
                                println!("{}", diff.json());
                            } else {
                                println!("{diff:#}");
                            }
                            last = this;
                            last_time = now;
                        }
                    } else {
                        let node = query().await?;
                        if json {
                            println!("{}", node.json());
                        } else {
                            println!("{node:#}");
                        }
                    }
                }
            }
            Command::Update { path, value } => {
                eprintln!(
                    "`update` is deprecated - please use `ohcldiag-dev inspect <path> -u <new value>`"
                );
                let client = new_client(driver.clone(), &vm)?;
                let value = client.update(path, value).await?;
                match value.kind {
                    inspect::ValueKind::String(s) => println!("{s}"),
                    _ => println!("{value}"),
                }
            }
            Command::Start { env, unset, args } => {
                let client = new_client(driver.clone(), &vm)?;

                let env = env
                    .into_iter()
                    .map(|EnvString { name, value }| (name, Some(value)))
                    .chain(unset.into_iter().map(|name| (name, None)));

                client.start(env, args).await?;
            }
            Command::Kmsg {
                follow,
                reconnect,
                verbose,
                #[cfg(windows)]
                serial,
                #[cfg(windows)]
                pipe_path,
            } => {
                let is_terminal = std::io::stdout().is_terminal();

                #[cfg(windows)]
                if serial {
                    use diag_client::hyperv::ComPortAccessInfo;
                    use futures::AsyncBufReadExt;

                    let vm_name = match &vm.id {
                        VmId::HyperV(name) => name,
                        _ => anyhow::bail!("--serial is only supported for Hyper-V VMs"),
                    };

                    let port_access_info = if let Some(pipe_path) = pipe_path.as_ref() {
                        ComPortAccessInfo::PortPipePath(pipe_path)
                    } else {
                        ComPortAccessInfo::NameAndPortNumber(vm_name, 3)
                    };

                    let pipe =
                        diag_client::hyperv::open_serial_port(&driver, port_access_info).await?;
                    let pipe = pal_async::pipe::PolledPipe::new(&driver, pipe)
                        .context("failed to make a polled pipe")?;
                    let pipe = futures::io::BufReader::new(pipe);

                    let mut lines = pipe.lines();
                    while let Some(line) = lines.next().await {
                        let line = line?;
                        if let Some(message) = kmsg::SyslogParsedEntry::new(&line) {
                            println!("{}", message.display(is_terminal));
                        } else {
                            println!("{line}");
                        }
                    }

                    return Ok(());
                }

                if verbose {
                    eprintln!("Connecting to the diagnostics server.");
                }

                let client = new_client(driver.clone(), &vm)?;
                'connect: loop {
                    if reconnect {
                        client.wait_for_server().await?;
                    }
                    let mut file_stream = client.kmsg(follow).await?;
                    if verbose {
                        eprintln!("Connected.");
                    }

                    while let Some(data) = file_stream.next().await {
                        match data {
                            Ok(data) => match kmsg::KmsgParsedEntry::new(&data) {
                                Ok(message) => println!("{}", message.display(is_terminal)),
                                Err(e) => println!("Invalid kmsg entry: {e:?}"),
                            },
                            Err(err) if reconnect && err.kind() == ErrorKind::ConnectionReset => {
                                if verbose {
                                    eprintln!(
                                        "Connection reset to the diagnostics server. Reconnecting."
                                    );
                                }
                                continue 'connect;
                            }
                            Err(err) => Err(err).context("failed to read kmsg")?,
                        }
                    }

                    if reconnect {
                        if verbose {
                            eprintln!("Lost connection to the diagnostics server. Reconnecting.");
                        }
                        continue 'connect;
                    }

                    break;
                }
            }
            Command::File { follow, file_path } => {
                let client = new_client(driver.clone(), &vm)?;
                let stream = client.read_file(follow, file_path).await?;
                futures::io::copy(stream, &mut AllowStdIo::new(term::raw_stdout()))
                    .await
                    .context("failed to copy trace file")?;
            }
            Command::Gdbserver { multi, pid } => {
                let client = new_client(driver.clone(), &vm)?;
                // Pass the --once flag so that gdbserver exits after the stdio
                // pipes are closed. Otherwise, gdbserver spins in a tight loop
                // and never exits.
                let gdbserver = "gdbserver --once";
                let command = if multi {
                    format!("{gdbserver} --multi -")
                } else if let Some(pid) = pid {
                    format!("{gdbserver} --attach - {pid}")
                } else {
                    format!("{gdbserver} --attach - \"$(cat /run/underhill.pid)\"")
                };

                run(&client, "/bin/sh", &["-c", &command]).await?;
            }
            Command::Gdbstub { port } => {
                let vsock = match vm.id {
                    VmId::HybridVsock(path) => {
                        diag_client::connect_hybrid_vsock(&driver, &path, port).await?
                    }
                    #[cfg(windows)]
                    VmId::HyperV(name) => {
                        let vm_id = diag_client::hyperv::vm_id_from_name(&name)?;
                        let stream =
                            diag_client::hyperv::connect_vsock(&driver, vm_id, port).await?;
                        PolledSocket::new(&driver, socket2::Socket::from(stream))?
                    }
                };

                let vsock = Arc::new(vsock.into_inner());
                // Spawn a thread to read stdin synchronously since pal_async
                // does not offer a way to read it asynchronously.
                let thread = std::thread::spawn({
                    let vsock = vsock.clone();
                    move || {
                        let _ = std::io::copy(&mut std::io::stdin(), &mut vsock.as_ref());
                    }
                });

                std::io::copy(&mut vsock.as_ref(), &mut term::raw_stdout())
                    .context("failed stdout copy")?;
                thread.join().unwrap();
            }
            Command::Crash {
                crash_type,
                pid,
                name,
            } => {
                let client = new_client(driver.clone(), &vm)?;
                let pid = if let Some(name) = name {
                    client.get_pid(&name).await?
                } else {
                    pid.unwrap()
                };
                println!("Crashing PID: {pid}");
                match crash_type {
                    CrashType::UhPanic => {
                        _ = client.crash(pid).await;
                    }
                }
            }
            Command::PacketCapture {
                output,
                seconds,
                snaplen,
            } => {
                let client = new_client(driver.clone(), &vm)?;
                println!(
                    "Starting network packet capture. Wait for timeout or Ctrl-C to quit anytime."
                );
                let (_, num_streams) = client
                    .packet_capture(PacketCaptureOperation::Query, 0, 0)
                    .await?;
                let file_stem = &output.file_stem().unwrap().to_string_lossy();
                let extension = &output.extension().unwrap_or(OsStr::new("pcap"));
                let mut new_output = PathBuf::from(&output);
                let streams = client
                    .packet_capture(PacketCaptureOperation::Start, num_streams, snaplen)
                    .await?
                    .0
                    .into_iter()
                    .enumerate()
                    .map(|(i, i_stream)| {
                        new_output.set_file_name(format!("{}-{}", &file_stem, i));
                        new_output.set_extension(extension);
                        let mut out = AllowStdIo::new(fs_err::File::create(&new_output)?);
                        Ok(async move { futures::io::copy(i_stream, &mut out).await })
                    })
                    .collect::<Result<Vec<_>, std::io::Error>>()?;
                capture_packets(client, streams, seconds).await;
            }
            Command::CoreDump {
                verbose,
                pid,
                name,
                dst,
            } => {
                ensure_not_terminal(&dst)?;
                let client = new_client(driver.clone(), &vm)?;
                let pid = if let Some(name) = name {
                    client.get_pid(&name).await?
                } else {
                    pid.unwrap()
                };
                println!("Dumping PID: {pid}");
                let file = create_or_stderr(&dst)?;
                client
                    .core_dump(
                        pid,
                        AllowStdIo::new(file),
                        AllowStdIo::new(std::io::stderr()),
                        verbose,
                    )
                    .await?;
            }
            Command::Restart => {
                let client = new_client(driver.clone(), &vm)?;
                client.restart().await?;
            }
            Command::PerfTrace { output } => {
                ensure_not_terminal(&output)?;

                let client = new_client(driver.clone(), &vm)?;

                // Flush the perf trace.
                client
                    .update("trace/perf/flush".to_owned(), "true".to_owned())
                    .await
                    .context("failed to flush perf")?;

                let file = create_or_stderr(&output)?;
                let stream = client
                    .read_file(false, "underhill.perfetto".to_owned())
                    .await
                    .context("failed to read trace file")?;

                futures::io::copy(stream, &mut AllowStdIo::new(file))
                    .await
                    .context("failed to copy trace file")?;
            }
            Command::VsockTcpRelay {
                vsock_port,
                tcp_port,
                allow_remote,
                reconnect,
            } => {
                let addr = if allow_remote { "0.0.0.0" } else { "127.0.0.1" };
                let listener = TcpListener::bind((addr, tcp_port))
                    .with_context(|| format!("binding to port {}", tcp_port))?;
                println!("TCP listening on {}:{}", addr, tcp_port);
                'connect: loop {
                    let (tcp_socket, tcp_addr) = listener.accept()?;
                    let tcp_socket = PolledSocket::new(&driver, tcp_socket)?;
                    println!("TCP accept on {:?}", tcp_addr);

                    // TODO: support reconnect attempt for vsock like kmsg
                    let vsock = match vm.id {
                        VmId::HybridVsock(ref path) => {
                            // TODO: reconnection attempt logic like kmsg is
                            // broken for hybrid_vsock with end of file error,
                            // if this is started before the vm is started
                            diag_client::connect_hybrid_vsock(&driver, path, vsock_port).await?
                        }
                        #[cfg(windows)]
                        VmId::HyperV(ref name) => {
                            let vm_id = diag_client::hyperv::vm_id_from_name(name)?;
                            let stream =
                                diag_client::hyperv::connect_vsock(&driver, vm_id, vsock_port)
                                    .await?;
                            PolledSocket::new(&driver, socket2::Socket::from(stream))?
                        }
                    };
                    println!("VSOCK connect to port {:?}", vsock_port);

                    let (tcp_read, mut tcp_write) = tcp_socket.split();
                    let (vsock_read, mut vsock_write) = vsock.split();
                    let tx = futures::io::copy(tcp_read, &mut vsock_write);
                    let rx = futures::io::copy(vsock_read, &mut tcp_write);
                    let result = futures::future::try_join(tx, rx).await;
                    match result {
                        Ok(_) => {}
                        Err(e) => match e.kind() {
                            ErrorKind::ConnectionReset => {}
                            _ => return Err(anyhow::Error::from(e)),
                        },
                    }
                    println!("Connection closed");

                    if reconnect {
                        println!("Reconnecting...");
                        continue 'connect;
                    }

                    break;
                }
            }
            Command::Pause => {
                let client = new_client(driver.clone(), &vm)?;
                client.pause().await?;
            }
            Command::Resume => {
                let client = new_client(driver.clone(), &vm)?;
                client.resume().await?;
            }
            Command::DumpSavedState { output } => {
                ensure_not_terminal(&output)?;
                let client = new_client(driver.clone(), &vm)?;
                let mut file = create_or_stderr(&output)?;
                file.write_all(&client.dump_saved_state().await?)?;
            }
            Command::MemoryProfileTrace { pid, name, output } => {
                let client = new_client(driver.clone(), &vm)?;
                let pid = if let Some(name) = name {
                    client.get_pid(&name).await?
                } else if let Some(pid) = pid {
                    pid
                } else {
                    anyhow::bail!("either --pid or --name must be specified");
                };
                // Do not write anything on the stdout in case the output
                // is set to stdout, to avoid breaking the output format
                // of the trace.
                let mut file = create_or_stderr(&output)?;
                file.write_all(&client.memory_profile_trace(pid).await?)?;
            }
            Command::EfiDiagnostics { log_level, output } => {
                let client = new_client(driver.clone(), &vm)?;
                let arg = format!(
                    "{},{}",
                    log_level.as_inspect_value(),
                    output.as_inspect_value()
                );
                let value = client
                    .update("vm/uefi/dump_efi_diagnostics", &arg)
                    .await
                    .context("failed to process EFI diagnostics")?;
                match value.kind {
                    inspect::ValueKind::String(s) => print!("{s}"),
                    _ => print!("{value}"),
                }
            }
        }
        Ok(())
    })
}

fn ensure_not_terminal(path: &Option<PathBuf>) -> anyhow::Result<()> {
    if path.is_none() && std::io::stdout().is_terminal() {
        anyhow::bail!("cannot write to terminal");
    }
    Ok(())
}

fn create_or_stderr(path: &Option<PathBuf>) -> std::io::Result<fs_err::File> {
    let file = match path {
        Some(path) => fs_err::File::create(path)?,
        None => fs_err::File::from_parts(term::raw_stdout(), "stdout"),
    };
    Ok(file)
}

async fn capture_packets(
    client: DiagClient,
    streams: Vec<impl Future<Output = Result<u64, std::io::Error>>>,
    capture_duration: Duration,
) {
    let mut capture_streams = FuturesUnordered::from_iter(streams);
    let (user_input_tx, mut user_input_rx) = mesh::channel();
    ctrlc::set_handler(move || user_input_tx.send(())).expect("Error setting Ctrl-C handler");

    let mut ctx = mesh::CancelContext::new().with_timeout(capture_duration);
    let mut stop_signaled = std::pin::pin!(ctx.until_cancelled(user_input_rx.recv()));

    let mut stop_streams = std::pin::pin!(async {
        if let Err(err) = client
            .packet_capture(PacketCaptureOperation::Stop, 0, 0)
            .await
        {
            eprintln!("Failed stop: {err}");
        }
    });

    #[derive(PartialEq)]
    enum State {
        Running,
        Stopping,
        StoppingStreamsDone,
        Stopped,
    }
    let mut state = State::Running;
    loop {
        enum Event {
            Continue,
            StopSignaled,
            StopComplete,
            StreamsDone,
        }
        let stop = async {
            match state {
                State::Running => {
                    (&mut stop_signaled).await.ok();
                    Event::StopSignaled
                }
                State::Stopping | State::StoppingStreamsDone => {
                    (&mut stop_streams).await;
                    Event::StopComplete
                }
                State::Stopped => std::future::pending::<Event>().await,
            }
        };
        let process_streams = async {
            if state == State::StoppingStreamsDone {
                std::future::pending::<()>().await;
            }
            match capture_streams.next().await {
                Some(_) => Event::Continue,
                None => Event::StreamsDone,
            }
        };
        let event = (stop, process_streams).race();

        // N.B Wait for all the copy tasks to complete to make sure the data is flushed to
        //     ensure compatibility with the packet capture protocol.
        match event.await {
            Event::Continue => continue,
            Event::StopSignaled => {
                println!("Stopping packet capture...");
                state = State::Stopping;
            }
            Event::StopComplete => {
                println!("Waiting for data to be flushed...");
                if state == State::Stopping {
                    state = State::Stopped;
                } else {
                    break;
                }
            }
            Event::StreamsDone if state == State::Stopping => {
                state = State::StoppingStreamsDone;
            }
            Event::StreamsDone => {
                if state != State::Stopped {
                    println!("Lost connection with network.");
                }
                break;
            }
        }
    }
    println!("All done.");
}
