// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Native AF_VSOCK workloads using an externally supplied upstream vsock_perf.

use super::vsock_workload::BoundRelay;
use super::vsock_workload::TRANSFER_TIMEOUT;
use super::vsock_workload::guest_network_interfaces;
use super::vsock_workload::guest_output;
use super::vsock_workload::workload_bytes;
use super::vsock_workload::workload_vm;
use anyhow::Context;
use futures::AsyncBufReadExt;
use futures::AsyncReadExt;
use futures::AsyncWriteExt;
use futures::FutureExt;
use futures::io::BufReader;
use mesh::CancelContext;
use pal_async::DefaultDriver;
use pal_async::socket::PolledSocket;
use pal_async::task::Spawn;
use pal_async::task::Task;
use pal_async::timer::PolledTimer;
use petri::PetriVm;
use petri::PetriVmArtifacts;
use petri::openvmm::OpenVmmPetriBackend;
use petri::pipette::PipetteClient;
use petri::pipette::process::Stdio;
use petri_artifacts_common::tags::MachineArch;
use serde::Serialize;
use serde_json::Value;
use serde_json::json;
use std::io;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering;
use std::time::Duration;
use std::time::Instant;
use unix_socket::UnixStream;

const GUEST_TOOL: &str = "/tmp/vsock_perf";
const INTERRUPTED_BYTES: u64 = 64 * 1024 * 1024 * 1024;

petri::multitest!({
    [("vsock_perf_pci", false), ("vsock_perf_mmio", true)]
        .into_iter()
        .map(|(name, mmio)| {
            let test = petri::SimpleTest::new_async(
                name,
                |resolver: &petri::ArtifactResolver<'_>| {
                    if MachineArch::host() != MachineArch::X86_64 {
                        return None;
                    }
                    let firmware =
                        petri::Firmware::linux_direct(resolver, MachineArch::X86_64, None);
                    PetriVmArtifacts::<OpenVmmPetriBackend>::new(
                        resolver,
                        firmware,
                        MachineArch::X86_64,
                        true,
                    )
                },
                async move |params, driver, artifacts| {
                    run_perf(params, driver, artifacts, mmio).await
                },
            );
            if std::env::var_os("PETRI_VSOCK_PERF_BIN").is_some() {
                test.into()
            } else {
                test.ignore().into()
            }
        })
        .collect()
});

#[derive(Clone, Copy)]
enum Direction {
    Tx,
    Rx,
}

impl Direction {
    fn name(self) -> &'static str {
        match self {
            Self::Tx => "tx",
            Self::Rx => "rx",
        }
    }

    fn port(self) -> u32 {
        match self {
            Self::Tx => 6400,
            Self::Rx => 6401,
        }
    }

    fn args(self, bytes: u64) -> Vec<String> {
        let mut args = vec!["--port".into(), self.port().to_string()];
        match self {
            Self::Tx => {
                args.extend(["--sender", "2", "--buf-size", "64K"].map(str::to_owned));
                args.extend(["--bytes".into(), bytes.to_string()]);
            }
            Self::Rx => {
                // Drain the credit window in one read even when the upstream
                // receiver observes readable data and hangup together.
                args.extend(
                    ["--buf-size", "1M", "--vsk-size", "256K", "--rcvlowat", "1"]
                        .map(str::to_owned),
                );
            }
        }
        args
    }
}

#[derive(Debug, Serialize)]
struct PerfReport {
    bytes: u64,
    gigabits_per_second: f64,
}

fn report_field<'a>(output: &'a str, prefix: &str) -> anyhow::Result<&'a str> {
    let mut fields = output
        .lines()
        .filter_map(|line| line.trim().strip_prefix(prefix));
    let value = fields
        .next()
        .with_context(|| format!("vsock_perf omitted {prefix:?}"))?;
    anyhow::ensure!(fields.next().is_none(), "vsock_perf duplicated {prefix:?}");
    Ok(value)
}

