// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Virtio-vsock save/restore tests with real guest sockets.

use super::super::ttrpc::attachment_device;
use super::super::ttrpc::launch_openvmm_with_log;
use super::super::ttrpc::pcie_root_port;
use super::super::ttrpc::virtio_device;
use anyhow::Context;
use futures::AsyncBufReadExt;
use futures::AsyncReadExt;
use futures::AsyncWriteExt;
use futures::io::BufReader;
use guid::Guid;
use mesh::CancelContext;
use openvmm_ttrpc_vmservice as vmservice;
use pal_async::DefaultDriver;
use pal_async::pipe::PolledPipe;
use pal_async::process::PolledChild;
use pal_async::socket::PolledSocket;
use pal_async::task::Spawn;
use pal_async::task::Task;
use pal_async::timer::PolledTimer;
use petri::PetriVmBuilder;
use petri::ResolvedArtifact;
use petri::TestChild;
use petri::openvmm::OpenVmmPetriBackend;
use petri_artifacts_common::tags::MachineArch;
use petri_artifacts_vmm_test::artifacts;
use std::io::Write;
use std::path::Path;
use std::process::Command;
use std::time::Duration;
use unix_socket::UnixListener;
use vmm_test_macros::openvmm_test;

#[openvmm_test(linux_direct_x64)]
async fn vsock_save_restore_reconnect_pci(
    config: PetriVmBuilder<OpenVmmPetriBackend>,
    _: (),
    driver: DefaultDriver,
) -> anyhow::Result<()> {
    save_restore_reconnect(config, &driver, false).await
}

#[openvmm_test(linux_direct_x64)]
async fn vsock_save_restore_reconnect_mmio(
    config: PetriVmBuilder<OpenVmmPetriBackend>,
    _: (),
    driver: DefaultDriver,
) -> anyhow::Result<()> {
    save_restore_reconnect(config, &driver, true).await
}

async fn wait_for_listener_log(
    agent: &petri::pipette::PipetteClient,
    driver: &DefaultDriver,
    expected: &str,
) -> anyhow::Result<()> {
    let mut last_log = Vec::new();
    CancelContext::new()
        .with_timeout(Duration::from_secs(30))
        .until_cancelled(async {
            loop {
                last_log = agent.read_file("/tmp/vsock-reset-listener.log").await?;
                if std::str::from_utf8(&last_log)?
                    .lines()
                    .any(|line| line == expected)
                {
                    return Ok::<_, anyhow::Error>(());
                }
                PolledTimer::new(driver)
                    .sleep(Duration::from_millis(10))
                    .await;
            }
        })
        .await
        .with_context(|| {
            format!(
                "guest listener did not report {expected:?}:\n{}",
                String::from_utf8_lossy(&last_log),
            )
        })?
}

async fn connect_guest_listener(
    driver: &DefaultDriver,
    path: &Path,
) -> anyhow::Result<PolledSocket<unix_socket::UnixStream>> {
    CancelContext::new()
        .with_timeout(Duration::from_secs(30))
        .until_cancelled(async {
            let mut socket = PolledSocket::connect_unix(driver, path).await?;
            socket.write_all(b"CONNECT 6300\n").await?;
            let mut socket = BufReader::new(socket);
            let mut response = String::new();
            socket.read_line(&mut response).await?;
            anyhow::ensure!(
                response.starts_with("OK "),
                "vsock connect failed: {response}"
            );
            Ok(socket.into_inner())
        })
        .await
        .context("connecting to preserved guest listener timed out")?
}

async fn echo(
    stream: &mut PolledSocket<unix_socket::UnixStream>,
    payload: &[u8],
) -> anyhow::Result<()> {
    CancelContext::new()
        .with_timeout(Duration::from_secs(30))
        .until_cancelled(async {
            stream.write_all(payload).await?;
            let mut received = vec![0; payload.len()];
            stream.read_exact(&mut received).await?;
            anyhow::ensure!(received == payload, "guest listener echo mismatch");
            Ok(())
        })
        .await
        .context("guest listener echo timed out")?
}

