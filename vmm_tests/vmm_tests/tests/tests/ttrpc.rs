// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Integration tests for OpenVMM's TTRPC interface.

use anyhow::Context;
use futures::AsyncReadExt;
use guid::Guid;
use mesh::CancelContext;
use openvmm_ttrpc_vmservice as vmservice;
use pal_async::DefaultPool;
use pal_async::pipe::PolledPipe;
use pal_async::process::PolledChild;
use pal_async::socket::PolledSocket;
use pal_async::task::Spawn;
use petri::ResolvedArtifact;
use petri_artifacts_vmm_test::artifacts;
use std::process::Stdio;
use std::time::Duration;
use unix_socket::UnixListener;
use unix_socket::UnixStream;

petri::test!(test_ttrpc_interface, |resolver| {
    // Only supported on x86_64 for now.
    if petri_artifacts_common::tags::MachineArch::host()
        != petri_artifacts_common::tags::MachineArch::X86_64
    {
        return None;
    }
    let openvmm = resolver.require(artifacts::OPENVMM_NATIVE);
    let kernel = resolver.require(artifacts::loadable::LINUX_DIRECT_TEST_KERNEL_NATIVE);
    let initrd = resolver.require(artifacts::loadable::LINUX_DIRECT_TEST_INITRD_NATIVE);
    Some([openvmm.erase(), kernel.erase(), initrd.erase()])
});