fn parse_report(output: &str, direction: Direction, expected: u64) -> anyhow::Result<PerfReport> {
    let (count, rate) = match direction {
        Direction::Tx => ("total bytes sent: ", "tx performance: "),
        Direction::Rx => ("total bytes received: ", "rx performance: "),
    };
    let bytes: u64 = report_field(output, count)?
        .parse()
        .context("invalid vsock_perf byte count")?;
    let rate = report_field(output, rate)?;
    let gigabits_per_second: f64 = rate
        .strip_suffix(" Gbits/s")
        .context("unexpected vsock_perf throughput units")?
        .parse()
        .context("invalid vsock_perf throughput")?;
    anyhow::ensure!(
        bytes == expected,
        "vsock_perf reported {bytes} bytes, expected exactly {expected}"
    );
    anyhow::ensure!(
        gigabits_per_second.is_finite() && gigabits_per_second > 0.0,
        "vsock_perf reported invalid throughput {gigabits_per_second}"
    );
    Ok(PerfReport {
        bytes,
        gigabits_per_second,
    })
}

#[derive(Default)]
struct Progress {
    bytes: AtomicU64,
    active: AtomicBool,
}

fn expected_disconnect(exit: i32, stderr: &str) -> bool {
    // POSIX shells report SIGPIPE as 128 + 13.
    exit == 141
        || (exit == 1
            && matches!(
                stderr.trim(),
                "write: Connection reset by peer"
                    | "read: Connection reset by peer"
                    | "write: Broken pipe"
                    | "read: Broken pipe"
                    | "write: Transport endpoint is not connected"
                    | "read: Transport endpoint is not connected"
                    | "'poll()' error"
            ))
}

#[derive(Debug, Serialize)]
struct HostReport {
    bytes: u64,
    seconds: f64,
    reset_error: Option<String>,
}

struct HostTransfer {
    task: Task<anyhow::Result<HostReport>>,
    progress: Arc<Progress>,
}

async fn connect_receiver(
    driver: &DefaultDriver,
    base: &Path,
    port: u32,
) -> anyhow::Result<PolledSocket<UnixStream>> {
    loop {
        let mut socket = PolledSocket::connect_unix(driver, base).await?;
        socket
            .write_all(format!("CONNECT {port}\n").as_bytes())
            .await?;
        let mut socket = BufReader::new(socket);
        let mut response = String::new();
        if socket.read_line(&mut response).await? != 0 {
            let local_port: u32 = response
                .strip_prefix("OK ")
                .context("vsock relay rejected the receiver connection")?
                .trim()
                .parse()
                .context("invalid vsock relay response")?;
            anyhow::ensure!(local_port != 0, "vsock relay returned port zero");
            return Ok(socket.into_inner());
        }
        // A request before the utility calls listen() is reset by the guest.
        tracing::debug!(port, "waiting for vsock_perf receiver to listen");
        PolledTimer::new(driver)
            .sleep(Duration::from_millis(20))
            .await;
    }
}

async fn transfer(
    socket: &mut PolledSocket<UnixStream>,
    progress: &Progress,
    bytes: u64,
    direction: Direction,
) -> io::Result<()> {
    let mut buffer = vec![0; 64 * 1024];
    match direction {
        Direction::Tx => loop {
            let count = socket.read(&mut buffer).await?;
            if count == 0 {
                return Ok(());
            }
            if buffer[..count].iter().any(|byte| *byte != 0) {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "nonzero vsock_perf payload",
                ));
            }
            let received = progress.bytes.fetch_add(count as u64, Ordering::Relaxed) + count as u64;
            if received > bytes {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "extra vsock_perf payload",
                ));
            }
        },
        Direction::Rx => {
            while progress.bytes.load(Ordering::Relaxed) < bytes {
                let count = (bytes - progress.bytes.load(Ordering::Relaxed))
                    .min(buffer.len() as u64) as usize;
                socket.write_all(&buffer[..count]).await?;
                progress.bytes.fetch_add(count as u64, Ordering::Relaxed);
            }
            socket.close().await?;
            if socket.read(&mut buffer[..1]).await? != 0 {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "unexpected receiver data",
                ));
            }
            Ok(())
        }
    }
}

