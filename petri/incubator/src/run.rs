// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Top-level API to run a command inside an incubator.

use crate::profile::DeviceConfig;
use crate::profile::IncubatorBackend;
use crate::profile::IncubatorProfile;
use crate::qemu;
use anyhow::Context;
use futures::AsyncReadExt;
use futures_concurrency::future::Race;
use pal_async::pipe::PolledPipe;
use pal_async::process::PolledChild;
use pal_async::task::Spawn;
use std::collections::BTreeMap;
use std::io::IsTerminal;
use std::io::Write;
use std::path::Path;
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

    // --- build the init script ---
    // Sets up the environment, mounts the virtio-9p share, brings up
    // networking, and launches pipette in TCP mode. Pipette then waits for
    // the host to connect and send commands.

    // QEMU user-mode networking defaults: guest is 10.0.2.15/24, gateway 10.0.2.2,
    // DNS forwarder at 10.0.2.3.
    let init_script = "\
        #!/bin/sh\n\
        /bin/busybox --install /bin 2>/dev/null\n\
        mount -t devtmpfs none /dev\n\
        mount -t proc none /proc\n\
        mount -t sysfs none /sys\n\
        mkdir -p /dev/pts /share /root /tmp /etc\n\
        mount -t devpts devpts /dev/pts\n\
        mount -t 9p -o trans=virtio,version=9p2000.L hostshare /share\n\
        ip link set eth0 up\n\
        ip addr add 10.0.2.15/24 dev eth0\n\
        ip route add default via 10.0.2.2\n\
        echo 'nameserver 10.0.2.3' > /etc/resolv.conf\n\
        export VMM_TESTS_CONTENT_DIR=/share\n\
        export HOME=/root\n\
        cd /share\n\
        exec /share/pipette --transport tcp\n"
        .to_string();

    // --- inject init script into initrd ---

    let initrd_data = std::fs::read(&config.initrd).context("failed to read initrd")?;

    let patched_initrd = initrd_cpio::inject_into_initrd(
        &initrd_data,
        "tcg-init.sh",
        init_script.as_bytes(),
        0o100755, // regular file, rwxr-xr-x
    )
    .context("failed to inject init script into initrd")?;

    let patched_initrd_path = config.share_dir.join(".incubator-initrd.gz");
    std::fs::write(&patched_initrd_path, &patched_initrd)
        .context("failed to write patched initrd")?;

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

    let kernel_cmdline = format!("{} rdinit=/tcg-init.sh", qemu_config.cmdline);

    let mut cmd = qemu::build_qemu_command(
        qemu_config,
        &config.profile.devices,
        &config.kernel,
        &patched_initrd_path,
        &config.share_dir,
        host_port,
        &kernel_cmdline,
    );

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
        let serial_pipe = PolledPipe::new(&driver, child_pipe_to_file(qemu_stdout))
            .context("failed to create polled pipe for serial output")?;
        let serial_log_path = serial_log.clone();
        let relay_task = driver.spawn("serial-relay", async move {
            relay_serial_output(serial_pipe, &serial_log_path, ready_tx).await;
        });

        // Capture QEMU stderr for diagnostics.
        let stderr_pipe = PolledPipe::new(&driver, child_pipe_to_file(qemu_stderr))
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
    wait_for_pipette_ready(driver, config.timeout, qemu_child, ready_rx).await?;

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
    client.ping().await.context("ping failed")?;
    tracing::info!("ping OK");

    // Set up VFIO devices before running the guest command.
    let vfio_env = setup_vfio_devices(&client, &config.profile.devices).await?;

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

/// Wait for pipette to signal readiness via the serial console relay
/// thread. Races against QEMU exit and a timeout.
async fn wait_for_pipette_ready(
    driver: &impl pal_async::driver::Driver,
    timeout: Duration,
    qemu_child: &mut PolledChild<std::process::Child>,
    ready_rx: mesh::OneshotReceiver<()>,
) -> anyhow::Result<()> {
    enum Event {
        Ready,
        QemuExited(std::process::ExitStatus),
        Timeout,
    }

    let event = (
        async {
            match ready_rx.await {
                Ok(()) => Event::Ready,
                // Sender dropped without sending — relay thread exited
                // without seeing the marker (QEMU likely crashed).
                Err(_) => Event::QemuExited(std::process::ExitStatus::default()),
            }
        },
        async {
            match qemu_child.wait().await {
                Ok(status) => Event::QemuExited(status),
                Err(_) => Event::QemuExited(std::process::ExitStatus::default()),
            }
        },
        async {
            pal_async::timer::PolledTimer::new(driver)
                .sleep(timeout)
                .await;
            Event::Timeout
        },
    )
        .race()
        .await;

    match event {
        Event::Ready => Ok(()),
        Event::QemuExited(status) => {
            anyhow::bail!("QEMU exited before pipette was ready (status: {status})");
        }
        Event::Timeout => {
            anyhow::bail!("timed out waiting for pipette ready signal");
        }
    }
}