fn test_ttrpc_interface(
    params: petri::PetriTestParams<'_>,
    [openvmm, kernel_path, initrd_path]: [ResolvedArtifact; 3],
) -> anyhow::Result<()> {
    let mut socket_path = std::env::temp_dir();
    socket_path.push(Guid::new_random().to_string());
    let pidfile_path = std::env::temp_dir().join(format!("{}.pid", Guid::new_random()));

    tracing::info!(socket_path = %socket_path.display(), "launching OpenVMM with ttrpc");

    let (stderr_read, stderr_write) = pal::pipe_pair()?;
    let (stdout_read, stdout_write) = pal::pipe_pair()?;
    let child = std::process::Command::new(openvmm)
        .arg("--ttrpc")
        .arg(&socket_path)
        .arg("--pidfile")
        .arg(&pidfile_path)
        .stdin(Stdio::null())
        .stdout(stdout_write)
        .stderr(stderr_write)
        .spawn()?;

    DefaultPool::run_with(async |driver| {
        let mut child = PolledChild::<std::process::Child>::new(&driver, child)?;

        // Start pumping stderr immediately so the pipe buffer doesn't fill
        // up and block the child.
        let stderr_task = driver.spawn(
            "stderr",
            petri::log_task(
                params.logger.log_file("stderr")?,
                PolledPipe::new(&driver, stderr_read)?,
                "openvmm stderr",
            ),
        );

        // Wait for stdout to close (readiness signal). If the child
        // crashes at startup, stdout closes too and we detect the exit
        // when the pidfile is missing.
        let mut stdout = PolledPipe::new(&driver, stdout_read)?;
        let mut buf = [0u8; 1];
        let n = stdout
            .read(&mut buf)
            .await
            .context("reading from openvmm stdout")?;
        anyhow::ensure!(n == 0, "openvmm wrote unexpected data to stdout");
        drop(stdout);

        // Verify the pidfile was created with the correct PID. If it's
        // missing, wait briefly for the child to exit (the PidfileGuard
        // deletes it on drop) and report the exit status.
        let pid_content = match std::fs::read_to_string(&pidfile_path) {
            Ok(s) => s,
            Err(e) => {
                let wait_result = CancelContext::new()
                    .with_timeout(Duration::from_secs(10))
                    .until_cancelled(child.wait())
                    .await;
                match wait_result {
                    Ok(Ok(status)) => {
                        let _ = stderr_task.await;
                        anyhow::bail!("openvmm exited with {status} before pidfile was created");
                    }
                    _ => {
                        return Err(e).context("failed to read pidfile");
                    }
                }
            }
        };
        assert_eq!(
            pid_content,
            format!("{}\n", child.get().id()),
            "pidfile should contain the child PID"
        );

        let ttrpc_path = socket_path.clone();
        let client = mesh_rpc::Client::new(
            &driver,
            mesh_rpc::client::UnixDialier::new(driver.clone(), ttrpc_path),
        );
        for i in 0..3 {
            let mut com1_path = std::env::temp_dir();
            com1_path.push(Guid::new_random().to_string());

            let mut console_path = std::env::temp_dir();
            console_path.push(Guid::new_random().to_string());

            let virtiofs_root = std::env::temp_dir().join(Guid::new_random().to_string());
            std::fs::create_dir_all(&virtiofs_root).unwrap();

            let consomme_nic_id = Guid::new_random().to_string();

            // On iteration 0, test `connect: true` for both serial and
            // virtio console by pre-creating listeners that the VM will
            // connect to. On other iterations, test the default
            // `connect: false` (VM creates the socket).
            let use_connect = i == 0;
            let com1_listener = if use_connect {
                Some(UnixListener::bind(&com1_path).unwrap())
            } else {
                None
            };
            let console_listener = if use_connect {
                Some(UnixListener::bind(&console_path).unwrap())
            } else {
                None
            };

            client
                .call()
                .start(
                    vmservice::Vm::CreateVm,
                    vmservice::CreateVmRequest {
                        config: Some(vmservice::VmConfig {
                            memory_config: Some(vmservice::MemoryConfig {
                                memory_mb: 256,
                                ..Default::default()
                            }),
                            processor_config: Some(vmservice::ProcessorConfig {
                                processor_count: 2,
                                ..Default::default()
                            }),
                            boot_config: Some(vmservice::vm_config::BootConfig::DirectBoot(
                                vmservice::DirectBoot {
                                    kernel_path: kernel_path.get().to_string_lossy().to_string(),
                                    initrd_path: initrd_path.get().to_string_lossy().to_string(),
                                    kernel_cmdline:
                                        "console=ttyS0 rdinit=/bin/busybox panic=-1 -- poweroff -f"
                                            .to_string(),
                                },
                            )),
                            serial_config: Some(vmservice::SerialConfig {
                                ports: vec![vmservice::serial_config::Config {
                                    port: 0,
                                    socket_path: com1_path.to_string_lossy().into(),
                                    connect: use_connect,
                                }],
                            }),
                            devices_config: Some(vmservice::DevicesConfig {
                                nic_config: vec![vmservice::NicConfig {
                                    nic_id: consomme_nic_id.clone(),
                                    mac_address: "00-15-5D-12-12-12".to_string(),
                                    backend: Some(vmservice::nic_config::Backend::Consomme(
                                        vmservice::ConsommeBackend {
                                            cidr: String::new(),
                                            ports: vec![],
                                        },
                                    )),
                                    ..Default::default()
                                }],
                                virtio_console: Some(vmservice::VirtioConsoleConfig {
                                    socket_path: console_path.to_string_lossy().into(),
                                    connect: use_connect,
                                }),
                                virtiofs_config: vec![vmservice::VirtioFsConfig {
                                    tag: "testfs".to_string(),
                                    root_path: virtiofs_root.to_string_lossy().into(),
                                }],
                                ..Default::default()
                            }),
                            ..Default::default()
                        }),
                        log_id: String::new(),
                    },
                )
                .await
                .unwrap();

            // Exercise the Consomme port-forwarding modify paths. Sending an
            // invalid protocol value drives the request through the
            // `ModifyResource(Update|Remove)` -> `consomme_rpc` wiring and the
            // protocol validation in `parse_port_config`, returning an error
            // before touching the device. This guards against regressions in
            // the bind/unbind routing without depending on guest timing or
            // host port availability.
            for modify_type in [vmservice::ModifyType::Update, vmservice::ModifyType::Remove] {
                let err = client
                    .call()
                    .start(
                        vmservice::Vm::ModifyResource,
                        vmservice::ModifyResourceRequest {
                            r#type: modify_type as i32,
                            resource: Some(
                                vmservice::modify_resource_request::Resource::NicConfig(
                                    vmservice::NicConfig {
                                        nic_id: consomme_nic_id.clone(),
                                        mac_address: "00-15-5D-12-12-12".to_string(),
                                        backend: Some(vmservice::nic_config::Backend::Consomme(
                                            vmservice::ConsommeBackend {
                                                cidr: String::new(),
                                                ports: vec![vmservice::PortConfig {
                                                    host_port: 8080,
                                                    guest_port: 80,
                                                    // Deliberately invalid protocol value.
                                                    protocol: 99,
                                                }],
                                            },
                                        )),
                                        ..Default::default()
                                    },
                                ),
                            ),
                        },
                    )
                    .await
                    .unwrap_err();
                assert!(
                    err.message.contains("invalid protocol"),
                    "expected invalid protocol error, got: {}",
                    err.message
                );
            }

            // Get the serial connection - either by accepting on our listener
            // (connect: true) or connecting to the VM's socket (connect: false).
            let com1 = if let Some(listener) = com1_listener {
                let (stream, _) = listener.accept().unwrap();
                stream
            } else {
                UnixStream::connect(&com1_path).unwrap()
            };

            // Get the console connection the same way.
            let console = if let Some(listener) = console_listener {
                let (stream, _) = listener.accept().unwrap();
                stream
            } else {
                UnixStream::connect(&console_path).unwrap()
            };

            let _com1_task = driver.spawn(
                "com1",
                petri::log_task(
                    params.logger.log_file("linux").unwrap(),
                    PolledSocket::new(&driver, com1).unwrap(),
                    "linux com1",
                ),
            );

            let _console_task = driver.spawn(
                "console",
                petri::log_task(
                    params.logger.log_file("virtio-console").unwrap(),
                    PolledSocket::new(&driver, console).unwrap(),
                    "virtio console",
                ),
            );

            assert_eq!(
                client
                    .call()
                    .timeout(Some(Duration::from_millis(100)))
                    .start(vmservice::Vm::WaitVm, (),)
                    .await
                    .unwrap_err()
                    .code,
                mesh_rpc::service::Code::DeadlineExceeded as i32
            );

            let waiter = client.call().start(vmservice::Vm::WaitVm, ());

            match i {
                0 | 2 => {
                    client
                        .call()
                        .start(vmservice::Vm::ResumeVm, ())
                        .await
                        .unwrap();

                    waiter.await.unwrap();

                    if i == 0 {
                        client
                            .call()
                            .start(vmservice::Vm::TeardownVm, ())
                            .await
                            .unwrap();

                        client
                            .call()
                            .start(vmservice::Vm::WaitVm, ())
                            .await
                            .unwrap_err();
                    } else {
                        let _ = client.call().start(vmservice::Vm::Quit, ()).await;
                    }
                }
                1 => {
                    client
                        .call()
                        .start(vmservice::Vm::TeardownVm, ())
                        .await
                        .unwrap();

                    waiter.await.unwrap_err();
                }
                _ => unreachable!(),
            }

            // Clean up temp files from this iteration.
            let _ = std::fs::remove_file(&com1_path);
            let _ = std::fs::remove_file(&console_path);
            let _ = std::fs::remove_dir_all(&virtiofs_root);
        }

        let exit_status = child.wait().await?;
        let _ = std::fs::remove_file(&socket_path);

        // Surface the OpenVMM exit status so that abnormal exits (e.g. an abort
        // from a panic — the workspace uses `panic = 'abort'`) are visible in
        // test logs alongside any pidfile/cleanup assertion below.
        tracing::info!(?exit_status, "openvmm exited");
        assert!(
            exit_status.success(),
            "openvmm exited abnormally: {:?}",
            exit_status
        );

        // Verify the pidfile was cleaned up on exit.
        assert!(
            !pidfile_path.exists(),
            "pidfile should be removed after exit"
        );

        Ok(())
    })
}