async fn save_restore_reconnect(
    config: PetriVmBuilder<OpenVmmPetriBackend>,
    driver: &DefaultDriver,
    mmio: bool,
) -> anyhow::Result<()> {
    let (mut vm, mut agent) = config
        .with_virtio_vsock(true)
        .modify_backend(move |backend| {
            let backend = backend.without_save_restore();
            if mmio {
                backend.with_virtio_vsock_mmio()
            } else {
                backend.with_pcie_root_topology(1, 1, 1)
            }
        })
        .run()
        .await?;
    assert_eq!(agent.local_vsock_cid(), Some(3));
    let boot_id = agent.read_file("/proc/sys/kernel/random/boot_id").await?;
    let marker = Guid::new_random().to_string().into_bytes();
    let marker_path = "/tmp/vsock-save-restore-marker";
    agent.write_file(marker_path, marker.as_slice()).await?;
    agent
        .write_file("/tmp/vsock-reset-listener.log", &[][..])
        .await?;
    let output = agent
        .command("sh")
        .args([
            "-c",
            "/pipette vsock-listener 6300 </dev/null >/tmp/vsock-reset-listener.log 2>&1 & echo $!",
        ])
        .output()
        .await?;
    anyhow::ensure!(output.status.success(), "starting guest listener failed");
    let listener_pid: u32 = std::str::from_utf8(&output.stdout)?.trim().parse()?;
    wait_for_listener_log(&agent, driver, &format!("listening {listener_pid}")).await?;
    let path = vm.backend().vtl0_vsock_path()?.to_path_buf();
    let mut stream = connect_guest_listener(driver, &path).await?;
    echo(&mut stream, b"before pause").await?;

    // Capture alone must leave the original socket usable on resume.
    vm.backend().pause().await?;
    let saved = vm.backend().save_state().await?;
    assert!(!saved.is_empty());
    vm.backend().resume().await?;
    CancelContext::new()
        .with_timeout(Duration::from_secs(30))
        .until_cancelled(agent.ping())
        .await
        .context("original vsock connection did not survive pause/save/resume")??;
    echo(&mut stream, b"same accepted stream after save/resume").await?;

    for iteration in 1..=3 {
        vm.backend().pulse_save_restore().await?;
        let disconnect = CancelContext::new()
            .with_timeout(Duration::from_secs(30))
            .until_cancelled(agent.wait())
            .await
            .context("old vsock session did not disconnect after restore")?;
        tracing::debug!(?disconnect, "old pipette connection closed");

        agent = CancelContext::new()
            .with_timeout(Duration::from_secs(30))
            .until_cancelled(vm.backend().wait_for_agent(false))
            .await
            .context("pipette did not reconnect after vsock restore")??;
        assert_eq!(agent.local_vsock_cid(), Some(3));
        assert_eq!(
            agent.read_file("/proc/sys/kernel/random/boot_id").await?,
            boot_id
        );
        assert_eq!(agent.read_file(marker_path).await?, marker);
        agent.write_file(marker_path, marker.as_slice()).await?;
        wait_for_listener_log(&agent, driver, &format!("reset {iteration}")).await?;
        stream = connect_guest_listener(driver, &path).await?;
        echo(&mut stream, b"same listener after restore").await?;
        let log = agent.read_file("/tmp/vsock-reset-listener.log").await?;
        let log = std::str::from_utf8(&log)?;
        tracing::info!(
            listener_pid,
            iteration,
            guest_log = log,
            "guest listener survived transport reset"
        );
        anyhow::ensure!(
            log.lines()
                .filter(|line| line.starts_with("listening "))
                .count()
                == 1,
            "guest listener was restarted instead of preserved"
        );
    }

    drop(stream);
    let stopped = agent
        .command("kill")
        .args(["-TERM", &listener_pid.to_string()])
        .output()
        .await?;
    anyhow::ensure!(stopped.status.success(), "stopping guest listener failed");
    agent.power_off().await?;
    vm.wait_for_clean_teardown().await?;
    Ok(())
}

