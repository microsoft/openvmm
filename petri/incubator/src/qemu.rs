// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! QEMU process management.

use crate::GUEST_SHARE_ROOT;
use crate::profile::DeviceConfig;
use crate::profile::QemuTcgConfig;
use anyhow::Context;
use futures::AsyncReadExt;
use futures_concurrency::future::Race;
use pal_async::pipe::PolledPipe;
use pal_async::process::PolledChild;
use std::collections::BTreeMap;
use std::io::Write;
use std::path::Path;
use std::process::Command;
use std::time::Duration;

/// Filename of the injected init script, run by the kernel as `rdinit`.
const INIT_SCRIPT_NAME: &str = "tcg-init.sh";

/// Build the QEMU command line for a TCG launch.
pub fn build_qemu_command(
    config: &QemuTcgConfig,
    devices: &[DeviceConfig],
    kernel: &Path,
    initrd: &Path,
    share_dir: &Path,
    host_pipette_port: u16,
) -> anyhow::Result<Command> {
    let mut cmd = Command::new(&config.binary);

    cmd.arg("-machine").arg(&config.machine);
    cmd.arg("-cpu").arg(&config.cpu);
    cmd.arg("-m").arg(&config.memory);
    cmd.arg("-smp").arg(&config.smp);
    cmd.arg("-nographic");
    cmd.arg("-kernel").arg(kernel);
    cmd.arg("-initrd").arg(initrd);
    cmd.arg("-append")
        .arg(format!("{} rdinit=/{INIT_SCRIPT_NAME}", config.cmdline));
    cmd.arg("-no-reboot");

    // 9p: share the host directory into the guest
    cmd.arg("-fsdev").arg(format!(
        "local,id=fsdev0,path={},security_model=none",
        share_dir.display()
    ));
    cmd.arg("-device")
        .arg("virtio-9p-pci,fsdev=fsdev0,mount_tag=hostshare");

    // User-mode networking with port forwarding for pipette TCP
    cmd.arg("-netdev").arg(format!(
        "user,id=net0,hostfwd=tcp::{host_pipette_port}-:{guest_port}",
        guest_port = pipette_client::PIPETTE_PORT,
    ));
    cmd.arg("-device")
        .arg("virtio-net-pci,netdev=net0,romfile=");

    // Console on serial (diagnostic only)
    cmd.arg("-serial").arg("mon:stdio");

    // Extra devices from the profile.
    // Each device gets its own PCIe root port at a known PCI device number
    // (`addr=`), so the VFIO setup code can find the bridge by its devfn
    // in sysfs and enumerate the child behind it.
    for (i, device) in devices.iter().enumerate() {
        let rp_id = format!("hosting_rp{i}");
        let addr = EXTRA_DEVICE_ADDR_BASE + i;
        // Each root port needs a unique `slot` within its chassis. QEMU
        // defaults every pcie-root-port to `chassis=0,slot=0`, so a second
        // port collides with "Can't add chassis slot, error -16" (EBUSY).
        // Number the slots from 1.
        let slot = i + 1;
        cmd.arg("-device").arg(format!(
            "pcie-root-port,id={rp_id},addr={addr:#x},slot={slot}"
        ));

        match device {
            DeviceConfig::VirtioBlk(cfg) => {
                let node_name = format!("disk{i}");
                let size_bytes = parse_size(&cfg.size)
                    .with_context(|| format!("invalid size for device '{}'", cfg.name))?;
                cmd.arg("-blockdev")
                    .arg(format!("null-co,node-name={node_name},size={size_bytes}"));
                cmd.arg("-device")
                    .arg(format!("virtio-blk-pci,drive={node_name},bus={rp_id},iommu_platform=on,disable-legacy=on,romfile="));
            }
            DeviceConfig::Edu(cfg) => {
                // A conventional PCI endpoint with a register-programmed DMA
                // engine. Widen `dma_mask` when provided so the engine can
                // address high aarch64 guest physical addresses (its 28-bit
                // default would clamp them).
                let mut dev = format!("edu,bus={rp_id}");
                if let Some(mask) = &cfg.dma_mask {
                    dev.push_str(&format!(",dma_mask={mask}"));
                }
                cmd.arg("-device").arg(dev);
            }
            DeviceConfig::IvshmemPlain(cfg) => {
                // BAR2 is a prefetchable, RAM-backed memory window served by a
                // host memory backend — a valid P2P DMA target BAR.
                let mem_id = format!("ivshmem_mem{i}");
                let size_bytes = parse_size(&cfg.size)
                    .with_context(|| format!("invalid size for device '{}'", cfg.name))?;
                cmd.arg("-object")
                    .arg(format!("memory-backend-ram,id={mem_id},size={size_bytes}"));
                cmd.arg("-device")
                    .arg(format!("ivshmem-plain,memdev={mem_id},bus={rp_id}"));
            }
        }
    }

    Ok(cmd)
}