impl HostTransfer {
    fn start(
        driver: &DefaultDriver,
        base: &Path,
        direction: Direction,
        bytes: u64,
        allow_disconnect: bool,
    ) -> anyhow::Result<Self> {
        let listener = match direction {
            Direction::Tx => Some(BoundRelay::bind(driver, base, direction.port())?),
            Direction::Rx => None,
        };
        let base = base.to_owned();
        let progress = Arc::new(Progress::default());
        let stats = progress.clone();
        let driver = driver.clone();
        let task = driver.clone().spawn("vsock-perf-peer", async move {
            let result = CancelContext::new()
                .with_timeout(TRANSFER_TIMEOUT)
                .until_cancelled(async {
                    let mut socket = match listener {
                        Some(mut listener) => {
                            let (socket, _) = listener.socket.accept().await?;
                            PolledSocket::new(&driver, socket)?
                        }
                        None => connect_receiver(&driver, &base, direction.port()).await?,
                    };
                    stats.active.store(true, Ordering::Relaxed);
                    let start = Instant::now();
                    let reset_error = match transfer(&mut socket, &stats, bytes, direction).await {
                        Ok(()) => None,
                        Err(err)
                            if allow_disconnect
                                && matches!(
                                    err.kind(),
                                    io::ErrorKind::ConnectionReset
                                        | io::ErrorKind::ConnectionAborted
                                        | io::ErrorKind::BrokenPipe
                                        | io::ErrorKind::NotConnected
                                        | io::ErrorKind::WriteZero
                                ) =>
                        {
                            Some(err.to_string())
                        }
                        Err(err) => return Err(err).context("native vsock_perf peer failed"),
                    };
                    Ok(HostReport {
                        bytes: stats.bytes.load(Ordering::Relaxed),
                        seconds: start.elapsed().as_secs_f64(),
                        reset_error,
                    })
                })
                .await
                .context("native vsock_perf peer timed out");
            stats.active.store(false, Ordering::Relaxed);
            result?
        });
        Ok(Self { task, progress })
    }

    async fn wait_for_bytes(
        &mut self,
        driver: &DefaultDriver,
        threshold: u64,
    ) -> anyhow::Result<()> {
        let progress = self.progress.clone();
        let reached = async {
            while progress.bytes.load(Ordering::Relaxed) < threshold {
                PolledTimer::new(driver)
                    .sleep(Duration::from_millis(5))
                    .await;
            }
        };
        CancelContext::new().with_timeout(Duration::from_secs(30)).until_cancelled(async {
            futures::select! {
                () = reached.fuse() => {
                    anyhow::ensure!(progress.active.load(Ordering::Relaxed), "workload ended before interruption");
                    Ok(())
                }
                result = (&mut self.task).fuse() => {
                    let report = result?;
                    anyhow::bail!("native workload ended before interruption: {report:?}")
                }
            }
        }).await.context("vsock_perf did not reach the interruption threshold")?
    }

    async fn finish(self) -> anyhow::Result<HostReport> {
        self.task.await
    }
}

struct PerfCase<'a> {
    phase: &'a str,
    direction: Direction,
    bytes: u64,
    pause: bool,
}

async fn completed_case(
    params: &petri::PetriTestParams<'_>,
    driver: &DefaultDriver,
    vm: &mut PetriVm<OpenVmmPetriBackend>,
    agent: &PipetteClient,
    case: PerfCase<'_>,
) -> anyhow::Result<Value> {
    let PerfCase {
        phase,
        direction,
        bytes,
        pause,
    } = case;
    let mut host = HostTransfer::start(
        driver,
        vm.backend().vtl0_vsock_path()?,
        direction,
        bytes,
        false,
    )?;
    let child = agent
        .command(GUEST_TOOL)
        .args(direction.args(bytes))
        .env("LC_ALL", "C")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .await?;
    let (output, host) = futures::try_join!(guest_output(child), async {
        if pause {
            host.wait_for_bytes(driver, (bytes / 4).min(16 * 1024 * 1024))
                .await?;
            anyhow::ensure!(
                host.progress.bytes.load(Ordering::Relaxed) < bytes,
                "transfer finished before pause"
            );
            vm.backend().pause().await?;
            PolledTimer::new(driver).sleep(Duration::from_secs(1)).await;
            vm.backend().resume().await?;
        }
        host.finish().await
    },)?;
    params.log_source.write_attachment(
        &format!("{phase}-vsock-perf.stdout"),
        output.stdout.as_slice(),
    )?;
    params.log_source.write_attachment(
        &format!("{phase}-vsock-perf.stderr"),
        output.stderr.as_slice(),
    )?;
    let guest = parse_report(std::str::from_utf8(&output.stdout)?, direction, bytes)?;
    anyhow::ensure!(
        host.bytes == bytes,
        "native host peer transferred {} bytes, expected {bytes}",
        host.bytes
    );
    Ok(json!({
        "phase": phase, "direction": direction.name(), "vsock_port": direction.port(),
        "bytes_requested": bytes, "paused": pause, "guest": guest, "host": host,
    }))
}