petri::test!(vsock_save_restore_file_snapshot, |resolver| {
    if MachineArch::host() != MachineArch::X86_64 {
        return None;
    }
    Some([
        resolver.require(artifacts::OPENVMM_NATIVE).erase(),
        resolver
            .require(artifacts::loadable::LINUX_DIRECT_TEST_KERNEL_NATIVE)
            .erase(),
        resolver
            .require(artifacts::loadable::LINUX_DIRECT_TEST_INITRD_NATIVE)
            .erase(),
        resolver
            .require(petri_artifacts_common::artifacts::PIPETTE_LINUX_X64_MUSL)
            .erase(),
    ])
});

async fn vsock_save_restore_file_snapshot(
    params: petri::PetriTestParams<'_>,
    driver: DefaultDriver,
    artifacts: [ResolvedArtifact; 4],
) -> anyhow::Result<()> {
    for pci in [false, true] {
        file_snapshot_case(&params, &driver, &artifacts, pci).await?;
    }
    Ok(())
}

petri::test!(vsock_save_restore_ttrpc_pci, |resolver| {
    if MachineArch::host() != MachineArch::X86_64 {
        return None;
    }
    Some([
        resolver.require(artifacts::OPENVMM_NATIVE).erase(),
        resolver
            .require(artifacts::loadable::LINUX_DIRECT_TEST_KERNEL_NATIVE)
            .erase(),
        resolver
            .require(artifacts::loadable::LINUX_DIRECT_TEST_INITRD_NATIVE)
            .erase(),
        resolver
            .require(petri_artifacts_common::artifacts::PIPETTE_LINUX_X64_MUSL)
            .erase(),
    ])
});

