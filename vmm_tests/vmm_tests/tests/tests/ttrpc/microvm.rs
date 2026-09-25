// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! microVM lifecycle and snapshot tests for the TTRPC interface.

use super::launch_openvmm;
use anyhow::Context;
use futures::AsyncBufReadExt;
use futures::AsyncReadExt;
use futures::AsyncWriteExt;
#[cfg(windows)]
use guid::Guid;
use mesh::CancelContext;
use openvmm_ttrpc_vmservice as vmservice;
use pal_async::DefaultDriver;
#[cfg(windows)]
use pal_async::pipe::PolledPipe;
use pal_async::socket::PolledSocket;
#[cfg(windows)]
use pal_async::windows::pipe::ListeningPipe;
#[cfg(windows)]
use pal_async::windows::pipe::NamedPipeServer;
use petri::ResolvedArtifact;
use petri_artifacts_vmm_test::artifacts;
use std::path::Path;
use std::path::PathBuf;
use std::time::Duration;
#[cfg(unix)]
use unix_socket::UnixListener;
use unix_socket::UnixStream;

/// Resolves the artifacts of
/// [`test_ttrpc_microvm_linux_direct_lifecycle_and_snapshot`], which is
/// registered by the parent module to keep its `ttrpc::` test name.
pub(super) fn lifecycle_and_snapshot_artifacts(
    resolver: &petri::ArtifactResolver<'_>,
) -> Option<[ResolvedArtifact; 3]> {
    Some([
        resolver.require(artifacts::OPENVMM_NATIVE).erase(),
        resolver
            .require(artifacts::loadable::LINUX_DIRECT_TEST_KERNEL_X64)
            .erase(),
        resolver
            .require(artifacts::loadable::LINUX_DIRECT_TEST_INITRD_X64)
            .erase(),
    ])
}

fn microvm_portb_config(path: &Path) -> vmservice::SerialConfig {
    vmservice::SerialConfig {
        ports: vec![vmservice::serial_config::Config {
            port: 0,
            socket_path: path.to_string_lossy().into_owned(),
            connect: false,
        }],
    }
}

async fn read_restore_ready_event(
    reader: impl futures::AsyncRead + Unpin,
) -> anyhow::Result<Vec<u8>> {
    let mut reader = futures::io::BufReader::new(reader);
    let mut bytes = Vec::new();
    CancelContext::new()
        .with_timeout(Duration::from_secs(15))
        .until_cancelled(reader.read_until(b'\n', &mut bytes))
        .await
        .with_context(|| {
            format!(
                "timed out after {} bytes of the restore readiness event",
                bytes.len()
            )
        })??;
    Ok(bytes)
}

#[cfg(unix)]
struct RestoreReadyListener {
    path: PathBuf,
    listener: UnixListener,
}

#[cfg(windows)]
struct RestoreReadyListener {
    path: PathBuf,
    _server: NamedPipeServer,
    listener: ListeningPipe,
}

#[cfg(unix)]
impl RestoreReadyListener {
    fn bind(_driver: &DefaultDriver, path: PathBuf) -> anyhow::Result<Self> {
        let listener = UnixListener::bind(&path)?;
        Ok(Self { path, listener })
    }

    async fn read_event(self, driver: &DefaultDriver) -> anyhow::Result<Vec<u8>> {
        let mut listener = PolledSocket::new(driver, self.listener)?;
        let (connection, _) = listener.accept().await?;
        read_restore_ready_event(PolledSocket::new(driver, connection)?).await
    }
}

#[cfg(windows)]
impl RestoreReadyListener {
    fn bind(driver: &DefaultDriver, _path: PathBuf) -> anyhow::Result<Self> {
        let path = PathBuf::from(format!(
            "//./pipe/openvmm-restore-ready-{}-{}",
            std::process::id(),
            Guid::new_random()
        ));
        let server = NamedPipeServer::create(&path)?;
        let listener = server.accept(driver)?;
        Ok(Self {
            path,
            _server: server,
            listener,
        })
    }

    async fn read_event(self, driver: &DefaultDriver) -> anyhow::Result<Vec<u8>> {
        let connection = CancelContext::new()
            .with_timeout(Duration::from_secs(15))
            .until_cancelled(self.listener)
            .await
            .context("timed out accepting restore readiness pipe")??;
        read_restore_ready_event(PolledPipe::new(driver, connection)?).await
    }
}

impl RestoreReadyListener {
    fn path(&self) -> &Path {
        &self.path
    }
}

