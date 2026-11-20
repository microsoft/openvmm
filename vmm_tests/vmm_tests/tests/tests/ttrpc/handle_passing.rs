// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Integration tests for OpenVMM's handle-passing protocol on the TTRPC
//! endpoint.
//!
//! These boot a real `openvmm --rpc` process and register handles by name
//! over the handle-passing protocol (see
//! `openvmm_ttrpc_vmservice/src/resource_passing.md`):
//!
//! * An access token, confirming that a virtio-fs share's
//!   `WindowsOptions.user_token_name` resolves through that registration
//!   during `CreateVm`. How virtio-fs uses the token is covered by the
//!   virtiofs crate's tests.
//! * A host physical disk, confirming that `HandleDisk.handle_name` backs
//!   virtio-blk and NVMe devices whose data the guest and host both observe.
//!   Exposing a physical disk requires an elevated process, so this test is
//!   skipped on non-elevated hosts.

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
use petri::requirements::TestCaseRequirements;
use petri::requirements::TestRequirement;
use petri_artifacts_vmm_test::artifacts;
use std::io::Write as _;
use std::os::windows::fs::FileExt as _;
use std::os::windows::io::AsHandle;
use std::os::windows::io::AsRawHandle;
use std::path::Path;
use std::path::PathBuf;
use std::time::Duration;
use unix_socket::UnixListener;
use unix_socket::UnixStream;
use windows_sys::Win32::Security::TOKEN_DUPLICATE;
use windows_sys::Win32::Security::TOKEN_QUERY;

/// The 4-byte handle-passing handshake magic: `0xFD 'H' 'P' 0x01`.
const HANDSHAKE_MAGIC: [u8; 4] = [0xFD, b'H', b'P', 0x01];

const OPCODE_REGISTER: u8 = 1;
const OPCODE_DEREGISTER: u8 = 2;

const TOKEN_NAME: &str = "user-token";

const DISK_NAME: &str = "physical-disk";
const DISK_SIZE: u64 = 16 * 1024 * 1024;
const DISK_SERIAL: &str = "ttrpc-physdisk";
/// I/O size for raw disk access, which must be sector aligned.
const DISK_BLOCK_SIZE: usize = 4096;
/// The host writes `HOST_MARKER` at this block (1 MiB) before boot.
const HOST_MARKER: &str = "ttrpc-physdisk-host";
const HOST_MARKER_BLOCK: u64 = 256;
/// The guest writes `GUEST_MARKER` at this block (2 MiB) over virtio-blk.
const GUEST_MARKER: &str = "ttrpc-physdisk-guest";
const GUEST_MARKER_BLOCK: u64 = 512;

petri::test!(handle_passing_virtiofs_user_token, |resolver| {
    let openvmm = resolver.require(artifacts::OPENVMM_NATIVE);
    let kernel = resolver.require(artifacts::loadable::LINUX_DIRECT_TEST_KERNEL_NATIVE);
    let initrd = resolver.require(artifacts::loadable::LINUX_DIRECT_TEST_INITRD_NATIVE);
    Some([openvmm.erase(), kernel.erase(), initrd.erase()])
});

petri::multitest!(vec![
    petri::SimpleTest::new_async(
        "handle_passing_physical_disk",
        |resolver| {
            let openvmm = resolver.require(artifacts::OPENVMM_NATIVE);
            let kernel = resolver.require(artifacts::loadable::LINUX_DIRECT_TEST_KERNEL_NATIVE);
            let initrd = resolver.require(artifacts::loadable::LINUX_DIRECT_TEST_INITRD_NATIVE);
            let pipette = match petri_artifacts_common::tags::MachineArch::host() {
                petri_artifacts_common::tags::MachineArch::X86_64 => resolver
                    .require(petri_artifacts_common::artifacts::PIPETTE_LINUX_X64_MUSL)
                    .erase(),
                petri_artifacts_common::tags::MachineArch::Aarch64 => resolver
                    .require(petri_artifacts_common::artifacts::PIPETTE_LINUX_AARCH64_MUSL)
                    .erase(),
            };
            Some([openvmm.erase(), kernel.erase(), initrd.erase(), pipette])
        },
        handle_passing_physical_disk,
    )
    // Exposing a disk image as a host physical disk requires elevation.
    .requirements(TestCaseRequirements::new(TestRequirement::HostCapability(
        host_is_elevated
    )))
    .into()
]);

