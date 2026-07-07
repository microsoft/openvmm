// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Network throughput performance test.
//!
//! Boots a minimal Linux VM with a NIC and measures TCP throughput (Gbps) and
//! UDP packet rate (pps) between the guest and host across multiple iterations.
//! Uses warm mode: the VM is booted once and reused for all iterations.
//!
//! Supports two network backends:
//! - **Consomme**: userspace TCP stack (single NIC, no root required).
//!   Uses linux_direct with a read-only erofs image carrying iperf3.
//! - **TAP**: kernel networking via a TAP device in a network namespace
//!   (Linux only)
//!
//! Each backend can use either VMBus (NETVSP) or virtio-net (PCIe) as
//! the NIC frontend, selected via the `--nic` flag.

use crate::report::MetricResult;
use anyhow::Context as _;
use petri::pipette::cmd;

use petri_artifacts_common::tags::MachineArch;

/// Which NIC frontend to use for the network test.
#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub enum NicBackend {
    /// VMBus synthetic NIC (NETVSP).
    Vmbus,
    /// Virtio-net on PCIe.
    #[value(name = "virtio-net")]
    VirtioNet,
}

impl std::fmt::Display for NicBackend {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        use clap::ValueEnum;
        f.write_str(self.to_possible_value().unwrap().get_name())
    }
}

/// Which network backend to use.
#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub enum NetBackend {
    /// Consomme userspace networking (single NIC).
    Consomme,
    /// TAP device in a network namespace (Linux only).
    Tap,
}

/// Network throughput test via iperf3.
pub struct NetworkTest {
    /// Print guest serial output and take framebuffer screenshots.
    pub diag: bool,
    /// Which NIC frontend to test.
    pub nic: NicBackend,
    /// Which network backend to use.
    pub backend: NetBackend,
    /// If set, record per-phase perf traces in this directory.
    pub perf_dir: Option<std::path::PathBuf>,
}

/// State kept across warm iterations: the running VM and pipette agent.
pub struct NetworkTestState {
    vm: petri::PetriVm<petri::openvmm::OpenVmmPetriBackend>,
    agent: petri::pipette::PipetteClient,
    /// The host IP address for iperf3 connections.
    host_ip: String,
    /// Channel to the iperf3 helper child process.
    iperf_requests: mesh::Sender<crate::iperf_helper::IperfRequest>,
    /// Mesh instance (kept alive so the helper process stays running).
    _helper_mesh: mesh_process::Mesh,
    /// Async driver for timers.
    driver: pal_async::DefaultDriver,
}

fn build_firmware(resolver: &petri::ArtifactResolver<'_>) -> petri::Firmware {
    petri::Firmware::linux_direct(resolver, MachineArch::host())
}

fn require_petritools_erofs(
    resolver: &petri::ArtifactResolver<'_>,
) -> petri_artifacts_core::ResolvedArtifact {
    use petri_artifacts_vmm_test::artifacts::petritools::*;
    match MachineArch::host() {
        MachineArch::X86_64 => resolver.require(PETRITOOLS_EROFS_X64).erase(),
        MachineArch::Aarch64 => resolver.require(PETRITOOLS_EROFS_AARCH64).erase(),
    }
}

/// Register artifacts needed by the network test.
pub fn register_artifacts(resolver: &petri::ArtifactResolver<'_>) {
    let firmware = build_firmware(resolver);
    petri::PetriVmArtifacts::<petri::openvmm::OpenVmmPetriBackend>::new(
        resolver,
        firmware,
        MachineArch::host(),
        true,
    );
    require_petritools_erofs(resolver);
}

impl crate::harness::WarmPerfTest for NetworkTest {
    type State = NetworkTestState;

    fn name(&self) -> &str {
        match (self.backend, self.nic) {
            (NetBackend::Consomme, NicBackend::Vmbus) => "network_vmbus",
            (NetBackend::Consomme, NicBackend::VirtioNet) => "network_virtio",
            (NetBackend::Tap, NicBackend::Vmbus) => "network_tap_vmbus",
            (NetBackend::Tap, NicBackend::VirtioNet) => "network_tap_virtio",
        }
    }