async fn interrupted_case(
    params: &petri::PetriTestParams<'_>,
    driver: &DefaultDriver,
    vm: &mut PetriVm<OpenVmmPetriBackend>,
    agent: PipetteClient,
    direction: Direction,
) -> anyhow::Result<(PipetteClient, Value)> {
    let phase = format!("restore-interruption-{}", direction.name());
    let prefix = format!("/tmp/vsock-perf-{phase}");
    let command = format!(
        "{GUEST_TOOL} {} >{prefix}.out 2>{prefix}.err; result=$?; printf '%s\\n' \"$result\" >{prefix}.exit; exit \"$result\"",
        direction.args(INTERRUPTED_BYTES).join(" "),
    );
    let mut host = HostTransfer::start(
        driver,
        vm.backend().vtl0_vsock_path()?,
        direction,
        INTERRUPTED_BYTES,
        true,
    )?;
    let child = agent
        .command("sh")
        .args(["-c", &command])
        .env("LC_ALL", "C")
        .spawn()
        .await?;
    let pid = child.id();
    host.wait_for_bytes(driver, 16 * 1024 * 1024).await?;
    anyhow::ensure!(
        host.progress.bytes.load(Ordering::Relaxed) < INTERRUPTED_BYTES,
        "workload finished before restore"
    );
    CancelContext::new()
        .with_timeout(Duration::from_secs(30))
        .until_cancelled(vm.backend().pulse_save_restore())
        .await
        .context("vsock_perf save/reset/restore pulse timed out")??;
    let disconnect = CancelContext::new()
        .with_timeout(Duration::from_secs(30))
        .until_cancelled(agent.wait())
        .await
        .context("old agent did not disconnect")?;
    tracing::debug!(?disconnect, "old vsock_perf agent connection closed");
    drop(child);
    let host = host.finish().await?;
    anyhow::ensure!(
        host.bytes >= 16 * 1024 * 1024 && host.bytes < INTERRUPTED_BYTES,
        "unexpected interrupted byte count"
    );
    let agent = CancelContext::new()
        .with_timeout(Duration::from_secs(30))
        .until_cancelled(vm.backend().wait_for_agent(false))
        .await
        .context("agent did not reconnect after vsock_perf restore")??;
    CancelContext::new()
        .with_timeout(Duration::from_secs(30))
        .until_cancelled(async {
            loop {
                let output = agent
                    .command("sh")
                    .args([
                        "-c",
                        &format!("test -f {prefix}.exit && test ! -e /proc/{pid}"),
                    ])
                    .output()
                    .await?;
                if output.status.success() {
                    return Ok::<_, anyhow::Error>(());
                }
                anyhow::ensure!(
                    output.status.code() == Some(1),
                    "checking interrupted utility exit failed: {}; {}",
                    output.status,
                    String::from_utf8_lossy(&output.stderr),
                );
                PolledTimer::new(driver)
                    .sleep(Duration::from_millis(10))
                    .await;
            }
        })
        .await
        .context("interrupted vsock_perf process did not exit")??;
    let exit = agent.read_file(format!("{prefix}.exit")).await?;
    let exit: i32 = std::str::from_utf8(&exit)?.trim().parse()?;
    let stdout = agent.read_file(format!("{prefix}.out")).await?;
    let stderr = agent.read_file(format!("{prefix}.err")).await?;
    params
        .log_source
        .write_attachment(&format!("{phase}-vsock-perf.stdout"), stdout.as_slice())?;
    params
        .log_source
        .write_attachment(&format!("{phase}-vsock-perf.stderr"), stderr.as_slice())?;
    anyhow::ensure!(
        exit != 0,
        "old vsock_perf completed successfully instead of being interrupted"
    );
    anyhow::ensure!(
        expected_disconnect(exit, std::str::from_utf8(&stderr)?),
        "unexpected vsock_perf interruption: exit={exit}, stderr={}",
        String::from_utf8_lossy(&stderr),
    );
    Ok((
        agent,
        json!({
            "phase": phase, "direction": direction.name(), "vsock_port": direction.port(),
            "expected_disconnect": true, "guest_exit": exit, "host": host,
        }),
    ))
}