/// First PCI device number (`addr=`) used for extra-device root ports.
///
/// QEMU's built-in devices use low device numbers. We start at 16 (0x10)
/// to avoid collisions. The root port for the i-th extra device has
/// devfn = `(EXTRA_DEVICE_ADDR_BASE + i) << 3`.
const EXTRA_DEVICE_ADDR_BASE: usize = 16;

/// Parse a human-readable size string (e.g., "64M", "1G", "512K") to bytes.
///
/// Accepts the suffixes K, M, G, and T, optionally followed by B. A plain
/// integer (no suffix) is interpreted as a byte count.
//
// Copied from openvmm's CLI memory parser (`parse_memory` in
// openvmm_entry::cli_args) to keep behavior consistent without taking a
// dependency just to share this small helper.
fn parse_size(s: &str) -> anyhow::Result<u64> {
    || -> Option<u64> {
        let mut b = s.as_bytes();
        if s.ends_with('B') {
            b = &b[..b.len() - 1]
        }
        if b.is_empty() {
            return None;
        }
        let multi = match b[b.len() - 1] as char {
            'T' => Some(1024 * 1024 * 1024 * 1024),
            'G' => Some(1024 * 1024 * 1024),
            'M' => Some(1024 * 1024),
            'K' => Some(1024),
            _ => None,
        };
        if multi.is_some() {
            b = &b[..b.len() - 1]
        }
        let n: u64 = std::str::from_utf8(b).ok()?.parse().ok()?;
        n.checked_mul(multi.unwrap_or(1))
    }()
    .with_context(|| format!("invalid size '{s}'"))
}

/// Build the guest init script (`rdinit`).
///
/// Sets up the environment, mounts the virtio-9p share, brings up networking,
/// and launches pipette in TCP mode. Pipette then waits for the host to
/// connect and send commands.
fn build_init_script(guest_pipette_path: &str) -> String {
    let guest_pipette_path = shell_single_quote(guest_pipette_path);
    let guest_share_root = shell_single_quote(GUEST_SHARE_ROOT);

    // QEMU user-mode networking defaults: guest is 10.0.2.15/24,
    // gateway 10.0.2.2, DNS forwarder at 10.0.2.3.
    format!(
        "\
        #!/bin/sh\n\
        /bin/busybox --install /bin 2>/dev/null\n\
        mount -t devtmpfs none /dev\n\
        mount -t proc none /proc\n\
        mount -t sysfs none /sys\n\
        mkdir -p /dev/pts {guest_share_root} /root /tmp /etc\n\
        mount -t devpts devpts /dev/pts\n\
        mount -t 9p -o trans=virtio,version=9p2000.L hostshare {guest_share_root}\n\
        ip link set eth0 up\n\
        ip addr add 10.0.2.15/24 dev eth0\n\
        ip route add default via 10.0.2.2\n\
        echo 'nameserver 10.0.2.3' > /etc/resolv.conf\n\
        export HOME=/root\n\
        cd {guest_share_root}\n\
        exec {guest_pipette_path} --transport tcp\n"
    )
}