    fn warmup_iterations(&self) -> u32 {
        1
    }

    async fn setup(
        &self,
        resolver: &petri::ArtifactResolver<'_>,
        driver: &pal_async::DefaultDriver,
    ) -> anyhow::Result<NetworkTestState> {
        // Verify host iperf3 is available.
        let iperf3 = std::env::var("IPERF3").unwrap_or_else(|_| "iperf3".into());
        let status = std::process::Command::new(&iperf3)
            .arg("--version")
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status();
        anyhow::ensure!(
            status.is_ok_and(|s| s.success()),
            "iperf3 not found on host; install it or set IPERF3 to the full path of the iperf3 executable"
        );

        // Spawn the iperf3 helper child process. For TAP, use the
        // namespace variant which calls unshare() before connecting.
        let helper_name = match self.backend {
            NetBackend::Consomme => "iperf-helper",
            #[cfg(target_os = "linux")]
            NetBackend::Tap => "tap-ns-helper",
            #[cfg(not(target_os = "linux"))]
            NetBackend::Tap => anyhow::bail!("TAP backend is only supported on Linux"),
        };

        let helper_mesh =
            mesh_process::Mesh::new("iperf-helper".to_string()).context("failed to create mesh")?;

        let (ready_send, ready_recv) = mesh::oneshot();
        helper_mesh
            .launch_host(
                mesh_process::ProcessConfig::new(helper_name)
                    .args([helper_name])
                    .skip_worker_arg(true),
                crate::iperf_helper::IperfHelperInit { ready: ready_send },
            )
            .await
            .context("failed to launch iperf helper")?;

        let ready = ready_recv
            .await
            .context("iperf helper did not respond")?
            .map_err(|e| anyhow::anyhow!("iperf helper failed: {e}"))?;

        // For TAP backend, ask the helper to create the TAP device.
        #[cfg(target_os = "linux")]
        let tap_fd = if self.backend == NetBackend::Tap {
            use mesh::rpc::RpcSend;
            let fd = ready
                .requests
                .call_failable(
                    crate::iperf_helper::IperfRequest::SetupTap,
                    crate::iperf_helper::TapConfig {
                        name: "tap0".to_string(),
                        cidr: "192.168.100.1/24".to_string(),
                    },
                )
                .await
                .context("TAP setup failed")?;
            Some(fd)
        } else {
            None
        };

        let firmware = build_firmware(resolver);
        let artifacts = petri::PetriVmArtifacts::<petri::openvmm::OpenVmmPetriBackend>::new(
            resolver,
            firmware,
            MachineArch::host(),
            true,
        )
        .context("firmware/arch not compatible with OpenVMM backend")?;

        let mut post_test_hooks = Vec::new();
        let log_source = crate::log_source();
        let test_name = match self.backend {
            NetBackend::Consomme => "network_consomme",
            NetBackend::Tap => "network_tap",
        };
        let params = petri::PetriTestParams {
            test_name,
            logger: &log_source,
            post_test_hooks: &mut post_test_hooks,
        };

        // Open the perf rootfs erofs image for the virtio-blk device.
        let erofs_path = require_petritools_erofs(resolver);
        let erofs_file = fs_err::File::open(&erofs_path)?;

        let mut builder = petri::PetriVmBuilder::minimal(params, artifacts, driver)?
            .with_processor_topology(petri::ProcessorTopology {
                vp_count: 2,
                ..Default::default()
            })
            .with_memory(petri::MemoryConfig {
                startup_bytes: 1024 * 1024 * 1024, // 1 GB
                ..Default::default()
            });

        // Configure NICs and erofs device based on backend.
        match self.backend {
            NetBackend::Consomme => {
                builder = builder.modify_backend({
                    let nic = self.nic;
                    move |c| {
                        let (c, blk_port) = match nic {
                            NicBackend::Vmbus => {
                                (c.with_pcie_root_topology(1, 1, 1).with_nic(), "s0rc0rp0")
                            }
                            NicBackend::VirtioNet => (
                                c.with_pcie_root_topology(1, 1, 2)
                                    .with_virtio_nic("s0rc0rp0"),
                                "s0rc0rp1",
                            ),
                        };
                        c.with_custom_config(|config| {
                            use disk_backend_resources::FileDiskHandle;
                            use openvmm_defs::config::PcieDeviceConfig;
                            use vm_resource::IntoResource;

                            config.pcie_devices.push(PcieDeviceConfig {
                                port_name: blk_port.into(),
                                resource: virtio_resources::VirtioPciDeviceHandle(
                                    virtio_resources::blk::VirtioBlkHandle {
                                        disk: FileDiskHandle(erofs_file.into()).into_resource(),
                                        read_only: true,
                                    }
                                    .into_resource(),
                                )
                                .into_resource(),
                            });
                        })
                    }
                });
            }
            #[cfg(target_os = "linux")]
            NetBackend::Tap => {
                let tap_fd = tap_fd.unwrap();
                builder = tap::configure_builder(builder, tap_fd, self.nic, erofs_file);
            }
            #[cfg(not(target_os = "linux"))]
            NetBackend::Tap => unreachable!(),
        }

        if !self.diag {
            builder = builder.without_screenshots();
        } else {
            builder = builder.with_serial_output();
        }

        let (vm, agent) = builder.run().await.context("failed to boot VM")?;
        let sh = agent.unix_shell();

        // Guest networking and iperf3 setup depends on backend.
        let host_ip;
        match self.backend {
            NetBackend::Consomme => {
                // Bring up networking on the real root (busybox in initrd).
                cmd!(sh, "ifconfig eth0 up").run().await?;
                cmd!(sh, "udhcpc eth0").run().await?;

                host_ip = detect_host_ip().context("failed to detect host IP")?;
                tracing::info!(host_ip = %host_ip, "detected host IP");
            }
            #[cfg(target_os = "linux")]
            NetBackend::Tap => {
                host_ip = tap::setup_guest_networking(&agent).await?;
            }
            #[cfg(not(target_os = "linux"))]
            NetBackend::Tap => unreachable!(),
        }

        // Mount the erofs image (iperf3 pre-installed) and prepare chroot.
        agent
            .mount("/dev/vda", "/perf", "erofs", 1 /* MS_RDONLY */, true)
            .await
            .context("failed to mount erofs on /dev/vda")?;
        agent
            .prepare_chroot("/perf")
            .await
            .context("failed to prepare chroot at /perf")?;

        Ok(NetworkTestState {
            vm,
            agent,
            host_ip,
            iperf_requests: ready.requests,
            _helper_mesh: helper_mesh,
            driver: driver.clone(),
        })
    }

