// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! QEMU backend for Petri. Currently only supports Aarch64 CPU emulation on
//! Linux x64 and ARM64 hosts, but could be modified to support more qemu-system-arch
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
use crate::TestChild;
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
    machine: Option<String>,
    cpu: Option<String>,
    smp: Option<String>,
    memory: Option<String>,
    command_line: String,
    firmware_vars: Option<Arc<TempPath>>,
}

impl QemuPetriBackend {
    /// Use a caller-supplied QEMU executable.
    pub fn from_binary(path: PathBuf) -> Self {
        Self {
            qemu_path: ResolvedArtifact::from_local_path(path),
        }
    }
}

/// Resources needed at runtime for a QEMU Petri VM
pub struct QemuPetriRuntime {
    driver: DefaultDriver,
    qemu_process: Arc<Mutex<TestChild>>,
    host_pipette_port: u16,
    log_tasks: Vec<Task<anyhow::Result<()>>>,
    output_dir: PathBuf,
    // QEMU opens disks after spawn; keep temporary media alive until teardown.
    _boot_disks: Vec<crate::PcieVirtioBlkDrive>,
    _firmware_vars: Option<Arc<TempPath>>,
}

#[async_trait]
impl PetriVmmBackend for QemuPetriBackend {
    type VmmConfig = QemuPetriConfig;
    type VmRuntime = QemuPetriRuntime;
    const SUPPORTS_VMBUS: bool = false;
    const SUPPORTS_CPU_EMULATION: bool = true;
    const SUPPORTS_FILE_SHARING: bool = true;

    fn check_compat(_firmware: &Firmware, _arch: MachineArch) -> bool {
        // Registered QEMU tests resolve an x64 executable; Incubator supplies its own.
        MachineArch::host() == MachineArch::X86_64 && cfg!(target_os = "linux")
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
        let mut script = String::from(
            "#!/bin/sh\n\
            ip link set eth0 up\n\
            ip addr add 10.0.2.15/24 dev eth0\n\
            ip route add default via 10.0.2.2\n\
            echo 'nameserver 10.0.2.3' > /etc/resolv.conf\n",
        );

        script.push_str(&format!(
            "exec '/{}' --transport tcp\n",
            pipette_path.replace('\'', "'\\''")
        ));

        Some(script)
    }

    fn new(resolver: &ArtifactResolver<'_>, arch: MachineArch) -> Self {
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
        _properties: &PetriVmProperties,
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
            prebuilt_initrd.as_ref(),
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
        let qemu_process = TestChild::new(qemu_process);

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
                _boot_disks: config.pcie_virtio_blk_drives,
                _firmware_vars: qemu_config.firmware_vars,
            },
            config
                .firmware
                .into_runtime_config(config.vmbus_storage_controllers),
        ))
    }
}

/// Copy a cached firmware template into disposable writable flash.
pub fn copy_firmware_vars(template: &Path) -> anyhow::Result<Arc<TempPath>> {
    let vars = tempfile::Builder::new()
        .prefix("petri-flash1-")
        .tempfile()
        .context("failed to create disposable firmware flash")?
        .into_temp_path();
    fs_err::copy(template, &vars).context("failed to copy writable firmware flash template")?;
    Ok(Arc::new(vars))
}

impl QemuPetriConfig {
    /// Use paired pflash images instead of `-bios`, retaining the caller's
    /// disposable writable flash until the VM stops.
    pub fn with_firmware_vars(mut self, path: Option<Arc<TempPath>>) -> Self {
        self.firmware_vars = path;
        self
    }

