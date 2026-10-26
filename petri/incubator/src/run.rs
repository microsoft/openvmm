// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Run a command using Petri's VM lifecycle and Pipette connection.

use crate::profile::IncubatorBackend;
use crate::profile::IncubatorBoot;
use crate::profile::IncubatorProfile;
use crate::profile::parse_size;
use crate::qemu;
use anyhow::Context;
use futures_concurrency::future::Race;
use petri::Firmware;
use petri::PetriLogSource;
use petri::PetriVmArtifacts;
use petri::PetriVmBuilder;
use petri::PetriVmmBackend;
use petri::ResolvedArtifact;
use petri_artifacts_common::tags::MachineArch;
use std::collections::BTreeMap;
use std::path::PathBuf;
use std::time::Duration;
use std::time::Instant;

/// Configuration for one disposable VM and guest command.
pub struct IncubatorConfig {
    /// Hardware and boot profile.
    pub profile: IncubatorProfile,
    /// Root VM kernel, not the kernel of any nested test.
    pub kernel: PathBuf,
    /// Root VM initrd.
    pub initrd: PathBuf,
    /// Host directory mounted at `/share`.
    pub share_dir: PathBuf,
    /// Host directory for persistent logs.
    pub output_dir: PathBuf,
    /// Guest Pipette path under `/share`.
    pub guest_pipette_path: String,
    /// Program and arguments to execute.
    pub guest_command: Vec<String>,
    /// Guest environment overrides.
    pub guest_env: BTreeMap<String, String>,
    /// Guest working directory.
    pub guest_current_dir: Option<String>,
    /// Boot timeout for Linux direct; boot and command timeout for mshv.
    pub timeout: Duration,
    /// Override the QEMU executable.
    pub qemu_binary_override: Option<PathBuf>,
    /// Allocate a guest PTY and forward terminal control characters.
    pub allocate_pty: bool,
}

/// Guest command result.
pub struct IncubatorOutput {
    /// Guest exit code, including `128 + signal` for signal termination.
    pub exit_code: Option<i32>,
    /// Total wall time.
    pub elapsed: Duration,
}

