// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Concurrent VM boot sweep test.
//!
//! Launches N VMs simultaneously and measures boot time distribution
//! and memory overhead at various concurrency levels.

use super::boot_time;
use super::boot_time::BootProfile;
use super::platform;
use crate::report::MetricStats;
use anyhow::Context as _;
use std::path::PathBuf;

/// Concurrent VM boot sweep test configuration.
pub struct ScaleBootTest {
    /// Boot profile to use for each VM.
    pub profile: BootProfile,
    /// Per-VM RAM in MiB (default: 256 for density).
    pub mem_mb: u64,
    /// Explicit N values to test, or `None` for the default geometric sweep.
    pub vms: Option<Vec<u32>>,
    /// Maximum number of concurrent VMs (default: 64).
    pub max_vms: u32,
    /// Pre-built initrd (only used for minimal profiles).
    initrd: Option<tempfile::TempPath>,
}

impl ScaleBootTest {
    /// Create a new scale boot test, building the initrd up front for minimal profiles.
    pub fn new(
        profile: BootProfile,
        mem_mb: u64,
        vms: Option<Vec<u32>>,
        max_vms: u32,
        resolver: &petri::ArtifactResolver<'_>,
    ) -> anyhow::Result<Self> {
        let initrd = profile.prepare_initrd(resolver)?;
        Ok(Self {
            profile,
            mem_mb,
            vms,
            max_vms,
            initrd,
        })
    }

    fn initrd_path(&self) -> Option<PathBuf> {
        self.initrd.as_ref().map(|p| p.to_path_buf())
    }
}

/// Register artifacts needed by the scale boot test.
pub fn register_artifacts(resolver: &petri::ArtifactResolver<'_>) {
    boot_time::register_artifacts(resolver);
}

/// Compute the N values for the sweep.
fn sweep_values(vms: &Option<Vec<u32>>, max_vms: u32) -> Vec<u32> {
    match vms {
        Some(explicit) => explicit
            .iter()
            .copied()
            .filter(|&n| n > 0 && n <= max_vms)
            .collect(),
        None => {
            let mut vals = Vec::new();
            let mut n = 1u32;
            while n <= max_vms {
                vals.push(n);
                n *= 2;
            }
            vals
        }
    }
}

fn percentile(sorted: &[f64], p: f64) -> f64 {
    if sorted.is_empty() {
        return 0.0;
    }
    let idx = (p / 100.0 * (sorted.len() - 1) as f64).round() as usize;
    sorted[idx.min(sorted.len() - 1)]
}