fn shell_single_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}

/// Prepare the boot initrd by injecting the init script into the base initrd.
///
/// Reads the gzip-compressed base initrd, injects the `rdinit` script (see
/// [`build_init_script`]) under [`INIT_SCRIPT_NAME`], and writes the patched
/// initrd to a uniquely-named temporary file under `scratch_dir`. The returned
/// [`tempfile::TempPath`] deletes the file when dropped, so the caller must
/// keep it alive for as long as QEMU needs to read the initrd.
///
/// A unique temp file (rather than a fixed name) is required because multiple
/// incubator processes run concurrently under nextest and share the same
/// output directory; a fixed path would race.
pub fn prepare_initrd(
    base_initrd: &Path,
    scratch_dir: &Path,
    guest_pipette_path: &str,
) -> anyhow::Result<tempfile::TempPath> {
    let initrd_data = std::fs::read(base_initrd).context("failed to read initrd")?;

    let patched_initrd = initrd_cpio::inject_into_initrd(
        &initrd_data,
        INIT_SCRIPT_NAME,
        build_init_script(guest_pipette_path).as_bytes(),
        0o100755, // regular file, rwxr-xr-x
    )
    .context("failed to inject init script into initrd")?;

    std::fs::create_dir_all(scratch_dir).context("failed to create incubator output dir")?;
    let mut patched_initrd_file = tempfile::Builder::new()
        .prefix(".incubator-initrd")
        .suffix(".gz")
        .tempfile_in(scratch_dir)
        .context("failed to create patched initrd temp file")?;
    patched_initrd_file
        .write_all(&patched_initrd)
        .context("failed to write patched initrd")?;

    Ok(patched_initrd_file.into_temp_path())
}