/// Cold-boot a VM, execute the command through Pipette, and tear down the VM.
pub fn run_in_incubator(config: IncubatorConfig) -> anyhow::Result<IncubatorOutput> {
    let start = Instant::now();
    let signals = TerminationSignals::new()?;
    anyhow::ensure!(!config.guest_command.is_empty(), "empty guest command");
    config.profile.validate()?;
    if matches!(config.profile.incubator, IncubatorBackend::Openvmm(_)) {
        anyhow::ensure!(
            config.qemu_binary_override.is_none(),
            "--qemu-binary is not valid for the openvmm launcher"
        );
        anyhow::ensure!(
            cfg!(target_os = "linux") && MachineArch::host() == MachineArch::X86_64,
            "OpenVMM Incubator requires an x86-64 Linux host"
        );
    }
    fs_err::create_dir_all(&config.output_dir)?;
    let log_dir = tempfile::Builder::new()
        .prefix("incubator-")
        .tempdir_in(&config.output_dir)?
        .keep();
    let logs = PetriLogSource::file_only(&log_dir)?;
    tracing::info!(path = %log_dir.display(), "incubator logs");
    let initrd = crate::bootstrap::prepare_initrd(&config)?;
    let firmware = match &config.profile.boot {
        IncubatorBoot::LinuxDirect => Firmware::LinuxDirect {
            kernel: ResolvedArtifact::from_local_path(config.kernel.clone()),
            initrd: ResolvedArtifact::from_local_path(config.initrd.clone()),
        },
        IncubatorBoot::Mshv(boot) => Firmware::Uefi {
            uefi_firmware: ResolvedArtifact::from_local_path(boot.firmware.clone()),
            guest: petri::UefiGuest::Mshv(petri::mshv_boot::MshvBoot {
                kernel: ResolvedArtifact::from_local_path(config.kernel.clone()),
                initrd: initrd.clone(),
                boot_artifacts: ResolvedArtifact::from_local_path(boot.boot_artifacts.clone()),
            }),
            uefi_config: petri::UefiConfig {
                default_boot_always_attempt: true,
                ..Default::default()
            },
        },
    };
    let bound_command = matches!(config.profile.boot, IncubatorBoot::Mshv(_));
    let code = pal_async::DefaultPool::run_with(async |driver| match &config.profile.incubator {
        IncubatorBackend::Openvmm(profile) => {
            let artifacts = PetriVmArtifacts {
                backend: petri::openvmm::OpenVmmPetriBackend::from_binary(profile.binary.clone()),
                firmware,
                arch: MachineArch::X86_64,
                agent_image: None,
                openhcl_agent_image: None,
                pipette_binary: None,
                nested_test: None,
            };
            let mut builder = PetriVmBuilder::minimal("incubator", &logs, artifacts, &driver)?
                .with_memory(petri::MemoryConfig {
                    startup_bytes: parse_size(&profile.memory)?,
                    ..Default::default()
                })
                .with_processor_topology(petri::ProcessorTopology {
                    vp_count: profile.processors,
                    ..Default::default()
                })
                .with_external_pipette()
                .with_shared_directory("hostshare", &config.share_dir, crate::GUEST_SHARE_ROOT);
            if profile.nested_virt {
                builder = builder.with_nested_virt();
            }
            builder = match config.profile.boot {
                IncubatorBoot::Mshv(_) => builder.with_mshv_platform(),
                IncubatorBoot::LinuxDirect => builder
                    .with_prebuilt_initrd(initrd)
                    .with_no_vmbus()
                    .with_virtio_vsock(false)
                    .with_serial_output()
                    .modify_backend(|backend| {
                        backend
                            .with_pcie_root_topology_at(0, 1, 1, 1)
                            .with_tcp_pipette_nic("s0rc0rp0", petri::openvmm::NIC_MAC_ADDRESS)
                    }),
            };
            execute(builder, &driver, &config, &signals, bound_command).await
        }
        IncubatorBackend::QemuTcg(profile) => {
            let artifacts = PetriVmArtifacts {
                backend: petri::qemu::QemuPetriBackend::from_binary(
                    config
                        .qemu_binary_override
                        .clone()
                        .unwrap_or_else(|| profile.binary.clone().into()),
                ),
                firmware,
                arch: MachineArch::Aarch64,
                agent_image: None,
                openhcl_agent_image: None,
                pipette_binary: None,
                nested_test: None,
            };
            let devices = qemu::petri_devices(&config.profile.devices)?;
            let profile = profile.clone();
            let share = config.share_dir.clone();
            let builder = PetriVmBuilder::minimal("incubator", &logs, artifacts, &driver)?
                .with_external_pipette()
                .with_prebuilt_initrd(initrd)
                .modify_backend(move |backend| {
                    backend
                        .with_share_9p(share)
                        .with_devices(devices)
                        .with_platform(
                            profile.machine,
                            profile.cpu,
                            profile.smp,
                            profile.memory,
                            profile.cmdline,
                        )
                });
            execute(builder, &driver, &config, &signals, bound_command).await
        }
    })?;
    Ok(IncubatorOutput {
        exit_code: Some(code),
        elapsed: start.elapsed(),
    })
}

