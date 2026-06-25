// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Top-level API to run a command inside an incubator.

use crate::profile::IncubatorBackend;
use crate::profile::IncubatorProfile;
use crate::qemu;
use anyhow::Context;
use futures::AsyncReadExt;
use pal_async::pipe::PolledPipe;
use pal_async::process::PolledChild;
use pal_async::task::Spawn;
use std::io::IsTerminal;
use std::path::PathBuf;
use std::time::Duration;
use std::time::Instant;

/// Configuration for an incubator run.
pub struct IncubatorConfig {
    /// The parsed profile.
    pub profile: IncubatorProfile,
    /// Path to the guest kernel image.
    pub kernel: PathBuf,
    /// Path to the base initrd (gzip-compressed CPIO).
    pub initrd: PathBuf,
    /// Directory to share into the VM at `/share`.
    pub share_dir: PathBuf,
    /// The command to run inside the VM: program followed by arguments.
    pub guest_command: Vec<String>,
    /// Timeout for the VM to boot and pipette to become ready. Once pipette
    /// is connected, the guest command itself runs without a timeout.
    pub timeout: Duration,
    /// If set, override the QEMU binary path specified in the profile.
    pub qemu_binary_override: Option<PathBuf>,
}

/// Result of an incubator run.
pub struct IncubatorOutput {
    /// The guest command's exit code, if it was captured.
    pub exit_code: Option<i32>,
    /// Total wall time for the run.
    pub elapsed: Duration,
}

/// Run a command inside an incubator.
///
/// Boots an emulated VM according to the profile, mounts `share_dir` at
/// `/share` inside the guest, connects to pipette over TCP, executes the
/// command, and returns the exit code. Stdout/stderr are relayed to the
/// host process in real time.
pub fn run_in_incubator(config: IncubatorConfig) -> anyhow::Result<IncubatorOutput> {
    let start = Instant::now();

    // --- pick a host port for pipette TCP forwarding ---

    let host_port = pick_free_port().context("failed to find a free port")?;

    // --- prepare the boot initrd (inject the init script) ---

    let patched_initrd_path = qemu::prepare_initrd(&config.initrd, &config.share_dir)?;

    // --- launch QEMU ---

    let IncubatorBackend::QemuTcg(ref qemu_config) = config.profile.incubator;

    // Apply QEMU binary override if specified.
    let qemu_config_override;
    let qemu_config = if let Some(ref qemu_binary) = config.qemu_binary_override {
        qemu_config_override = crate::profile::QemuTcgConfig {
            binary: qemu_binary.display().to_string(),
            ..qemu_config.clone()
        };
        &qemu_config_override
    } else {
        qemu_config
    };

    let mut cmd = qemu::build_qemu_command(
        qemu_config,
        &config.profile.devices,
        &config.kernel,
        &patched_initrd_path,
        &config.share_dir,
        host_port,
    )?;

    // QEMU runs in the background. Serial console goes to a pipe;
    // an async task copies output to a log file and signals when
    // pipette prints its readiness marker.
    let serial_log = config.share_dir.join("incubator-serial.log");
    tracing::info!(path = %serial_log.display(), "serial log");
    cmd.stdin(std::process::Stdio::null());
    cmd.stdout(std::process::Stdio::piped());
    cmd.stderr(std::process::Stdio::piped());

    let mut qemu_child = cmd.spawn().context("failed to launch QEMU")?;
    let qemu_stdout = qemu_child.stdout.take().expect("stdout should be piped");
    let qemu_stderr = qemu_child.stderr.take().expect("stderr should be piped");

    // --- run everything inside the async executor ---

    let result: anyhow::Result<_> = pal_async::DefaultPool::run_with(async |driver| {
        let mut qemu_child = PolledChild::<std::process::Child>::new(&driver, qemu_child)
            .context("failed to create PolledChild")?;

        // Relay serial output to the log file in a spawned task.
        // Sends a signal when pipette's "PIPETTE READY" marker appears.
        let (ready_tx, ready_rx) = mesh::oneshot::<()>();
        let serial_pipe = PolledPipe::new(&driver, qemu::child_pipe_to_file(qemu_stdout))
            .context("failed to create polled pipe for serial output")?;
        let serial_log_path = serial_log.clone();
        let relay_task = driver.spawn("serial-relay", async move {
            qemu::relay_serial_output(serial_pipe, &serial_log_path, ready_tx).await;
        });

        // Capture QEMU stderr for diagnostics.
        let stderr_pipe = PolledPipe::new(&driver, qemu::child_pipe_to_file(qemu_stderr))
            .context("failed to create polled pipe for stderr")?;
        let stderr_task = driver.spawn("qemu-stderr", async move {
            let mut buf = Vec::new();
            let mut pipe = stderr_pipe;
            let _ = pipe.read_to_end(&mut buf).await;
            String::from_utf8_lossy(&buf).to_string()
        });

        let result = run_via_pipette(&driver, host_port, &config, &mut qemu_child, ready_rx).await;

        let exit_code = match result {
            Ok(code) => Some(code),
            Err(e) => {
                tracing::error!("pipette session failed: {e:#}");
                None
            }
        };

        // On success, pipette sent a power_off so QEMU should exit soon.
        // On failure, QEMU is still running — kill it.
        let child = qemu_child.get_mut();
        if exit_code.is_none() {
            let _ = child.kill();
        }
        let _ = child.wait();

        // Wait for the serial relay to finish flushing.
        relay_task.await;

        // Log any QEMU stderr output.
        let stderr_output = stderr_task.await;
        if !stderr_output.is_empty() {
            tracing::warn!(stderr = %stderr_output, "QEMU stderr output");
        }

        Ok(exit_code)
    });

    let elapsed = start.elapsed();

    Ok(IncubatorOutput {
        exit_code: result?,
        elapsed,
    })
}

