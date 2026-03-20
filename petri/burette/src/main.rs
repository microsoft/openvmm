// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! OpenVMM performance test runner.
//!
//! A standalone binary that runs performance benchmarks against OpenVMM
//! using the petri test framework, producing JSON reports.
//!
//! # Usage
//!
//! ```bash
//! # Run all benchmarks
//! burette run -o report.json
//!
//! # Run only boot time test
//! burette run --test boot_time -o report.json
//!
//! # Run with custom iteration count
//! burette run --iterations 20 -o report.json
//!
//! # Compare two reports
//! burette compare baseline.json candidate.json
//! ```

mod harness;
mod report;
mod tests;

use anyhow::Context as _;
use clap::Parser;
use report::MetricStats;
use std::path::Path;
use std::path::PathBuf;
use std::sync::OnceLock;

/// Global log source for petri, initialized once.
static LOG_SOURCE: OnceLock<petri::PetriLogSource> = OnceLock::new();

/// Get or initialize the global log source.
fn log_source() -> petri::PetriLogSource {
    LOG_SOURCE
        .get()
        .expect("log source not initialized")
        .clone()
}

#[derive(Parser)]
#[command(name = "burette", about = "OpenVMM performance benchmarks")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(clap::Subcommand)]
enum Command {
    /// Run performance benchmarks.
    Run(RunArgs),
    /// Compare two JSON performance reports.
    Compare(CompareArgs),
    /// Package binaries and artifacts into a self-contained tarball
    /// for running on a remote machine without the repo or Rust.
    Package(PackageArgs),
}

#[derive(clap::Args)]
struct RunArgs {
    /// Output JSON report file.
    #[arg(short, long, default_value = "perf_report.json")]
    output: PathBuf,

    /// Directory for petri logs.
    #[arg(long, default_value = "vmm_test_results/burette")]
    log_dir: PathBuf,

    /// Run only a specific test (e.g. "boot_time"). Omit to run all.
    #[arg(long)]
    test: Option<String>,

    /// Override the number of iterations per test.
    #[arg(long)]
    iterations: Option<u32>,

    /// Boot time profile: defines the VM configuration to measure.
    /// Available profiles: standard, quiet-serial, minimal, minimal-private.
    /// Default: minimal-private (fastest).
    #[arg(long, default_value = "minimal-private")]
    profile: String,

    /// Guest RAM in MiB. Default: 2048.
    #[arg(long, default_value = "2048")]
    mem_mb: u64,

    /// Print guest dmesg and /proc/uptime after the first boot for
    /// diagnostics. Does not affect measurements.
    #[arg(long)]
    diag: bool,

    /// Number of concurrent VMs for scale_boot (e.g. "32" for single point,
    /// or "1,2,4,8,16" for explicit sweep). Omit for default geometric sweep.
    #[arg(long, value_delimiter = ',')]
    vms: Option<Vec<u32>>,

    /// Maximum number of concurrent VMs for scale_boot sweep.
    #[arg(long, default_value = "64")]
    max_vms: u32,
}

#[derive(clap::Args)]
struct CompareArgs {
    /// Baseline JSON report file.
    baseline: PathBuf,

    /// Candidate JSON report file.
    candidate: PathBuf,

    /// Output JSON diff file (optional).
    #[arg(short, long)]
    output: Option<PathBuf>,
}

#[derive(clap::Args)]
struct PackageArgs {
    /// Output tarball path.
    #[arg(short, long, default_value = "burette_bundle.tar.gz")]
    output: PathBuf,
}

fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();

    match cli.command {
        Command::Run(args) => cmd_run(args),
        Command::Compare(args) => cmd_compare(args),
        Command::Package(args) => cmd_package(args),
    }
}

