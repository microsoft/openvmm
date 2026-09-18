// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! QEMU backend for Petri. Currently only supports Aarch64 CPU emulation on
//! Linux x64 hosts, but could be modified to support more qemu-system-arch
//! configurations in the future.

pub mod devices;

use crate::Drive;
use crate::Firmware;
use crate::ModifyFn;
use crate::NoPetriVmFramebufferAccess;
use crate::NoPetriVmInspector;
use crate::OpenHclServicingFlags;
use crate::PetriHaltReason;
use crate::PetriHaltReasonDetail;
use crate::PetriInitrd;
use crate::PetriVmConfig;
use crate::PetriVmResources;
use crate::PetriVmRuntime;
use crate::PetriVmRuntimeConfig;
use crate::PetriVmmBackend;
use crate::ShutdownKind;
use crate::VmmQuirks;
use crate::openhcl_diag::OpenHclDiagHandler;
use crate::vm::PetriVmProperties;
use anyhow::Context;
use async_trait::async_trait;
use devices::DeviceConfig;
use futures::lock::Mutex;
use futures_concurrency::future::Race;
use get_resources::ged::FirmwareEvent;
use guid::Guid;
use pal_async::DefaultDriver;
use pal_async::pipe::PolledPipe;
use pal_async::process::PolledChild;
use pal_async::task::Spawn;
use pal_async::task::Task;
use pal_async::timer::PolledTimer;
use petri_artifacts_common::tags::GuestQuirksInner;
use petri_artifacts_common::tags::MachineArch;
use petri_artifacts_core::ArtifactResolver;
use petri_artifacts_core::ResolvedArtifact;
use pipette_client::PipetteClient;
use std::path::Path;
use std::path::PathBuf;
use std::process::Command;
use std::sync::Arc;
use std::time::Duration;
use tempfile::TempPath;
use vtl2_settings_proto::Vtl2Settings;

/// The QEMU Petri backend
#[derive(Debug)]
pub struct QemuPetriBackend {
    qemu_path: ResolvedArtifact,
}

/// QEMU-specific emulator configuration.
#[derive(Debug, Default)]
pub struct QemuPetriConfig {
    share_9p: Option<PathBuf>,
    devices: Vec<DeviceConfig>,
}

/// Resources needed at runtime for a QEMU Petri VM
pub struct QemuPetriRuntime {
    driver: DefaultDriver,
    qemu_process: Arc<Mutex<PolledChild<std::process::Child>>>,
    host_pipette_port: u16,
    log_tasks: Vec<Task<anyhow::Result<()>>>,
    output_dir: PathBuf,
}

#[async_trait]
impl PetriVmmBackend for QemuPetriBackend {
    type VmmConfig = QemuPetriConfig;
    type VmRuntime = QemuPetriRuntime;
    const SUPPORTS_VMBUS: bool = false;

    fn check_compat(_firmware: &Firmware, _arch: MachineArch) -> bool {
        // Our QEMU bachend only supports linux X64 at this time
        MachineArch::X86_64 == MachineArch::host() && cfg!(target_os = "linux")
    }

    fn quirks(_firmware: &Firmware) -> (GuestQuirksInner, VmmQuirks) {
        (GuestQuirksInner::default(), VmmQuirks::default())
    }

    fn default_servicing_flags() -> OpenHclServicingFlags {
        OpenHclServicingFlags {
            enable_nvme_keepalive: false,
            enable_mana_keepalive: false,
            override_version_checks: true,
            stop_timeout_hint_secs: None,
        }
    }

    fn create_guest_dump_disk() -> anyhow::Result<
        Option<(
            Arc<TempPath>,
            Box<dyn FnOnce() -> anyhow::Result<Box<dyn fatfs::ReadWriteSeek>>>,
        )>,
    > {
        Ok(None)
    }

