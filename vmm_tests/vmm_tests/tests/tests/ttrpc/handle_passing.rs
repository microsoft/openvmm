// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Integration tests for OpenVMM's handle-passing protocol on the TTRPC
//! endpoint.
//!
//! This boots a real `openvmm --rpc` process, registers an access token by
//! name over the handle-passing protocol (see
//! `openvmm_ttrpc_vmservice/src/resource_passing.md`), and confirms that a
//! virtio-fs share's `WindowsOptions.user_token_name` resolves through that
//! registration during `CreateVm`. How virtio-fs uses the token is covered by
//! the virtiofs crate's tests.
//!
//! It also registers a VHD handle and confirms that `DiskBackend.virtual_disk`
//! backs a virtio-blk device whose data the guest and host both observe.

use anyhow::Context;
use futures::AsyncWriteExt as _;
use mesh::CancelContext;
use openvmm_ttrpc_vmservice as vmservice;
use pal::windows::BorrowedHandleExt;
use pal::windows::security::open_process_token;
use pal_async::DefaultDriver;
use pal_async::socket::PolledSocket;
use pal_async::task::Spawn;
use petri::ResolvedArtifact;
use petri::pipette::cmd;
use petri_artifacts_vmm_test::artifacts;
use std::os::windows::fs::FileExt as _;
use std::os::windows::io::AsHandle;
use std::os::windows::io::AsRawHandle;
use std::path::Path;
use std::time::Duration;
use unix_socket::UnixStream;
use windows_sys::Win32::Security::TOKEN_DUPLICATE;
use windows_sys::Win32::Security::TOKEN_QUERY;

/// The 4-byte handle-passing handshake magic: `0xFD 'H' 'P' 0x01`.
const HANDSHAKE_MAGIC: [u8; 4] = [0xFD, b'H', b'P', 0x01];

const OPCODE_REGISTER: u8 = 1;
const OPCODE_DEREGISTER: u8 = 2;

const TOKEN_NAME: &str = "user-token";

const DISK_NAME: &str = "disk";
const DISK_SIZE: u64 = 16 * 1024 * 1024;
/// I/O size for raw disk access, which must be sector aligned.
const DISK_BLOCK_SIZE: usize = 4096;
/// The host writes `HOST_MARKER` at this block (1 MiB) before boot.
const HOST_MARKER: &str = "ttrpc-disk-host";
const HOST_MARKER_BLOCK: u64 = 256;
/// The guest writes `GUEST_MARKER` at this block (2 MiB) over virtio-blk.
const GUEST_MARKER: &str = "ttrpc-disk-guest";
const GUEST_MARKER_BLOCK: u64 = 512;

petri::test!(handle_passing_virtiofs_user_token, |resolver| {
    let openvmm = resolver.require(artifacts::OPENVMM_NATIVE);
    let kernel = resolver.require(artifacts::loadable::LINUX_DIRECT_TEST_KERNEL_NATIVE);
    let initrd = resolver.require(artifacts::loadable::LINUX_DIRECT_TEST_INITRD_NATIVE);
    Some([openvmm.erase(), kernel.erase(), initrd.erase()])
});

