// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Boot time performance test.
//!
//! Measures the time from VM launch to pipette agent readiness using
//! Linux direct boot (kernel + initrd, no UEFI firmware). This isolates
//! the VMM's launch overhead from firmware initialization time.
//! Uses cold mode: a fresh VM is booted for each iteration.

use crate::report::MetricResult;
use anyhow::Context as _;
use std::sync::OnceLock;

const ARCH: petri_artifacts_common::tags::MachineArch =
    petri_artifacts_common::tags::MachineArch::X86_64;

/// Boot time configuration profile.
///
/// Each profile defines a specific combination of VM features to measure.
/// This lets us track boot time across different configurations and
/// detect regressions in specific code paths.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BootProfile {
    /// Full device set, serial agent, CIDATA disk, shared memory.
    /// The "standard" configuration that vmm_tests use.
    Standard,
    /// Like Standard but with kernel console output suppressed
    /// (`quiet loglevel=0`). Isolates serial emulation overhead.
    QuietSerial,
    /// Pipette-as-init, minimal devices, no serial, shared memory.
    /// Measures VMM + kernel boot without serial overhead.
    Minimal,
    /// Pipette-as-init, minimal devices, no serial, private memory.
    /// Fastest configuration — eliminates mmap overhead for guest RAM.
    MinimalPrivate,
}

impl BootProfile {
    /// Parse a profile name from a string.
    pub fn from_name(name: &str) -> Option<Self> {
        match name {
            "standard" => Some(Self::Standard),
            "quiet-serial" => Some(Self::QuietSerial),
            "minimal" => Some(Self::Minimal),
            "minimal-private" => Some(Self::MinimalPrivate),
            _ => None,
        }
    }

    /// List all available profile names.
    pub fn all_names() -> &'static [&'static str] {
        &["standard", "quiet-serial", "minimal", "minimal-private"]
    }

    /// Whether this profile uses private memory.
    fn uses_private_memory(&self) -> bool {
        matches!(self, Self::MinimalPrivate)
    }

    /// Whether this profile uses the minimal device set.
    fn uses_minimal_builder(&self) -> bool {
        matches!(self, Self::Minimal | Self::MinimalPrivate)
    }

    /// Whether this profile suppresses kernel console output.
    fn uses_quiet_serial(&self) -> bool {
        matches!(self, Self::QuietSerial)
    }
}

/// Boot time test: measures launch-to-pipette-connect time via Linux direct boot.
pub struct BootTimeTest {
    /// The configuration profile to use.
    pub profile: BootProfile,
    /// Print guest diagnostics (dmesg, uptime) after the first boot.
    pub diag: bool,
    /// RAM size in MiB (default: 2048).
    pub mem_mb: u64,
    /// Cached pre-built initrd (built once, reused across iterations).
    pub prebuilt_initrd: OnceLock<tempfile::TempPath>,
}

fn build_firmware(resolver: &petri::ArtifactResolver<'_>) -> petri::Firmware {
    petri::Firmware::linux_direct(resolver, ARCH)
}

impl crate::harness::ColdPerfTest for BootTimeTest {
    fn name(&self) -> &str {
        "boot_time"
    }

    fn default_iterations(&self) -> u32 {
        10
    }

    fn warmup_iterations(&self) -> u32 {
        1
    }

    fn register_artifacts(&self, resolver: &petri::ArtifactResolver<'_>) {
        let firmware = build_firmware(resolver);
        petri::PetriVmArtifacts::<petri::openvmm::OpenVmmPetriBackend>::new(
            resolver, firmware, ARCH, true,
        );
    }