/// Relay QEMU serial output to a log file, signaling when
/// pipette's readiness marker appears.
async fn relay_serial_output(
    mut stdout: PolledPipe,
    log_path: &Path,
    ready_tx: mesh::OneshotSender<()>,
) {
    let mut log = match std::fs::File::create(log_path) {
        Ok(f) => f,
        Err(e) => {
            tracing::error!(error = %e, "failed to create serial log");
            return;
        }
    };

    let mut ready_tx = Some(ready_tx);
    let mut buf = vec![0u8; 4096];
    let mut line = Vec::new();

    loop {
        let n = match stdout.read(&mut buf).await {
            Ok(0) | Err(_) => break,
            Ok(n) => n,
        };

        let chunk = &buf[..n];
        let _ = log.write_all(chunk);

        // Scan for the readiness marker, line by line.
        if ready_tx.is_some() {
            for &byte in chunk {
                if byte == b'\n' {
                    if line
                        .windows(b"PIPETTE READY".len())
                        .any(|w| w == b"PIPETTE READY")
                    {
                        if let Some(tx) = ready_tx.take() {
                            tx.send(());
                        }
                    }
                    line.clear();
                } else {
                    line.push(byte);
                }
            }
        }
    }
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

/// Convert a child process's stdout/stderr pipe into a [`std::fs::File`] so it
/// can be wrapped in a [`PolledPipe`]. The owned-handle type differs by
/// platform, but the conversion is otherwise identical.
#[cfg(unix)]
fn child_pipe_to_file(pipe: impl Into<std::os::unix::io::OwnedFd>) -> std::fs::File {
    std::fs::File::from(pipe.into())
}

#[cfg(windows)]
fn child_pipe_to_file(pipe: impl Into<std::os::windows::io::OwnedHandle>) -> std::fs::File {
    std::fs::File::from(pipe.into())
}

/// Set up VFIO devices inside the incubator.
///
/// Each extra device in the profile sits behind its own PCIe root port
/// at a known PCI device number (see [`qemu::EXTRA_DEVICE_ADDR_BASE`]). This
/// function discovers the child device's BDF by finding the bridge at
/// that slot in sysfs, then unbinds the child from its driver and binds
/// it to vfio-pci.
///
/// Returns a map of environment variables to set for the guest command,
/// e.g., `INCUBATOR_VFIO_BDF_TEST_DISK=0000:01:00.0`.
async fn setup_vfio_devices(
    client: &pipette_client::PipetteClient,
    devices: &[DeviceConfig],
) -> anyhow::Result<BTreeMap<String, String>> {
    let mut env = BTreeMap::new();

    // Collect (device_index, config) for devices that need VFIO binding.
    let vfio_devices: Vec<_> = devices
        .iter()
        .enumerate()
        .filter_map(|(i, d)| match d {
            DeviceConfig::VirtioBlk(cfg) if cfg.vfio => Some((i, cfg)),
            DeviceConfig::VirtioBlk(_) => None,
        })
        .collect();

    if vfio_devices.is_empty() {
        return Ok(env);
    }

    tracing::info!("setting up {} VFIO device(s)", vfio_devices.len());

    for (device_index, cfg) in &vfio_devices {
        let addr = qemu::EXTRA_DEVICE_ADDR_BASE + device_index;

        // The root port for this device is deterministically at
        // 0000:00:{addr:02x}.0 (see `qemu::build_qemu_command`). Read its
        // secondary bus number from sysfs; the assigned device sits at
        // slot 0, function 0 of that bus.
        let rp_bdf = format!("0000:00:{addr:02x}.0");
        let secondary_bus_path = format!("/sys/bus/pci/devices/{rp_bdf}/secondary_bus_number");
        let secondary_bus_raw = client
            .read_file(&secondary_bus_path)
            .await
            .with_context(|| {
                format!(
                    "failed to read secondary bus number for device '{}' (root port {rp_bdf})",
                    cfg.name
                )
            })?;
        // sysfs reports the secondary bus number in decimal.
        let secondary_bus_str = String::from_utf8_lossy(&secondary_bus_raw);
        let secondary_bus: u8 = secondary_bus_str.trim().parse().with_context(|| {
            format!(
                "unexpected secondary bus number {secondary_bus_str:?} for device '{}'",
                cfg.name
            )
        })?;
        let bdf = format!("0000:{secondary_bus:02x}:00.0");

        // Confirm the child device actually exists before trying to rebind it.
        client
            .read_file(format!("/sys/bus/pci/devices/{bdf}/vendor"))
            .await
            .with_context(|| {
                format!(
                    "no device found behind root port {rp_bdf} (expected {bdf}) for device '{}'",
                    cfg.name
                )
            })?;

        tracing::info!(name = %cfg.name, %bdf, %addr, "binding device to vfio-pci");

        // Unbind from current driver
        let _ = client
            .write_file(
                format!("/sys/bus/pci/devices/{bdf}/driver/unbind"),
                bdf.as_bytes(),
            )
            .await;

        // Set driver override to vfio-pci
        client
            .write_file(
                format!("/sys/bus/pci/devices/{bdf}/driver_override"),
                b"vfio-pci".as_slice(),
            )
            .await
            .context("failed to set driver_override")?;

        // Bind to vfio-pci
        client
            .write_file("/sys/bus/pci/drivers/vfio-pci/bind", bdf.as_bytes())
            .await
            .context("failed to bind to vfio-pci")?;

        // Export env var: name "test-disk" → INCUBATOR_VFIO_BDF_TEST_DISK
        let env_name = format!(
            "INCUBATOR_VFIO_BDF_{}",
            cfg.name.to_uppercase().replace('-', "_")
        );
        tracing::info!(%env_name, %bdf, "VFIO device ready");
        env.insert(env_name, bdf);
    }

    Ok(env)
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