    /// Override the emulated machine, CPU, topology, and kernel command line.
    pub fn with_platform(
        mut self,
        machine: String,
        cpu: String,
        smp: String,
        memory: String,
        command_line: String,
    ) -> Self {
        self.machine = Some(machine);
        self.cpu = Some(cpu);
        self.smp = Some(smp);
        self.memory = Some(memory);
        self.command_line = command_line;
        self
    }

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
    prebuilt_initrd: Option<&PetriInitrd>,
) -> anyhow::Result<Command> {
    let PetriVmConfig {
        name: _,
        arch,
        host_log_levels,
        firmware,
        hibernation_enabled,
        ipmi_enabled,
        memory:
            crate::MemoryConfig {
                startup_bytes,
                dynamic_memory_range,
                numa_mem_sizes,
                private_memory,
                transparent_hugepages: _, // ignored
            },
        proc_topology:
            crate::ProcessorTopology {
                vp_count,
                enable_smt,
                vps_per_socket,
                apic_mode,
            },
        nested_virt_enabled: _, // this is always enabled currently
        vmgs,
        tpm,
        vmbus_storage_controllers,
        pcie_nvme_drives,
        pcie_virtio_blk_drives,
        physical_nvme_devices,
        guest_shares,
    } = config;

    let QemuPetriConfig {
        share_9p,
        devices,
        machine,
        cpu,
        smp,
        memory,
        command_line,
        firmware_vars,
    } = qemu_config;

    // make sure we aren't silently ignoring any configurations
    assert!(matches!(arch, MachineArch::Aarch64));
    assert!(matches!(
        host_log_levels,
        None | Some(crate::OpenvmmLogConfig::TestDefault)
    ));
    assert!(!*hibernation_enabled);
    assert!(!*ipmi_enabled);
    assert!(dynamic_memory_range.is_none());
    assert!(numa_mem_sizes.is_none());
    assert!(private_memory.is_none());
    assert!(enable_smt.is_none());
    assert!(vps_per_socket.is_none());
    assert!(apic_mode.is_none());
    assert!(matches!(vmgs, crate::PetriVmgsResource::Ephemeral));
    assert!(tpm.is_none());
    assert!(vmbus_storage_controllers.is_empty());
    assert!(pcie_nvme_drives.is_empty());
    assert!(physical_nvme_devices.is_empty());

    let mut cmd = Command::new(binary);

    cmd.arg("-accel").arg("tcg");
    cmd.arg("-machine").arg(
        machine
            .as_deref()
            .unwrap_or("virt,virtualization=on,iommu=smmuv3,gic-version=3"),
    );
    cmd.arg("-cpu").arg(cpu.as_deref().unwrap_or("max"));
    // TODO: more complex memory topologies
    cmd.arg("-m").arg(
        memory
            .clone()
            .unwrap_or_else(|| (startup_bytes / (1024 * 1024)).to_string()),
    );
    // TODO: more complex CPU topologies
    cmd.arg("-smp")
        .arg(smp.clone().unwrap_or_else(|| vp_count.to_string()));
    cmd.arg("-nographic");
    let is_sbsa = machine.as_deref().and_then(|m| m.split(',').next()) == Some("sbsa-ref");
    if is_sbsa {
        // SBSA always creates this display; its x86 option ROM is not needed.
        cmd.arg("-global").arg("bochs-display.romfile=");
    }

    match firmware {
        Firmware::LinuxDirect { kernel, .. } => {
            anyhow::ensure!(
                firmware_vars.is_none(),
                "QEMU linux-direct boot does not support firmware flash"
            );
            anyhow::ensure!(
                pcie_virtio_blk_drives.is_empty(),
                "QEMU linux-direct boot does not support PCIe boot drives"
            );
            let PetriInitrd { path, rdinit_param } =
                prebuilt_initrd.context("QEMU linux-direct boot requires a prebuilt initrd")?;
            cmd.arg("-kernel").arg(kernel);
            cmd.arg("-initrd").arg(path.as_ref());
            cmd.arg("-append")
                .arg(format!("{command_line} rdinit={rdinit_param}").trim());
        }
        Firmware::Uefi {
            uefi_firmware,
            guest: crate::UefiGuest::Mshv(_),
            ..
        } => {
            anyhow::ensure!(
                command_line.trim().is_empty(),
                "QEMU UEFI boot takes kernel options from the mshv boot recipe"
            );
            let [boot_disk] = pcie_virtio_blk_drives.as_slice() else {
                anyhow::bail!("QEMU mshv boot requires exactly one PCIe virtio boot disk");
            };
            anyhow::ensure!(
                boot_disk.port_name == "s0rc0rp0" && !boot_disk.drive.is_dvd,
                "QEMU mshv boot requires a disk at the default PCIe boot port"
            );
            let Some(crate::Disk::Temporary(path)) = &boot_disk.drive.disk else {
                anyhow::bail!("QEMU mshv boot requires a temporary raw boot disk");
            };
            if let Some(vars) = firmware_vars {
                for (unit, path, read_only) in [
                    (0, uefi_firmware.get(), "on"),
                    (1, vars.as_ref().as_ref(), "off"),
                ] {
                    let path = path
                        .to_str()
                        .context("firmware flash path is not UTF-8")?
                        .replace(',', ",,");
                    cmd.arg("-drive").arg(format!(
                        "if=pflash,unit={unit},format=raw,readonly={read_only},file={path}"
                    ));
                }
            } else {
                cmd.arg("-bios").arg(uefi_firmware);
            }
            cmd.arg("-blockdev").arg(
                serde_json::json!({
                    "driver": "raw",
                    "node-name": "mshv-boot",
                    "file": {
                        "driver": "file",
                        "filename": path.to_str().context("mshv boot disk path is not UTF-8")?,
                    },
                })
                .to_string(),
            );
            if is_sbsa {
                cmd.arg("-device")
                    .arg("ide-hd,drive=mshv-boot,bus=ide.0,bootindex=0");
            } else {
                cmd.arg("-device")
                    .arg("pcie-root-port,id=mshv-boot-port,slot=0");
                cmd.arg("-device").arg(
                    "virtio-blk-pci,drive=mshv-boot,bus=mshv-boot-port,bootindex=0,disable-legacy=on,romfile=",
                );
            }
        }
        _ => anyhow::bail!("qemu backend supports only linux-direct or mshv UEFI boot"),
    };

    cmd.arg("-no-reboot");

    // 9p: share the host directory into the guest
    let mut guest_shares = guest_shares
        .iter()
        .map(|(k, v)| (k.as_str(), v.host_path.as_path()))
        .collect::<Vec<_>>();
    if let Some(custom_share) = share_9p {
        guest_shares.push((SHARE_9P_MOUNT_TAG, custom_share.as_path()));
    }
    for (i, (tag, path)) in guest_shares.iter().enumerate() {
        cmd.arg("-fsdev").arg(format!(
            "local,id=fsdev{i},path={},security_model=none",
            path.to_str().context("share path not utf8")?
        ));
        cmd.arg("-device")
            .arg(format!("virtio-9p-pci,fsdev=fsdev{i},mount_tag={tag}"));
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
    for (i, device) in devices.iter().enumerate() {
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

#[cfg(test)]
mod tests {
    use super::*;
    use test_with_tracing::test;

    fn linux_direct_config() -> PetriVmConfig {
        PetriVmConfig {
            name: "test".into(),
            arch: MachineArch::Aarch64,
            host_log_levels: None,
            firmware: Firmware::LinuxDirect {
                kernel: ResolvedArtifact::from_local_path("kernel with spaces".into()),
                initrd: ResolvedArtifact::from_local_path("base-initrd".into()),
            },
            hibernation_enabled: false,
            ipmi_enabled: false,
            memory: Default::default(),
            proc_topology: Default::default(),
            nested_virt_enabled: true,
            vmgs: crate::PetriVmgsResource::Ephemeral,
            tpm: None,
            vmbus_storage_controllers: Default::default(),
            pcie_nvme_drives: Vec::new(),
            pcie_virtio_blk_drives: Vec::new(),
            physical_nvme_devices: Default::default(),
            guest_shares: Default::default(),
        }
    }

    #[test]
    fn preserves_standalone_platform_options() -> anyhow::Result<()> {
        let config = linux_direct_config();
        let platform = QemuPetriConfig::default().with_platform(
            "virt,gic-version=3".into(),
            "max,sve=off".into(),
            "4,sockets=2,cores=2".into(),
            "4096".into(),
            "console=ttyAMA0 debug".into(),
        );
        let initrd = PetriInitrd {
            path: crate::TempOrPersistentPath::Persistent("patched initrd".into()),
            rdinit_param: "/tcg-init.sh".into(),
        };
        let cmd = build_qemu_command(Path::new("qemu"), &config, &platform, 12345, Some(&initrd))?;
        let args = cmd
            .get_args()
            .map(|s| s.to_str().unwrap())
            .collect::<Vec<_>>();
        for pair in [
            ["-accel", "tcg"],
            ["-machine", "virt,gic-version=3"],
            ["-cpu", "max,sve=off"],
            ["-m", "4096"],
            ["-smp", "4,sockets=2,cores=2"],
            ["-kernel", "kernel with spaces"],
            ["-initrd", "patched initrd"],
            ["-append", "console=ttyAMA0 debug rdinit=/tcg-init.sh"],
        ] {
            assert!(
                args.array_windows::<2>().any(|args| args == &pair),
                "{pair:?}"
            );
        }
        let default_cmd = build_qemu_command(
            Path::new("qemu"),
            &config,
            &QemuPetriConfig::default(),
            12345,
            Some(&initrd),
        )?;
        assert!(default_cmd.get_args().any(|a| a == "rdinit=/tcg-init.sh"));
        Ok(())
    }

    #[test]
    fn mshv_uefi_uses_firmware_and_owned_raw_media() -> anyhow::Result<()> {
        let mut config = linux_direct_config();
        let unused = || ResolvedArtifact::from_local_path("unused".into());
        config.firmware = Firmware::Uefi {
            uefi_firmware: ResolvedArtifact::from_local_path("firmware with spaces.fd".into()),
            guest: crate::UefiGuest::Mshv(crate::mshv_boot::MshvBoot {
                host_boot: unused(),
                kernel: unused(),
                initrd: crate::mshv_boot::MshvInitrd::Base(unused()),
                boot_artifacts: unused(),
                kernel_cmdline: "console=ttyAMA0,115200 earlycon panic=-1".into(),
                hypervisor_cmdline: "MSHV_ROOT=\\Windows MSHV_ENABLE=TRUE MSHV_SCHEDULER_TYPE=ROOT MSHV_FORCE_NESTED=TRUE".into(),
            }),
            uefi_config: Default::default(),
        };
        let platform = QemuPetriConfig::default();
        let build = |config: &PetriVmConfig, platform: &QemuPetriConfig| {
            build_qemu_command(Path::new("qemu"), config, platform, 12345, None)
        };
        let error = build(&config, &platform).unwrap_err();
        assert!(
            error
                .to_string()
                .contains("exactly one PCIe virtio boot disk")
        );
        let disk = tempfile::Builder::new()
            .prefix("mshv boot,")
            .tempfile()?
            .into_temp_path();
        config
            .pcie_virtio_blk_drives
            .push(crate::PcieVirtioBlkDrive {
                port_name: "s0rc0rp0".into(),
                drive: Drive::new(Some(crate::Disk::Temporary(Arc::new(disk))), false),
            });
        let command = build(&config, &platform)?;
        let args = command
            .get_args()
            .map(|a| a.to_str().unwrap())
            .collect::<Vec<_>>();
        assert!(
            args.windows(2)
                .any(|a| a == ["-bios", "firmware with spaces.fd"])
        );
        assert!(
            !args
                .iter()
                .any(|a| ["-kernel", "-initrd", "-append"].contains(a))
        );
        assert!(
            args.iter()
                .any(|a| a.starts_with("virtio-blk-pci,drive=mshv-boot,")
                    && a.contains("bootindex=0"))
        );
        let blockdev = args.windows(2).find(|a| a[0] == "-blockdev").unwrap()[1];
        let blockdev: serde_json::Value = serde_json::from_str(blockdev)?;
        assert_eq!(blockdev["driver"], "raw");
        assert!(
            blockdev["file"]["filename"]
                .as_str()
                .unwrap()
                .contains("mshv boot,")
        );
        let vars = Arc::new(
            tempfile::Builder::new()
                .prefix("firmware vars,")
                .tempfile()?
                .into_temp_path(),
        );
        let pflash = QemuPetriConfig::default()
            .with_platform(
                "sbsa-ref".into(),
                "max".into(),
                "1".into(),
                "8G".into(),
                String::new(),
            )
            .with_firmware_vars(Some(vars.clone()));
        let command = build(&config, &pflash)?;
        let args = command
            .get_args()
            .map(|a| a.to_str().unwrap())
            .collect::<Vec<_>>();
        assert!(!args.contains(&"-bios"));
        assert!(
            args.windows(2)
                .any(|pair| pair == ["-global", "bochs-display.romfile="])
        );
        assert!(
            args.windows(2)
                .any(|pair| pair == ["-device", "ide-hd,drive=mshv-boot,bus=ide.0,bootindex=0"])
        );
        assert!(!args.iter().any(|arg| arg.contains("mshv-boot-port")
            || arg.starts_with("virtio-blk-pci,drive=mshv-boot,")));
        let drives = args
            .windows(2)
            .filter(|pair| pair[0] == "-drive")
            .map(|pair| pair[1])
            .collect::<Vec<_>>();
        assert_eq!(
            drives,
            [
                "if=pflash,unit=0,format=raw,readonly=on,file=firmware with spaces.fd",
                &format!(
                    "if=pflash,unit=1,format=raw,readonly=off,file={}",
                    vars.to_str().unwrap().replace(',', ",,")
                ),
            ]
        );
        assert!(
            build(&linux_direct_config(), &pflash)
                .unwrap_err()
                .to_string()
                .contains("does not support firmware flash")
        );
        let invalid = QemuPetriConfig {
            command_line: "ignored options".into(),
            ..Default::default()
        };
        assert!(
            build(&config, &invalid)
                .unwrap_err()
                .to_string()
                .contains("kernel options")
        );
        config.pcie_virtio_blk_drives[0].drive.disk = Some(crate::Disk::Memory(4096));
        assert!(
            build(&config, &platform)
                .unwrap_err()
                .to_string()
                .contains("temporary raw boot disk")
        );
        Ok(())
    }
}