petri::test!(handle_passing_virtual_disk, |resolver| {
    let openvmm = resolver.require(artifacts::OPENVMM_NATIVE);
    let kernel = resolver.require(artifacts::loadable::LINUX_DIRECT_TEST_KERNEL_NATIVE);
    let initrd = resolver.require(artifacts::loadable::LINUX_DIRECT_TEST_INITRD_NATIVE);
    let pipette = super::require_pipette(resolver);
    Some([openvmm.erase(), kernel.erase(), initrd.erase(), pipette])
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
            create_vm_request(
                &kernel_path,
                initrd_path.get(),
                String::new(),
                virtiofs_config(&share_root, "does-not-exist"),
            ),
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
            create_vm_request(
                &kernel_path,
                initrd_path.get(),
                String::new(),
                virtiofs_config(&share_root, TOKEN_NAME),
            ),
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

async fn handle_passing_virtual_disk(
    params: petri::PetriTestParams<'_>,
    driver: DefaultDriver,
    [openvmm, kernel_path, initrd_path, pipette_path]: [ResolvedArtifact; 4],
) -> anyhow::Result<()> {
    let tempdir = tempfile::tempdir()?;
    let socket_path = tempdir.path().join("ttrpc.sock");
    let pidfile_path = tempdir.path().join("openvmm.pid");
    let hvsocket_path = tempdir.path().join("hvsocket");
    let com1_path = tempdir.path().join("com1.sock");
    let pipette_initrd_file = super::pipette_initrd(tempdir.path(), &initrd_path, &pipette_path)?;

    // The test reads and writes markers through the image file rather than
    // the disk, so only OpenVMM accesses the disk directly.
    let image_path = tempdir.path().join("disk.vhd");
    {
        let image = std::fs::File::create(&image_path)?;
        image.set_len(DISK_SIZE)?;
        image.seek_write(
            &marker_block(HOST_MARKER),
            HOST_MARKER_BLOCK * DISK_BLOCK_SIZE as u64,
        )?;
        disk_vhd1::Vhd1Disk::make_fixed(&image)?;
    }
    // The handle keeps the image attached for raw access until it is dropped.
    let vhd = disk_vhdmp::VhdmpDisk::options()
        .read_only(false)
        .open(&image_path)
        .context("failed to open disk image")?;

    // As in the virtio-fs test, an inheritable duplicate stands in for one a
    // real client would `DuplicateHandle` into OpenVMM.
    let inherited_disk = vhd.as_handle().duplicate(true, None)?;
    let disk_value = inherited_disk.as_raw_handle() as u64;
    let (mut child, client, stderr_task) =
        super::launch_openvmm(&driver, &params, &openvmm, &socket_path, &pidfile_path).await?;
    drop(inherited_disk);

    let mut conn = PolledSocket::connect_unix(&driver, &socket_path)
        .await
        .context("connecting handle-passing socket")?;
    super::resource_passing_handshake(&mut conn, HANDSHAKE_MAGIC)
        .await
        .context("handle-passing handshake")?;
    let (status, msg) = request(&mut conn, OPCODE_REGISTER, DISK_NAME, Some(disk_value)).await?;
    anyhow::ensure!(status == 0, "register '{DISK_NAME}' failed: {msg}");

    let mut pipette_listener = super::listen_for_pipette(&driver, &hvsocket_path)?;
    client
        .call()
        .start(
            vmservice::Vm::CreateVm,
            create_vm_request(
                &kernel_path,
                pipette_initrd_file.path(),
                format!(
                    "console={} rdinit=/pipette panic=-1 initcall_blacklist=virtio_vsock_init",
                    super::linux_console()
                ),
                disk_config(&hvsocket_path, &com1_path),
            ),
        )
        .await
        .map_err(|err| anyhow::anyhow!("CreateVm failed: {}", err.message))?;

    // The device holds its own duplicate, so the name is no longer needed.
    let (status, msg) = request(&mut conn, OPCODE_DEREGISTER, DISK_NAME, None).await?;
    anyhow::ensure!(status == 0, "deregister '{DISK_NAME}' failed: {msg}");
    drop(conn);

    let _com1_task = driver.spawn(
        "com1",
        petri::log_task(
            params.log_source.log_file("linux")?,
            PolledSocket::new(&driver, UnixStream::connect(&com1_path)?)?,
            "linux com1",
        ),
    );
    let waiter = client.call().start(vmservice::Vm::WaitVm, ());
    client
        .call()
        .start(vmservice::Vm::ResumeVm, ())
        .await
        .map_err(|err| anyhow::anyhow!("ResumeVm failed: {}", err.message))?;
    let agent = super::accept_pipette(
        &driver,
        &params,
        &mut pipette_listener,
        Duration::from_mins(2),
    )
    .await?;

    super::wait_for_block_device(&agent, "vda").await?;
    let sh = agent.unix_shell();
    let sectors: u64 = cmd!(sh, "cat /sys/block/vda/size")
        .read()
        .await?
        .trim()
        .parse()
        .context("parse vda size")?;
    anyhow::ensure!(
        sectors * 512 == DISK_SIZE,
        "unexpected vda size: {sectors} sectors"
    );
    let len = HOST_MARKER.len();
    let script = format!(
        "dd if=/dev/vda bs={DISK_BLOCK_SIZE} skip={HOST_MARKER_BLOCK} count=1 2>/dev/null | head -c {len}"
    );
    let marker = cmd!(sh, "sh -c {script}").read().await?;
    anyhow::ensure!(
        marker == HOST_MARKER,
        "guest read {marker:?}, expected {HOST_MARKER:?}"
    );

    let command = format!(
        "printf {GUEST_MARKER} | dd of=/dev/vda bs={DISK_BLOCK_SIZE} seek={GUEST_MARKER_BLOCK} conv=notrunc,fsync 2>/dev/null"
    );
    cmd!(sh, "sh -c {command}").run().await?;

    agent.power_off().await?;
    CancelContext::new()
        .with_timeout(Duration::from_mins(1))
        .until_cancelled(waiter)
        .await
        .context("timed out waiting for guest poweroff")?
        .map_err(|err| anyhow::anyhow!("WaitVm failed: {}", err.message))?;
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
    anyhow::ensure!(
        exit_status.success(),
        "openvmm exited abnormally: {exit_status:?}"
    );

    // The guest's virtio-blk write reached the disk image.
    drop(vhd);
    let mut block = vec![0; DISK_BLOCK_SIZE];
    std::fs::File::open(&image_path)?
        .seek_read(&mut block, GUEST_MARKER_BLOCK * DISK_BLOCK_SIZE as u64)
        .context("failed to read guest marker")?;
    anyhow::ensure!(
        block.starts_with(GUEST_MARKER.as_bytes()),
        "host read {:?} from {}, expected {GUEST_MARKER:?}",
        String::from_utf8_lossy(&block[..GUEST_MARKER.len()]),
        image_path.display()
    );
    Ok(())
}

/// Returns a disk block beginning with `marker`.
fn marker_block(marker: &str) -> Vec<u8> {
    let mut block = vec![0; DISK_BLOCK_SIZE];
    block[..marker.len()].copy_from_slice(marker.as_bytes());
    block
}

/// Returns VM configuration exposing the registered disk as a writable
/// virtio-blk device, with pipette reachable over `hvsocket_path`.
fn disk_config(hvsocket_path: &Path, com1_path: &Path) -> vmservice::VmConfig {
    let disk = vmservice::DiskBackend {
        kind: Some(vmservice::disk_backend::Kind::VirtualDisk(
            vmservice::VirtualDisk {
                handle_name: DISK_NAME.to_string(),
            },
        )),
    };
    vmservice::VmConfig {
        pcie: Some(vmservice::PcieTopologyConfig {
            root_complexes: vec![vmservice::PcieRootComplex {
                name: "rc0".to_string(),
                end_bus: 255,
                low_mmio: 64 * 1024 * 1024,
                high_mmio: 1024 * 1024 * 1024,
                root_ports: vec![super::pcie_root_port(
                    "blk",
                    false,
                    Some(super::attachment_device(super::virtio_device(
                        vmservice::virtio_device::Kind::Blk(vmservice::VirtioBlk {
                            backend: Some(disk),
                            read_only: false,
                            serial: None,
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
                socket_path: com1_path.to_string_lossy().into_owned(),
                connect: false,
            }],
        }),
        hvsocket_config: Some(vmservice::HvSocketConfig {
            path: hvsocket_path.to_string_lossy().into_owned(),
        }),
        ..Default::default()
    }
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

/// Returns VM configuration that exposes `share_root` over virtio-fs,
/// accessed as the token registered as `user_token_name`.
fn virtiofs_config(share_root: &Path, user_token_name: &str) -> vmservice::VmConfig {
    vmservice::VmConfig {
        devices_config: Some(vmservice::DevicesConfig {
            virtiofs_config: vec![vmservice::VirtioFs {
                tag: "userfs".to_string(),
                root_path: share_root.to_string_lossy().into_owned(),
                mount_options: String::new(),
                windows_options: Some(vmservice::virtio_fs::WindowsOptions {
                    user_token_name: user_token_name.to_string(),
                    shared_mem: false,
                }),
            }],
            ..Default::default()
        }),
        ..Default::default()
    }
}

/// Builds a direct-boot `CreateVm` request with the devices in `config`.
fn create_vm_request(
    kernel_path: &ResolvedArtifact,
    initrd_path: &Path,
    kernel_cmdline: String,
    config: vmservice::VmConfig,
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
                    initrd_path: initrd_path.to_string_lossy().into_owned(),
                    kernel_cmdline,
                },
            )),
            ..config
        }),
        log_id: String::new(),
    }
}