    async fn run_once(&self, state: &mut NetworkTestState) -> anyhow::Result<Vec<MetricResult>> {
        let mut metrics = Vec::new();
        let label = self.nic;
        let pid = state.vm.backend().pid();
        let mut recorder = crate::harness::PerfRecorder::new(self.perf_dir.as_deref(), pid)?;
        let host_ip = &state.host_ip;

        let prefix = format!("net_{label}");
        let base_port: u16 = 5201;

        // TCP TX (guest sends to host)
        let name = format!("{prefix}_tcp_tx_gbps");
        recorder.start(&name)?;
        let m = self
            .run_iperf3(state, host_ip, base_port, &name, IperfMode::TcpTx)
            .await
            .context("TCP TX test failed")?;
        recorder.stop()?;
        metrics.push(m);

        // TCP RX (host sends to guest, -R flag)
        let name = format!("{prefix}_tcp_rx_gbps");
        recorder.start(&name)?;
        let m = self
            .run_iperf3(state, host_ip, base_port + 1, &name, IperfMode::TcpRx)
            .await
            .context("TCP RX test failed")?;
        recorder.stop()?;
        metrics.push(m);

        // UDP TX (guest sends to host)
        let name = format!("{prefix}_udp_tx_pps");
        recorder.start(&name)?;
        let m = self
            .run_iperf3(state, host_ip, base_port + 2, &name, IperfMode::UdpTx)
            .await
            .context("UDP TX test failed")?;
        recorder.stop()?;
        metrics.push(m);

        Ok(metrics)
    }