/// Connect to pipette inside the VM over TCP and execute the command.
async fn run_via_pipette(
    driver: &pal_async::DefaultDriver,
    host_port: u16,
    config: &IncubatorConfig,
    qemu_child: &mut PolledChild<std::process::Child>,
    ready_rx: mesh::OneshotReceiver<()>,
) -> anyhow::Result<i32> {
    // Wait for pipette to print its readiness marker on the serial
    // console, or for QEMU to exit (indicating a boot failure).
    let addr = std::net::SocketAddr::from(([127, 0, 0, 1], host_port));
    tracing::info!(%addr, "waiting for pipette ready signal");
    qemu::wait_for_pipette_ready(driver, config.timeout, qemu_child, ready_rx).await?;

    tracing::info!("pipette ready, connecting");
    let conn = pal_async::socket::PolledSocket::connect_tcp(driver, addr)
        .await
        .context("failed to connect to pipette")?;

    let output_dir = config.share_dir.join("test_results");
    std::fs::create_dir_all(&output_dir).context("failed to create test results dir")?;

    let client = pipette_client::PipetteClient::new(&driver, conn, &output_dir)
        .await
        .context("failed to connect to pipette")?;

    tracing::info!("connected to pipette");

    // Set up VFIO devices before running the guest command.
    let vfio_env = qemu::setup_vfio_devices(&client, &config.profile.devices).await?;

    tracing::info!("executing command");

    let (program, args) = config
        .guest_command
        .split_first()
        .context("empty guest command")?;

    let use_pty = std::io::stdin().is_terminal();

    let mut cmd = client.command(program);
    cmd.args(args);
    cmd.env("VMM_TESTS_CONTENT_DIR", "/share");
    cmd.env("HOME", "/root");
    cmd.current_dir("/share");

    // Pass VFIO device BDFs as environment variables
    for (key, value) in &vfio_env {
        cmd.env(key, value);
    }

    if use_pty {
        cmd.pty(true);
    }

    // Put the host terminal into raw mode so that Ctrl-C, etc.
    // flow through to the guest PTY instead of being handled locally.
    let raw_guard = if use_pty {
        Some(RawModeGuard::enter().context("failed to enter raw mode")?)
    } else {
        None
    };

    let result = async {
        let mut child = cmd
            .spawn()
            .await
            .context("failed to spawn command in guest")?;
        child.wait().await.context("failed to wait for command")
    }
    .await;

    // Restore terminal before printing anything.
    drop(raw_guard);

    let status = result?;
    tracing::info!(%status, "command exited");

    let exit_code = if let Some(code) = status.code() {
        code
    } else if let Some(signal) = status.signal() {
        tracing::warn!("command killed by signal {signal}");
        128 + signal
    } else {
        tracing::warn!("command exited with unknown status");
        1
    };

    // Power off the VM
    let _ = client.power_off().await;

    Ok(exit_code)
}

/// Find a free TCP port by binding to port 0 and reading the assigned port.
fn pick_free_port() -> anyhow::Result<u16> {
    let listener =
        std::net::TcpListener::bind("127.0.0.1:0").context("failed to bind ephemeral port")?;
    let port = listener
        .local_addr()
        .context("failed to get local addr")?
        .port();
    Ok(port)
}

/// RAII guard that puts the terminal into raw mode and restores it on drop,
/// so that Ctrl-C and similar control sequences flow through to the guest PTY
/// instead of being interpreted by the host terminal.
struct RawModeGuard;

impl RawModeGuard {
    fn enter() -> anyhow::Result<Self> {
        crossterm::terminal::enable_raw_mode().context("failed to enable raw mode")?;
        Ok(Self)
    }
}

impl Drop for RawModeGuard {
    fn drop(&mut self) {
        if let Err(e) = crossterm::terminal::disable_raw_mode() {
            tracing::warn!(error = %e, "failed to restore terminal mode");
        }
    }
}
