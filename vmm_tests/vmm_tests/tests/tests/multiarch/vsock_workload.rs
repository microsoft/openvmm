// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Shared setup and process helpers for opt-in userspace vsock workloads.

use anyhow::Context;
use mesh::CancelContext;
use pal_async::DefaultDriver;
use pal_async::socket::PolledSocket;
use petri::PetriVmArtifacts;
use petri::PetriVmBuilder;
use petri::openvmm::OpenVmmPetriBackend;
use petri::pipette::PipetteClient;
use std::path::Path;
use std::time::Duration;
use unix_socket::UnixListener;

pub(super) const TRANSFER_TIMEOUT: Duration = Duration::from_secs(300);

pub(super) struct BoundRelay {
    pub(super) socket: PolledSocket<UnixListener>,
    // Close the socket before deleting its pathname on Windows.
    _path: tempfile::TempPath,
}

impl BoundRelay {
    pub(super) fn bind(driver: &DefaultDriver, base: &Path, port: u32) -> anyhow::Result<Self> {
        let path = format!("{}_{port}", base.display());
        let listener = UnixListener::bind(&path).context("binding the vsock workload relay")?;
        let cleanup = tempfile::TempPath::from_path(path);
        Ok(Self {
            socket: PolledSocket::new(driver, listener)?,
            _path: cleanup,
        })
    }
}

pub(super) async fn guest_output(
    child: pipette_client::process::Child,
) -> anyhow::Result<pipette_client::process::Output> {
    let output = CancelContext::new()
        .with_timeout(TRANSFER_TIMEOUT)
        .until_cancelled(child.wait_with_output())
        .await
        .context("guest workload timed out")??;
    anyhow::ensure!(
        output.status.success(),
        "guest workload failed: {:?}\nstdout:\n{}\nstderr:\n{}",
        output.status,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    Ok(output)
}

pub(super) fn workload_bytes(variable: &str) -> anyhow::Result<u64> {
    let mib: u64 = match std::env::var(variable) {
        Ok(value) => value
            .parse()
            .with_context(|| format!("{variable} must be an integer"))?,
        Err(std::env::VarError::NotPresent) => 1024,
        Err(err) => return Err(err).with_context(|| format!("invalid {variable}")),
    };
    anyhow::ensure!(
        (16..=4096).contains(&mib),
        "{variable} must be 16..=4096 MiB"
    );
    Ok(mib * 1024 * 1024)
}

pub(super) fn workload_vm(
    params: &petri::PetriTestParams<'_>,
    driver: &DefaultDriver,
    artifacts: PetriVmArtifacts<OpenVmmPetriBackend>,
    mmio: bool,
) -> anyhow::Result<PetriVmBuilder<OpenVmmPetriBackend>> {
    Ok(
        PetriVmBuilder::minimal(params.test_name, params.log_source, artifacts, driver)?
            .with_virtio_vsock(true)
            .without_screenshots()
            .with_processor_topology(petri::ProcessorTopology {
                vp_count: 2,
                ..Default::default()
            })
            .with_memory(petri::MemoryConfig {
                startup_bytes: 1024 * 1024 * 1024,
                ..Default::default()
            })
            .with_watchdog_timeout(Some(Duration::from_secs(1200)))
            .modify_backend(move |backend| {
                let backend = backend.without_save_restore();
                if mmio {
                    backend.with_virtio_vsock_mmio()
                } else {
                    backend.with_pcie_root_topology(1, 1, 1)
                }
            }),
    )
}

pub(super) async fn guest_network_interfaces(agent: &PipetteClient) -> anyhow::Result<String> {
    // The test kernel also exposes inactive virtual tunnel interfaces such as sit0.
    let interfaces = agent
        .command("sh")
        .args([
            "-c",
            r#"for iface in /sys/class/net/*; do if [ -e "$iface/device" ]; then echo "unexpected NIC: $iface"; exit 1; fi; done; ls /sys/class/net"#,
        ])
        .output()
        .await?;
    anyhow::ensure!(
        interfaces.status.success(),
        "vsock workload must not have a device-backed guest NIC: {}",
        String::from_utf8_lossy(&interfaces.stdout)
    );
    let interfaces = String::from_utf8(interfaces.stdout)?;
    for interface in interfaces.split_whitespace().filter(|name| *name != "lo") {
        let flags = agent
            .read_file(format!("/sys/class/net/{interface}/flags"))
            .await?;
        let flags = std::str::from_utf8(&flags)?.trim().trim_start_matches("0x");
        let flags = u32::from_str_radix(flags, 16)?;
        anyhow::ensure!(flags & 1 == 0, "non-loopback interface {interface} is up");
    }
    Ok(interfaces)
}