fn microvm_restore_request(
    snapshot_path: &Path,
    portb_path: &Path,
    restore_ready_path: &Path,
    restore_memory_bytes: u64,
) -> vmservice::CreateVmRequest {
    vmservice::CreateVmRequest {
        config: Some(vmservice::VmConfig {
            serial_config: Some(microvm_portb_config(portb_path)),
            machine_profile: vmservice::vm_config::MachineProfile::Microvm as i32,
            ..Default::default()
        }),
        log_id: String::new(),
        microvm_snapshot: Some(vmservice::MicrovmSnapshotConfig {
            restore_path: snapshot_path.to_string_lossy().into_owned(),
            restore_entropy: true,
            restore_processor_count: 1,
            restore_memory_bytes,
            restore_ready_path: restore_ready_path.to_string_lossy().into_owned(),
            ..Default::default()
        }),
    }
}

pub(super) async fn test_ttrpc_microvm_linux_direct_lifecycle_and_snapshot(
    params: petri::PetriTestParams<'_>,
    driver: DefaultDriver,
    [openvmm, kernel, initrd]: [ResolvedArtifact; 3],
) -> anyhow::Result<()> {
    use std::hash::Hash;
    use std::hash::Hasher;

    const MEMORY_MB: u64 = 128;
    const READY_MARKER: &[u8] = b"OPENVMM-LINUX-MPTABLE-SNAPSHOT-READY";
    const SNAPSHOT_CONTINUED_MARKER: &[u8] = b"SNAPSHOT-CONTINUED=1";
    const STATE_MARKER: &[u8] = b"STATE=1";
    const COMMAND_STATE: u8 = 5;
    const COMMAND_SHUTDOWN: u8 = 6;
    const KERNEL_OVERRIDE: &str = "OPENVMM_MICROVM_TEST_KERNEL";
    const INITRD_OVERRIDE: &str = "OPENVMM_MICROVM_TEST_INITRD";

    let portb_output = |bytes: &[u8]| -> anyhow::Result<String> {
        use std::fmt::Write as _;
        let mut output = String::new();
        for byte in bytes {
            writeln!(
                output,
                "printf '\\x{:02x}' | dd of=/dev/port bs=1 seek=233 count=1 conv=notrunc 2>/dev/null",
                byte
            )?;
        }
        Ok(output)
    };

    let kernel_override = std::env::var_os(KERNEL_OVERRIDE).map(PathBuf::from);
    let initrd_override = std::env::var_os(INITRD_OVERRIDE).map(PathBuf::from);
    anyhow::ensure!(
        kernel_override.is_some() == initrd_override.is_some(),
        "{KERNEL_OVERRIDE} and {INITRD_OVERRIDE} must be set together"
    );
    let kernel_path = kernel_override.as_deref().unwrap_or_else(|| kernel.get());
    let initrd_path = initrd_override.as_deref().unwrap_or_else(|| initrd.get());
    anyhow::ensure!(
        kernel_path.is_file(),
        "microVM test kernel does not exist: {}",
        kernel_path.display()
    );
    anyhow::ensure!(
        initrd_path.is_file(),
        "microVM test initrd does not exist: {}",
        initrd_path.display()
    );
    let processor_counts: &[u32] = if kernel_override.is_some() {
        &[1, 2, 4, 8]
    } else {
        // The packaged OpenVMM dependency kernel has CONFIG_X86_MPPARSE disabled.
        &[1]
    };

    let base_initrd = std::fs::read(initrd_path)
        .with_context(|| format!("failed to read initrd {}", initrd_path.display()))?;
    for &processor_count in processor_counts {
        let ready_marker = format!("OPENVMM-LINUX-MPTABLE-READY-{processor_count}");
        let cpu_failure_marker = format!("OPENVMM-LINUX-MPTABLE-CPU-FAILED-{processor_count}");
        let ioapic_failure_marker =
            format!("OPENVMM-LINUX-MPTABLE-IOAPIC-FAILED-{processor_count}");
        let acpi_failure_marker = format!("OPENVMM-LINUX-MPTABLE-ACPI-FAILED-{processor_count}");
        let smbios_failure_marker =
            format!("OPENVMM-LINUX-MPTABLE-SMBIOS-FAILED-{processor_count}");
        let script = format!(
            "#!/bin/busybox sh\n\
             mount -t devtmpfs devtmpfs /dev 2>/dev/null || true\n\
             mount -t proc proc /proc 2>/dev/null || true\n\
             mount -t sysfs sysfs /sys 2>/dev/null || true\n\
             cpus=$(grep -c '^processor' /proc/cpuinfo)\n\
             ioapic=0\n\
             grep -Eqi 'IO-?APIC' /proc/iomem && ioapic=1\n\
             grep -Eqi 'IO-?APIC' /proc/interrupts && ioapic=1\n\
             dmesg | grep -Eqi 'IO-?APIC' && ioapic=1\n\
             if [ \"$cpus\" != \"{processor_count}\" ]; then\n\
             {}\
             elif [ \"$ioapic\" != 1 ]; then\n\
             {}\
             elif [ -d /sys/firmware/acpi ]; then\n\
             {}\
             elif [ -e /sys/firmware/dmi/tables/DMI ] \\\n\
                 || [ -e /sys/firmware/dmi/tables/smbios_entry_point ]; then\n\
             {}\
             else\n\
             {}\
             fi\n\
             printf '\\x00' | dd of=/dev/port bs=1 seek=1540 count=1 conv=notrunc 2>/dev/null\n\
             while :; do sleep 1; done\n",
            portb_output(cpu_failure_marker.as_bytes())?,
            portb_output(ioapic_failure_marker.as_bytes())?,
            portb_output(acpi_failure_marker.as_bytes())?,
            portb_output(smbios_failure_marker.as_bytes())?,
            portb_output(ready_marker.as_bytes())?,
        );
        let probe_dir = if cfg!(target_os = "linux") {
            tempfile::Builder::new()
                .prefix("openvmm-ttrpc-linux-mptable-probe-")
                .tempdir_in("/tmp")
        } else {
            tempfile::tempdir()
        }?;
        let probe_initrd = initrd_cpio::inject_into_initrd(
            &base_initrd,
            "microvm-test",
            script.as_bytes(),
            0o100755,
        )
        .context("failed to inject the microVM lifecycle script")?;
        let probe_initrd_path = probe_dir.path().join("microvm-initrd.cpio.gz");
        std::fs::write(&probe_initrd_path, probe_initrd)
            .context("failed to write the microVM lifecycle initrd")?;
        let rpc_path = probe_dir.path().join("rpc.sock");
        let pidfile_path = probe_dir.path().join("openvmm.pid");
        let portb_path = probe_dir.path().join("portb.sock");
        let (mut child, client, _stderr_task) =
            launch_openvmm(&driver, &params, &openvmm, &rpc_path, &pidfile_path).await?;
        client
            .call()
            .start(
                vmservice::Vm::CreateVm,
                vmservice::CreateVmRequest {
                    config: Some(vmservice::VmConfig {
                        memory_config: Some(vmservice::MemoryConfig {
                            memory_mb: MEMORY_MB,
                            ..Default::default()
                        }),
                        processor_config: Some(vmservice::ProcessorConfig {
                            processor_count,
                            ..Default::default()
                        }),
                        serial_config: Some(microvm_portb_config(&portb_path)),
                        boot_config: Some(vmservice::vm_config::BootConfig::DirectBoot(
                            vmservice::DirectBoot {
                                kernel_path: kernel_path.to_string_lossy().into_owned(),
                                initrd_path: probe_initrd_path.to_string_lossy().into_owned(),
                                kernel_cmdline: "rdinit=/microvm-test".to_owned(),
                            },
                        )),
                        machine_profile: vmservice::vm_config::MachineProfile::Microvm as i32,
                        ..Default::default()
                    }),
                    log_id: String::new(),
                    microvm_snapshot: None,
                },
            )
            .await
            .map_err(|status| {
                anyhow::anyhow!(
                    "Linux direct {processor_count}-vCPU CreateVM failed: {}",
                    status.message
                )
            })?;
        let portb = PolledSocket::new(&driver, UnixStream::connect(&portb_path)?)?;
        let (mut portb_read, _portb_write) = portb.split();
        client
            .call()
            .start(vmservice::Vm::ResumeVm, ())
            .await
            .map_err(|status| {
                anyhow::anyhow!(
                    "Linux direct {processor_count}-vCPU ResumeVM failed: {}",
                    status.message
                )
            })?;
        let mut output = Vec::new();
        wait_for_bytes(&mut portb_read, &mut output, ready_marker.as_bytes()).await?;
        CancelContext::new()
            .with_timeout(Duration::from_secs(15))
            .until_cancelled(drain_until_closed(&mut portb_read, &mut output))
            .await
            .with_context(|| {
                format!("timed out draining Linux direct {processor_count}-vCPU server")
            })??;
        anyhow::ensure!(
            child.wait().await?.success(),
            "Linux direct {processor_count}-vCPU server failed"
        );
    }

    let script = format!(
        r#"#!/bin/busybox sh
mount -t devtmpfs devtmpfs /dev 2>/dev/null || true
read_port() {{
    dd if=/dev/port bs=1 skip="$1" count=1 2>/dev/null | od -An -tu1 | tr -d ' '
}}
write_port() {{
    octal=$(printf '%03o' "$2")
    printf "\\$octal" | dd of=/dev/port bs=1 seek="$1" count=1 conv=notrunc 2>/dev/null
}}
stty -F /dev/hvc0 raw -echo
next_byte() {{
    dd if=/dev/hvc0 bs=1 count=1 2>/dev/null | od -An -tu1 | tr -d ' '
}}
generation=0
{}
write_port 1541 0
generation=$((generation + 1))
{}
status=$(read_port 234)
if [ $((status & 8)) -eq 0 ]; then
{}
    write_port 1540 255
    while :; do sleep 1; done
fi
write_port 234 165
index=0
while [ "$index" -lt 19 ]; do
    read_port 233 >/dev/null
    index=$((index + 1))
done
target=$(read_port 233)
range_count=$(read_port 233)
remaining=$((range_count * 16 + 64))
index=0
while [ "$index" -lt "$remaining" ]; do
    read_port 233 >/dev/null
    index=$((index + 1))
done
write_port 1541 2
if [ "$target" != 1 ]; then
{}
    write_port 1540 255
    while :; do sleep 1; done
fi
case "$range_count" in
    0)
{}
        ;;
    1)
{}
        ;;
    *)
{}
        write_port 1540 255
        while :; do sleep 1; done
        ;;