    async fn run_once(
        &self,
        resolver: &petri::ArtifactResolver<'_>,
        driver: &pal_async::DefaultDriver,
    ) -> anyhow::Result<Vec<MetricResult>> {
        let firmware = build_firmware(resolver);

        let artifacts = petri::PetriVmArtifacts::<petri::openvmm::OpenVmmPetriBackend>::new(
            resolver, firmware, ARCH, true,
        )
        .context("firmware/arch not compatible with OpenVMM backend")?;

        let mut post_test_hooks = Vec::new();
        let log_source = crate::log_source();
        let params = petri::PetriTestParams {
            test_name: "boot_time",
            logger: &log_source,
            post_test_hooks: &mut post_test_hooks,
        };

        let config = if self.profile.uses_minimal_builder() {
            // Minimal path: pipette-as-init, stripped device set.
            let uses_private = self.profile.uses_private_memory();
            let mut builder = petri::PetriVmBuilder::minimal(params, artifacts, driver)?
                .with_processor_topology(petri::ProcessorTopology {
                    vp_count: 1,
                    ..Default::default()
                })
                .with_memory(petri::MemoryConfig {
                    startup_bytes: self.mem_mb * 1024 * 1024,
                    ..Default::default()
                });

            if uses_private {
                builder = builder.modify_backend(|c| {
                    c.with_custom_config(|c| {
                        c.memory.private_memory = true;
                    })
                });
            }

            // Pre-build the modified initrd on first iteration (gzip
            // decompress + CPIO inject + recompress). Cached and reused
            // for subsequent iterations so this cost is outside the
            // measurement window.
            let initrd_path = match self.prebuilt_initrd.get() {
                Some(p) => p.to_path_buf(),
                None => {
                    let path = builder
                        .prepare_initrd()
                        .context("failed to prepare initrd")?;
                    self.prebuilt_initrd.set(path).ok();
                    self.prebuilt_initrd.get().unwrap().to_path_buf()
                }
            };
            builder = builder.with_prebuilt_initrd(initrd_path);
            builder
        } else {
            // Standard path: full device set, serial agent, CIDATA disk.
            let mut config = petri::PetriVmBuilder::new(params, artifacts, driver)?
                .with_processor_topology(petri::ProcessorTopology {
                    vp_count: 1,
                    ..Default::default()
                })
                .with_memory(petri::MemoryConfig {
                    startup_bytes: self.mem_mb * 1024 * 1024,
                    ..Default::default()
                });

            if self.profile.uses_quiet_serial() {
                config = config.modify_backend(|c| {
                    c.with_custom_config(|c| {
                        if let openvmm_defs::config::LoadMode::Linux { cmdline, .. } =
                            &mut c.load_mode
                        {
                            *cmdline = cmdline.replace(" debug ", " quiet loglevel=0 ");
                        }
                    })
                });
            }
            config
        };

        // Measure: start timing right before run(), stop when pipette connects.
        let start = std::time::Instant::now();
        let (vm, agent) = config.run().await.context("failed to boot VM")?;
        let elapsed = start.elapsed();

        let boot_time_ms = elapsed.as_secs_f64() * 1000.0;
        tracing::info!(boot_time_ms, "boot complete");

        if self.diag {
            self.print_diagnostics(&agent).await;
        }

        // Clean shutdown.
        agent.power_off().await.context("failed to power off")?;
        vm.wait_for_clean_teardown()
            .await
            .context("failed to tear down VM")?;

        Ok(vec![MetricResult {
            name: "boot_time_ms".to_string(),
            unit: "ms".to_string(),
            value: boot_time_ms,
        }])
    }
}

impl BootTimeTest {
    /// Print guest-side diagnostics (dmesg and /proc/uptime) after the first
    /// boot. This runs only when `--diag` is passed and does not affect timing.
    async fn print_diagnostics(&self, agent: &petri::pipette::PipetteClient) {
        // Guest uptime (seconds since kernel start).
        match agent.command("cat").arg("/proc/uptime").output().await {
            Ok(out) => {
                let text = String::from_utf8_lossy(&out.stdout);
                eprintln!("\n=== /proc/uptime ===\n{text}");
            }
            Err(e) => eprintln!("failed to read /proc/uptime: {e:#}"),
        }

        // Kernel log with timestamps.
        match agent.command("dmesg").output().await {
            Ok(out) => {
                let text = String::from_utf8_lossy(&out.stdout);
                eprintln!("\n=== dmesg ===\n{text}");
            }
            Err(e) => eprintln!("failed to read dmesg: {e:#}"),
        }
    }
}