/// Two-pass artifact resolution, following the petri-tool pattern.
///
/// `register` runs in both collector and resolver passes to record
/// artifact requirements. The returned `TestArtifacts` can then be
/// used to create a resolver for actual test execution.
fn resolve_artifacts(
    register: impl Fn(&petri::ArtifactResolver<'_>),
) -> anyhow::Result<petri::TestArtifacts> {
    let resolver =
        petri_artifact_resolver_openvmm_known_paths::OpenvmmKnownPathsTestArtifactResolver::new("");
    let mut requirements = petri::TestArtifactRequirements::new();

    // Pass 1: collect what artifacts are needed.
    register(&petri::ArtifactResolver::collector(&mut requirements));

    // Pass 2: resolve to actual paths.
    let artifacts = requirements
        .resolve(&resolver)
        .context("failed to resolve test artifacts")?;

    // Touch all artifacts so they're fully resolved.
    register(&petri::ArtifactResolver::resolver(&artifacts));

    Ok(artifacts)
}

fn cmd_run(args: RunArgs) -> anyhow::Result<()> {
    // Set up logging.
    std::fs::create_dir_all(&args.log_dir)
        .with_context(|| format!("failed to create log dir: {}", args.log_dir.display()))?;

    let log_source =
        petri::try_init_tracing(&args.log_dir).context("failed to initialize tracing")?;
    LOG_SOURCE
        .set(log_source)
        .ok()
        .context("log source already initialized")?;

    // Determine which tests to run.
    let available_tests = ["boot_time", "scale_boot", "memory"];
    let tests_to_run: Vec<&str> = if let Some(ref name) = args.test {
        if !available_tests.contains(&name.as_str()) {
            anyhow::bail!(
                "unknown test: {name}. Available tests: {}",
                available_tests.join(", ")
            );
        }
        vec![name.as_str()]
    } else {
        available_tests.to_vec()
    };

    let mut all_stats: Vec<MetricStats> = Vec::new();

    for test_name in &tests_to_run {
        match *test_name {
            "boot_time" => {
                let profile =
                    tests::boot_time::BootProfile::from_name(&args.profile).ok_or_else(|| {
                        anyhow::anyhow!(
                            "unknown profile '{}'. Available: {}",
                            args.profile,
                            tests::boot_time::BootProfile::all_names().join(", ")
                        )
                    })?;

                let artifacts = resolve_artifacts(tests::boot_time::register_artifacts)?;
                let resolver = petri::ArtifactResolver::resolver(&artifacts);

                let test =
                    tests::boot_time::BootTimeTest::new(profile, args.diag, args.mem_mb, &resolver)
                        .context("boot_time prep")?;

                let stats = pal_async::DefaultPool::run_with(async |driver| {
                    harness::run_cold_test(&test, &resolver, &driver, args.iterations).await
                })
                .context("boot_time test failed")?;
                all_stats.extend(stats);
            }
            "scale_boot" => {
                let profile =
                    tests::boot_time::BootProfile::from_name(&args.profile).ok_or_else(|| {
                        anyhow::anyhow!(
                            "unknown profile '{}'. Available: {}",
                            args.profile,
                            tests::boot_time::BootProfile::all_names().join(", ")
                        )
                    })?;

                let artifacts = resolve_artifacts(tests::scale_boot::register_artifacts)?;
                let resolver = petri::ArtifactResolver::resolver(&artifacts);

                let test = tests::scale_boot::ScaleBootTest::new(
                    profile,
                    args.mem_mb,
                    args.vms.clone(),
                    args.max_vms,
                    &resolver,
                )
                .context("scale_boot prep")?;

                let stats = pal_async::DefaultPool::run_with(async |driver| {
                    tests::scale_boot::run_scale_test(&test, &resolver, &driver).await
                })
                .context("scale_boot test failed")?;
                all_stats.extend(stats);
            }
            "memory" => {
                let profile =
                    tests::boot_time::BootProfile::from_name(&args.profile).ok_or_else(|| {
                        anyhow::anyhow!(
                            "unknown profile '{}'. Available: {}",
                            args.profile,
                            tests::boot_time::BootProfile::all_names().join(", ")
                        )
                    })?;

                let artifacts = resolve_artifacts(tests::memory::register_artifacts)?;
                let resolver = petri::ArtifactResolver::resolver(&artifacts);

                let test = tests::memory::MemoryTest::new(profile, args.mem_mb, &resolver)
                    .context("memory prep")?;

                let stats = pal_async::DefaultPool::run_with(async |driver| {
                    harness::run_cold_test(&test, &resolver, &driver, args.iterations).await
                })
                .context("memory test failed")?;
                all_stats.extend(stats);
            }
            _ => unreachable!(),
        }
    }

    // Build and write report.
    let report = report::PerfReport::new(all_stats)?;
    report.print_summary();

    let json = report.to_json()?;
    std::fs::write(&args.output, &json)
        .with_context(|| format!("failed to write report to {}", args.output.display()))?;
    println!("\nReport written to {}", args.output.display());

    Ok(())
}

fn cmd_package(args: PackageArgs) -> anyhow::Result<()> {
    use petri_artifacts_core::AsArtifactHandle;
    use petri_artifacts_core::ResolveTestArtifact;

    let resolver =
        petri_artifact_resolver_openvmm_known_paths::OpenvmmKnownPathsTestArtifactResolver::new("");

    // Resolve artifact paths via the standard resolver infrastructure.
    let burette_path =
        petri_artifact_resolver_openvmm_known_paths::get_output_executable_path("burette")
            .context("failed to find burette binary")?;
    let openvmm_path = resolver
        .resolve(petri_artifacts_vmm_test::artifacts::OPENVMM_NATIVE.erase())
        .context("failed to resolve openvmm binary")?;
    let pipette_path = resolver
        .resolve(petri_artifacts_common::artifacts::PIPETTE_LINUX_X64.erase())
        .context("failed to resolve pipette binary")?;
    let kernel_path = resolver
        .resolve(
            petri_artifacts_vmm_test::artifacts::loadable::LINUX_DIRECT_TEST_KERNEL_X64.erase(),
        )
        .context("failed to resolve vmlinux kernel")?;
    let initrd_path = resolver
        .resolve(
            petri_artifacts_vmm_test::artifacts::loadable::LINUX_DIRECT_TEST_INITRD_X64.erase(),
        )
        .context("failed to resolve initrd")?;

    // Stage files into a temporary directory.
    let staging = tempfile::tempdir().context("failed to create staging dir")?;
    let bundle = staging.path().join("burette_bundle");

    // (source_path, relative destination in bundle)
    let files: &[(&Path, &str)] = &[
        (&burette_path, "burette"),
        (&openvmm_path, "openvmm"),
        (&pipette_path, "pipette"),
        (&kernel_path, "x64/vmlinux"),
        (&initrd_path, "x64/initrd"),
    ];

    // Verify all source files exist.
    for (path, name) in files {
        anyhow::ensure!(
            path.exists(),
            "missing artifact: {} (expected at {})",
            name,
            path.display()
        );
    }

    // Copy files into staging directory, stripping debug symbols from
    // ELF binaries to reduce tarball size.
    for (src, name) in files {
        let dest = bundle.join(name);
        if let Some(parent) = dest.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("failed to create dir for {name}"))?;
        }
        std::fs::copy(src, &dest).with_context(|| format!("failed to copy {name}"))?;

        // Strip if it looks like an ELF executable (not vmlinux/initrd).
        if !name.contains('/') {
            let _ = std::process::Command::new("strip").arg(&dest).status();
        }

        println!("  adding {name} ({})", human_size(&dest)?);
    }

    // Create tarball using system tar.
    let status = std::process::Command::new("tar")
        .args(["czf"])
        .arg(&args.output)
        .arg("-C")
        .arg(staging.path())
        .arg("burette_bundle")
        .status()
        .context("failed to run tar")?;

    anyhow::ensure!(status.success(), "tar exited with {status}");

    let output_size = human_size(&args.output)?;
    println!("\nCreated {} ({output_size})", args.output.display());
    println!("\nTo use on a remote machine:");
    println!("  scp {} remote:", args.output.display());
    println!("  ssh remote");
    println!(
        "  tar xzf {}",
        args.output.file_name().unwrap().to_string_lossy()
    );
    println!("  cd burette_bundle");
    println!("  VMM_TESTS_CONTENT_DIR=$PWD ./burette run -o report.json");

    Ok(())
}

