// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Integration test for OpenVMM's handle-passing protocol on the TTRPC
//! endpoint.
//!
//! This boots a real `openvmm --rpc` process, registers an access token by
//! name over the handle-passing protocol (see
//! `openvmm_ttrpc_vmservice/src/resource_passing.md`), and confirms that a
//! virtio-fs share's `WindowsOptions.user_token_name` resolves through that
//! registration during `CreateVm`. How virtio-fs uses the token is covered by
//! the virtiofs crate's tests.

use anyhow::Context;
use futures::AsyncWriteExt as _;
use openvmm_ttrpc_vmservice as vmservice;
use pal::windows::BorrowedHandleExt;
use pal::windows::security::open_process_token;
use pal_async::DefaultDriver;
use pal_async::socket::PolledSocket;
use petri::ResolvedArtifact;
use petri_artifacts_vmm_test::artifacts;
use std::os::windows::io::AsHandle;
use std::os::windows::io::AsRawHandle;
use std::path::Path;
use unix_socket::UnixStream;
use windows_sys::Win32::Security::TOKEN_DUPLICATE;
use windows_sys::Win32::Security::TOKEN_QUERY;

/// The 4-byte handle-passing handshake magic: `0xFD 'H' 'P' 0x01`.
const HANDSHAKE_MAGIC: [u8; 4] = [0xFD, b'H', b'P', 0x01];

const OPCODE_REGISTER: u8 = 1;
const OPCODE_DEREGISTER: u8 = 2;

const TOKEN_NAME: &str = "user-token";

petri::test!(handle_passing_virtiofs_user_token, |resolver| {
    let openvmm = resolver.require(artifacts::OPENVMM_NATIVE);
    let kernel = resolver.require(artifacts::loadable::LINUX_DIRECT_TEST_KERNEL_NATIVE);
    let initrd = resolver.require(artifacts::loadable::LINUX_DIRECT_TEST_INITRD_NATIVE);
    Some([openvmm.erase(), kernel.erase(), initrd.erase()])
});

async fn handle_passing_virtiofs_user_token(
    params: petri::PetriTestParams<'_>,
    driver: DefaultDriver,
    [openvmm, kernel_path, initrd_path]: [ResolvedArtifact; 3],
) -> anyhow::Result<()> {
    let tempdir = tempfile::tempdir()?;
    let socket_path = tempdir.path().join("ttrpc.sock");
    let pidfile_path = tempdir.path().join("openvmm.pid");
    let share_root = tempdir.path().join("share");
    std::fs::create_dir(&share_root)?;

    // A real client would `DuplicateHandle` the token into OpenVMM. Making it
    // inheritable before spawning OpenVMM gives the child a copy with the same
    // value, which avoids cross-process handle manipulation in this test.
    let token = open_process_token(TOKEN_DUPLICATE | TOKEN_QUERY)?
        .as_handle()
        .duplicate(true, None)?;
    let token_value = token.as_raw_handle() as u64;
    let (mut child, client, stderr_task) =
        super::launch_openvmm(&driver, &params, &openvmm, &socket_path, &pidfile_path).await?;
    drop(token);

    let mut conn = PolledSocket::connect_unix(&driver, &socket_path)
        .await
        .context("connecting handle-passing socket")?;
    super::resource_passing_handshake(&mut conn, HANDSHAKE_MAGIC)
        .await
        .context("handle-passing handshake")?;
    let (status, msg) = request(&mut conn, OPCODE_REGISTER, TOKEN_NAME, Some(token_value)).await?;
    anyhow::ensure!(status == 0, "register '{TOKEN_NAME}' failed: {msg}");

    // An unregistered name fails to resolve.
    let err = client
        .call()
        .start(
            vmservice::Vm::CreateVm,
            create_vm_request(&kernel_path, &initrd_path, &share_root, "does-not-exist"),
        )
        .await
        .unwrap_err();
    anyhow::ensure!(
        err.message
            .contains("failed to resolve virtio-fs user token"),
        "expected a user token resolution error, got: {}",
        err.message
    );

    // The registered name resolves to the passed token.
    client
        .call()
        .start(
            vmservice::Vm::CreateVm,
            create_vm_request(&kernel_path, &initrd_path, &share_root, TOKEN_NAME),
        )
        .await
        .map_err(|err| anyhow::anyhow!("CreateVm failed: {}", err.message))?;

    let (status, msg) = request(&mut conn, OPCODE_DEREGISTER, TOKEN_NAME, None).await?;
    anyhow::ensure!(status == 0, "deregister '{TOKEN_NAME}' failed: {msg}");
    drop(conn);

    client
        .call()
        .start(vmservice::Vm::TeardownVm, ())
        .await
        .map_err(|err| anyhow::anyhow!("TeardownVm failed: {}", err.message))?;
    let _ = client.call().start(vmservice::Vm::Quit, ()).await;
    let exit_status = child.wait().await?;
    stderr_task
        .await
        .context("failed to drain openvmm stderr after process exit")?;
    tracing::info!(?exit_status, "openvmm exited");
    anyhow::ensure!(
        exit_status.success(),
        "openvmm exited abnormally: {exit_status:?}"
    );
    Ok(())
}

/// Sends a request, with an inline handle value for `Register`, and returns
/// the response.
async fn request(
    sock: &mut PolledSocket<UnixStream>,
    opcode: u8,
    name: &str,
    handle: Option<u64>,
) -> anyhow::Result<(u8, String)> {
    let mut frame = vec![opcode, name.len() as u8];
    frame.extend_from_slice(name.as_bytes());
    if let Some(handle) = handle {
        frame.extend_from_slice(&handle.to_le_bytes());
    }
    sock.write_all(&frame).await?;
    super::read_resource_passing_response(sock).await
}

/// Builds a direct-boot `CreateVm` request that exposes `share_root` over
/// virtio-fs, accessed as the token registered as `user_token_name`.
fn create_vm_request(
    kernel_path: &ResolvedArtifact,
    initrd_path: &ResolvedArtifact,
    share_root: &Path,
    user_token_name: &str,
) -> vmservice::CreateVmRequest {
    vmservice::CreateVmRequest {
        config: Some(vmservice::VmConfig {
            memory_config: Some(vmservice::MemoryConfig {
                memory_mb: 256,
                ..Default::default()
            }),
            processor_config: Some(vmservice::ProcessorConfig {
                processor_count: 1,
                ..Default::default()
            }),
            boot_config: Some(vmservice::vm_config::BootConfig::DirectBoot(
                vmservice::DirectBoot {
                    kernel_path: kernel_path.get().to_string_lossy().into_owned(),
                    initrd_path: initrd_path.get().to_string_lossy().into_owned(),
                    kernel_cmdline: String::new(),
                },
            )),
            devices_config: Some(vmservice::DevicesConfig {
                virtiofs_config: vec![vmservice::VirtioFs {
                    tag: "userfs".to_string(),
                    root_path: share_root.to_string_lossy().into_owned(),
                    read_only: false,
                    windows_options: Some(vmservice::virtio_fs::WindowsOptions {
                        user_token_name: user_token_name.to_string(),
                    }),
                }],
                ..Default::default()
            }),
            ..Default::default()
        }),
        log_id: String::new(),
    }
}