    fn build_custom_init_script(pipette_path: &str) -> Option<String> {
        Some(format!(
            "#!/bin/sh\n\
            ip link set eth0 up\n\
            ip addr add 10.0.2.15/24 dev eth0\n\
            ip route add default via 10.0.2.2\n\
            echo 'nameserver 10.0.2.3' > /etc/resolv.conf\n\
            exec '/{}' --transport tcp\n",
            pipette_path.replace('\'', "'\\''")
        ))
    }

    fn new(resolver: &ArtifactResolver<'_>, arch: MachineArch) -> Self {
        // QEMU bachend only supports linux X64 with an aarch64 guest
        // TODO: we should have QEMU_SYSTEM_<GUEST_ARCH>_NATIVE symbols
        // and match here based on guest arch.
        assert_eq!(arch, MachineArch::Aarch64);
        QemuPetriBackend {
            qemu_path: resolver
                .require(petri_artifacts_vmm_test::artifacts::QEMU_SYSTEM_AARCH64_LINUX_X64)
                .erase(),
        }
    }

    async fn run(
        self,
        config: PetriVmConfig,
        modify_vmm_config: Option<ModifyFn<Self::VmmConfig>>,
        resources: &PetriVmResources,
        _properties: PetriVmProperties,
    ) -> anyhow::Result<(Self::VmRuntime, PetriVmRuntimeConfig)> {
        let PetriVmResources {
            driver,
            log_source,
            prebuilt_initrd,
        } = resources;

        let mut qemu_config = QemuPetriConfig::default();
        if let Some(f) = modify_vmm_config {
            qemu_config = f.0(qemu_config);
        }

        let host_pipette_port = pick_free_port().context("failed to find a free port")?;

        let mut cmd = build_qemu_command(
            self.qemu_path.get(),
            &config,
            &qemu_config,
            host_pipette_port,
            prebuilt_initrd
                .as_ref()
                .context("QEMU requires a prebuilt initrd")?,
        )?;
        cmd.stdin(std::process::Stdio::null());
        cmd.stdout(std::process::Stdio::piped());
        cmd.stderr(std::process::Stdio::piped());

        tracing::info!(?cmd, "launching qemu");
        let mut qemu_process = cmd.spawn().context("failed to launch QEMU")?;
        let qemu_stdout = qemu_process.stdout.take().expect("stdout should be piped");
        let qemu_stderr = qemu_process.stderr.take().expect("stderr should be piped");

        let qemu_process = PolledChild::<std::process::Child>::new(driver, qemu_process)
            .context("failed to create PolledChild")?;

        let mut log_tasks = Vec::new();

        let qemu_stdout_pipe = PolledPipe::new(driver, child_pipe_to_file(qemu_stdout))
            .context("failed to create polled pipe for qemu stdout")?;
        // Since we pass `-serial mon:stdio` to qemu, the guest outputs to stdout.
        let qemu_stdout_log_file = log_source.log_file("guest")?;
        let qemu_stdout_task = driver.spawn(
            "qemu_stdout",
            crate::log_task(qemu_stdout_log_file, qemu_stdout_pipe, "qemu_stdout"),
        );
        log_tasks.push(qemu_stdout_task);

        let qemu_stderr_pipe = PolledPipe::new(driver, child_pipe_to_file(qemu_stderr))
            .context("failed to create polled pipe for qemu stderr")?;
        // QEMU error messages are printed to stderr.
        let qemu_stderr_log_file = log_source.log_file("qemu")?;
        let qemu_stderr_task = driver.spawn(
            "qemu_stderr",
            crate::log_task(qemu_stderr_log_file, qemu_stderr_pipe, "qemu_stderr"),
        );
        log_tasks.push(qemu_stderr_task);

        Ok((
            QemuPetriRuntime {
                driver: driver.clone(),
                qemu_process: Arc::new(Mutex::new(qemu_process)),
                host_pipette_port,
                log_tasks,
                output_dir: log_source.output_dir().to_owned(),
            },
            config
                .firmware
                .into_runtime_config(config.vmbus_storage_controllers),
        ))
    }
}

impl QemuPetriConfig {
    /// Share a directory with the guest via 9p.
    pub fn with_share_9p(mut self, path: impl AsRef<Path>) -> Self {
        self.share_9p = Some(path.as_ref().to_path_buf());
        self
    }