    async fn teardown(&self, state: NetworkTestState) -> anyhow::Result<()> {
        state.agent.power_off().await?;
        state.vm.wait_for_clean_teardown().await?;
        state
            .iperf_requests
            .send(crate::iperf_helper::IperfRequest::Stop);
        state._helper_mesh.shutdown().await;
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// iperf3 helpers
// ---------------------------------------------------------------------------

/// Which iperf3 test variant to run.
enum IperfMode {
    /// TCP, guest sends to host.
    TcpTx,
    /// TCP, host sends to guest (-R).
    TcpRx,
    /// UDP, guest sends to host.
    UdpTx,
}

impl NetworkTest {
    /// Run a single iperf3 sub-test via the helper child process.
    async fn run_iperf3(
        &self,
        state: &NetworkTestState,
        host_ip: &str,
        port: u16,
        metric_name: &str,
        mode: IperfMode,
    ) -> anyhow::Result<MetricResult> {
        use mesh::rpc::RpcSend;

        // Ask the helper to start an iperf3 server (blocks until client done).
        let json_future = state.iperf_requests.call_failable(
            crate::iperf_helper::IperfRequest::RunIperf3,
            crate::iperf_helper::Iperf3Args {
                args: vec![
                    "-s".into(),
                    "-1".into(),
                    "-J".into(),
                    "-p".into(),
                    port.to_string(),
                ],
            },
        );

        // Brief delay to let the server bind.
        pal_async::timer::PolledTimer::new(&state.driver)
            .sleep(std::time::Duration::from_millis(500))
            .await;

        // Run guest iperf3 client.
        run_guest_iperf3_client(&state.agent, host_ip, port, &mode, metric_name).await?;

        // Collect JSON from the helper.
        let json = json_future.await.context("iperf3 helper RPC failed")?;

        if !json.is_empty() {
            tracing::debug!(metric = metric_name, json = %json, "raw iperf3 output");
        }

        anyhow::ensure!(
            !json.is_empty(),
            "iperf3 server produced no output for {metric_name}"
        );

        parse_result(&json, metric_name, &mode)
    }
}

/// Run guest iperf3 client with the given mode. Returns after the client exits.
async fn run_guest_iperf3_client(
    agent: &petri::pipette::PipetteClient,
    host_ip: &str,
    port: u16,
    mode: &IperfMode,
    metric_name: &str,
) -> anyhow::Result<()> {
    let mut sh = agent.unix_shell();
    sh.chroot("/perf");
    let port_str = port.to_string();
    match mode {
        IperfMode::TcpTx => {
            cmd!(sh, "iperf3 -c {host_ip} -p {port_str} -t 10 -J")
                .ignore_status()
                .run()
                .await
        }
        IperfMode::TcpRx => {
            cmd!(sh, "iperf3 -c {host_ip} -p {port_str} -t 10 -R -J")
                .ignore_status()
                .run()
                .await
        }
        IperfMode::UdpTx => {
            cmd!(sh, "iperf3 -c {host_ip} -p {port_str} -t 10 -u -b 0 -J")
                .ignore_status()
                .run()
                .await
        }
    }
    .with_context(|| format!("guest iperf3 client failed for {metric_name}"))
}

/// Parse iperf3 JSON into a MetricResult based on the test mode.
fn parse_result(json: &str, metric_name: &str, mode: &IperfMode) -> anyhow::Result<MetricResult> {
    match mode {
        IperfMode::TcpTx => parse_tcp_throughput(json, metric_name, false),
        IperfMode::TcpRx => parse_tcp_throughput(json, metric_name, true),
        IperfMode::UdpTx => parse_udp_pps(json, metric_name),
    }
}

// ---------------------------------------------------------------------------
// TAP namespace setup (Linux only)
// ---------------------------------------------------------------------------

#[cfg(target_os = "linux")]
mod tap {
    use anyhow::Context as _;
    use net_backend_resources::mac_address::MacAddress;
    use openvmm_defs::config::DeviceVtl;
    use openvmm_defs::config::PcieDeviceConfig;
    use petri::pipette::cmd;
    use vm_resource::IntoResource;

    /// MAC address for the TAP NIC (one byte different from Consomme's).
    const TAP_MAC_ADDRESS: MacAddress = MacAddress::new([0x00, 0x15, 0x5D, 0x12, 0x12, 0x13]);

    /// Add a VMBus synthnic backed by a TAP fd to the VM config.
    fn add_tap_nic(config: &mut openvmm_defs::config::Config, tap_fd: std::os::fd::OwnedFd) {
        let endpoint = net_backend_resources::tap::TapHandle { fd: tap_fd }.into_resource();
        const TAP_NETVSP_INSTANCE: guid::Guid = guid::guid!("a1b2c3d4-e5f6-7890-abcd-ef1234567890");

        config.vmbus_devices.push((
            DeviceVtl::Vtl0,
            netvsp_resources::NetvspHandle {
                instance_id: TAP_NETVSP_INSTANCE,
                mac_address: TAP_MAC_ADDRESS,
                endpoint,
                max_queues: None,
            }
            .into_resource(),
        ));
    }

    /// Add a virtio-net NIC backed by a TAP fd to the VM config (PCIe).
    fn add_virtio_tap_nic(config: &mut openvmm_defs::config::Config, tap_fd: std::os::fd::OwnedFd) {
        let endpoint = net_backend_resources::tap::TapHandle { fd: tap_fd }.into_resource();

        config.pcie_devices.push(PcieDeviceConfig {
            port_name: "s0rc0rp1".into(),
            resource: virtio_resources::VirtioPciDeviceHandle(
                virtio_resources::net::VirtioNetHandle {
                    max_queues: None,
                    mac_address: TAP_MAC_ADDRESS,
                    endpoint,
                }
                .into_resource(),
            )
            .into_resource(),
        });
    }

    /// Configure the VM builder to add both a Consomme NIC and a TAP NIC,
    /// plus a read-only virtio-blk device with the erofs image.
    pub(super) fn configure_builder(
        builder: petri::PetriVmBuilder<petri::openvmm::OpenVmmPetriBackend>,
        tap_fd: std::os::fd::OwnedFd,
        nic: super::NicBackend,
        erofs_file: fs_err::File,
    ) -> petri::PetriVmBuilder<petri::openvmm::OpenVmmPetriBackend> {
        builder.modify_backend(move |c| {
            let (c, blk_port) = match nic {
                super::NicBackend::Vmbus => {
                    (c.with_pcie_root_topology(1, 1, 1).with_nic(), "s0rc0rp0")
                }
                super::NicBackend::VirtioNet => (
                    c.with_pcie_root_topology(1, 1, 3)
                        .with_virtio_nic("s0rc0rp0"),
                    "s0rc0rp2",
                ),
            };
            c.with_custom_config(|config| {
                use disk_backend_resources::FileDiskHandle;
                use vm_resource::IntoResource;

                // Attach erofs image as read-only virtio-blk device.
                config.pcie_devices.push(PcieDeviceConfig {
                    port_name: blk_port.into(),
                    resource: virtio_resources::VirtioPciDeviceHandle(
                        virtio_resources::blk::VirtioBlkHandle {
                            disk: FileDiskHandle(erofs_file.into()).into_resource(),
                            read_only: true,
                        }
                        .into_resource(),
                    )
                    .into_resource(),
                });

                // Add TAP NIC.
                match nic {
                    super::NicBackend::Vmbus => add_tap_nic(config, tap_fd),
                    super::NicBackend::VirtioNet => add_virtio_tap_nic(config, tap_fd),
                }
            })
        })
    }

    /// Configure guest networking for the TAP backend.
    /// Returns the host IP to use for iperf3 connections.
    pub(super) async fn setup_guest_networking(
        agent: &petri::pipette::PipetteClient,
    ) -> anyhow::Result<String> {
        let sh = agent.unix_shell();

        // Discover NICs by MAC and configure them in one shot.
        // Consomme NIC (…:12) gets DHCP; TAP NIC (…:13) gets a static IP
        // and a route for the TAP subnet.
        let script = r#"
for dev in /sys/class/net/*/; do
  name=$(basename "$dev")
  mac=$(cat "$dev/address")
  case "$mac" in
    00:15:5d:12:12:12) ifconfig "$name" up && udhcpc -i "$name" -n -q ;;
    00:15:5d:12:12:13) ifconfig "$name" 192.168.100.2 netmask 255.255.255.0 up
                       ip route replace 192.168.100.0/24 dev "$name" ;;
  esac
done
"#;
        cmd!(sh, "sh -c {script}")
            .run()
            .await
            .context("failed to configure guest NICs")?;

        Ok("192.168.100.1".to_string())
    }
}

// ---------------------------------------------------------------------------
// Shared helpers
// ---------------------------------------------------------------------------

/// Detect the host's primary IP address by finding the default route source.
fn detect_host_ip() -> anyhow::Result<String> {
    let socket = std::net::UdpSocket::bind("0.0.0.0:0")
        .context("failed to bind UDP socket for host IP detection")?;
    socket
        .connect("1.1.1.1:80")
        .context("failed to connect UDP socket (no default route?)")?;
    let addr = socket
        .local_addr()
        .context("failed to get local address of UDP socket")?;
    Ok(addr.ip().to_string())
}

/// Parse TCP throughput from iperf3 JSON output.
fn parse_tcp_throughput(json: &str, metric_name: &str, sent: bool) -> anyhow::Result<MetricResult> {
    let v: serde_json::Value = serde_json::from_str(json).context("failed to parse iperf3 JSON")?;

    let field = if sent { "sum_sent" } else { "sum_received" };
    let bps = v["end"][field]["bits_per_second"].as_f64();

    let alt_field = if sent { "sum_received" } else { "sum_sent" };
    let bps = match bps {
        Some(v) if v > 0.0 => v,
        _ => v["end"][alt_field]["bits_per_second"]
            .as_f64()
            .with_context(|| {
                tracing::error!(json = %json, "failed to find bits_per_second in iperf3 TCP output");
                format!("missing bits_per_second in iperf3 output for {metric_name}")
            })?,
    };

    let gbps = bps / 1_000_000_000.0;
    Ok(MetricResult {
        name: metric_name.to_string(),
        unit: "Gbps".to_string(),
        value: gbps,
    })
}

/// Parse UDP packets-per-second from iperf3 JSON output.
fn parse_udp_pps(json: &str, metric_name: &str) -> anyhow::Result<MetricResult> {
    let v: serde_json::Value = serde_json::from_str(json).context("failed to parse iperf3 JSON")?;

    let packets = v["end"]["sum"]["packets"].as_f64();
    let seconds = v["end"]["sum"]["seconds"].as_f64();

    let pps = match (packets, seconds) {
        (Some(p), Some(s)) if s > 0.0 => p / s,
        _ => {
            let bps = v["end"]["sum"]["bits_per_second"].as_f64().with_context(|| {
                tracing::error!(json = %json, "failed to find packets/seconds in iperf3 UDP output");
                format!("missing packets or seconds in iperf3 output for {metric_name}")
            })?;
            bps / (1460.0 * 8.0)
        }
    };

    Ok(MetricResult {
        name: metric_name.to_string(),
        unit: "pps".to_string(),
        value: pps,
    })
}