/// Wait for pipette to signal readiness via the serial console relay
/// task. Races against QEMU exit and a timeout.
pub async fn wait_for_pipette_ready(
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
                // Sender dropped without sending — relay task exited
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
pub async fn relay_serial_output(
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
    // Rolling window of recently-seen bytes, used to detect the readiness
    // marker even if it straddles a read boundary. We never need to retain
    // more than `marker.len() - 1` bytes between chunks (anything longer
    // could not be the start of a new match), so this stays small.
    let marker = pipette_client::PIPETTE_READY_MARKER.as_bytes();
    let mut window = Vec::new();

    loop {
        let n = match stdout.read(&mut buf).await {
            Ok(0) | Err(_) => break,
            Ok(n) => n,
        };

        let chunk = &buf[..n];
        let _ = log.write_all(chunk);

        // Scan for the readiness marker. This does not depend on the marker
        // being newline-terminated: we append the new bytes and look for the
        // marker anywhere in the window, then trim the window back down.
        if ready_tx.is_some() {
            window.extend_from_slice(chunk);
            if window.windows(marker.len()).any(|w| w == marker) {
                if let Some(tx) = ready_tx.take() {
                    tx.send(());
                }
            } else {
                let keep = marker.len().saturating_sub(1);
                if window.len() > keep {
                    window.drain(..window.len() - keep);
                }
            }
        }
    }
}

/// Convert a child process's stdout/stderr pipe into a [`std::fs::File`] so it
/// can be wrapped in a [`PolledPipe`]. The owned-handle type differs by
/// platform, but the conversion is otherwise identical.
#[cfg(unix)]
pub fn child_pipe_to_file(pipe: impl Into<std::os::unix::io::OwnedFd>) -> std::fs::File {
    std::fs::File::from(pipe.into())
}

#[cfg(windows)]
pub fn child_pipe_to_file(pipe: impl Into<std::os::windows::io::OwnedHandle>) -> std::fs::File {
    std::fs::File::from(pipe.into())
}

/// Set up VFIO devices inside the incubator.
///
/// Each extra device in the profile sits behind its own PCIe root port
/// at a known PCI device number (see [`EXTRA_DEVICE_ADDR_BASE`]). This
/// function discovers the child device's BDF by finding the bridge at
/// that slot in sysfs, then unbinds the child from its driver and binds
/// it to vfio-pci.
///
/// Returns a map of environment variables to set for the guest command,
/// e.g., `INCUBATOR_VFIO_BDF_TEST_DISK=0000:01:00.0`. If any provisioned
/// device declares a `provides` capability, the returned map also includes
/// `PETRI_CAPABILITIES` listing those capabilities (comma-separated).
pub async fn setup_vfio_devices(
    client: &pipette_client::PipetteClient,
    devices: &[DeviceConfig],
) -> anyhow::Result<BTreeMap<String, String>> {
    let mut env = BTreeMap::new();
    let mut capabilities = Vec::new();

    // Collect (device_index, device) for devices that need VFIO binding.
    // VFIO binding is device-type-agnostic: any extra device behind its own
    // root port can be unbound from its driver and rebound to vfio-pci.
    let vfio_devices: Vec<_> = devices
        .iter()
        .enumerate()
        .filter(|(_, d)| d.vfio())
        .collect();

    if vfio_devices.is_empty() {
        return Ok(env);
    }

    tracing::info!("setting up {} VFIO device(s)", vfio_devices.len());

    for (device_index, device) in &vfio_devices {
        let name = device.name();
        let addr = EXTRA_DEVICE_ADDR_BASE + device_index;

        // The root port for this device is deterministically at
        // 0000:00:{addr:02x}.0 (see `build_qemu_command`). Read its
        // secondary bus number from sysfs; the assigned device sits at
        // slot 0, function 0 of that bus.
        let rp_bdf = format!("0000:00:{addr:02x}.0");
        let secondary_bus_path = format!("/sys/bus/pci/devices/{rp_bdf}/secondary_bus_number");
        let secondary_bus_raw = client
            .read_file(&secondary_bus_path)
            .await
            .with_context(|| {
                format!(
                    "failed to read secondary bus number for device '{name}' (root port {rp_bdf})"
                )
            })?;
        // sysfs reports the secondary bus number in decimal.
        let secondary_bus_str = String::from_utf8_lossy(&secondary_bus_raw);
        let secondary_bus: u8 = secondary_bus_str.trim().parse().with_context(|| {
            format!("unexpected secondary bus number {secondary_bus_str:?} for device '{name}'")
        })?;
        let bdf = format!("0000:{secondary_bus:02x}:00.0");

        // Confirm the child device actually exists before trying to rebind it.
        client
            .read_file(format!("/sys/bus/pci/devices/{bdf}/vendor"))
            .await
            .with_context(|| {
                format!(
                    "no device found behind root port {rp_bdf} (expected {bdf}) for device '{name}'"
                )
            })?;

        tracing::info!(%name, %bdf, %addr, "binding device to vfio-pci");

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
            name.to_uppercase().replace('-', "_")
        );
        tracing::info!(%env_name, %bdf, "VFIO device ready");
        env.insert(env_name, bdf);

        // Advertise the capability this device provides (derived from its
        // name), now that it has been successfully provisioned. Tests gate on
        // this via `requires(...)`.
        capabilities.push(device.capability());
    }

    // Advertise all provisioned capabilities to the guest command via
    // PETRI_CAPABILITIES (comma-separated), which petri's requirement
    // evaluation reads. Augment any capabilities already present in the
    // incubator's environment rather than overwriting them, so that
    // host-provided capabilities are preserved.
    if !capabilities.is_empty() {
        let mut value = capabilities.join(",");
        if let Ok(existing) = std::env::var("PETRI_CAPABILITIES") {
            if !existing.is_empty() {
                value = format!("{existing},{value}");
            }
        }
        env.insert("PETRI_CAPABILITIES".to_string(), value);
    }

    Ok(env)
}