/// Returns whether the test process is elevated. Probe failures report as
/// elevated so that the test body surfaces the error rather than skipping.
fn host_is_elevated() -> bool {
    pal::windows::security::is_process_elevated().unwrap_or(true)
}

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

async fn handle_passing_physical_disk(
    params: petri::PetriTestParams<'_>,
    driver: DefaultDriver,
    [openvmm, kernel_path, initrd_path, pipette_path]: [ResolvedArtifact; 4],
) -> anyhow::Result<()> {
    anyhow::ensure!(
        pal::windows::security::is_process_elevated()
            .context("failed to query process elevation")?,
        "physical disk test requires an elevated process"
    );
    let tempdir = tempfile::tempdir()?;
    let socket_path = tempdir.path().join("ttrpc.sock");
    let pidfile_path = tempdir.path().join("openvmm.pid");
    let hvsocket_path = tempdir.path().join("hvsocket");
    let com1_path = tempdir.path().join("com1.sock");

    let initrd = std::fs::read(initrd_path.get()).context("failed to read test initrd")?;
    let pipette = std::fs::read(pipette_path.get()).context("failed to read pipette")?;
    let pipette_initrd = initrd_cpio::inject_into_initrd(&initrd, "pipette", &pipette, 0o100755)
        .context("failed to inject pipette into test initrd")?;
    let mut pipette_initrd_file = tempfile::NamedTempFile::new_in(tempdir.path())?;
    pipette_initrd_file.write_all(&pipette_initrd)?;

    // Expose a fixed VHD as a host disk to get a real `\\.\PhysicalDriveN`.
    // The test reads and writes markers through the image file rather than
    // the raw disk, so only OpenVMM accesses the disk directly.
    let image_path = tempdir.path().join("physical-disk.vhd");
    {
        let image = std::fs::File::create(&image_path)?;
        image.set_len(DISK_SIZE)?;
        image.seek_write(
            &marker_block(HOST_MARKER),
            HOST_MARKER_BLOCK * DISK_BLOCK_SIZE as u64,
        )?;
        disk_vhd1::Vhd1Disk::make_fixed(&image)?;
    }
    let mounted = MountedImage::mount(&image_path)?;
    let disk_path = format!(r"\\.\PhysicalDrive{}", mounted.disk_number);
    // OpenVMM requires a handle opened for synchronous I/O, which is how the
    // standard library opens files.
    let disk = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(&disk_path)
        .with_context(|| format!("failed to open {disk_path}"))?;

    // As in the virtio-fs test, an inheritable duplicate stands in for one a
    // real client would `DuplicateHandle` into OpenVMM.
    let inherited_disk = disk.as_handle().duplicate(true, None)?;
    drop(disk);
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

    let mut pipette_listener = PolledSocket::new(
        &driver,
        UnixListener::bind(format!(
            "{}_{}",
            hvsocket_path.to_string_lossy(),
            pipette_client::PIPETTE_PORT
        ))?,
    )?;
    let console = match petri_artifacts_common::tags::MachineArch::host() {
        petri_artifacts_common::tags::MachineArch::X86_64 => "ttyS0",
        petri_artifacts_common::tags::MachineArch::Aarch64 => "ttyAMA0",
    };
    client
        .call()
        .start(
            vmservice::Vm::CreateVm,
            physical_disk_create_vm_request(
                &kernel_path,
                pipette_initrd_file.path(),
                console,
                &hvsocket_path,
                &com1_path,
            ),
        )
        .await
        .map_err(|err| anyhow::anyhow!("CreateVm failed: {}", err.message))?;

    // The devices hold their own duplicates, so the name is no longer needed.
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
    let agent = CancelContext::new()
        .with_timeout(Duration::from_mins(2))
        .until_cancelled(async {
            let (conn, _) = pipette_listener.accept().await?;
            pipette_client::PipetteClient::new(
                &driver,
                PolledSocket::new(&driver, conn)?,
                params.log_source.output_dir(),
            )
            .await
        })
        .await
        .context("timed out connecting to pipette")??;

    let sh = agent.unix_shell();
    for name in ["vda", "nvme0n1"] {
        let script = format!(
            "i=0; while [ $i -lt 200 ]; do \
             test -b /dev/{name} && exit 0; \
             i=$((i + 1)); sleep 0.1; done; exit 1"
        );
        cmd!(sh, "sh -c {script}")
            .run()
            .await
            .with_context(|| format!("waiting for /dev/{name}"))?;
        let size_path = format!("/sys/block/{name}/size");
        let sectors: u64 = cmd!(sh, "cat {size_path}")
            .read()
            .await?
            .trim()
            .parse()
            .with_context(|| format!("parse {name} size"))?;
        anyhow::ensure!(
            sectors * 512 == DISK_SIZE,
            "unexpected {name} size: {sectors} sectors"
        );
        let marker = read_guest_marker(&agent, name, HOST_MARKER_BLOCK, HOST_MARKER.len()).await?;
        anyhow::ensure!(
            marker == HOST_MARKER,
            "guest read {marker:?} from {name}, expected {HOST_MARKER:?}"
        );
    }
    let serial = cmd!(sh, "cat /sys/block/vda/serial").read().await?;
    anyhow::ensure!(
        serial.trim() == DISK_SERIAL,
        "expected virtio-blk serial {DISK_SERIAL:?}, found {:?}",
        serial.trim()
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

    // The guest's virtio-blk write reached the host disk.
    drop(mounted);
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

/// A disk image mounted as a host disk, dismounted on drop.
struct MountedImage {
    path: PathBuf,
    disk_number: u32,
}

impl MountedImage {
    /// Mounts the fixed VHD at `path` read/write as an online host disk.
    fn mount(path: &Path) -> anyhow::Result<Self> {
        let image = powershell_literal(path);
        powershell(&format!(
            "Mount-DiskImage -ImagePath {image} -StorageType VHD -Access ReadWrite \
             -NoDriveLetter | Out-Null"
        ))
        .context("failed to mount disk image (requires administrator)")?;
        // Establish cleanup before any subsequent setup or parsing can fail.
        let mut mounted = Self {
            path: path.to_path_buf(),
            disk_number: 0,
        };
        let output = powershell(&format!(
            "$disk = Get-DiskImage -ImagePath {image} | Get-Disk; \
             if ($disk.IsOffline) {{ Set-Disk -Number $disk.Number -IsOffline $false }}; \
             if ($disk.IsReadOnly) {{ Set-Disk -Number $disk.Number -IsReadOnly $false }}; \
             $disk.Number"
        ))
        .context("failed to configure mounted disk image")?;
        mounted.disk_number = output
            .trim()
            .parse()
            .with_context(|| format!("invalid disk number {output:?}"))?;
        Ok(mounted)
    }
}

impl Drop for MountedImage {
    fn drop(&mut self) {
        let image = powershell_literal(&self.path);
        if let Err(err) = powershell(&format!("Dismount-DiskImage -ImagePath {image} | Out-Null")) {
            tracing::warn!(
                error = err.as_ref() as &dyn std::error::Error,
                path = %self.path.display(),
                "failed to dismount disk image"
            );
        }
    }
}

/// Quotes `path` as a PowerShell single-quoted string literal.
fn powershell_literal(path: &Path) -> String {
    format!("'{}'", path.to_string_lossy().replace('\'', "''"))
}

/// Runs a PowerShell script, failing on any error, and returns its stdout.
fn powershell(script: &str) -> anyhow::Result<String> {
    let output = std::process::Command::new("powershell.exe")
        .args(["-NoLogo", "-NoProfile", "-NonInteractive", "-Command"])
        .arg(format!("$ErrorActionPreference = 'Stop'; {script}"))
        .output()
        .context("failed to run powershell")?;
    anyhow::ensure!(
        output.status.success(),
        "powershell failed ({}): {}",
        output.status,
        String::from_utf8_lossy(&output.stderr).trim()
    );
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

/// Returns a disk block beginning with `marker`.
fn marker_block(marker: &str) -> Vec<u8> {
    let mut block = vec![0; DISK_BLOCK_SIZE];
    block[..marker.len()].copy_from_slice(marker.as_bytes());
    block
}

/// Reads the `len`-byte marker at `block` of the guest's `/dev/{name}`.
async fn read_guest_marker(
    agent: &pipette_client::PipetteClient,
    name: &str,
    block: u64,
    len: usize,
) -> anyhow::Result<String> {
    let sh = agent.unix_shell();
    let script = format!(
        "dd if=/dev/{name} bs={DISK_BLOCK_SIZE} skip={block} count=1 2>/dev/null | head -c {len}"
    );
    cmd!(sh, "sh -c {script}").read().await
}

/// Builds a direct-boot `CreateVm` request exposing the registered physical
/// disk as a writable virtio-blk device and a read-only NVMe namespace.
fn physical_disk_create_vm_request(
    kernel_path: &ResolvedArtifact,
    initrd_path: &Path,
    console: &str,
    hvsocket_path: &Path,
    com1_path: &Path,
) -> vmservice::CreateVmRequest {
    let disk = || vmservice::DiskBackend {
        kind: Some(vmservice::disk_backend::Kind::Handle(
            vmservice::HandleDisk {
                handle_name: DISK_NAME.to_string(),
            },
        )),
    };
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
                    kernel_path: kernel_path.get().to_string_lossy().into_owned(),
                    initrd_path: initrd_path.to_string_lossy().into_owned(),
                    kernel_cmdline: format!(
                        "console={console} rdinit=/pipette panic=-1 initcall_blacklist=virtio_vsock_init"
                    ),
                },
            )),
            pcie: Some(vmservice::PcieTopologyConfig {
                root_complexes: vec![vmservice::PcieRootComplex {
                    name: "rc0".to_string(),
                    end_bus: 255,
                    low_mmio: 64 * 1024 * 1024,
                    high_mmio: 1024 * 1024 * 1024,
                    root_ports: vec![
                        super::pcie_root_port(
                            "blk",
                            false,
                            Some(super::attachment_device(super::virtio_device(
                                vmservice::virtio_device::Kind::Blk(vmservice::VirtioBlk {
                                    backend: Some(disk()),
                                    read_only: false,
                                    serial: Some(DISK_SERIAL.to_string()),
                                }),
                            ))),
                        ),
                        super::pcie_root_port(
                            "nvme",
                            false,
                            Some(super::attachment_device(vmservice::PcieDeviceKind {
                                kind: Some(vmservice::pcie_device_kind::Kind::Nvme(
                                    vmservice::NvmeConfig {
                                        controller_id: "nvme0".to_string(),
                                        namespaces: vec![vmservice::NvmeNamespace {
                                            nsid: 1,
                                            backend: Some(disk()),
                                            read_only: true,
                                        }],
                                    },
                                )),
                            })),
                        ),
                    ],
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
        }),
        log_id: String::new(),
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
                    mount_options: String::new(),
                    windows_options: Some(vmservice::virtio_fs::WindowsOptions {
                        user_token_name: user_token_name.to_string(),
                        shared_mem: false,
                    }),
                }],
                ..Default::default()
            }),
            ..Default::default()
        }),
        log_id: String::new(),
    }
}