esac
while :; do
    command=$(next_byte)
    case "$command" in
        {COMMAND_STATE})
            if [ "$generation" = 1 ]; then
{}
            else
{}
            fi
            ;;
        {COMMAND_SHUTDOWN})
            exit_status=$(next_byte)
            write_port 1540 "$exit_status"
            while :; do sleep 1; done
            ;;
        *)
{}
            ;;
    esac
done
"#,
        portb_output(READY_MARKER)?,
        portb_output(SNAPSHOT_CONTINUED_MARKER)?,
        portb_output(b"RESTORE-PACKET-MISSING")?,
        portb_output(b"RESTORE-TARGET-INVALID")?,
        portb_output(b"RESTORE-TARGET=1 MEMORY-RANGES=0")?,
        portb_output(b"RESTORE-TARGET=1 MEMORY-RANGES=1")?,
        portb_output(b"RESTORE-RANGE-COUNT-INVALID")?,
        portb_output(STATE_MARKER)?,
        portb_output(b"STATE-INVALID")?,
        portb_output(b"UNKNOWN-COMMAND")?,
    );

    let tempdir = if cfg!(target_os = "linux") {
        tempfile::Builder::new()
            .prefix("openvmm-ttrpc-linux-mptable-")
            .tempdir_in("/tmp")
    } else {
        tempfile::tempdir()
    }?;
    let snapshot_path = tempdir.path().join("snapshot");
    let fingerprint = |path: &Path| -> anyhow::Result<u64> {
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        std::fs::read(path)?.hash(&mut hasher);
        Ok(hasher.finish())
    };
    let initrd_data =
        initrd_cpio::inject_into_initrd(&base_initrd, "microvm-test", script.as_bytes(), 0o100755)
            .context("failed to inject the microVM snapshot script")?;
    let initrd = tempdir.path().join("microvm-initrd.cpio.gz");
    std::fs::write(&initrd, initrd_data).context("failed to write the microVM test initrd")?;

    let rpc_path = tempdir.path().join("capture-rpc.sock");
    let pidfile_path = tempdir.path().join("capture.pid");
    let portb_path = tempdir.path().join("capture-portb.sock");
    let (mut child, client, _stderr_task) =
        launch_openvmm(&driver, &params, &openvmm, &rpc_path, &pidfile_path).await?;
    client
        .call()
        .start(
            vmservice::Vm::CreateVm,
            vmservice::CreateVmRequest {
                config: Some(vmservice::VmConfig {
                    memory_config: Some(vmservice::MemoryConfig {
                        memory_mb: MEMORY_MB,
                        ..Default::default()
                    }),
                    processor_config: Some(vmservice::ProcessorConfig {
                        processor_count: 2,
                        ..Default::default()
                    }),
                    serial_config: Some(microvm_portb_config(&portb_path)),
                    boot_config: Some(vmservice::vm_config::BootConfig::DirectBoot(
                        vmservice::DirectBoot {
                            kernel_path: kernel_path.to_string_lossy().into_owned(),
                            initrd_path: initrd.to_string_lossy().into_owned(),
                            kernel_cmdline: "maxcpus=1 rdinit=/microvm-test".to_owned(),
                        },
                    )),
                    machine_profile: vmservice::vm_config::MachineProfile::Microvm as i32,
                    ..Default::default()
                }),
                log_id: String::new(),
                microvm_snapshot: Some(vmservice::MicrovmSnapshotConfig {
                    destination_path: snapshot_path.to_string_lossy().into_owned(),
                    quiesce_timeout_ms: 5_000,
                    memory_capacity_bytes: 512 * 1024 * 1024,
                    ..Default::default()
                }),
            },
        )
        .await
        .map_err(|status| {
            anyhow::anyhow!("Linux direct capture CreateVM failed: {}", status.message)
        })?;
    let portb = PolledSocket::new(&driver, UnixStream::connect(&portb_path)?)?;
    let (mut portb_read, _portb_write) = portb.split();
    client
        .call()
        .start(vmservice::Vm::ResumeVm, ())
        .await
        .map_err(|status| {
            anyhow::anyhow!("Linux direct capture ResumeVM failed: {}", status.message)
        })?;
    let mut source_output = Vec::new();
    wait_for_bytes(&mut portb_read, &mut source_output, READY_MARKER).await?;
    CancelContext::new()
        .with_timeout(Duration::from_secs(15))
        .until_cancelled(drain_until_closed(&mut portb_read, &mut source_output))
        .await
        .context("timed out draining Linux direct capture source")??;
    anyhow::ensure!(
        child.wait().await?.success(),
        "Linux direct capture server failed"
    );
    let (manifest, _) =
        openvmm_helpers::snapshot::restore::read_snapshot(&snapshot_path, MEMORY_MB * 1024 * 1024)?;
    let contract = manifest
        .machine_contract
        .context("captured microVM snapshot is missing its machine contract")?;
    assert_eq!(
        contract.boot_layout_version,
        openvmm_helpers::snapshot::microvm::MICROVM_BOOT_LAYOUT_VERSION
    );
    anyhow::ensure!(
        !pidfile_path.exists(),
        "Linux direct capture source PID remained after snapshot commit"
    );

    let before = ["manifest.bin", "state.bin", "memory.bin"]
        .map(|name| fingerprint(&snapshot_path.join(name)))
        .into_iter()
        .collect::<anyhow::Result<Vec<_>>>()?;

    for restore_index in 0..2 {
        let restore_memory_bytes = if restore_index == 0 {
            MEMORY_MB * 1024 * 1024
        } else {
            256 * 1024 * 1024
        };
        let restore_target_marker = if restore_index == 0 {
            b"RESTORE-TARGET=1 MEMORY-RANGES=0".as_slice()
        } else {
            b"RESTORE-TARGET=1 MEMORY-RANGES=1".as_slice()
        };
        let rpc_path = tempdir
            .path()
            .join(format!("restore-{restore_index}-rpc.sock"));
        let pidfile_path = tempdir.path().join(format!("restore-{restore_index}.pid"));
        let portb_path = tempdir
            .path()
            .join(format!("restore-{restore_index}-portb.sock"));
        let restore_ready = RestoreReadyListener::bind(
            &driver,
            tempdir
                .path()
                .join(format!("restore-{restore_index}-ready.sock")),
        )?;
        let (mut child, client, _stderr_task) =
            launch_openvmm(&driver, &params, &openvmm, &rpc_path, &pidfile_path).await?;
        client
            .call()
            .start(
                vmservice::Vm::CreateVm,
                microvm_restore_request(
                    &snapshot_path,
                    &portb_path,
                    restore_ready.path(),
                    restore_memory_bytes,
                ),
            )
            .await
            .map_err(|status| {
                anyhow::anyhow!(
                    "Linux direct restore {restore_index} CreateVM failed: {}",
                    status.message
                )
            })?;
        let portb = PolledSocket::new(&driver, UnixStream::connect(&portb_path)?)?;
        let (mut portb_read, mut portb_write) = portb.split();
        let mut resume_cancel = CancelContext::new().with_timeout(Duration::from_secs(15));
        let (resume, readiness) = futures::join!(
            resume_cancel.until_cancelled(client.call().start(vmservice::Vm::ResumeVm, ())),
            restore_ready.read_event(&driver)
        );
        resume
            .with_context(|| format!("timed out resuming Linux direct restore {restore_index}"))?
            .map_err(|status| {
                anyhow::anyhow!(
                    "Linux direct restore {restore_index} ResumeVM failed: {}",
                    status.message
                )
            })?;
        anyhow::ensure!(
            readiness? == openvmm_defs::worker::RESTORE_READY_EVENT_V1,
            "Linux direct restore {restore_index} did not publish readiness"
        );

        let mut restore_output = Vec::new();
        wait_for_bytes(
            &mut portb_read,
            &mut restore_output,
            SNAPSHOT_CONTINUED_MARKER,
        )
        .await
        .with_context(|| {
            format!("Linux direct restore {restore_index} did not continue after snapshot")
        })?;
        wait_for_bytes(&mut portb_read, &mut restore_output, restore_target_marker)
            .await
            .with_context(|| {
                format!("Linux direct restore {restore_index} did not report restore targets")
            })?;
        portb_write.write_all(&[COMMAND_STATE]).await?;
        portb_write.flush().await?;
        wait_for_bytes(&mut portb_read, &mut restore_output, STATE_MARKER)
            .await
            .with_context(|| {
                format!("Linux direct restore {restore_index} did not answer state query")
            })?;
        portb_write.write_all(&[COMMAND_SHUTDOWN, 0]).await?;
        portb_write.flush().await?;
        CancelContext::new()
            .with_timeout(Duration::from_secs(15))
            .until_cancelled(drain_until_closed(&mut portb_read, &mut restore_output))
            .await
            .with_context(|| {
                let tail = restore_output.len().saturating_sub(256);
                format!(
                    "timed out draining Linux direct restore {restore_index} after output {:?}",
                    String::from_utf8_lossy(&restore_output[tail..])
                )
            })??;
        let status = CancelContext::new()
            .with_timeout(Duration::from_secs(15))
            .until_cancelled(child.wait())
            .await
            .with_context(|| {
                format!("timed out waiting for Linux direct restore {restore_index} server")
            })??;
        anyhow::ensure!(
            status.success(),
            "Linux direct restore {restore_index} server failed"
        );
        anyhow::ensure!(
            !pidfile_path.exists(),
            "Linux direct restore {restore_index} PID remained after shutdown"
        );
        let after = ["manifest.bin", "state.bin", "memory.bin"]
            .map(|name| fingerprint(&snapshot_path.join(name)))
            .into_iter()
            .collect::<anyhow::Result<Vec<_>>>()?;
        anyhow::ensure!(
            before == after,
            "Linux direct restore {restore_index} modified snapshot artifacts"
        );
    }
    Ok(())
}