    /// Add a devices to the emulator configuration.
    pub fn with_devices(mut self, devices: impl IntoIterator<Item = DeviceConfig>) -> Self {
        self.devices.extend(devices);
        self
    }
}

#[async_trait]
impl PetriVmRuntime for QemuPetriRuntime {
    type VmInspector = NoPetriVmInspector;
    type VmFramebufferAccess = NoPetriVmFramebufferAccess;

    async fn teardown(mut self) -> anyhow::Result<()> {
        futures::future::join_all(self.log_tasks.into_iter().map(|t| t.cancel())).await;
        let mut qemu_process = self.qemu_process.lock().await;
        qemu_process
            .get_mut()
            .kill()
            .context("unable to kill qemu process")?;
        Ok(())
    }

    async fn wait_for_halt(&mut self, _allow_reset: bool) -> anyhow::Result<PetriHaltReasonDetail> {
        let status = self.qemu_process.lock().await.wait().await?;
        Ok(PetriHaltReasonDetail {
            reason: if status.success() {
                PetriHaltReason::PowerOff
            } else {
                PetriHaltReason::Other
            },
            detail: format!("QEMU exited with status: {:?}", status.code()),
        })
    }

    async fn wait_for_agent(&mut self, _set_high_vtl: bool) -> anyhow::Result<PipetteClient> {
        let driver = self.driver.clone();
        let output_dir = self.output_dir.clone();
        let addr = std::net::SocketAddr::from(([127, 0, 0, 1], self.host_pipette_port));
        let client_core = async move || {
            tracing::debug!("connecting to pipette");
            let socket = pal_async::socket::PolledSocket::connect_tcp(&driver, addr)
                .await
                .context("failed to connect to pipette")?;
            tracing::debug!("handshaking with pipette");
            PipetteClient::new(&driver, socket, &output_dir)
                .await
                .context("failed to create pipette client")
        };

        self.poll_until_exit_or_timeout(None, async move |_| match client_core().await {
            Ok(client) => {
                tracing::info!("completed pipette handshake");
                Ok(Some(client))
            }
            Err(_err) => {
                tracing::debug!("failed to connect to pipette server, retrying");
                Ok(None)
            }
        })
        .await
    }

    fn openhcl_diag(&self) -> Option<OpenHclDiagHandler> {
        // QEMU doesn't support OpenHCL
        None
    }

    async fn wait_for_boot_event(
        &mut self,
        _timeout: Option<Duration>,
    ) -> anyhow::Result<Option<FirmwareEvent>> {
        anyhow::bail!("QEMU backend only supports linux direct, which doesn't emit a boot event")
    }

    async fn wait_for_enlightened_shutdown_ready(&mut self) -> anyhow::Result<()> {
        tracing::info!("QEMU doesn't support enlightened shutdown, immediately returning");
        Ok(())
    }

    async fn send_enlightened_shutdown(&mut self, _kind: ShutdownKind) -> anyhow::Result<()> {
        anyhow::bail!("QEMU doesn't support enlightened shutdown")
    }

    async fn restart_openhcl(
        &mut self,
        _new_openhcl: &ResolvedArtifact,
        _flags: OpenHclServicingFlags,
    ) -> anyhow::Result<()> {
        anyhow::bail!("QEMU doesn't support OpenHCL")
    }

    async fn save_openhcl(
        &mut self,
        _new_openhcl: &ResolvedArtifact,
        _flags: OpenHclServicingFlags,
    ) -> anyhow::Result<()> {
        anyhow::bail!("QEMU doesn't support OpenHCL")
    }

    async fn restore_openhcl(&mut self) -> anyhow::Result<()> {
        anyhow::bail!("QEMU doesn't support OpenHCL")
    }

    async fn update_command_line(&mut self, _command_line: &str) -> anyhow::Result<()> {
        todo!()
    }

    fn take_framebuffer_access(&mut self) -> Option<NoPetriVmFramebufferAccess> {
        None
    }

