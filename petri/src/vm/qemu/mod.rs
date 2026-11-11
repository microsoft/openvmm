// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! QEMU backend for Petri: Linux direct boot with Aarch64 CPU emulation or
//! native KVM acceleration.

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
use devices::VirtioBlkBacking;
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
    kvm: bool,
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
    pid: i32,
    command: Vec<String>,
}

#[async_trait]
impl PetriVmmBackend for QemuPetriBackend {
    type VmmConfig = QemuPetriConfig;
    type VmRuntime = QemuPetriRuntime;
    const SUPPORTS_VMBUS: bool = false;
    const SUPPORTS_CPU_EMULATION: bool = true;
    const SUPPORTS_FILE_SHARING: bool = true;

    fn check_compat(firmware: &Firmware, arch: MachineArch) -> bool {
        cfg!(target_os = "linux")
            && matches!(firmware, Firmware::LinuxDirect { .. })
            && (arch == MachineArch::host()
                || MachineArch::X86_64 == MachineArch::host() && arch == MachineArch::Aarch64)
    }

    fn check_default_compat(firmware: &Firmware, arch: MachineArch) -> bool {
        MachineArch::host() == MachineArch::X86_64
            && arch == MachineArch::Aarch64
            && Self::check_compat(firmware, arch)
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
            prebuilt_initrd
                .as_ref()
                .context("QEMU requires a prebuilt initrd")?,
        )?;
        cmd.stdin(std::process::Stdio::null());
        cmd.stdout(std::process::Stdio::piped());
        cmd.stderr(std::process::Stdio::piped());

        tracing::info!(?cmd, "launching qemu");
        let command = std::iter::once(cmd.get_program())
            .chain(cmd.get_args())
            .map(|arg| {
                arg.to_str()
                    .map(str::to_owned)
                    .context("QEMU command is not UTF-8")
            })
            .collect::<anyhow::Result<Vec<_>>>()?;
        let mut qemu_process = cmd.spawn().context("failed to launch QEMU")?;
        let pid = qemu_process
            .id()
            .try_into()
            .context("QEMU PID out of range")?;
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
                pid,
                command,
            },
            config
                .firmware
                .into_runtime_config(config.vmbus_storage_controllers),
        ))
    }
}

impl QemuPetriConfig {
    /// Require native KVM acceleration. Failure to use KVM is a launch error.
    pub fn with_kvm(mut self) -> Self {
        self.kvm = true;
        self
    }

