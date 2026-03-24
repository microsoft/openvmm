// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Per-VM memory overhead test.
//!
//! Boots a single minimal VM and measures host-side memory consumption
//! for the entire openvmm process tree (RSS, PSS, and derived VMM overhead).

use super::boot_time;
use super::boot_time::BootProfile;
use super::platform;
use crate::report::MetricResult;
use anyhow::Context as _;

/// Single-VM memory overhead test.
pub struct MemoryTest {
    /// Boot profile to use.
    pub profile: BootProfile,
    /// Guest RAM in MiB.
    pub mem_mb: u64,
    /// Pre-built initrd (only used for minimal profiles).
    initrd: Option<tempfile::TempPath>,
}

impl MemoryTest {
    /// Create a new memory test, building the initrd up front for minimal profiles.
    pub fn new(
        profile: BootProfile,
        mem_mb: u64,
        resolver: &petri::ArtifactResolver<'_>,
    ) -> anyhow::Result<Self> {
        let initrd = profile.prepare_initrd(resolver)?;
        Ok(Self {
            profile,
            mem_mb,
            initrd,
        })
    }
}

/// Register artifacts needed by the memory test.
pub fn register_artifacts(resolver: &petri::ArtifactResolver<'_>) {
    boot_time::register_artifacts(resolver);
}

impl crate::harness::ColdPerfTest for MemoryTest {
    fn name(&self) -> &str {
        "memory"
    }

    fn default_iterations(&self) -> u32 {
        3
    }

    fn warmup_iterations(&self) -> u32 {
        1
    }

    async fn run_once(
        &self,
        resolver: &petri::ArtifactResolver<'_>,
        driver: &pal_async::DefaultDriver,
    ) -> anyhow::Result<Vec<MetricResult>> {
        let artifacts = boot_time::build_artifacts(resolver)?;

        let mut post_test_hooks = Vec::new();
        let log_source = crate::log_source();
        let params = petri::PetriTestParams {
            test_name: "memory",
            logger: &log_source,
            post_test_hooks: &mut post_test_hooks,
        };

        let mut builder = self
            .profile
            .create_builder(params, artifacts, driver)?
            .with_processor_topology(petri::ProcessorTopology {
                vp_count: 1,
                ..Default::default()
            })
            .with_memory(petri::MemoryConfig {
                startup_bytes: self.mem_mb * 1024 * 1024,
                ..Default::default()
            });

        if let Some(ref initrd) = self.initrd {
            builder = builder.with_prebuilt_initrd(initrd.to_path_buf());
        }

        let (mut vm, agent) = builder.run().await.context("failed to boot VM")?;

        // Measure memory via process tree.
        let pid = vm.backend().pid();
        let pids = platform::collect_process_tree(pid);
        let mem = platform::measure_tree_memory(&pids).context("failed to measure memory")?;

        // Log detailed smaps breakdown for the main openvmm process.
        // Also use it to separate guest RAM from VMM overhead.
        let guest_mem_size = self.mem_mb * 1024 * 1024;
        let detail = platform::read_smaps_detail(pid, guest_mem_size).ok();
        if let Some(ref detail) = detail {
            tracing::info!(
                rss_anon_kib = detail.rss_anon_kib,
                rss_file_kib = detail.rss_file_kib,
                rss_shmem_kib = detail.rss_shmem_kib,
                guest_ram_private_kib = detail.guest_ram_private_kib,
                "memory breakdown by category"
            );
            for m in detail.mappings.iter().take(20) {
                if m.private_kib > 0 {
                    tracing::info!(
                        private_kib = m.private_kib,
                        rss_kib = m.rss_kib,
                        perms = m.perms,
                        name = m.name,
                        range = format!("{:x}-{:x}", m.addr_range.start, m.addr_range.end),
                        "mapping"
                    );
                }
            }
        }

        // VMM overhead = total private bytes minus guest RAM that's in
        // private mappings (named [anon:guest-ram-*]). In shared memory
        // mode, guest RAM is MAP_SHARED so private_kib already excludes
        // it and guest_ram_private_kib is 0.
        //
        // When we have smaps detail, use RssAnon as a more precise measure:
        // it excludes CoW pages from shared mappings (memfd guest RAM) that
        // inflate Private_Clean/Private_Dirty in the rollup.
        let guest_ram_private_kib = detail
            .as_ref()
            .map(|d| d.guest_ram_private_kib)
            .unwrap_or(0);
        let vmm_overhead_kib = if let Some(ref detail) = detail {
            // RssAnon = anonymous private pages only (excludes file-backed
            // and shared mapping pages). Subtract guest-ram-private to get
            // VMM-only anonymous overhead.
            detail.rss_anon_kib.saturating_sub(guest_ram_private_kib)
        } else {
            mem.private_kib.saturating_sub(guest_ram_private_kib)
        };

        let mut metrics = vec![
            MetricResult {
                name: "memory_rss_kib".to_string(),
                unit: "KiB".to_string(),
                value: mem.rss_kib as f64,
            },
            MetricResult {
                name: "memory_private_kib".to_string(),
                unit: "KiB".to_string(),
                value: mem.private_kib as f64,
            },
            MetricResult {
                name: "memory_vmm_overhead_kib".to_string(),
                unit: "KiB".to_string(),
                value: vmm_overhead_kib as f64,
            },
            MetricResult {
                name: "memory_process_count".to_string(),
                unit: "count".to_string(),
                value: mem.process_count as f64,
            },
        ];

        if let Some(pss) = mem.pss_kib {
            metrics.push(MetricResult {
                name: "memory_pss_kib".to_string(),
                unit: "KiB".to_string(),
                value: pss as f64,
            });
        }

        // Clean shutdown.
        agent.power_off().await.context("failed to power off")?;
        vm.wait_for_clean_teardown()
            .await
            .context("failed to tear down VM")?;

        Ok(metrics)
    }
}