async fn vsock_save_restore_ttrpc_pci(
    params: petri::PetriTestParams<'_>,
    driver: DefaultDriver,
    [openvmm, kernel, initrd, pipette]: [ResolvedArtifact; 4],
) -> anyhow::Result<()> {
    let tempdir = tempfile::tempdir()?;
    let initrd = initrd_cpio::inject_into_initrd(
        &std::fs::read(initrd.get())?,
        "pipette",
        &std::fs::read(pipette.get())?,
        0o100755,
    )?;
    let mut initrd_file = tempfile::NamedTempFile::new_in(tempdir.path())?;
    initrd_file.write_all(&initrd)?;
    let vsock_path = tempdir.path().join("vsock");
    let serial_path = tempdir.path().join("serial");
    let snapshot_path = tempdir.path().join("snapshot");
    let mut listener = PolledSocket::new(
        &driver,
        UnixListener::bind(format!(
            "{}_{}",
            vsock_path.display(),
            pipette_client::PIPETTE_PORT
        ))?,
    )?;
    let mut serial = PolledSocket::new(&driver, UnixListener::bind(&serial_path)?)?;
    let mut config = vmservice::VmConfig {
        disable_vmbus: true,
        memory_config: Some(vmservice::MemoryConfig {
            memory_mb: 256,
            backing_file_path: Some(
                tempdir
                    .path()
                    .join("memory.bin")
                    .to_string_lossy()
                    .into_owned(),
            ),
            ..Default::default()
        }),
        boot_config: Some(vmservice::vm_config::BootConfig::DirectBoot(
            vmservice::DirectBoot {
                kernel_path: kernel.get().to_string_lossy().into_owned(),
                initrd_path: initrd_file.path().to_string_lossy().into_owned(),
                kernel_cmdline:
                    "console=ttyS0 rdinit=/pipette panic=-1 initcall_blacklist=hv_sock_init".into(),
            },
        )),
        pcie: Some(vmservice::PcieTopologyConfig {
            root_complexes: vec![vmservice::PcieRootComplex {
                name: "rc0".into(),
                end_bus: 255,
                low_mmio: 64 * 1024 * 1024,
                high_mmio: 1024 * 1024 * 1024,
                root_ports: vec![pcie_root_port(
                    "vsock",
                    false,
                    Some(attachment_device(virtio_device(
                        vmservice::virtio_device::Kind::Vsock(vmservice::VirtioVsock {
                            socket_path: vsock_path.to_string_lossy().into_owned(),
                            guest_cid: Some(42),
                        }),
                    ))),
                )],
                ..Default::default()
            }],
            ..Default::default()
        }),
        serial_config: Some(vmservice::SerialConfig {
            ports: vec![vmservice::serial_config::Config {
                port: 0,
                socket_path: serial_path.to_string_lossy().into_owned(),
                connect: true,
            }],
        }),
        ..Default::default()
    };
    let (mut source, client, source_stderr) = launch_openvmm_with_log(
        &driver,
        &params,
        &openvmm,
        &tempdir.path().join("source-rpc"),
        &tempdir.path().join("source.pid"),
        "ttrpc-vsock-source-stderr",
    )
    .await?;
    let source_pid = source.get().id();
    client
        .call()
        .start(
            vmservice::Vm::CreateVm,
            vmservice::CreateVmRequest {
                config: Some(config.clone()),
                log_id: "vsock-save-restore-source".into(),
            },
        )
        .await
        .expect("CreateVm failed");
    let (stream, _) = serial.accept().await?;
    let source_serial = driver.spawn(
        "ttrpc-vsock-source-serial",
        petri::log_task(
            params.log_source.log_file("ttrpc-vsock-source-serial")?,
            PolledSocket::new(&driver, stream)?,
            "vsock source serial",
        ),
    );
    client
        .call()
        .start(vmservice::Vm::ResumeVm, ())
        .await
        .expect("initial ResumeVm failed");
    let agent = snapshot_agent(
        &driver,
        &mut listener,
        params.log_source.output_dir(),
        Duration::from_secs(120),
    )
    .await?;
    assert_eq!(agent.local_vsock_cid(), Some(42));
    let boot_id = agent.read_file("/proc/sys/kernel/random/boot_id").await?;
    let marker = Guid::new_random().to_string().into_bytes();
    let marker_path = "/tmp/vsock-ttrpc-snapshot-marker";
    agent.write_file(marker_path, marker.as_slice()).await?;

    client
        .call()
        .start(vmservice::Vm::PauseVm, ())
        .await
        .expect("PauseVm failed");
    client
        .call()
        .start(vmservice::Vm::ResumeVm, ())
        .await
        .expect("ResumeVm after pause failed");
    CancelContext::new()
        .with_timeout(Duration::from_secs(30))
        .until_cancelled(agent.ping())
        .await
        .context("ttrpc pause/resume broke the original vsock connection")??;

    let existing_destination = tempdir.path().join("existing-snapshot");
    std::fs::create_dir(&existing_destination)?;
    let err = client
        .call()
        .start(
            vmservice::Vm::SaveVm,
            vmservice::SaveVmRequest {
                destination_dir: existing_destination.to_string_lossy().into_owned(),
            },
        )
        .await
        .unwrap_err();
    assert_eq!(err.code, mesh_rpc::service::Code::FailedPrecondition as i32);
    CancelContext::new()
        .with_timeout(Duration::from_secs(30))
        .until_cancelled(agent.ping())
        .await
        .context("rejected SaveVm broke the original vsock connection")??;

    let saved = client
        .call()
        .start(
            vmservice::Vm::SaveVm,
            vmservice::SaveVmRequest {
                destination_dir: snapshot_path.to_string_lossy().into_owned(),
            },
        )
        .await
        .expect("SaveVm with virtio-vsock failed");
    let snapshot_id: Guid = saved.snapshot_id.parse()?;
    assert!(!snapshot_id.is_zero());
    let (manifest, state) = openvmm_helpers::snapshot::read_snapshot(&snapshot_path)?;
    assert_eq!(manifest.snapshot_id, snapshot_id);
    assert!(!state.is_empty());
    let properties = client
        .call()
        .start(
            vmservice::Vm::PropertiesVm,
            vmservice::PropertiesVmRequest { types: Vec::new() },
        )
        .await
        .expect("PropertiesVm after save failed");
    assert_eq!(properties.state, vmservice::VmState::Paused as i32);
    let err = client
        .call()
        .start(vmservice::Vm::ResumeVm, ())
        .await
        .unwrap_err();
    assert!(err.message.contains("SaveVM"), "{err:?}");
    client
        .call()
        .start(vmservice::Vm::TeardownVm, ())
        .await
        .expect("source TeardownVm failed");
    let disconnected = CancelContext::new()
        .with_timeout(Duration::from_secs(30))
        .until_cancelled(agent.wait())
        .await
        .context("source vsock connection survived TeardownVm")?;
    tracing::debug!(?disconnected, "source vsock session closed");
    let quit = client.call().start(vmservice::Vm::Quit, ()).await;
    tracing::debug!(?quit, "source RPC server quit requested");
    finish_snapshot_process(&mut source, [source_serial, source_stderr]).await?;

    match std::fs::remove_file(&vsock_path) {
        Ok(()) => {}
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
        Err(err) => return Err(err).context("failed to remove the source relay endpoint"),
    }
    config.memory_config.as_mut().unwrap().backing_file_path = None;
    let (mut restored, client, restored_stderr) = launch_openvmm_with_log(
        &driver,
        &params,
        &openvmm,
        &tempdir.path().join("restored-rpc"),
        &tempdir.path().join("restored.pid"),
        "ttrpc-vsock-restored-stderr",
    )
    .await?;
    assert_ne!(restored.get().id(), source_pid);
    let err = client
        .call()
        .start(
            vmservice::Vm::RestoreVm,
            vmservice::RestoreVmRequest {
                source_dir: snapshot_path.to_string_lossy().into_owned(),
                config: Some(config.clone()),
                expected_snapshot_id: Some(Guid::new_random().to_string()),
                resume: false,
            },
        )
        .await
        .unwrap_err();
    assert_eq!(err.code, mesh_rpc::service::Code::FailedPrecondition as i32);
    let result = client
        .call()
        .start(
            vmservice::Vm::RestoreVm,
            vmservice::RestoreVmRequest {
                source_dir: snapshot_path.to_string_lossy().into_owned(),
                config: Some(config),
                expected_snapshot_id: Some(saved.snapshot_id.clone()),
                resume: false,
            },
        )
        .await
        .expect("RestoreVm with virtio-vsock failed");
    assert_eq!(result.snapshot_id, saved.snapshot_id);
    let (stream, _) = serial.accept().await?;
    let restored_serial = driver.spawn(
        "ttrpc-vsock-restored-serial",
        petri::log_task(
            params.log_source.log_file("ttrpc-vsock-restored-serial")?,
            PolledSocket::new(&driver, stream)?,
            "vsock restored serial",
        ),
    );
    let properties = client
        .call()
        .start(
            vmservice::Vm::PropertiesVm,
            vmservice::PropertiesVmRequest { types: Vec::new() },
        )
        .await
        .expect("PropertiesVm after restore failed");
    assert_eq!(properties.state, vmservice::VmState::Paused as i32);
    client
        .call()
        .start(vmservice::Vm::ResumeVm, ())
        .await
        .expect("restored ResumeVm failed");
    let agent = snapshot_agent(
        &driver,
        &mut listener,
        params.log_source.output_dir(),
        Duration::from_secs(30),
    )
    .await?;
    assert_eq!(agent.local_vsock_cid(), Some(42));
    assert_eq!(
        agent.read_file("/proc/sys/kernel/random/boot_id").await?,
        boot_id
    );
    assert_eq!(agent.read_file(marker_path).await?, marker);
    agent.write_file(marker_path, marker.as_slice()).await?;
    agent.ping().await?;
    let waiter = client.call().start(vmservice::Vm::WaitVm, ());
    agent.power_off().await?;
    CancelContext::new()
        .with_timeout(Duration::from_secs(30))
        .until_cancelled(waiter)
        .await
        .context("restored guest did not power off")?
        .expect("WaitVm after restored guest poweroff failed");
    client
        .call()
        .start(vmservice::Vm::TeardownVm, ())
        .await
        .expect("restored TeardownVm failed");
    let quit = client.call().start(vmservice::Vm::Quit, ()).await;
    tracing::debug!(?quit, "restored RPC server quit requested");
    finish_snapshot_process(&mut restored, [restored_serial, restored_stderr]).await?;
    Ok(())
}