    async fn reset(&mut self) -> anyhow::Result<()> {
        todo!()
    }

    async fn get_guest_state_file(&self) -> anyhow::Result<Option<PathBuf>> {
        todo!()
    }

    async fn set_vtl2_settings(&mut self, _settings: &Vtl2Settings) -> anyhow::Result<()> {
        anyhow::bail!("QEMU doesn't support OpenHCL")
    }

    async fn set_vmbus_drive(
        &mut self,
        _drive: &Drive,
        _controller_id: &Guid,
        _controller_location: u32,
    ) -> anyhow::Result<()> {
        anyhow::bail!("QEMU doesn't support VMBus")
    }
}

impl QemuPetriRuntime {
    /// Poll `f` until it produces a value, failing if the QEMU process exits
    /// first and giving up once `timeout` has elapsed.
    async fn poll_until_exit_or_timeout<T>(
        &mut self,
        timeout: Option<Duration>,
        f: impl AsyncFn(&Self) -> anyhow::Result<Option<T>>,
    ) -> anyhow::Result<T> {
        let mut qemu_process = self.qemu_process.lock().await;
        (
            async {
                let mut timer = PolledTimer::new(&self.driver);
                loop {
                    if let Some(v) = f(self).await? {
                        return Ok(v);
                    }
                    timer.sleep(Duration::from_secs(10)).await;
                }
            },
            async {
                let status = qemu_process.wait().await?;
                Err(anyhow::anyhow!(
                    "QEMU exited unexpectedly (status: {status})"
                ))
            },
            async {
                PolledTimer::new(&self.driver)
                    .sleep(timeout.unwrap_or(Duration::from_mins(10)))
                    .await;
                Err(anyhow::anyhow!("Timed out waiting for operation"))
            },
        )
            .race()
            .await
    }
}

/// Convert a child process's stdout/stderr pipe into a [`std::fs::File`] so it
/// can be wrapped in a [`PolledPipe`]. The owned-handle type differs by
/// platform, but the conversion is otherwise identical.
#[cfg(unix)]
pub fn child_pipe_to_file(pipe: impl Into<std::os::unix::io::OwnedFd>) -> std::fs::File {
    std::fs::File::from(pipe.into())
}

/// Convert a child process's stdout/stderr pipe into a [`std::fs::File`] so it
/// can be wrapped in a [`PolledPipe`]. The owned-handle type differs by
/// platform, but the conversion is otherwise identical.
#[cfg(windows)]
pub fn child_pipe_to_file(pipe: impl Into<std::os::windows::io::OwnedHandle>) -> std::fs::File {
    std::fs::File::from(pipe.into())
}

/// Find a free TCP port by binding to port 0 and reading the assigned port.
fn pick_free_port() -> anyhow::Result<u16> {
    let listener =
        std::net::TcpListener::bind("127.0.0.1:0").context("failed to bind ephemeral port")?;
    let port = listener
        .local_addr()
        .context("failed to get local addr")?
        .port();
    Ok(port)
}

/// First PCI device number (`addr=`) used for extra-device root ports.
///
/// QEMU's built-in devices use low device numbers. We start at 16 (0x10)
/// to avoid collisions. The root port for the i-th extra device has
/// devfn = `(EXTRA_DEVICE_ADDR_BASE + i) << 3`.
pub const EXTRA_DEVICE_ADDR_BASE: usize = 16;

/// Mount tag for the host 9P share
pub const SHARE_9P_MOUNT_TAG: &str = "hostshare";