fn human_size(path: &Path) -> anyhow::Result<String> {
    let size = std::fs::metadata(path)
        .with_context(|| format!("failed to stat {}", path.display()))?
        .len();
    Ok(if size >= 1024 * 1024 {
        format!("{:.1} MiB", size as f64 / (1024.0 * 1024.0))
    } else if size >= 1024 {
        format!("{:.1} KiB", size as f64 / 1024.0)
    } else {
        format!("{size} B")
    })
}

fn cmd_compare(args: CompareArgs) -> anyhow::Result<()> {
    let baseline_json = std::fs::read_to_string(&args.baseline)
        .with_context(|| format!("failed to read {}", args.baseline.display()))?;
    let candidate_json = std::fs::read_to_string(&args.candidate)
        .with_context(|| format!("failed to read {}", args.candidate.display()))?;

    let baseline = report::PerfReport::from_json(&baseline_json)?;
    let candidate = report::PerfReport::from_json(&candidate_json)?;

    let comparison = report::compare_reports(&baseline, &candidate);
    comparison.print_summary();

    if let Some(output) = args.output {
        let json = comparison.to_json()?;
        std::fs::write(&output, &json)
            .with_context(|| format!("failed to write comparison to {}", output.display()))?;
        println!("\nComparison written to {}", output.display());
    }

    Ok(())
}