    /// Set additional kernel command-line arguments.
    pub fn with_kernel_command_line(mut self, command_line: String) -> Self {
        self.command_line = command_line;
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
    /// Get the PID of the QEMU child process.
    pub fn pid(&self) -> i32 {
        self.pid
    }

    /// The executable and arguments used to launch this VM.
    pub fn command(&self) -> &[String] {
        &self.command
    }

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

/// Build the QEMU command line.
pub fn build_qemu_command(
    binary: &Path,
    config: &PetriVmConfig,
    qemu_config: &QemuPetriConfig,
    host_pipette_port: u16,
    prebuilt_initrd: &PetriInitrd,
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
        kvm,
    } = qemu_config;

    anyhow::ensure!(
        !kvm || cfg!(target_os = "linux") && *arch == MachineArch::host(),
        "QEMU KVM requires a native guest architecture on Linux"
    );
    // make sure we aren't silently ignoring any configurations
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
    assert!(pcie_virtio_blk_drives.is_empty());
    assert!(physical_nvme_devices.is_empty());

    let mut cmd = Command::new(binary);
    cmd.arg("-name").arg("debug-threads=on");

    let default_machine = match arch {
        MachineArch::X86_64 => "q35",
        MachineArch::Aarch64 if *kvm => "virt,gic-version=host",
        MachineArch::Aarch64 => "virt,virtualization=on,iommu=smmuv3,gic-version=3",
    };
    if *kvm {
        cmd.arg("-accel").arg("kvm");
    }
    cmd.arg("-machine")
        .arg(machine.as_deref().unwrap_or(default_machine));
    cmd.arg("-cpu")
        .arg(cpu.as_deref().unwrap_or(if *kvm { "host" } else { "max" }));
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

    let PetriInitrd { path, rdinit_param } = prebuilt_initrd;

    match firmware {
        Firmware::LinuxDirect { kernel, .. } => {
            cmd.arg("-kernel").arg(kernel);
            cmd.arg("-initrd").arg(path.as_ref());
        }
        _ => anyhow::bail!("qemu backend only supports linux direct"),
    };

    cmd.arg("-append")
        .arg(format!("{command_line} rdinit={rdinit_param}").trim());
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
        "user,id=net0,hostfwd=tcp:127.0.0.1:{host_pipette_port}-:{guest_port}",
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
                let backing = match &cfg.backing {
                    VirtioBlkBacking::Null(size) => serde_json::json!({
                        "driver": "null-co", "node-name": node_name, "size": size
                    }),
                    VirtioBlkBacking::File {
                        path,
                        read_only,
                        direct,
                    } => {
                        let filename = path.to_str().context("QEMU disk path is not UTF-8")?;
                        serde_json::json!({
                            "driver": "raw",
                            "node-name": node_name,
                            "read-only": read_only,
                            "file": {
                                "driver": "file",
                                "filename": filename,
                                "aio": "io_uring",
                                "cache": { "direct": direct, "no-flush": false }
                            }
                        })
                    }
                };
                cmd.arg("-blockdev").arg(serde_json::to_string(&backing)?);
                let options = &cfg.options;
                let mut dev = format!("virtio-blk-pci,drive={node_name},bus={rp_id},romfile=");
                for (property, value) in [
                    ("packed", options.packed),
                    ("request-merging", options.request_merging),
                    ("disable-legacy", options.disable_legacy),
                    ("iommu_platform", options.iommu_platform),
                ] {
                    if let Some(value) = value {
                        dev.push_str(&format!(",{property}={}", if value { "on" } else { "off" }));
                    }
                }
                for (property, value) in [
                    ("queue-size", options.queue_size.map(u32::from)),
                    ("num-queues", options.num_queues.map(u32::from)),
                    ("logical_block_size", options.logical_block_size),
                    ("physical_block_size", options.physical_block_size),
                ] {
                    if let Some(value) = value {
                        dev.push_str(&format!(",{property}={value}"));
                    }
                }
                cmd.arg("-device").arg(dev);
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
    use devices::VirtioBlkDeviceConfig;
    use devices::VirtioBlkOptions;
    use test_with_tracing::test;

    fn linux_direct_config(arch: MachineArch) -> PetriVmConfig {
        PetriVmConfig {
            name: "test".into(),
            arch,
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

    fn initrd() -> PetriInitrd {
        PetriInitrd {
            path: crate::TempOrPersistentPath::Persistent("patched initrd".into()),
            rdinit_param: "/tcg-init.sh".into(),
        }
    }

    fn raw_disk(path: &str, read_only: bool, direct: bool) -> VirtioBlkDeviceConfig {
        VirtioBlkDeviceConfig {
            name: "test-disk".into(),
            backing: VirtioBlkBacking::File {
                path: path.into(),
                read_only,
                direct,
            },
            vfio: false,
            options: VirtioBlkOptions {
                packed: Some(false),
                queue_size: Some(128),
                num_queues: Some(2),
                request_merging: Some(true),
                disable_legacy: Some(false),
                iommu_platform: Some(false),
                logical_block_size: Some(1024),
                physical_block_size: Some(2048),
            },
        }
    }

    #[test]
    fn preserves_standalone_platform_options() -> anyhow::Result<()> {
        let config = linux_direct_config(MachineArch::Aarch64);
        let platform = QemuPetriConfig::default().with_platform(
            "virt,gic-version=3".into(),
            "max,sve=off".into(),
            "4,sockets=2,cores=2".into(),
            "4096".into(),
            "console=ttyAMA0 debug".into(),
        );
        let initrd = initrd();
        let cmd = build_qemu_command(Path::new("qemu"), &config, &platform, 12345, &initrd)?;
        let args = cmd
            .get_args()
            .map(|s| s.to_str().unwrap())
            .collect::<Vec<_>>();
        for pair in [
            ["-name", "debug-threads=on"],
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
            &initrd,
        )?;
        assert!(default_cmd.get_args().any(|a| a == "rdinit=/tcg-init.sh"));
        assert!(!default_cmd.get_args().any(|a| a == "-accel"));
        Ok(())
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn native_kvm_raw_disks_preserve_paths_and_cache_modes() -> anyhow::Result<()> {
        let config = linux_direct_config(MachineArch::host());
        for direct in [false, true] {
            let platform = QemuPetriConfig::default().with_kvm().with_devices([
                DeviceConfig::VirtioBlk(raw_disk("tools image.erofs", true, false)),
                DeviceConfig::VirtioBlk(raw_disk("data,with=options.raw", false, direct)),
            ]);
            let cmd = build_qemu_command(Path::new("qemu"), &config, &platform, 12345, &initrd())?;
            let args = cmd
                .get_args()
                .map(|s| s.to_str().unwrap())
                .collect::<Vec<_>>();
            assert!(args.windows(2).any(|p| p == ["-accel", "kvm"]));
            assert!(args.windows(2).any(|p| p == ["-cpu", "host"]));
            assert!(
                args.windows(2).any(|p| {
                    p[0] == "-netdev" && p[1].contains("hostfwd=tcp:127.0.0.1:12345-:")
                })
            );
            let disks = args
                .windows(2)
                .filter(|p| p[0] == "-blockdev")
                .map(|p| serde_json::from_str::<serde_json::Value>(p[1]))
                .collect::<Result<Vec<_>, _>>()?;
            assert_eq!(disks.len(), 2);
            assert_eq!(disks[0]["read-only"], true);
            assert_eq!(disks[0]["file"]["filename"], "tools image.erofs");
            assert_eq!(disks[1]["driver"], "raw");
            assert_eq!(disks[1]["read-only"], false);
            assert_eq!(disks[1]["file"]["filename"], "data,with=options.raw");
            assert_eq!(disks[1]["file"]["aio"], "io_uring");
            assert_eq!(disks[1]["file"]["cache"]["direct"], direct);
            assert_eq!(disks[1]["file"]["cache"]["no-flush"], false);
        }
        Ok(())
    }

    #[test]
    fn raw_disk_device_settings_are_caller_selected() -> anyhow::Result<()> {
        for (disk, expected) in [
            (
                raw_disk("data.raw", false, false),
                "virtio-blk-pci,drive=disk0,bus=hosting_rp0,romfile=,packed=off,request-merging=on,disable-legacy=off,iommu_platform=off,queue-size=128,num-queues=2,logical_block_size=1024,physical_block_size=2048",
            ),
            (
                VirtioBlkDeviceConfig {
                    options: VirtioBlkOptions {
                        packed: Some(true),
                        queue_size: Some(512),
                        num_queues: Some(4),
                        request_merging: Some(false),
                        disable_legacy: Some(true),
                        iommu_platform: Some(true),
                        logical_block_size: Some(4096),
                        physical_block_size: Some(4096),
                    },
                    ..raw_disk("data.raw", false, false)
                },
                "virtio-blk-pci,drive=disk0,bus=hosting_rp0,romfile=,packed=on,request-merging=off,disable-legacy=on,iommu_platform=on,queue-size=512,num-queues=4,logical_block_size=4096,physical_block_size=4096",
            ),
        ] {
            let cmd = build_qemu_command(
                Path::new("qemu"),
                &linux_direct_config(MachineArch::Aarch64),
                &QemuPetriConfig::default().with_devices([DeviceConfig::VirtioBlk(disk)]),
                12345,
                &initrd(),
            )?;
            let args = cmd.get_args().collect::<Vec<_>>();
            assert!(
                args.windows(2)
                    .any(|p| p[0] == "-device" && p[1] == expected)
            );
        }
        Ok(())
    }

    #[test]
    fn null_and_file_devices_share_root_port_allocation() -> anyhow::Result<()> {
        let platform = QemuPetriConfig::default().with_devices([
            DeviceConfig::VirtioBlk(VirtioBlkDeviceConfig {
                name: "nested-disk".into(),
                backing: VirtioBlkBacking::Null(64 * 1024 * 1024),
                vfio: true,
                options: VirtioBlkOptions {
                    iommu_platform: Some(true),
                    disable_legacy: Some(true),
                    ..Default::default()
                },
            }),
            DeviceConfig::VirtioBlk(raw_disk("data.raw", false, false)),
        ]);
        let cmd = build_qemu_command(
            Path::new("qemu"),
            &linux_direct_config(MachineArch::Aarch64),
            &platform,
            12345,
            &initrd(),
        )?;
        let args = cmd
            .get_args()
            .map(|arg| arg.to_str().unwrap())
            .collect::<Vec<_>>();
        let blocks = args
            .windows(2)
            .filter(|p| p[0] == "-blockdev")
            .map(|p| serde_json::from_str::<serde_json::Value>(p[1]))
            .collect::<Result<Vec<_>, _>>()?;
        assert_eq!(blocks[0]["driver"], "null-co");
        assert_eq!(blocks[0]["size"], 64 * 1024 * 1024);
        assert_eq!(blocks[1]["driver"], "raw");
        assert_eq!(blocks[1]["file"]["filename"], "data.raw");
        for expected in [
            "pcie-root-port,id=hosting_rp0,addr=0x10,slot=1",
            "pcie-root-port,id=hosting_rp1,addr=0x11,slot=2",
            "virtio-blk-pci,drive=disk0,bus=hosting_rp0,romfile=,disable-legacy=on,iommu_platform=on",
        ] {
            assert!(
                args.windows(2)
                    .any(|p| p[0] == "-device" && p[1] == expected)
            );
        }
        Ok(())
    }

    #[test]
    fn default_backend_still_resolves_the_emulator_artifact() {
        let mut requirements = crate::TestArtifactRequirements::new();
        let resolver = ArtifactResolver::collector(&mut requirements);
        let _backend = QemuPetriBackend::new(&resolver, MachineArch::Aarch64);
        assert!(requirements.required_artifacts().any(|id| {
            id == petri_artifacts_vmm_test::artifacts::QEMU_SYSTEM_AARCH64_LINUX_X64.erase()
        }));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn native_guests_require_an_explicit_qemu_backend() {
        let arch = MachineArch::host();
        let mut requirements = crate::TestArtifactRequirements::new();
        let resolver = ArtifactResolver::collector(&mut requirements);
        let default = crate::PetriVmArtifacts::<QemuPetriBackend>::new(
            &resolver,
            linux_direct_config(arch).firmware,
            arch,
            false,
        );
        assert!(default.is_none());
        let explicit = crate::PetriVmArtifacts::new_with_backend(
            &resolver,
            linux_direct_config(arch).firmware,
            arch,
            false,
            QemuPetriBackend::from_binary("/installed/native-qemu".into()),
        )
        .expect("explicit native QEMU must remain supported");
        assert_eq!(
            explicit.backend.qemu_path.get(),
            Path::new("/installed/native-qemu")
        );
        assert!(requirements.required_artifacts().next().is_none());
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn default_emulator_artifacts_are_only_collected_on_x64_hosts() {
        let mut requirements = crate::TestArtifactRequirements::new();
        let artifacts = crate::PetriVmArtifacts::<QemuPetriBackend>::new(
            &ArtifactResolver::collector(&mut requirements),
            linux_direct_config(MachineArch::Aarch64).firmware,
            MachineArch::Aarch64,
            false,
        );
        if MachineArch::host() == MachineArch::X86_64 {
            assert!(artifacts.is_some());
            assert!(requirements.required_artifacts().any(|id| {
                id == petri_artifacts_vmm_test::artifacts::QEMU_SYSTEM_AARCH64_LINUX_X64.erase()
            }));
        } else {
            assert!(artifacts.is_none());
            assert!(requirements.required_artifacts().next().is_none());
        }
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn kvm_rejects_cross_architecture_guests() {
        let arch = match MachineArch::host() {
            MachineArch::X86_64 => MachineArch::Aarch64,
            MachineArch::Aarch64 => MachineArch::X86_64,
        };
        assert!(
            build_qemu_command(
                Path::new("qemu"),
                &linux_direct_config(arch),
                &QemuPetriConfig::default().with_kvm(),
                12345,
                &initrd(),
            )
            .is_err()
        );
    }
}
