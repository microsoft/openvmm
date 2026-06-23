// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Performance test harness for OpenVMM.
//!
//! Provides traits, iteration loops, and statistics computation for
//! performance benchmarks using the petri test framework.

use anyhow::Context as _;
use std::time::Instant;

use crate::report::MetricResult;
use crate::report::MetricStats;
use std::future::Future;

/// `perf record` wrapper for capturing CPU profiles of a specific process,
/// scoped to specific test phases. Linux only; no-ops on other platforms.
///
/// If created with `None` for the directory, all methods are no-ops.
pub struct PerfRecorder {
    dir: Option<std::path::PathBuf>,
    #[cfg_attr(not(target_os = "linux"), expect(dead_code))]
    pid: i32,
    #[cfg(target_os = "linux")]
    child: Option<(std::process::Child, std::path::PathBuf)>,
}

impl PerfRecorder {
    /// Create a new recorder targeting `pid`. If `dir` is `Some`, traces
    /// are saved there; if `None`, `start`/`stop` are no-ops.
    pub fn new(dir: Option<impl Into<std::path::PathBuf>>, pid: i32) -> anyhow::Result<Self> {
        let dir = if let Some(d) = dir {
            let d = d.into();
            std::fs::create_dir_all(&d)
                .with_context(|| format!("failed to create perf dir: {}", d.display()))?;
            Some(d)
        } else {
            None
        };
        Ok(Self {
            dir,
            pid,
            #[cfg(target_os = "linux")]
            child: None,
        })
    }

    /// Start recording. The trace is saved as `<dir>/<name>.data`.
    ///
    /// No-op if this recorder was created without a directory.
    /// If a recording is already in progress, it is stopped first.
    #[cfg(target_os = "linux")]
    pub fn start(&mut self, name: &str) -> anyhow::Result<()> {
        let Some(dir) = self.dir.clone() else {
            return Ok(());
        };
        // Stop any in-progress recording.
        if self.child.is_some() {
            self.stop()?;
        }
        let data_path = dir.join(format!("{name}.data"));
        let pid_str = self.pid.to_string();
        let child = std::process::Command::new("perf")
            .args([
                "record",
                "-p",
                &pid_str,
                "-g",
                "--call-graph",
                "dwarf,16384",
                "-F",
                "997",
                "-o",
            ])
            .arg(&data_path)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .context("failed to spawn perf record — is perf installed?")?;
        tracing::info!(path = %data_path.display(), "perf recording started");
        self.child = Some((child, data_path));
        Ok(())
    }

    /// Stop the current recording.
    #[cfg(target_os = "linux")]
    // UNSAFETY: Sending SIGINT to the perf child process via libc::kill.
    #[expect(unsafe_code)]
    pub fn stop(&mut self) -> anyhow::Result<()> {
        if let Some((child, data_path)) = self.child.take() {
            // SAFETY: Sending SIGINT to a child process we own.
            // The pid is valid because we just spawned it and haven't
            // waited on it yet.
            unsafe {
                libc::kill(child.id() as i32, libc::SIGINT);
            }
            let output = child
                .wait_with_output()
                .context("failed to wait for perf record")?;
            if !output.status.success() {
                let stderr = String::from_utf8_lossy(&output.stderr);
                tracing::warn!(
                    status = %output.status,
                    stderr = %stderr,
                    "perf record exited non-zero"
                );
            }
            tracing::info!(path = %data_path.display(), "perf recording saved");
        }
        Ok(())
    }

    #[cfg(not(target_os = "linux"))]
    pub fn start(&mut self, _name: &str) -> anyhow::Result<()> {
        if self.dir.is_some() {
            anyhow::bail!("--perf-dir is only supported on Linux");
        }
        Ok(())
    }

    #[cfg(not(target_os = "linux"))]
    pub fn stop(&mut self) -> anyhow::Result<()> {
        Ok(())
    }
}

impl Drop for PerfRecorder {
    fn drop(&mut self) {
        let _ = self.stop();
    }
}

/// A performance test that boots a fresh VM per iteration (cold mode).
///
/// Used for measurements like boot time where each iteration needs
/// an independent VM lifecycle.
pub trait ColdPerfTest {
    /// Human-readable name for this test.
    fn name(&self) -> &str;

    /// Default number of iterations.
    fn default_iterations(&self) -> u32 {
        10
    }

    /// Number of warmup iterations to discard before measuring.
    fn warmup_iterations(&self) -> u32 {
        1
    }

    /// Run a single iteration. Returns one or more metric results.
    fn run_once(
        &self,
        resolver: &petri::ArtifactResolver<'_>,
        driver: &pal_async::DefaultDriver,
    ) -> impl Future<Output = anyhow::Result<Vec<MetricResult>>>;
}

/// A performance test that boots once and runs the workload multiple times
/// (warm mode).
///
/// Used for I/O throughput tests where boot cost should be amortized
/// and steady-state performance is measured.
pub trait WarmPerfTest {
    /// The state returned by `setup` and passed to each `run_once`.
    type State: Send;

    /// Human-readable name for this test.
    fn name(&self) -> &str;

    /// Default number of iterations.
    fn default_iterations(&self) -> u32 {
        5
    }
    /// Number of warmup iterations to discard before measuring.
    fn warmup_iterations(&self) -> u32 {
        0
    }

    /// Boot the VM and prepare the workload environment.
    fn setup(
        &self,
        resolver: &petri::ArtifactResolver<'_>,
        driver: &pal_async::DefaultDriver,
    ) -> impl Future<Output = anyhow::Result<Self::State>>;

