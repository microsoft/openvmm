// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! JSON report types and serialization for performance test results.

use serde::Deserialize;
use serde::Serialize;

/// A single metric value produced by one iteration of a perf test.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MetricResult {
    /// Metric name, e.g. `"boot_time_ms"`, `"fio_randread_iops"`.
    pub name: String,
    /// Unit of measurement, e.g. `"ms"`, `"MiB/s"`, `"IOPS"`, `"Gbps"`.
    pub unit: String,
    /// The measured value.
    pub value: f64,
}

/// Aggregated statistics for a single metric across iterations.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MetricStats {
    /// Metric name.
    pub name: String,
    /// Unit of measurement.
    pub unit: String,
    /// Number of measured iterations (excluding warmup).
    pub iterations: u32,
    /// Arithmetic mean.
    pub mean: f64,
    /// Sample standard deviation.
    pub std_dev: f64,
    /// Minimum observed value.
    pub min: f64,
    /// Maximum observed value.
    pub max: f64,
}

/// A complete performance test report.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PerfReport {
    /// Git revision (SHA) at the time the binary was built.
    pub git_revision: String,
    /// Git branch at the time the binary was built.
    #[serde(default)]
    pub git_branch: String,
    /// Git commit date (ISO 8601). Retained for backward compatibility
    /// with older reports; new reports set this to an empty string.
    #[serde(default)]
    pub git_commit_date: String,
    /// Timestamp when the test run started (ISO 8601).
    pub date: String,
    /// All metric results.
    pub results: Vec<MetricStats>,
}

impl PerfReport {
    /// Create a new report with compile-time git info and the current timestamp.
    pub fn new(results: Vec<MetricStats>) -> anyhow::Result<Self> {
        let git_revision = option_env!("BUILD_GIT_SHA")
            .unwrap_or("unknown")
            .to_string();
        let git_branch = option_env!("BUILD_GIT_BRANCH")
            .unwrap_or("unknown")
            .to_string();
        let date = jiff::Timestamp::now().to_string();

        Ok(Self {
            git_revision,
            git_branch,
            git_commit_date: String::new(),
            date,
            results,
        })
    }

    /// Serialize the report to a pretty-printed JSON string.
    pub fn to_json(&self) -> anyhow::Result<String> {
        serde_json::to_string_pretty(self).map_err(Into::into)
    }

    /// Deserialize a report from a JSON string.
    pub fn from_json(json: &str) -> anyhow::Result<Self> {
        serde_json::from_str(json).map_err(Into::into)
    }

    /// Print a human-readable summary to stdout.
    pub fn print_summary(&self) {
        println!("Performance Report");
        println!("  Git revision:    {}", self.git_revision);
        if !self.git_branch.is_empty() {
            println!("  Git branch:      {}", self.git_branch);
        }
        if !self.git_commit_date.is_empty() {
            println!("  Git commit date: {}", self.git_commit_date);
        }
        println!("  Run date:        {}", self.date);
        println!();
        println!(
            "  {:<30} {:>10} {:>12} {:>12} {:>12} {:>12} {:>6}",
            "Metric", "Unit", "Mean", "StdDev", "Min", "Max", "N"
        );
        println!("  {}", "-".repeat(96));
        for m in &self.results {
            println!(
                "  {:<30} {:>10} {:>12.2} {:>12.2} {:>12.2} {:>12.2} {:>6}",
                m.name, m.unit, m.mean, m.std_dev, m.min, m.max, m.iterations
            );
        }
    }
}

/// Compare two reports and print a human-readable table of deltas.
pub fn compare_reports(baseline: &PerfReport, candidate: &PerfReport) -> ComparisonReport {
    let mut comparisons = Vec::new();

    for candidate_metric in &candidate.results {
        if let Some(baseline_metric) = baseline
            .results
            .iter()
            .find(|m| m.name == candidate_metric.name)
        {
            let delta = candidate_metric.mean - baseline_metric.mean;
            let delta_pct = if baseline_metric.mean.abs() > f64::EPSILON {
                (delta / baseline_metric.mean) * 100.0
            } else {
                0.0
            };

            comparisons.push(MetricComparison {
                name: candidate_metric.name.clone(),
                unit: candidate_metric.unit.clone(),
                baseline_mean: baseline_metric.mean,
                candidate_mean: candidate_metric.mean,
                delta,
                delta_pct,
            });
        }
    }

    ComparisonReport {
        baseline_revision: baseline.git_revision.clone(),
        candidate_revision: candidate.git_revision.clone(),
        comparisons,
    }
}

/// Result of comparing two performance reports.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ComparisonReport {
    /// Git revision of the baseline.
    pub baseline_revision: String,
    /// Git revision of the candidate.
    pub candidate_revision: String,
    /// Per-metric comparisons.
    pub comparisons: Vec<MetricComparison>,
}

impl ComparisonReport {
    /// Print a human-readable comparison table.
    pub fn print_summary(&self) {
        println!("Performance Comparison");
        println!("  Baseline:  {}", self.baseline_revision);
        println!("  Candidate: {}", self.candidate_revision);
        println!();
        println!(
            "  {:<30} {:>10} {:>12} {:>12} {:>12} {:>8}",
            "Metric", "Unit", "Baseline", "Candidate", "Delta", "Delta%"
        );
        println!("  {}", "-".repeat(86));
        for c in &self.comparisons {
            let direction = if c.delta_pct > 1.0 {
                "+"
            } else if c.delta_pct < -1.0 {
                ""
            } else {
                "~"
            };
            println!(
                "  {:<30} {:>10} {:>12.2} {:>12.2} {:>12.2} {:>7.1}%{}",
                c.name, c.unit, c.baseline_mean, c.candidate_mean, c.delta, c.delta_pct, direction
            );
        }
    }

    /// Serialize the comparison to a pretty-printed JSON string.
    pub fn to_json(&self) -> anyhow::Result<String> {
        serde_json::to_string_pretty(self).map_err(Into::into)
    }
}

/// Comparison of a single metric between baseline and candidate.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MetricComparison {
    /// Metric name.
    pub name: String,
    /// Unit of measurement.
    pub unit: String,
    /// Baseline mean value.
    pub baseline_mean: f64,
    /// Candidate mean value.
    pub candidate_mean: f64,
    /// Absolute delta (candidate - baseline).
    pub delta: f64,
    /// Relative delta as percentage.
    pub delta_pct: f64,
}
