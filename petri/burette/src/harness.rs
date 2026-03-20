// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Performance test harness for OpenVMM.
//!
//! Provides traits, iteration loops, and statistics computation for
//! performance benchmarks using the petri test framework.

// WarmPerfTest and helpers are used in later phases (block I/O, network).
#![expect(dead_code)]

use anyhow::Context as _;
use std::time::Duration;
use std::time::Instant;

use crate::report::MetricResult;
use crate::report::MetricStats;
use std::future::Future;

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

    /// Register required artifacts with the resolver.
    ///
    /// Called during artifact resolution (both collector and resolver
    /// passes). Does not require `&self` — implementors should define
    /// a module-level function and delegate.
    fn register_artifacts(resolver: &petri::ArtifactResolver<'_>)
    where
        Self: Sized;

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

    /// Register required artifacts with the resolver.
    fn register_artifacts(resolver: &petri::ArtifactResolver<'_>)
    where
        Self: Sized;

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

    tracing::info!(test = test.name(), iterations, "starting warm perf test");

    tracing::info!(test = test.name(), "setting up VM");
    let mut state = test
        .setup(resolver, driver)
        .await
        .with_context(|| format!("{}: setup", test.name()))?;

    let mut all_samples: Vec<Vec<MetricResult>> = Vec::new();

    for i in 0..iterations {
        let label = format!("iteration {}/{}", i + 1, iterations);
        tracing::info!(test = test.name(), label, "running");
        let start = Instant::now();
        let results = test
            .run_once(&mut state)
            .await
            .with_context(|| format!("{}: {label}", test.name()))?;
        let elapsed = start.elapsed();

        tracing::info!(
            test = test.name(),
            label,
            elapsed_ms = elapsed.as_millis(),
            "iteration complete"
        );
        all_samples.push(results);
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

/// Format a duration as a human-readable string.
pub fn format_duration(d: Duration) -> String {
    let secs = d.as_secs();
    if secs >= 60 {
        format!("{}m{:02}s", secs / 60, secs % 60)
    } else {
        format!("{:.1}s", d.as_secs_f64())
    }
}