    /// Run a single workload iteration inside the already-booted VM.
    fn run_once(
        &self,
        state: &mut Self::State,
    ) -> impl Future<Output = anyhow::Result<Vec<MetricResult>>>;

    /// Tear down the VM after all iterations complete.
    fn teardown(&self, state: Self::State) -> impl Future<Output = anyhow::Result<()>>;
}

/// Run a cold performance test for the given number of iterations,
/// discarding warmup iterations.
pub async fn run_cold_test(
    test: &(impl ColdPerfTest + ?Sized),
    resolver: &petri::ArtifactResolver<'_>,
    driver: &pal_async::DefaultDriver,
    iterations: Option<u32>,
) -> anyhow::Result<Vec<MetricStats>> {
    let iterations = iterations.unwrap_or(test.default_iterations());
    let warmup = test.warmup_iterations();
    let total = warmup + iterations;

    tracing::info!(
        test = test.name(),
        warmup,
        iterations,
        "starting cold perf test"
    );

    let mut all_samples: Vec<Vec<MetricResult>> = Vec::new();

    for i in 0..total {
        let is_warmup = i < warmup;
        let label = if is_warmup {
            format!("warmup {}/{}", i + 1, warmup)
        } else {
            format!("iteration {}/{}", i - warmup + 1, iterations)
        };

        tracing::info!(test = test.name(), label, "running");
        let start = Instant::now();
        let results = test
            .run_once(resolver, driver)
            .await
            .with_context(|| format!("{}: {label}", test.name()))?;
        let elapsed = start.elapsed();

        if is_warmup {
            tracing::info!(
                test = test.name(),
                label,
                elapsed_ms = elapsed.as_millis(),
                "warmup complete (discarded)"
            );
        } else {
            tracing::info!(
                test = test.name(),
                label,
                elapsed_ms = elapsed.as_millis(),
                "iteration complete"
            );
            all_samples.push(results);
        }
    }

    compute_stats(test.name(), &all_samples, iterations)
}

/// Run a warm performance test: boot once, run N iterations, tear down.
pub async fn run_warm_test(
    test: &(impl WarmPerfTest + ?Sized),
    resolver: &petri::ArtifactResolver<'_>,
    driver: &pal_async::DefaultDriver,
    iterations: Option<u32>,
) -> anyhow::Result<Vec<MetricStats>> {
    let iterations = iterations.unwrap_or(test.default_iterations());
    let warmup = test.warmup_iterations();
    let total = warmup + iterations;

    tracing::info!(
        test = test.name(),
        warmup,
        iterations,
        "starting warm perf test"
    );

    tracing::info!(test = test.name(), "setting up VM");
    let mut state = test
        .setup(resolver, driver)
        .await
        .with_context(|| format!("{}: setup", test.name()))?;

    let mut all_samples: Vec<Vec<MetricResult>> = Vec::new();

    for i in 0..total {
        let is_warmup = i < warmup;
        let label = if is_warmup {
            format!("warmup {}/{}", i + 1, warmup)
        } else {
            format!("iteration {}/{}", i - warmup + 1, iterations)
        };

        tracing::info!(test = test.name(), label, "running");
        let start = Instant::now();
        let results = test
            .run_once(&mut state)
            .await
            .with_context(|| format!("{}: {label}", test.name()))?;
        let elapsed = start.elapsed();

        if is_warmup {
            tracing::info!(
                test = test.name(),
                label,
                elapsed_ms = elapsed.as_millis(),
                "warmup complete (discarded)"
            );
        } else {
            tracing::info!(
                test = test.name(),
                label,
                elapsed_ms = elapsed.as_millis(),
                "iteration complete"
            );
            all_samples.push(results);
        }
    }

    tracing::info!(test = test.name(), "tearing down VM");
    test.teardown(state)
        .await
        .with_context(|| format!("{}: teardown", test.name()))?;

    compute_stats(test.name(), &all_samples, iterations)
}

/// Aggregate per-metric samples across iterations into statistics.
fn compute_stats(
    test_name: &str,
    all_samples: &[Vec<MetricResult>],
    iterations: u32,
) -> anyhow::Result<Vec<MetricStats>> {
    if all_samples.is_empty() {
        anyhow::bail!("{test_name}: no samples collected");
    }

    // Collect metric names from the first iteration.
    let metric_names: Vec<(String, String)> = all_samples[0]
        .iter()
        .map(|m| (m.name.clone(), m.unit.clone()))
        .collect();

    let mut stats = Vec::new();

    for (name, unit) in &metric_names {
        let values: Vec<f64> = all_samples
            .iter()
            .filter_map(|sample| sample.iter().find(|m| &m.name == name).map(|m| m.value))
            .collect();

        if values.is_empty() {
            anyhow::bail!("{test_name}: metric {name} missing from all iterations");
        }

        let n = values.len() as f64;
        let mean = values.iter().sum::<f64>() / n;
        let variance = if values.len() > 1 {
            values.iter().map(|v| (v - mean).powi(2)).sum::<f64>() / (n - 1.0)
        } else {
            0.0
        };
        let std_dev = variance.sqrt();
        let min = values.iter().copied().fold(f64::INFINITY, f64::min);
        let max = values.iter().copied().fold(f64::NEG_INFINITY, f64::max);

        stats.push(MetricStats {
            name: name.clone(),
            unit: unit.clone(),
            iterations,
            mean,
            std_dev,
            min,
            max,
        });
    }

    Ok(stats)
}
