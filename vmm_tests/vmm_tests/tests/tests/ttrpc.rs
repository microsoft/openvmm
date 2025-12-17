// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Integration tests for OpenVMM's TTRPC interface.

use anyhow::Context;
use guid::Guid;
use openvmm_ttrpc_vmservice as vmservice;
use pal_async::DefaultPool;
use pal_async::pipe::PolledPipe;
use pal_async::socket::PolledSocket;
use pal_async::task::Spawn;
use petri::ResolvedArtifact;
use petri_artifacts_vmm_test::artifacts;
use std::io::Read;
use std::process::Stdio;
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

    tracing::info!(socket_path = %socket_path.display(), "launching OpenVMM with ttrpc");

    let (stderr_read, stderr_write) = pal::pipe_pair()?;
    let mut child = std::process::Command::new(openvmm)
        .arg("--ttrpc")
        .arg(&socket_path)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(stderr_write)
        .spawn()?;

    // Wait for stdout to close.
    let mut stdout = child.stdout.take().context("failed to take stdout")?;
    let mut b = [0];
    assert_eq!(stdout.read(&mut b)?, 0);

    DefaultPool::run_with(async |driver| {
        let driver = driver;
        let _stderr_task = driver.spawn(
            "stderr",
            petri::log_task(
                params.logger.log_file("stderr").unwrap(),
                PolledPipe::new(&driver, stderr_read).unwrap(),
                "openvmm stderr",
            ),
        );

        let ttrpc_path = socket_path.clone();
        let client = mesh_rpc::Client::new(
            &driver,
            mesh_rpc::client::UnixDialier::new(driver.clone(), ttrpc_path),
        );
        for i in 0..3 {
            let mut com1_path = std::env::temp_dir();
            com1_path.push(Guid::new_random().to_string());

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
                                }],
                            }),
                            ..Default::default()
                        }),
                        log_id: String::new(),
                    },
                )
                .await
                .unwrap();

            let com1 = UnixStream::connect(&com1_path).unwrap();

            let _com1_task = driver.spawn(
                "com1",
                petri::log_task(
                    params.logger.log_file("linux").unwrap(),
                    PolledSocket::new(&driver, com1).unwrap(),
                    "linux com1",
                ),
            );

            assert_eq!(
                client
                    .call()
                    .timeout(Some(std::time::Duration::from_millis(100)))
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
        }
    });

    child.wait()?;
    let _ = std::fs::remove_file(&socket_path);

    Ok(())
}