/// Build the QEMU command line for a TCG launch.
pub fn build_qemu_command(
    binary: &Path,
    config: &PetriVmConfig,
    qemu_config: &QemuPetriConfig,
    host_pipette_port: u16,
    prebuilt_initrd: &PetriInitrd,
) -> anyhow::Result<Command> {
    let mut cmd = Command::new(binary);

    cmd.arg("-machine")
        .arg("virt,virtualization=on,iommu=smmuv3,gic-version=3");
    cmd.arg("-cpu").arg("max");
    // TODO: more complex memory topologies
    cmd.arg("-m")
        .arg((config.memory.startup_bytes / (1024 * 1024)).to_string());
    // TODO: more complex CPU topologies
    cmd.arg("-smp")
        .arg(config.proc_topology.vp_count.to_string());
    cmd.arg("-nographic");

    let PetriInitrd { path, rdinit_param } = prebuilt_initrd;

    match &config.firmware {
        Firmware::LinuxDirect { kernel, .. } => {
            cmd.arg("-kernel").arg(kernel);
            cmd.arg("-initrd").arg(path.as_ref());
        }
        _ => anyhow::bail!("qemu backend only supports linux direct"),
    };

    cmd.arg("-append").arg(format!("rdinit={rdinit_param}"));
    cmd.arg("-no-reboot");

    // 9p: share the host directory into the guest
    if let Some(share_dir) = qemu_config.share_9p.as_ref() {
        cmd.arg("-fsdev").arg(format!(
            "local,id=fsdev0,path={},security_model=none",
            share_dir.display()
        ));
        cmd.arg("-device").arg(format!(
            "virtio-9p-pci,fsdev=fsdev0,mount_tag={SHARE_9P_MOUNT_TAG}"
        ));
    }

    // User-mode networking with port forwarding for pipette TCP
    cmd.arg("-netdev").arg(format!(
        "user,id=net0,hostfwd=tcp::{host_pipette_port}-:{guest_port}",
        guest_port = pipette_client::PIPETTE_PORT,
    ));
    cmd.arg("-device")
        .arg("virtio-net-pci,netdev=net0,romfile=");

    // Console on serial (diagnostic only)
    cmd.arg("-serial").arg("mon:stdio");

    // Extra devices from the profile.
    // Each device gets its own PCIe root port at a known PCI device number
    // (`addr=`), so the VFIO setup code can find the bridge by its devfn
    // in sysfs and enumerate the child behind it.
    for (i, device) in qemu_config.devices.iter().enumerate() {
        let rp_id = format!("hosting_rp{i}");
        let addr = EXTRA_DEVICE_ADDR_BASE + i;
        // Each root port needs a unique `slot` within its chassis. QEMU
        // defaults every pcie-root-port to `chassis=0,slot=0`, so a second
        // port collides with "Can't add chassis slot, error -16" (EBUSY).
        // Number the slots from 1.
        let slot = i + 1;
        cmd.arg("-device").arg(format!(
            "pcie-root-port,id={rp_id},addr={addr:#x},slot={slot}"
        ));

        match device {
            DeviceConfig::VirtioBlk(cfg) => {
                let node_name = format!("disk{i}");
                let size_bytes = cfg.size;
                cmd.arg("-blockdev")
                    .arg(format!("null-co,node-name={node_name},size={size_bytes}"));
                cmd.arg("-device")
                    .arg(format!("virtio-blk-pci,drive={node_name},bus={rp_id},iommu_platform=on,disable-legacy=on,romfile="));
            }
            DeviceConfig::Edu(cfg) => {
                // A conventional PCI endpoint with a register-programmed DMA
                // engine. Widen `dma_mask` when provided so the engine can
                // address high aarch64 guest physical addresses (its 28-bit
                // default would clamp them).
                let mut dev = format!("edu,bus={rp_id}");
                if let Some(mask) = &cfg.dma_mask {
                    dev.push_str(&format!(",dma_mask={mask:#x}"));
                }
                cmd.arg("-device").arg(dev);
            }
            DeviceConfig::IvshmemPlain(cfg) => {
                // BAR2 is a prefetchable, RAM-backed memory window served by a
                // host memory backend — a valid P2P DMA target BAR.
                let mem_id = format!("ivshmem_mem{i}");
                let size_bytes = cfg.size;
                cmd.arg("-object")
                    .arg(format!("memory-backend-ram,id={mem_id},size={size_bytes}"));
                cmd.arg("-device")
                    .arg(format!("ivshmem-plain,memdev={mem_id},bus={rp_id}"));
            }
        }
    }

    Ok(cmd)
}
