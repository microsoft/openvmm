// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! microVM lifecycle and snapshot tests for the TTRPC interface.

use super::launch_openvmm;
use anyhow::Context;
use futures::AsyncBufReadExt;
use futures::AsyncReadExt;
#[cfg(windows)]
use guid::Guid;
use mesh::CancelContext;
use openvmm_ttrpc_vmservice as vmservice;
use pal_async::DefaultDriver;
use pal_async::pipe::PolledPipe;
use pal_async::process::PolledChild;
use pal_async::socket::PolledSocket;
use pal_async::task::Spawn;
use pal_async::task::Task;
#[cfg(windows)]
use pal_async::windows::pipe::ListeningPipe;
#[cfg(windows)]
use pal_async::windows::pipe::NamedPipeServer;
use petri::ResolvedArtifact;
use petri::TestChild;
use petri_artifacts_vmm_test::artifacts;
use std::ffi::OsString;
use std::path::Path;
use std::path::PathBuf;
use std::process::Stdio;
use std::time::Duration;
#[cfg(unix)]
use unix_socket::UnixListener;
use unix_socket::UnixStream;

/// Resolves the artifacts of
/// [`test_ttrpc_microvm_linux_direct_lifecycle_and_snapshot`] and
/// [`test_ttrpc_microvm_device_snapshots`], which the parent module registers
/// to keep their `ttrpc::` test names.
///
/// Returns `None` on hosts other than x86_64: the native OpenVMM runs guests
/// of the host's architecture, and the microVM profile supports only x86_64
/// guests.
pub(super) fn lifecycle_and_snapshot_artifacts(
    resolver: &petri::ArtifactResolver<'_>,
) -> Option<[ResolvedArtifact; 3]> {
    if petri_artifacts_common::tags::MachineArch::host()
        != petri_artifacts_common::tags::MachineArch::X86_64
    {
        return None;
    }
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
            restore_processor_count: 1,
            restore_memory_bytes,
            restore_ready_path: restore_ready_path.to_string_lossy().into_owned(),
            ..Default::default()
        }),
    }
}

/// Environment variables that replace the packaged test kernel and initrd,
/// which they must do together.
const KERNEL_OVERRIDE: &str = "OPENVMM_MICROVM_TEST_KERNEL";
const INITRD_OVERRIDE: &str = "OPENVMM_MICROVM_TEST_INITRD";

/// The Linux direct guest of the microVM tests.
struct MicrovmTestGuest {
    /// The guest kernel.
    kernel: PathBuf,
    /// The contents of the guest's initrd, into which a test injects its
    /// script.
    initrd: Vec<u8>,
    /// Whether the overrides replaced the packaged kernel and initrd.
    overridden: bool,
}

/// Returns the packaged test kernel and initrd, `kernel` and `initrd`, or the
/// pair that [`KERNEL_OVERRIDE`] and [`INITRD_OVERRIDE`] name.
fn microvm_test_guest(
    kernel: &ResolvedArtifact,
    initrd: &ResolvedArtifact,
) -> anyhow::Result<MicrovmTestGuest> {
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
    Ok(MicrovmTestGuest {
        kernel: kernel_path.to_owned(),
        initrd: std::fs::read(initrd_path)
            .with_context(|| format!("failed to read initrd {}", initrd_path.display()))?,
        overridden: kernel_override.is_some(),
    })
}