async fn execute<T: PetriVmmBackend>(
    builder: PetriVmBuilder<T>,
    driver: &pal_async::DefaultDriver,
    config: &IncubatorConfig,
    signals: &TerminationSignals,
    bound_command: bool,
) -> anyhow::Result<i32> {
    let start = Instant::now();
    signals.check()?;
    let boot = builder
        .with_watchdog_timeout(None)
        .without_screenshots()
        .run();
    let (mut vm, client) = (
        boot,
        timeout(driver, config.timeout, "boot"),
        signals.cancelled(driver),
    )
        .race()
        .await?;
    let session = async {
        let vfio_env = qemu::setup_vfio_devices(&client, &config.profile.devices).await?;
        let mut command = client.command(&config.guest_command[0]);
        command.args(&config.guest_command[1..]);
        for (key, value) in config.guest_env.iter().chain(vfio_env.iter()) {
            command.env(key, value);
        }
        command.current_dir(
            config
                .guest_current_dir
                .as_deref()
                .unwrap_or(crate::GUEST_SHARE_ROOT),
        );
        command.pty(config.allocate_pty);
        let guard = config.allocate_pty.then(RawModeGuard::enter).transpose()?;
        let status = command.spawn().await?.wait().await?;
        drop(guard);
        status
            .code()
            .or_else(|| status.signal().map(|signal| 128 + signal))
            .context("guest command returned no exit code or signal")
    };
    let result = (
        session,
        async {
            let halt = vm.wait_for_halt().await?;
            anyhow::bail!("incubator halted before command completed: {halt:?}")
        },
        async {
            if bound_command {
                timeout(
                    driver,
                    config.timeout.saturating_sub(start.elapsed()),
                    "command",
                )
                .await
            } else {
                std::future::pending().await
            }
        },
        signals.cancelled(driver),
    )
        .race()
        .await;
    let result = match result {
        Ok(code) => {
            let shutdown = async {
                client.power_off().await?;
                vm.wait_for_clean_shutdown().await?;
                Ok(code)
            };
            (
                shutdown,
                timeout(driver, Duration::from_secs(10), "shutdown"),
            )
                .race()
                .await
        }
        Err(error) => Err(error),
    };
    let teardown = (
        vm.teardown(),
        timeout(driver, Duration::from_secs(10), "teardown"),
    )
        .race()
        .await;
    if let Err(error) = &teardown {
        tracing::error!(%error, "incubator teardown failed");
    }
    let code = result?;
    teardown?;
    Ok(code)
}

struct TerminationSignals {
    #[cfg(unix)]
    signal: std::sync::Arc<std::sync::atomic::AtomicUsize>,
    #[cfg(unix)]
    registrations: Vec<signal_hook::SigId>,
}

impl TerminationSignals {
    fn check(&self) -> anyhow::Result<()> {
        #[cfg(unix)]
        {
            let signal = self.signal.load(std::sync::atomic::Ordering::Relaxed);
            anyhow::ensure!(signal == 0, "incubator interrupted by signal {signal}");
        }
        Ok(())
    }

    fn new() -> anyhow::Result<Self> {
        #[cfg(unix)]
        {
            let mut guard = Self {
                signal: Default::default(),
                registrations: Vec::new(),
            };
            for signal in [signal_hook::consts::SIGINT, signal_hook::consts::SIGTERM] {
                guard.registrations.push(signal_hook::flag::register_usize(
                    signal,
                    guard.signal.clone(),
                    signal as usize,
                )?);
            }
            Ok(guard)
        }
        #[cfg(not(unix))]
        Ok(Self {})
    }

    async fn cancelled<T>(&self, driver: &pal_async::DefaultDriver) -> anyhow::Result<T> {
        #[cfg(unix)]
        loop {
            self.check()?;
            pal_async::timer::PolledTimer::new(driver)
                .sleep(Duration::from_millis(100))
                .await;
        }
        #[cfg(not(unix))]
        {
            let _ = driver;
            std::future::pending().await
        }
    }
}

impl Drop for TerminationSignals {
    fn drop(&mut self) {
        #[cfg(unix)]
        for id in self.registrations.drain(..) {
            signal_hook::low_level::unregister(id);
        }
    }
}

async fn timeout<T>(
    driver: &pal_async::DefaultDriver,
    duration: Duration,
    phase: &str,
) -> anyhow::Result<T> {
    pal_async::timer::PolledTimer::new(driver)
        .sleep(duration)
        .await;
    anyhow::bail!("incubator {phase} timed out after {duration:?}")
}

struct RawModeGuard;

impl RawModeGuard {
    fn enter() -> anyhow::Result<Self> {
        crossterm::terminal::enable_raw_mode().context("failed to enable raw mode")?;
        Ok(Self)
    }
}

impl Drop for RawModeGuard {
    fn drop(&mut self) {
        if let Err(error) = crossterm::terminal::disable_raw_mode() {
            tracing::warn!(%error, "failed to restore terminal mode");
        }
    }
}