async fn file_snapshot_case(
    params: &petri::PetriTestParams<'_>,
    driver: &DefaultDriver,
    [openvmm, kernel, initrd, pipette]: &[ResolvedArtifact; 4],
    pci: bool,
) -> anyhow::Result<()> {
    let tempdir = tempfile::tempdir()?;
    let initrd = initrd_cpio::inject_into_initrd(
        &std::fs::read(initrd.get())?,
        "pipette",
        &std::fs::read(pipette.get())?,
        0o100755,
    )?;
    let mut initrd_file = tempfile::NamedTempFile::new_in(tempdir.path())?;
    initrd_file.write_all(&initrd)?;
    let vsock_path = tempdir.path().join("vsock");
    let snapshot_path = tempdir.path().join("snapshot");
    let memory_path = tempdir.path().join("memory.bin");
    let mut listener = PolledSocket::new(
        driver,
        UnixListener::bind(format!(
            "{}_{}",
            vsock_path.display(),
            pipette_client::PIPETTE_PORT
        ))?,
    )?;
    let transport = if pci { "pci" } else { "mmio" };
    let command = || {
        let mut command = Command::new(openvmm);
        command
            .current_dir(tempdir.path())
            .env("OPENVMM_LOG", "info")
            .arg("--kernel")
            .arg(kernel.get())
            .arg("--initrd")
            .arg(initrd_file.path())
            .args(["--processors", "1", "--hv", "--com1", "stderr"])
            .args([
                "--cmdline",
                "console=ttyS0 rdinit=/pipette panic=-1 initcall_blacklist=hv_sock_init",
            ])
            .arg("--virtio-vsock-path")
            .arg(&vsock_path)
            .args([
                "--virtio-vsock-bus",
                if pci { "pcie:vsock" } else { "mmio" },
            ]);
        if pci {
            command.args([
                "--pcie-root-complex",
                "rc0",
                "--pcie-root-port",
                "rc0:vsock",
            ]);
        }
        command
    };

    let (mut source, mut input, logs) = launch_snapshot_process(
        driver,
        params,
        command()
            .arg("--memory")
            .arg(format!("size=256M,file={}", memory_path.display())),
        &format!("{transport}-source"),
    )?;
    let agent = snapshot_agent(
        driver,
        &mut listener,
        params.log_source.output_dir(),
        Duration::from_secs(120),
    )
    .await?;
    assert_eq!(agent.local_vsock_cid(), Some(3));
    let boot_id = agent.read_file("/proc/sys/kernel/random/boot_id").await?;
    let marker = Guid::new_random().to_string().into_bytes();
    let marker_path = "/tmp/vsock-file-snapshot-marker";
    agent.write_file(marker_path, marker.as_slice()).await?;

    // REPL commands are serialized: quit only runs after snapshot save returns,
    // and does not resume the source's shared RAM backing.
    input.write_all(b"save-snapshot snapshot\nquit\n").await?;
    finish_snapshot_process(&mut source, logs).await?;
    drop(agent);
    let (manifest, state) = openvmm_helpers::snapshot::read_snapshot(&snapshot_path)?;
    assert!(!state.is_empty());
    assert_eq!(manifest.memory_size_bytes, 256 * 1024 * 1024);
    assert_eq!(manifest.vp_count, 1);
    assert_eq!(
        std::fs::metadata(snapshot_path.join("memory.bin"))?.len(),
        manifest.memory_size_bytes
    );

    // This is a test-owned path and its owner has exited. Unix socket nodes
    // may remain after the listener is closed.
    match std::fs::remove_file(&vsock_path) {
        Ok(()) => {}
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
        Err(err) => return Err(err).context("failed to remove the source relay endpoint"),
    }

    let (mut restored, mut input, logs) = launch_snapshot_process(
        driver,
        params,
        command()
            .args(["--memory", "256M", "--paused", "--restore-snapshot"])
            .arg(&snapshot_path),
        &format!("{transport}-restored"),
    )?;
    input.write_all(b"resume\n").await?;
    let agent = snapshot_agent(
        driver,
        &mut listener,
        params.log_source.output_dir(),
        Duration::from_secs(30),
    )
    .await?;
    assert_eq!(agent.local_vsock_cid(), Some(3));
    assert_eq!(
        agent.read_file("/proc/sys/kernel/random/boot_id").await?,
        boot_id
    );
    assert_eq!(agent.read_file(marker_path).await?, marker);
    agent.write_file(marker_path, marker.as_slice()).await?;
    agent.ping().await?;
    input.write_all(b"quit\n").await?;
    finish_snapshot_process(&mut restored, logs).await?;
    Ok(())
}