async fn run_perf(
    params: petri::PetriTestParams<'_>,
    driver: DefaultDriver,
    artifacts: PetriVmArtifacts<OpenVmmPetriBackend>,
    mmio: bool,
) -> anyhow::Result<()> {
    let path = std::env::var_os("PETRI_VSOCK_PERF_BIN")
        .context("set PETRI_VSOCK_PERF_BIN to a static Linux x86_64 vsock_perf binary")?;
    let binary = fs_err::read(&path).context("reading PETRI_VSOCK_PERF_BIN")?;
    anyhow::ensure!(
        binary.starts_with(b"\x7fELF"),
        "PETRI_VSOCK_PERF_BIN must be a Linux ELF executable"
    );
    let bytes = workload_bytes("PETRI_VSOCK_PERF_MIB")?;
    let (mut vm, mut agent) = workload_vm(&params, &driver, artifacts, mmio)?
        .run()
        .await?;
    anyhow::ensure!(
        agent.local_vsock_cid() == Some(3),
        "agent is not using virtio-vsock"
    );
    let interfaces = guest_network_interfaces(&agent).await?;
    agent.write_file(GUEST_TOOL, binary.as_slice()).await?;
    let chmod = agent
        .command("chmod")
        .args(["0755", GUEST_TOOL])
        .output()
        .await?;
    anyhow::ensure!(
        chmod.status.success(),
        "making vsock_perf executable failed: {}; {}",
        chmod.status,
        String::from_utf8_lossy(&chmod.stderr),
    );
    let help = agent
        .command(GUEST_TOOL)
        .arg("--help")
        .env("LC_ALL", "C")
        .output()
        .await?;
    anyhow::ensure!(
        help.status.code() == Some(1)
            && std::str::from_utf8(&help.stdout)?.contains("vsock performance"),
        "upstream vsock_perf is unavailable: {}; stdout: {}; stderr: {}",
        help.status,
        String::from_utf8_lossy(&help.stdout),
        String::from_utf8_lossy(&help.stderr),
    );
    params
        .log_source
        .write_attachment("vsock-perf-help.txt", help.stdout.as_slice())?;
    let boot_id = agent.read_file("/proc/sys/kernel/random/boot_id").await?;
    let marker = guid::Guid::new_random().to_string().into_bytes();
    agent
        .write_file("/tmp/vsock-perf-marker", marker.as_slice())
        .await?;
    let mut results = Vec::new();
    for direction in [Direction::Tx, Direction::Rx] {
        results.push(
            completed_case(
                &params,
                &driver,
                &mut vm,
                &agent,
                PerfCase {
                    phase: &format!("before-restore-{}", direction.name()),
                    direction,
                    bytes,
                    pause: true,
                },
            )
            .await?,
        );
    }
    for direction in [Direction::Tx, Direction::Rx] {
        let (restored, report) =
            interrupted_case(&params, &driver, &mut vm, agent, direction).await?;
        agent = restored;
        results.push(report);
        anyhow::ensure!(
            agent.read_file("/proc/sys/kernel/random/boot_id").await? == boot_id,
            "guest rebooted"
        );
        anyhow::ensure!(
            agent.read_file("/tmp/vsock-perf-marker").await? == marker,
            "guest RAM marker changed"
        );
        results.push(
            completed_case(
                &params,
                &driver,
                &mut vm,
                &agent,
                PerfCase {
                    phase: &format!("after-restore-{}", direction.name()),
                    direction,
                    bytes,
                    pause: false,
                },
            )
            .await?,
        );
    }
    let summary = json!({
        "tool": "upstream Linux vsock_perf", "host_binary": path.to_string_lossy(),
        "binary_bytes": binary.len(), "native_guest_af_vsock": true, "tcp_bridge": false,
        "no_guest_nic": true, "guest_network_interfaces": interfaces,
        "transport": if mmio { "mmio" } else { "pcie" }, "guest_cid": agent.local_vsock_cid(),
        "bytes_per_completed_transfer": bytes, "results": results,
    });
    params.log_source.write_attachment(
        "vsock-perf-summary.json",
        serde_json::to_vec_pretty(&summary)?.as_slice(),
    )?;
    CancelContext::new()
        .with_timeout(Duration::from_secs(30))
        .until_cancelled(agent.power_off())
        .await
        .context("vsock_perf guest power-off request timed out")??;
    CancelContext::new()
        .with_timeout(Duration::from_secs(30))
        .until_cancelled(vm.wait_for_clean_teardown())
        .await
        .context("vsock_perf VM shutdown or teardown timed out")??;
    Ok(())
}