async fn wait_for_bytes(
    reader: &mut (impl futures::AsyncRead + Unpin),
    output: &mut Vec<u8>,
    marker: &[u8],
) -> anyhow::Result<()> {
    CancelContext::new()
        .with_timeout(Duration::from_secs(60))
        .until_cancelled(async {
            let mut buffer = [0_u8; 4096];
            loop {
                let count = reader.read(&mut buffer).await?;
                anyhow::ensure!(
                    count != 0,
                    "portb closed before the expected marker after bytes {:?}",
                    String::from_utf8_lossy(&output[..output.len().min(256)])
                );
                output.extend_from_slice(&buffer[..count]);
                if output.windows(marker.len()).any(|window| window == marker) {
                    return Ok(());
                }
            }
        })
        .await
        .context("timed out waiting for portb output")?
}

async fn drain_until_closed(
    reader: &mut (impl futures::AsyncRead + Unpin),
    output: &mut Vec<u8>,
) -> std::io::Result<()> {
    let mut buffer = [0_u8; 4096];
    loop {
        match reader.read(&mut buffer).await {
            Ok(0) => return Ok(()),
            Ok(count) => output.extend_from_slice(&buffer[..count]),
            Err(error)
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::ConnectionReset
                        | std::io::ErrorKind::ConnectionAborted
                        | std::io::ErrorKind::BrokenPipe
                ) =>
            {
                return Ok(());
            }
            Err(error) => return Err(error),
        }
    }
}