async fn snapshot_agent(
    driver: &DefaultDriver,
    listener: &mut PolledSocket<UnixListener>,
    output_dir: &Path,
    timeout: Duration,
) -> anyhow::Result<pipette_client::PipetteClient> {
    CancelContext::new()
        .with_timeout(timeout)
        .until_cancelled(async {
            let (stream, _) = listener.accept().await?;
            pipette_client::PipetteClient::new(
                driver,
                PolledSocket::new(driver, stream)?,
                output_dir,
            )
            .await
        })
        .await
        .context("timed out connecting to the snapshot VM")?
}

fn launch_snapshot_process(
    driver: &DefaultDriver,
    params: &petri::PetriTestParams<'_>,
    command: &mut Command,
    name: &str,
) -> anyhow::Result<(TestChild, PolledPipe, [Task<anyhow::Result<()>>; 2])> {
    let (stdin_read, stdin_write) = pal::pipe_pair()?;
    let (stdout_read, stdout_write) = pal::pipe_pair()?;
    let (stderr_read, stderr_write) = pal::pipe_pair()?;
    let child = command
        .stdin(stdin_read)
        .stdout(stdout_write)
        .stderr(stderr_write)
        .spawn()?;
    let child = TestChild::new(PolledChild::<std::process::Child>::new(driver, child)?);
    let stdout = driver.spawn(
        "vsock-snapshot-stdout",
        petri::log_task(
            params.log_source.log_file(&format!("{name}-stdout"))?,
            PolledPipe::new(driver, stdout_read)?,
            "snapshot stdout",
        ),
    );
    let stderr = driver.spawn(
        "vsock-snapshot-stderr",
        petri::log_task(
            params.log_source.log_file(&format!("{name}-stderr"))?,
            PolledPipe::new(driver, stderr_read)?,
            "snapshot stderr",
        ),
    );
    Ok((
        child,
        PolledPipe::new(driver, stdin_write)?,
        [stdout, stderr],
    ))
}

async fn finish_snapshot_process(
    child: &mut TestChild,
    logs: [Task<anyhow::Result<()>>; 2],
) -> anyhow::Result<()> {
    let status = CancelContext::new()
        .with_timeout(Duration::from_secs(30))
        .until_cancelled(child.wait())
        .await
        .context("snapshot VM did not exit")??;
    for log in logs {
        log.await?;
    }
    anyhow::ensure!(status.success(), "snapshot VM exited with {status}");
    Ok(())
}