petri::test!(vsock_perf_report_validation, |_| Some(()));

async fn vsock_perf_report_validation(
    _params: petri::PetriTestParams<'_>,
    _driver: DefaultDriver,
    (): (),
) -> anyhow::Result<()> {
    let tx = "Run as sender\ntotal bytes sent: 1024\ntx performance: 1.25 Gbits/s\n";
    let rx = "Run as receiver\ntotal bytes received: 1024\nrx performance: 0.75 Gbits/s\n";
    assert_eq!(parse_report(tx, Direction::Tx, 1024)?.bytes, 1024);
    assert_eq!(
        parse_report(rx, Direction::Rx, 1024)?.gigabits_per_second,
        0.75
    );
    assert!(parse_report(tx, Direction::Rx, 1024).is_err());
    assert!(parse_report(tx, Direction::Tx, 1025).is_err());
    for invalid in [
        tx.replace("1024", "bad"),
        tx.replace("1.25", "NaN"),
        tx.replace("1.25", "-1"),
        tx.replace("1.25", "inf"),
        tx.replace("Gbits/s", "Mbits/s"),
        tx.replace("tx performance: 1.25 Gbits/s\n", ""),
        format!("{tx}total bytes sent: 1024\n"),
        format!("{tx}tx performance: 1.25 Gbits/s\n"),
    ] {
        assert!(
            parse_report(&invalid, Direction::Tx, 1024).is_err(),
            "{invalid}"
        );
    }
    for diagnostic in [
        "write: Connection reset by peer\n",
        "write: Transport endpoint is not connected\n",
        "write: Broken pipe\n",
        "'poll()' error\n",
    ] {
        assert!(expected_disconnect(1, diagnostic));
        assert!(!expected_disconnect(0, diagnostic));
    }
    assert!(expected_disconnect(141, ""));
    assert!(!expected_disconnect(137, ""));
    assert!(!expected_disconnect(
        1,
        "socket: Address family not supported by protocol\n"
    ));
    Ok(())
}

petri::test!(vsock_perf_peer_validation, |_| Some(()));

async fn vsock_perf_peer_validation(
    _params: petri::PetriTestParams<'_>,
    driver: DefaultDriver,
    (): (),
) -> anyhow::Result<()> {
    CancelContext::new()
        .with_timeout(Duration::from_secs(5))
        .until_cancelled(async {
            for (payload, expected, valid) in [
                (vec![0; 65536], 65536, true),
                (vec![0; 3], 2, false),
                (vec![1], 1, false),
            ] {
                let (sender, receiver) = UnixStream::pair()?;
                let mut sender = PolledSocket::new(&driver, sender)?;
                let mut receiver = PolledSocket::new(&driver, receiver)?;
                let progress = Progress::default();
                let (sent, received) = futures::join!(
                    async {
                        sender.write_all(&payload).await?;
                        sender.close().await
                    },
                    transfer(&mut receiver, &progress, expected, Direction::Tx),
                );
                sent?;
                assert_eq!(received.is_ok(), valid);
                if valid {
                    assert_eq!(progress.bytes.load(Ordering::Relaxed), expected);
                } else {
                    assert_eq!(received.unwrap_err().kind(), io::ErrorKind::InvalidData);
                }
            }

            let (sender, receiver) = UnixStream::pair()?;
            let mut sender = PolledSocket::new(&driver, sender)?;
            let mut receiver = PolledSocket::new(&driver, receiver)?;
            let progress = Progress::default();
            let ((), payload) = futures::try_join!(
                transfer(&mut sender, &progress, 65536, Direction::Rx),
                async {
                    let mut payload = Vec::new();
                    receiver.read_to_end(&mut payload).await?;
                    receiver.close().await?;
                    Ok::<_, io::Error>(payload)
                },
            )?;
            assert_eq!(payload, vec![0; 65536]);
            assert_eq!(progress.bytes.load(Ordering::Relaxed), 65536);
            Ok::<_, anyhow::Error>(())
        })
        .await
        .context("native peer protocol test timed out")?
}