/// Run the scale boot test, returning metrics for all N values.
pub async fn run_scale_test(
    test: &ScaleBootTest,
    resolver: &petri::ArtifactResolver<'_>,
    driver: &pal_async::DefaultDriver,
) -> anyhow::Result<Vec<MetricStats>> {
    let n_values = sweep_values(&test.vms, test.max_vms);
    if n_values.is_empty() {
        anyhow::bail!("no VM counts to test (max_vms={})", test.max_vms);
    }

    tracing::info!(?n_values, "starting scale boot sweep");

    let mut all_stats: Vec<MetricStats> = Vec::new();

    for &n in &n_values {
        // Check if we have enough memory.
        let required_bytes = (n as u64)
            .checked_mul(test.mem_mb)
            .and_then(|v| v.checked_mul(1024 * 1024))
            .context("VM count * mem_mb overflows u64")?;
        match platform::available_memory_bytes() {
            Ok(avail) => {
                let limit = avail * 90 / 100;
                if required_bytes > limit {
                    tracing::warn!(
                        n,
                        required_mib = required_bytes / (1024 * 1024),
                        available_mib = avail / (1024 * 1024),
                        "skipping N={n} and larger: would exceed 90% of available RAM"
                    );
                    break;
                }
            }
            Err(e) => {
                tracing::warn!("failed to check available memory: {e:#}");
            }
        }

        let mem_before = platform::available_memory_bytes().ok();

        tracing::info!(n, "launching {n} VMs concurrently");

        let futs: Vec<_> = (0..n)
            .map(|vm_idx| boot_one_vm(test, test.initrd_path(), resolver, driver, n, vm_idx))
            .collect();

        let results = futures::future::join_all(futs).await;

        // Separate successes and failures.
        let mut boot_times: Vec<f64> = Vec::new();
        let mut failures: u32 = 0;
        // Keep successful VMs alive for memory measurement.
        let mut live_vms: Vec<(
            petri::PetriVm<petri::openvmm::OpenVmmPetriBackend>,
            petri::pipette::PipetteClient,
        )> = Vec::new();

        for (vm_idx, result) in results.into_iter().enumerate() {
            match result {
                Ok((vm, agent, boot_ms)) => {
                    boot_times.push(boot_ms);
                    live_vms.push((vm, agent));
                }
                Err(e) => {
                    tracing::warn!(vm_idx, n, "VM {vm_idx} failed: {e:#}");
                    failures += 1;
                }
            }
        }

        // Memory measurement (before shutdown).
        let mem_after = platform::available_memory_bytes().ok();

        // Shut down all VMs concurrently.
        let shutdown_futs: Vec<_> = live_vms
            .into_iter()
            .map(|(vm, agent)| async move {
                if let Err(e) = agent.power_off().await {
                    tracing::warn!("failed to power off VM: {e:#}");
                }
                if let Err(e) = vm.wait_for_clean_teardown().await {
                    tracing::warn!("failed to tear down VM: {e:#}");
                }
            })
            .collect();
        futures::future::join_all(shutdown_futs).await;

        if boot_times.is_empty() {
            tracing::warn!(n, "all {n} VMs failed, skipping data point");
            continue;
        }

        // Compute stats.
        boot_times.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));

        let mean = boot_times.iter().sum::<f64>() / boot_times.len() as f64;
        let min = boot_times[0];
        let max = boot_times[boot_times.len() - 1];
        let p50 = percentile(&boot_times, 50.0);
        let p99 = percentile(&boot_times, 99.0);
        let first_ready_ms = min;
        let last_ready_ms = max;

        let prefix = format!("scale_{n}");

        let mut metrics = vec![
            stat(&prefix, "mean_boot_ms", "ms", mean),
            stat(&prefix, "min_boot_ms", "ms", min),
            stat(&prefix, "max_boot_ms", "ms", max),
            stat(&prefix, "p50_boot_ms", "ms", p50),
            stat(&prefix, "p99_boot_ms", "ms", p99),
            stat(&prefix, "first_ready_ms", "ms", first_ready_ms),
            stat(&prefix, "last_ready_ms", "ms", last_ready_ms),
        ];

        if failures > 0 {
            metrics.push(stat(&prefix, "failures", "count", failures as f64));
        }

        // Memory delta metrics.
        if let (Some(before), Some(after)) = (mem_before, mem_after) {
            if before > after {
                let delta_bytes = before - after;
                let total_mib = delta_bytes as f64 / (1024.0 * 1024.0);
                let per_vm_mib = total_mib / boot_times.len() as f64;
                metrics.push(stat(&prefix, "total_memory_mib", "MiB", total_mib));
                metrics.push(stat(&prefix, "per_vm_memory_mib", "MiB", per_vm_mib));
            }
        }

        tracing::info!(
            n,
            mean_ms = format!("{mean:.1}"),
            p99_ms = format!("{p99:.1}"),
            successes = boot_times.len(),
            failures,
            "N={n} complete"
        );

        all_stats.extend(metrics);
    }

    Ok(all_stats)
}

/// Create a `MetricStats` for a single-shot measurement (iterations=1, std_dev=0).
fn stat(prefix: &str, name: &str, unit: &str, value: f64) -> MetricStats {
    MetricStats {
        name: format!("{prefix}_{name}"),
        unit: unit.to_string(),
        iterations: 1,
        mean: value,
        std_dev: 0.0,
        min: value,
        max: value,
    }
}

/// Create a VM builder with the appropriate configuration for the profile.
fn make_builder(
    test: &ScaleBootTest,
    test_name: &str,
    resolver: &petri::ArtifactResolver<'_>,
    driver: &pal_async::DefaultDriver,
) -> anyhow::Result<petri::PetriVmBuilder<petri::openvmm::OpenVmmPetriBackend>> {
    let artifacts = boot_time::build_artifacts(resolver)?;

    let mut post_test_hooks = Vec::new();
    let log_source = crate::log_source();
    let params = petri::PetriTestParams {
        test_name,
        logger: &log_source,
        post_test_hooks: &mut post_test_hooks,
    };

    Ok(test
        .profile
        .create_builder(params, artifacts, driver)?
        .with_processor_topology(petri::ProcessorTopology {
            vp_count: 1,
            ..Default::default()
        })
        .with_memory(petri::MemoryConfig {
            startup_bytes: test.mem_mb * 1024 * 1024,
            ..Default::default()
        }))
}

/// Boot a single VM and return (vm, agent, boot_time_ms).
async fn boot_one_vm(
    test: &ScaleBootTest,
    initrd_path: Option<PathBuf>,
    resolver: &petri::ArtifactResolver<'_>,
    driver: &pal_async::DefaultDriver,
    n: u32,
    vm_idx: u32,
) -> anyhow::Result<(
    petri::PetriVm<petri::openvmm::OpenVmmPetriBackend>,
    petri::pipette::PipetteClient,
    f64,
)> {
    let test_name = format!("scale_boot_{n}_vm_{vm_idx}");
    let mut builder = make_builder(test, &test_name, resolver, driver)?;
    if let Some(initrd_path) = initrd_path {
        builder = builder.with_prebuilt_initrd(initrd_path);
    }

    let start = std::time::Instant::now();
    let (vm, agent) = builder.run().await.context("failed to boot VM")?;
    let boot_ms = start.elapsed().as_secs_f64() * 1000.0;

    Ok((vm, agent, boot_ms))
}