/// Returns guest shell commands that write `bytes` to portb.
fn portb_output(bytes: &[u8]) -> anyhow::Result<String> {
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

    let MicrovmTestGuest {
        kernel: kernel_path,
        initrd: base_initrd,
        overridden,
    } = microvm_test_guest(&kernel, &initrd)?;
    let processor_counts: &[u32] = if overridden {
        &[1, 2, 4, 8]
    } else {
        // The packaged OpenVMM dependency kernel has CONFIG_X86_MPPARSE disabled.
        &[1]
    };

    // The CPU profile of the cold boots, `auto` unless one falls back to
    // `host`; restores use the snapshot's profile.
    let mut cpu_profile = "";
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
        let CreatedMicrovm {
            mut child,
            client,
            stderr_task: _stderr_task,
            portb_path,
            ..
        } = create_cold_boot(
            &driver,
            &params,
            &openvmm,
            probe_dir.path(),
            "probe",
            &format!("Linux direct {processor_count}-vCPU"),
            &mut cpu_profile,
            |portb_path, cpu_profile| vmservice::CreateVmRequest {
                config: Some(vmservice::VmConfig {
                    memory_config: Some(vmservice::MemoryConfig {
                        memory_mb: MEMORY_MB,
                        ..Default::default()
                    }),
                    processor_config: Some(vmservice::ProcessorConfig {
                        processor_count,
                        ..Default::default()
                    }),
                    serial_config: Some(microvm_portb_config(portb_path)),
                    boot_config: Some(vmservice::vm_config::BootConfig::DirectBoot(
                        vmservice::DirectBoot {
                            kernel_path: kernel_path.to_string_lossy().into_owned(),
                            initrd_path: probe_initrd_path.to_string_lossy().into_owned(),
                            kernel_cmdline: "rdinit=/microvm-test".to_owned(),
                        },
                    )),
                    machine_profile: vmservice::vm_config::MachineProfile::Microvm as i32,
                    cpu_profile: cpu_profile.to_owned(),
                    ..Default::default()
                }),
                log_id: String::new(),
                microvm_snapshot: None,
            },
        )
        .await?;
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
magic="$(read_port 233) $(read_port 233) $(read_port 233) $(read_port 233)"
flags=$(read_port 233)
target=$(read_port 233)
range_count=$(read_port 233)
if [ "$magic" != "79 86 82 1" ] || [ $((flags & 4)) -eq 0 ]; then
{}
    write_port 1540 255
    while :; do sleep 1; done
fi
remaining=$((17 + range_count * 16 + 64 + 16))
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
# portb takes no host input, so the restored guest reports its state and
# shuts down without commands.
if [ "$generation" = 1 ]; then
{}
else
{}
fi
write_port 1540 0
while :; do sleep 1; done
"#,
        portb_output(READY_MARKER)?,
        portb_output(SNAPSHOT_CONTINUED_MARKER)?,
        portb_output(b"RESTORE-PACKET-MISSING")?,
        portb_output(b"RESTORE-PACKET-INVALID")?,
        portb_output(b"RESTORE-TARGET-INVALID")?,
        portb_output(b"RESTORE-TARGET=1 MEMORY-RANGES=0")?,
        portb_output(b"RESTORE-TARGET=1 MEMORY-RANGES=1")?,
        portb_output(b"RESTORE-RANGE-COUNT-INVALID")?,
        portb_output(STATE_MARKER)?,
        portb_output(b"STATE-INVALID")?,
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

    let CreatedMicrovm {
        mut child,
        client,
        stderr_task: _stderr_task,
        portb_path,
        pidfile_path,
    } = create_cold_boot(
        &driver,
        &params,
        &openvmm,
        tempdir.path(),
        "capture",
        "Linux direct capture",
        &mut cpu_profile,
        |portb_path, cpu_profile| vmservice::CreateVmRequest {
            config: Some(vmservice::VmConfig {
                memory_config: Some(vmservice::MemoryConfig {
                    memory_mb: MEMORY_MB,
                    ..Default::default()
                }),
                processor_config: Some(vmservice::ProcessorConfig {
                    processor_count: 2,
                    ..Default::default()
                }),
                serial_config: Some(microvm_portb_config(portb_path)),
                boot_config: Some(vmservice::vm_config::BootConfig::DirectBoot(
                    vmservice::DirectBoot {
                        kernel_path: kernel_path.to_string_lossy().into_owned(),
                        initrd_path: initrd.to_string_lossy().into_owned(),
                        kernel_cmdline: "maxcpus=1 rdinit=/microvm-test".to_owned(),
                    },
                )),
                machine_profile: vmservice::vm_config::MachineProfile::Microvm as i32,
                cpu_profile: cpu_profile.to_owned(),
                ..Default::default()
            }),
            log_id: String::new(),
            microvm_snapshot: Some(vmservice::MicrovmSnapshotConfig {
                destination_path: snapshot_path.to_string_lossy().into_owned(),
                memory_capacity_bytes: 512 * 1024 * 1024,
                ..Default::default()
            }),
        },
    )
    .await?;
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
    // The capture has no deadline of its own, so this drain bounds it, and
    // the server is killed when its `TestChild` drops.
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
        let (mut portb_read, _portb_write) = portb.split();
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
        wait_for_bytes(&mut portb_read, &mut restore_output, STATE_MARKER)
            .await
            .with_context(|| {
                format!("Linux direct restore {restore_index} did not report its state")
            })?;
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

/// Written by the guest of [`test_ttrpc_microvm_device_snapshots`] when it
/// boots.
const DEVICES_READY_MARKER: &[u8] = b"OPENVMM-MICROVM-DEVICES-READY";
/// Written by that guest once a restore of its snapshot has passed the
/// post-restore gate.
const DEVICES_RESTORED_MARKER: &[u8] = b"OPENVMM-MICROVM-DEVICES-RESTORED";
/// The guest RAM of the device snapshot tests, in MiB.
const DEVICES_MEMORY_MB: u64 = 128;
/// The static network identity of the device snapshot tests.
const DEVICES_NETWORK: &str = "10.0.0.2/24";
/// The guest target of the device snapshot tests' aggregate filesystem.
const DEVICES_SHARE_TARGET: &str = "/mnt/share";

/// Returns the script of the guest of [`test_ttrpc_microvm_device_snapshots`].
///
/// The guest requests a snapshot, with paired scratch when `paired_scratch`
/// is set. Only a restore continues: it selects and reads the restore packet,
/// acknowledges the restore through the snapshot port, as every snapshot of a
/// tier requires before host input resumes, and exits with status 0.
fn device_snapshot_script(paired_scratch: bool) -> anyhow::Result<String> {
    Ok(format!(
        r#"#!/bin/busybox sh
mount -t devtmpfs devtmpfs /dev 2>/dev/null || true
read_port() {{
    dd if=/dev/port bs=1 skip="$1" count=1 2>/dev/null | od -An -tu1 | tr -d ' '
}}
write_port() {{
    octal=$(printf '%03o' "$2")
    printf "\\$octal" | dd of=/dev/port bs=1 seek="$1" count=1 conv=notrunc 2>/dev/null
}}
{ready}
write_port 1541 {policy}
status=$(read_port 234)
if [ $((status & 2)) -eq 0 ]; then
{missing}
    write_port 1540 255
    while :; do sleep 1; done
fi
write_port 234 165
index=0
while [ "$index" -lt 6 ]; do
    read_port 233 >/dev/null
    index=$((index + 1))
done
range_count=$(read_port 233)
remaining=$((17 + range_count * 16 + 64 + 16))
index=0
while [ "$index" -lt "$remaining" ]; do
    read_port 233 >/dev/null
    index=$((index + 1))
done
write_port 1541 2
{restored}
write_port 1540 0
while :; do sleep 1; done
"#,
        ready = portb_output(DEVICES_READY_MARKER)?,
        policy = u8::from(paired_scratch),
        missing = portb_output(b"RESTORE-PACKET-MISSING")?,
        restored = portb_output(DEVICES_RESTORED_MARKER)?,
    ))
}

/// The host files of the microVM devices of
/// [`test_ttrpc_microvm_device_snapshots`].
struct DeviceFiles {
    dir: PathBuf,
    /// The read-only distro layer.
    distro: PathBuf,
    /// The directory child of the aggregate filesystem, which hides its
    /// `secret` directory.
    data: PathBuf,
    /// The regular file child of the aggregate filesystem.
    config: PathBuf,
}

impl DeviceFiles {
    /// Creates the files in `dir`.
    fn new(dir: &Path) -> anyhow::Result<Self> {
        let distro = dir.join("distro.img");
        std::fs::write(&distro, vec![0; 1 << 20])?;
        let share = dir.join("share");
        let data = share.join("data");
        std::fs::create_dir_all(data.join("secret"))?;
        let config = share.join("config.json");
        std::fs::write(&config, b"{}")?;
        Ok(Self {
            dir: dir.to_owned(),
            distro,
            data,
            config,
        })
    }

    /// Creates a fresh scratch device for `name`.
    fn scratch(&self, name: &str) -> anyhow::Result<PathBuf> {
        let path = self.dir.join(format!("{name}-scratch.img"));
        std::fs::write(&path, vec![0; 1 << 20])?;
        Ok(path)
    }

    /// Returns the sandbox blocks of a request: the distro layer, and
    /// `scratch` if any.
    fn blocks(&self, scratch: Option<&Path>) -> Vec<vmservice::MicrovmSandboxBlock> {
        use vmservice::microvm_sandbox_block::Role;

        std::iter::once((Role::Distro, self.distro.as_path(), true))
            .chain(scratch.map(|scratch| (Role::Scratch, scratch, false)))
            .map(|(role, path, read_only)| vmservice::MicrovmSandboxBlock {
                role: role as i32,
                backend: Some(super::file_disk(path)),
                read_only,
            })
            .collect()
    }

    /// Returns the command line's options of [`Self::blocks`].
    fn block_options(&self, scratch: Option<&Path>) -> Vec<OsString> {
        let mut options = vec![
            "--microvm-sandbox-block".into(),
            format!("distro:file:{},ro", self.distro.display()).into(),
        ];
        if let Some(scratch) = scratch {
            options.extend([
                "--microvm-sandbox-block".into(),
                format!("scratch:file:{}", scratch.display()).into(),
            ]);
        }
        options
    }

    /// Returns the aggregate filesystem of a request, with a directory and a
    /// file child, and a denied path.
    fn filesystem(&self) -> vmservice::VirtioFs {
        vmservice::VirtioFs {
            tag: "microvm".to_owned(),
            guest_mount_target: DEVICES_SHARE_TARGET.to_owned(),
            denied_paths: vec![self.data.join("secret").to_string_lossy().into_owned()],
            aggregate_children: vec![
                vmservice::virtio_fs::AggregateChild {
                    name: "data".to_owned(),
                    host_path: self.data.to_string_lossy().into_owned(),
                    read_write: true,
                    file: false,
                },
                vmservice::virtio_fs::AggregateChild {
                    name: "config.json".to_owned(),
                    host_path: self.config.to_string_lossy().into_owned(),
                    read_write: false,
                    file: true,
                },
            ],
            ..Default::default()
        }
    }

    /// Returns the command line's options of [`Self::filesystem`].
    fn filesystem_options(&self) -> Vec<OsString> {
        vec![
            "--mount-aggregate".into(),
            DEVICES_SHARE_TARGET.into(),
            "--mount-child".into(),
            format!("data,{},rw", self.data.display()).into(),
            "--mount-child".into(),
            format!("config.json,{},ro,file", self.config.display()).into(),
            "--mount-deny".into(),
            self.data.join("secret").into(),
        ]
    }
}

/// Returns the network of a request: a NIC template, whose complete deny-all
/// policy a platform restore may replace, at `address`, or, with an empty
/// `address`, at the address of a restored snapshot.
fn device_network(address: &str) -> vmservice::MicrovmNetwork {
    use vmservice::microvm_network::Action;

    vmservice::MicrovmNetwork {
        address: address.to_owned(),
        profile: vmservice::microvm_network::Profile::Portable as i32,
        egress: Action::Deny as i32,
        host_loopback: Action::Deny as i32,
        ..Default::default()
    }
}

/// Returns the command line's options of [`device_network`].
fn device_network_options(address: &str) -> Vec<OsString> {
    let mut options = Vec::<OsString>::new();
    if !address.is_empty() {
        options.extend(["--net".into(), address.into()]);
    }
    options.extend(
        [
            "--network-profile",
            "portable",
            "--network-egress",
            "deny",
            "--host-loopback",
            "deny",
        ]
        .map(OsString::from),
    );
    options
}

/// Writes the initrd of a test guest named `name` to `dir`, with `script` as
/// its `/init`.
///
/// The script replaces the initrd's own `/init`, rather than being selected
/// with `rdinit=`, since a platform snapshot's kernel command line may hold
/// only the tokens that every clone of it shares.
fn write_test_initrd(
    dir: &Path,
    name: &str,
    guest: &MicrovmTestGuest,
    script: &str,
) -> anyhow::Result<PathBuf> {
    let initrd =
        initrd_cpio::inject_into_initrd(&guest.initrd, "init", script.as_bytes(), 0o100755)
            .with_context(|| format!("failed to inject the {name} script"))?;
    let path = dir.join(format!("{name}-initrd.cpio.gz"));
    std::fs::write(&path, initrd).with_context(|| format!("failed to write the {name} initrd"))?;
    Ok(path)
}

/// Returns the cold-boot configuration of a device snapshot test's guest,
/// booted from `initrd`, with portb at `portb_path`, the CPU profile
/// `cpu_profile`, and `memory` and `devices`.
fn device_cold_boot_config(
    guest: &MicrovmTestGuest,
    initrd: &Path,
    portb_path: &Path,
    cpu_profile: &str,
    memory: vmservice::MemoryConfig,
    devices: vmservice::DevicesConfig,
) -> vmservice::VmConfig {
    vmservice::VmConfig {
        memory_config: Some(vmservice::MemoryConfig {
            memory_mb: DEVICES_MEMORY_MB,
            ..memory
        }),
        processor_config: Some(vmservice::ProcessorConfig {
            processor_count: 1,
            ..Default::default()
        }),
        serial_config: Some(microvm_portb_config(portb_path)),
        boot_config: Some(vmservice::vm_config::BootConfig::DirectBoot(
            vmservice::DirectBoot {
                kernel_path: guest.kernel.to_string_lossy().into_owned(),
                initrd_path: initrd.to_string_lossy().into_owned(),
                kernel_cmdline: String::new(),
            },
        )),
        devices_config: Some(devices),
        machine_profile: vmservice::vm_config::MachineProfile::Microvm as i32,
        cpu_profile: cpu_profile.to_owned(),
        ..Default::default()
    }
}

/// Cold boots, over the management RPC, the microVM that `request` describes
/// for a portb endpoint and a CPU profile, and waits until it captures its
/// snapshot to `snapshot` and the server exits.
async fn capture_over_rpc(
    driver: &DefaultDriver,
    params: &petri::PetriTestParams<'_>,
    openvmm: &ResolvedArtifact,
    dir: &Path,
    name: &str,
    cpu_profile: &mut &'static str,
    snapshot: &Path,
    request: impl Fn(&Path, &str) -> vmservice::CreateVmRequest,
) -> anyhow::Result<()> {
    let CreatedMicrovm {
        mut child,
        client,
        stderr_task: _stderr_task,
        portb_path,
        pidfile_path,
    } = create_cold_boot(
        driver,
        params,
        openvmm,
        dir,
        name,
        &format!("{name} capture"),
        cpu_profile,
        request,
    )
    .await?;
    let portb = PolledSocket::new(driver, UnixStream::connect(&portb_path)?)?;
    let (mut portb_read, _portb_write) = portb.split();
    client
        .call()
        .start(vmservice::Vm::ResumeVm, ())
        .await
        .map_err(|status| anyhow::anyhow!("{name} capture ResumeVM failed: {}", status.message))?;
    let mut output = Vec::new();
    wait_for_bytes(&mut portb_read, &mut output, DEVICES_READY_MARKER).await?;
    CancelContext::new()
        .with_timeout(Duration::from_secs(15))
        .until_cancelled(drain_until_closed(&mut portb_read, &mut output))
        .await
        .with_context(|| format!("timed out draining the {name} capture source"))??;
    let status = CancelContext::new()
        .with_timeout(Duration::from_secs(15))
        .until_cancelled(child.wait())
        .await
        .with_context(|| format!("timed out waiting for the {name} capture server"))??;
    anyhow::ensure!(status.success(), "{name} capture server failed: {status}");
    anyhow::ensure!(
        !pidfile_path.exists(),
        "{name} capture source PID remained after the snapshot commit"
    );
    anyhow::ensure!(
        snapshot.join("manifest.bin").is_file(),
        "{name} capture published no snapshot"
    );
    Ok(())
}

/// The client of a restored microVM's console listener.
#[cfg(windows)]
type ConsoleClient = std::fs::File;
/// The client of a restored microVM's console listener.
#[cfg(not(windows))]
type ConsoleClient = UnixStream;

/// Returns the endpoint of a console listener that the restore `name`
/// attaches: a Unix socket beside the snapshots in `dir`, or a named pipe in
/// the namespace of microVM consoles.
fn console_listener(dir: &Path, name: &str) -> String {
    #[cfg(windows)]
    {
        let _ = dir;
        format!(
            "//./pipe/openvmm-microvm-ttrpc-{name}-{}",
            Guid::new_random()
        )
    }
    #[cfg(not(windows))]
    {
        dir.join(format!("{name}-console.sock"))
            .to_string_lossy()
            .into_owned()
    }
}

/// Connects a client to the console listener at `endpoint`.
fn connect_console(endpoint: &str) -> anyhow::Result<ConsoleClient> {
    #[cfg(windows)]
    let client = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(endpoint);
    #[cfg(not(windows))]
    let client = UnixStream::connect(endpoint);
    client.with_context(|| format!("failed to connect to the console listener {endpoint}"))
}

/// Restores, over the management RPC, the snapshot that `request` names for a
/// portb endpoint, and waits until its guest passes the restore gate and
/// exits. When the request attaches a console listener at `console`, a client
/// connects to it before the restored guest runs.
async fn restore_over_rpc(
    driver: &DefaultDriver,
    params: &petri::PetriTestParams<'_>,
    openvmm: &ResolvedArtifact,
    dir: &Path,
    name: &str,
    console: Option<&str>,
    request: impl FnOnce(&Path) -> vmservice::CreateVmRequest,
) -> anyhow::Result<()> {
    let rpc_path = dir.join(format!("{name}-rpc.sock"));
    let pidfile_path = dir.join(format!("{name}.pid"));
    let portb_path = dir.join(format!("{name}-portb.sock"));
    let (mut child, client, _stderr_task) =
        launch_openvmm(driver, params, openvmm, &rpc_path, &pidfile_path).await?;
    client
        .call()
        .start(vmservice::Vm::CreateVm, request(&portb_path))
        .await
        .map_err(|status| anyhow::anyhow!("{name} restore CreateVM failed: {}", status.message))?;
    let _console = console
        .map(connect_console)
        .transpose()
        .with_context(|| format!("the {name} restore attached no console listener"))?;
    let portb = PolledSocket::new(driver, UnixStream::connect(&portb_path)?)?;
    let (mut portb_read, _portb_write) = portb.split();
    CancelContext::new()
        .with_timeout(Duration::from_secs(15))
        .until_cancelled(client.call().start(vmservice::Vm::ResumeVm, ()))
        .await
        .with_context(|| format!("timed out resuming the {name} restore"))?
        .map_err(|status| anyhow::anyhow!("{name} restore ResumeVM failed: {}", status.message))?;
    let mut output = Vec::new();
    wait_for_bytes(&mut portb_read, &mut output, DEVICES_RESTORED_MARKER)
        .await
        .with_context(|| format!("the {name} restore did not pass the restore gate"))?;
    CancelContext::new()
        .with_timeout(Duration::from_secs(15))
        .until_cancelled(drain_until_closed(&mut portb_read, &mut output))
        .await
        .with_context(|| format!("timed out draining the {name} restore"))??;
    let status = CancelContext::new()
        .with_timeout(Duration::from_secs(15))
        .until_cancelled(child.wait())
        .await
        .with_context(|| format!("timed out waiting for the {name} restore server"))??;
    anyhow::ensure!(status.success(), "{name} restore server failed: {status}");
    Ok(())
}

/// Runs OpenVMM's command line with `args`, which writes portb to its stdout,
/// until its guest writes `marker` there and the process exits successfully.
async fn run_command_line(
    driver: &DefaultDriver,
    params: &petri::PetriTestParams<'_>,
    openvmm: &ResolvedArtifact,
    name: &str,
    args: Vec<OsString>,
    marker: &[u8],
) -> anyhow::Result<()> {
    tracing::info!(?args, "running the {name} OpenVMM command line");
    let (stderr_read, stderr_write) = pal::pipe_pair()?;
    let (stdout_read, stdout_write) = pal::pipe_pair()?;
    let child = std::process::Command::new(openvmm.get())
        .args(&args)
        .stdin(Stdio::null())
        .stdout(stdout_write)
        .stderr(stderr_write)
        .spawn()
        .with_context(|| format!("failed to run the {name} OpenVMM command line"))?;
    let mut child = TestChild::new(PolledChild::<std::process::Child>::new(driver, child)?);
    let _stderr_task = driver.spawn(
        "stderr",
        petri::log_task(
            params.log_source.log_file("stderr")?,
            PolledPipe::new(driver, stderr_read)?,
            "openvmm stderr",
        ),
    );
    let mut stdout = PolledPipe::new(driver, stdout_read)?;
    let mut output = Vec::new();
    wait_for_bytes(&mut stdout, &mut output, marker)
        .await
        .with_context(|| format!("the {name} OpenVMM command line did not reach its marker"))?;
    CancelContext::new()
        .with_timeout(Duration::from_secs(15))
        .until_cancelled(drain_until_closed(&mut stdout, &mut output))
        .await
        .with_context(|| format!("timed out draining the {name} OpenVMM command line"))??;
    let status = CancelContext::new()
        .with_timeout(Duration::from_secs(15))
        .until_cancelled(child.wait())
        .await
        .with_context(|| format!("timed out waiting for the {name} OpenVMM command line"))??;
    anyhow::ensure!(
        status.success(),
        "the {name} OpenVMM command line failed: {status}"
    );
    Ok(())
}

/// Returns the sandbox blocks of a snapshot's contract, as each block's role
/// and identity kind.
fn snapshot_blocks(snapshot: &Path) -> anyhow::Result<(String, Vec<(String, String)>)> {
    let (manifest, _) =
        openvmm_helpers::snapshot::restore::read_snapshot(snapshot, DEVICES_MEMORY_MB << 20)?;
    let contract = manifest
        .machine_contract
        .context("the microVM snapshot is missing its machine contract")?;
    Ok((
        manifest.snapshot_tier,
        contract
            .microvm_sandbox_blocks
            .into_iter()
            .map(|block| (block.role, block.identity_kind))
            .collect(),
    ))
}

/// Captures and restores microVMs with sandbox blocks of each snapshot tier,
/// a network device, a supplied RAM backing file, an aggregate host
/// filesystem, and a boot console without an endpoint, to which a restore
/// attaches a listener, over the management RPC, and restores a snapshot
/// captured over the RPC from the command line, and the reverse.
pub(super) async fn test_ttrpc_microvm_device_snapshots(
    params: petri::PetriTestParams<'_>,
    driver: DefaultDriver,
    [openvmm, kernel, initrd]: [ResolvedArtifact; 3],
) -> anyhow::Result<()> {
    use vmservice::microvm_snapshot_config::SnapshotTier;

    let guest = microvm_test_guest(&kernel, &initrd)?;
    let tempdir = if cfg!(target_os = "linux") {
        tempfile::Builder::new()
            .prefix("openvmm-ttrpc-devices-")
            .tempdir_in("/tmp")
    } else {
        tempfile::tempdir()
    }?;
    let dir = tempdir.path();
    let files = DeviceFiles::new(dir)?;
    let fresh_initrd = write_test_initrd(dir, "fresh", &guest, &device_snapshot_script(false)?)?;
    let paired_initrd = write_test_initrd(dir, "paired", &guest, &device_snapshot_script(true)?)?;
    let generation = vec![0x5a; 16];
    let mut cpu_profile = "";

    // A platform snapshot with the sandbox blocks with fresh scratch, a NIC
    // template, a boot console with no endpoint, and guest RAM in a file that
    // the capture copies and leaves in place. It leaves the filesystem slot
    // dormant, as the kernel command line of a platform snapshot holds only
    // what all its clones share.
    let platform = dir.join("platform");
    let ram = dir.join("platform-ram.bin");
    let scratch = files.scratch("platform")?;
    capture_over_rpc(
        &driver,
        &params,
        &openvmm,
        dir,
        "platform",
        &mut cpu_profile,
        &platform,
        |portb_path, cpu_profile| vmservice::CreateVmRequest {
            config: Some(device_cold_boot_config(
                &guest,
                &fresh_initrd,
                portb_path,
                cpu_profile,
                vmservice::MemoryConfig {
                    backing_file_path: ram.to_string_lossy().into_owned(),
                    ..Default::default()
                },
                vmservice::DevicesConfig {
                    virtio_consoles: vec![vmservice::VirtioConsoleConfig {
                        socket_path: String::new(),
                        connect: false,
                    }],
                    microvm_sandbox_blocks: files.blocks(Some(&scratch)),
                    microvm_network: Some(device_network(DEVICES_NETWORK)),
                    ..Default::default()
                },
            )),
            log_id: String::new(),
            microvm_snapshot: Some(vmservice::MicrovmSnapshotConfig {
                destination_path: platform.to_string_lossy().into_owned(),
                snapshot_tier: SnapshotTier::Platform as i32,
                ..Default::default()
            }),
        },
    )
    .await?;
    let (tier, blocks) = snapshot_blocks(&platform)?;
    assert_eq!(tier, "platform");
    assert_eq!(
        blocks,
        [("distro", "unbound"), ("scratch", "fresh")]
            .map(|(role, identity)| (role.to_owned(), identity.to_owned()))
    );
    anyhow::ensure!(
        std::fs::read(&ram)? == std::fs::read(platform.join("memory.bin"))?,
        "the platform capture did not copy the supplied RAM backing file"
    );

    // The RPC restores the platform snapshot with a fresh scratch device, the
    // template's policy, an aggregate filesystem on the dormant slot, and a
    // listener on the console that has no endpoint, then with an absent
    // scratch and a new policy.
    let restore_scratch = files.scratch("platform-restore")?;
    let console = console_listener(dir, "platform-restore");
    restore_over_rpc(
        &driver,
        &params,
        &openvmm,
        dir,
        "platform-restore",
        Some(&console),
        |portb_path| vmservice::CreateVmRequest {
            config: Some(vmservice::VmConfig {
                serial_config: Some(microvm_portb_config(portb_path)),
                devices_config: Some(vmservice::DevicesConfig {
                    virtiofs_config: vec![files.filesystem()],
                    virtio_consoles: vec![vmservice::VirtioConsoleConfig {
                        socket_path: console.clone(),
                        connect: false,
                    }],
                    microvm_sandbox_blocks: files.blocks(Some(&restore_scratch)),
                    microvm_network: Some(device_network("")),
                    ..Default::default()
                }),
                machine_profile: vmservice::vm_config::MachineProfile::Microvm as i32,
                ..Default::default()
            }),
            log_id: String::new(),
            microvm_snapshot: Some(vmservice::MicrovmSnapshotConfig {
                restore_path: platform.to_string_lossy().into_owned(),
                ..Default::default()
            }),
        },
    )
    .await?;
    restore_over_rpc(
        &driver,
        &params,
        &openvmm,
        dir,
        "platform-replacement",
        None,
        |portb_path| vmservice::CreateVmRequest {
            config: Some(vmservice::VmConfig {
                serial_config: Some(microvm_portb_config(portb_path)),
                devices_config: Some(vmservice::DevicesConfig {
                    microvm_sandbox_blocks: files.blocks(None),
                    microvm_network: Some(vmservice::MicrovmNetwork {
                        profile: vmservice::microvm_network::Profile::Portable as i32,
                        egress: vmservice::microvm_network::Action::Allow as i32,
                        ..Default::default()
                    }),
                    ..Default::default()
                }),
                machine_profile: vmservice::vm_config::MachineProfile::Microvm as i32,
                ..Default::default()
            }),
            log_id: String::new(),
            microvm_snapshot: Some(vmservice::MicrovmSnapshotConfig {
                restore_path: platform.to_string_lossy().into_owned(),
                restore_absent_scratch: true,
                restore_network_policy: true,
                ..Default::default()
            }),
        },
    )
    .await?;

    // The command line restores the snapshot that the RPC captured.
    let cli_scratch = files.scratch("platform-command-line")?;
    let mut args = vec![
        OsString::from("--machine"),
        "microvm".into(),
        "--restore-snapshot".into(),
        platform.clone().into(),
    ];
    args.extend(files.block_options(Some(&cli_scratch)));
    args.extend(device_network_options(""));
    args.extend(files.filesystem_options());
    run_command_line(
        &driver,
        &params,
        &openvmm,
        "platform command-line restore",
        args,
        DEVICES_RESTORED_MARKER,
    )
    .await?;

    // Snapshots of the tiers that bind the blocks to a storage generation pair
    // their scratch, which their restores take from the snapshot. They
    // capture an active aggregate filesystem, whose children and policy their
    // restores repeat.
    for (tier, tier_name) in [
        (SnapshotTier::WorkloadStart, "workload-start"),
        (SnapshotTier::InstanceCheckpoint, "instance-checkpoint"),
    ] {
        let snapshot = dir.join(tier_name);
        let scratch = files.scratch(tier_name)?;
        capture_over_rpc(
            &driver,
            &params,
            &openvmm,
            dir,
            tier_name,
            &mut cpu_profile,
            &snapshot,
            |portb_path, cpu_profile| vmservice::CreateVmRequest {
                config: Some(device_cold_boot_config(
                    &guest,
                    &paired_initrd,
                    portb_path,
                    cpu_profile,
                    Default::default(),
                    vmservice::DevicesConfig {
                        virtiofs_config: vec![files.filesystem()],
                        microvm_sandbox_blocks: files.blocks(Some(&scratch)),
                        ..Default::default()
                    },
                )),
                log_id: String::new(),
                microvm_snapshot: Some(vmservice::MicrovmSnapshotConfig {
                    destination_path: snapshot.to_string_lossy().into_owned(),
                    snapshot_tier: tier as i32,
                    snapshot_generation_id: generation.clone(),
                    ..Default::default()
                }),
            },
        )
        .await?;
        let (recorded_tier, blocks) = snapshot_blocks(&snapshot)?;
        assert_eq!(recorded_tier, tier_name);
        assert_eq!(
            blocks,
            [("distro", "generation"), ("scratch", "generation")]
                .map(|(role, identity)| (role.to_owned(), identity.to_owned()))
        );
        anyhow::ensure!(
            snapshot.join("scratch.img").is_file(),
            "the {tier_name} capture did not publish its paired scratch"
        );
        restore_over_rpc(
            &driver,
            &params,
            &openvmm,
            dir,
            &format!("{tier_name}-restore"),
            None,
            |portb_path| vmservice::CreateVmRequest {
                config: Some(vmservice::VmConfig {
                    serial_config: Some(microvm_portb_config(portb_path)),
                    devices_config: Some(vmservice::DevicesConfig {
                        virtiofs_config: vec![files.filesystem()],
                        microvm_sandbox_blocks: files.blocks(None),
                        ..Default::default()
                    }),
                    machine_profile: vmservice::vm_config::MachineProfile::Microvm as i32,
                    ..Default::default()
                }),
                log_id: String::new(),
                microvm_snapshot: Some(vmservice::MicrovmSnapshotConfig {
                    restore_path: snapshot.to_string_lossy().into_owned(),
                    restore_generation_id: generation.clone(),
                    ..Default::default()
                }),
            },
        )
        .await?;
    }

    // The RPC restores a platform snapshot with the devices that the command
    // line captured, and binds an aggregate filesystem to its dormant slot.
    let command_line = dir.join("command-line");
    let scratch = files.scratch("command-line")?;
    let mut args = vec![
        OsString::from("--machine"),
        "microvm".into(),
        "--processors".into(),
        "1".into(),
        "--memory".into(),
        format!(
            "size={DEVICES_MEMORY_MB}M,file={}",
            dir.join("command-line-ram.bin").display()
        )
        .into(),
        "--kernel".into(),
        guest.kernel.clone().into(),
        "--initrd".into(),
        fresh_initrd.clone().into(),
        "--snapshot-destination".into(),
        command_line.clone().into(),
        "--snapshot-tier".into(),
        "platform".into(),
    ];
    if !cpu_profile.is_empty() {
        args.extend(["--cpu-profile".into(), cpu_profile.into()]);
    }
    args.extend(files.block_options(Some(&scratch)));
    args.extend(device_network_options(DEVICES_NETWORK));
    run_command_line(
        &driver,
        &params,
        &openvmm,
        "platform command-line capture",
        args,
        DEVICES_READY_MARKER,
    )
    .await?;
    let restore_scratch = files.scratch("command-line-restore")?;
    restore_over_rpc(
        &driver,
        &params,
        &openvmm,
        dir,
        "command-line-restore",
        None,
        |portb_path| vmservice::CreateVmRequest {
            config: Some(vmservice::VmConfig {
                serial_config: Some(microvm_portb_config(portb_path)),
                devices_config: Some(vmservice::DevicesConfig {
                    virtiofs_config: vec![files.filesystem()],
                    microvm_sandbox_blocks: files.blocks(Some(&restore_scratch)),
                    microvm_network: Some(device_network("")),
                    ..Default::default()
                }),
                machine_profile: vmservice::vm_config::MachineProfile::Microvm as i32,
                ..Default::default()
            }),
            log_id: String::new(),
            microvm_snapshot: Some(vmservice::MicrovmSnapshotConfig {
                restore_path: command_line.to_string_lossy().into_owned(),
                ..Default::default()
            }),
        },
    )
    .await
}

/// An OpenVMM process serving a created microVM over ttrpc.
struct CreatedMicrovm {
    child: TestChild,
    client: mesh_rpc::Client,
    /// Pumps OpenVMM's stderr, for the process's lifetime.
    stderr_task: Task<anyhow::Result<()>>,
    /// The VM's portb endpoint.
    portb_path: PathBuf,
    pidfile_path: PathBuf,
}

/// Launches OpenVMM with its files in `dir`, named after `name`, and cold
/// boots the microVM that `request` describes for a portb endpoint and a CPU
/// profile, with `*cpu_profile`.
///
/// Where the empty `*cpu_profile`, `auto`, fails because the backend cannot
/// present the built-in CPU profile that it selected
/// ([`petri::openvmm::auto_cpu_profile_needs_host_fallback`]), retries once
/// with `host`, which then serves the test's later cold boots. Any other
/// failure, including that of the retry, fails the cold boot of
/// `description`.
async fn create_cold_boot(
    driver: &DefaultDriver,
    params: &petri::PetriTestParams<'_>,
    openvmm: &ResolvedArtifact,
    dir: &Path,
    name: &str,
    description: &str,
    cpu_profile: &mut &'static str,
    request: impl Fn(&Path, &str) -> vmservice::CreateVmRequest,
) -> anyhow::Result<CreatedMicrovm> {
    let rpc_path = dir.join(format!("{name}-rpc.sock"));
    let pidfile_path = dir.join(format!("{name}.pid"));
    let (child, client, stderr_task) =
        launch_openvmm(driver, params, openvmm, &rpc_path, &pidfile_path).await?;
    let mut portb_path = dir.join(format!("{name}-portb.sock"));
    if let Err(status) = client
        .call()
        .start(vmservice::Vm::CreateVm, request(&portb_path, cpu_profile))
        .await
    {
        anyhow::ensure!(
            cpu_profile.is_empty()
                && petri::openvmm::auto_cpu_profile_needs_host_fallback(&status.message),
            "{description} CreateVM failed: {}",
            status.message
        );
        tracing::warn!(
            error = %status.message,
            "{description} CreateVM with the auto CPU profile failed because the backend \
             cannot present the built-in profile that auto selected; retrying the test's \
             cold boots with the host CPU profile"
        );
        *cpu_profile = "host";
        // The failed request's portb endpoint may remain bound.
        portb_path = dir.join(format!("{name}-host-portb.sock"));
        client
            .call()
            .start(vmservice::Vm::CreateVm, request(&portb_path, cpu_profile))
            .await
            .map_err(|status| {
                anyhow::anyhow!(
                    "{description} CreateVM with the host CPU profile failed: {}",
                    status.message
                )
            })?;
    }
    Ok(CreatedMicrovm {
        child,
        client,
        stderr_task,
        portb_path,
        pidfile_path,
    })
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
