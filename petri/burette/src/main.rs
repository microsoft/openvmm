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
//! burette run --test boot-time -o report.json
//!
//! # Run with custom iteration count
//! burette run --iterations 20 -o report.json
//!
//! # Compare two reports
//! burette compare baseline.json candidate.json
//! ```

mod harness;
mod iperf_helper;
mod report;
mod tests;

use anyhow::Context as _;
use clap::Parser;
use report::MetricStats;
use std::path::Path;
use std::path::PathBuf;
use std::sync::OnceLock;
use tests::boot_time::BootProfile;
use tests::disk_io::DiskBackend;
use tests::network::NetBackend;
use tests::network::NicBackend;

/// Available performance tests.
#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
enum TestName {
    /// Measures launch-to-pipette-connect time via Linux direct boot.
    BootTime,
    /// Measures boot time across concurrent VM counts.
    ScaleBoot,
    /// Measures VMM memory overhead.
    Memory,
    /// Network throughput via iperf3.
    Network,
    /// Block I/O throughput via fio (Alpine VM + data disk).
    DiskIo,
    /// virtio-fs file server throughput via fio.
    VirtioFs,
}

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

    /// Run only a specific test. Omit to run all.
    #[arg(long)]
    test: Option<TestName>,

    /// Override the number of iterations per test.
    #[arg(long)]
    iterations: Option<u32>,

    /// Boot time profile: defines the VM configuration to measure.
    #[arg(long, default_value = "minimal-private")]
    profile: BootProfile,

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

    /// NIC backend for the network test.
    #[arg(long, default_value = "vmbus")]
    nic: NicBackend,

    /// Network endpoint backend.
    #[arg(long, default_value = "consomme")]
    backend: NetBackend,

    /// Record `perf record -p <pid> -g` traces scoped to each test,
    /// saving per-test .data files in this directory. Linux only.
    #[arg(long)]
    perf_dir: Option<PathBuf>,

    /// Disk backend for the disk_io test.
    #[arg(long, default_value = "virtio-blk")]
    disk_backend: DiskBackend,

    /// Path to raw data disk file for the disk_io test.
    /// Must be on fast storage (e.g. NVMe) for meaningful results.
    /// If omitted, uses a RAM-backed disk (measures virtio/storvsc overhead
    /// without host filesystem noise).
    #[arg(long)]
    data_disk: Option<PathBuf>,

    /// Data disk size in GiB for the disk_io test.
    #[arg(long, default_value = "4")]
    data_disk_size_gib: u64,

    /// Test file size in MiB for the virtio_fs test.
    #[arg(long, default_value = "512")]
    virtiofs_file_size_mib: u64,
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

    /// Keep debug symbols in ELF binaries (larger bundle, but
    /// enables `perf report` symbol resolution).
    #[arg(long)]
    no_strip: bool,
}

fn main() -> anyhow::Result<()> {
    // Check for helper subprocess modes before any threads spawn.
    // The TAP helper must call unshare(CLONE_NEWUSER) while single-threaded.
    match std::env::args().nth(1).as_deref() {
        Some("iperf-helper") => {
            iperf_helper::run_helper();
            return Ok(());
        }
        #[cfg(target_os = "linux")]
        Some("tap-ns-helper") => {
            iperf_helper::linux::run_tap_helper();
            return Ok(());
        }
        _ => {}
    }

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

    let default_level = if args.diag {
        tracing::level_filters::LevelFilter::DEBUG
    } else {
        tracing::level_filters::LevelFilter::INFO
    };

    let log_source = petri::try_init_tracing(&args.log_dir, default_level)
        .context("failed to initialize tracing")?;
    LOG_SOURCE
        .set(log_source)
        .ok()
        .context("log source already initialized")?;

    // Determine which tests to run.
    let all_tests = [
        TestName::BootTime,
        TestName::ScaleBoot,
        TestName::Memory,
        TestName::Network,
        TestName::DiskIo,
        TestName::VirtioFs,
    ];
    let tests_to_run: Vec<TestName> = if let Some(name) = args.test {
        vec![name]
    } else {
        all_tests.to_vec()
    };

    let mut all_stats: Vec<MetricStats> = Vec::new();

    for test_name in &tests_to_run {
        match test_name {
            TestName::BootTime => {
                let artifacts = resolve_artifacts(tests::boot_time::register_artifacts)?;
                let resolver = petri::ArtifactResolver::resolver(&artifacts);

                let test = tests::boot_time::BootTimeTest::new(
                    args.profile,
                    args.diag,
                    args.mem_mb,
                    &resolver,
                )
                .context("boot_time prep")?;

                let stats = pal_async::DefaultPool::run_with(async |driver| {
                    harness::run_cold_test(&test, &resolver, &driver, args.iterations).await
                })
                .context("boot_time test failed")?;
                all_stats.extend(stats);
            }
            TestName::ScaleBoot => {
                let artifacts = resolve_artifacts(tests::scale_boot::register_artifacts)?;
                let resolver = petri::ArtifactResolver::resolver(&artifacts);

                let test = tests::scale_boot::ScaleBootTest::new(
                    args.profile,
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
            TestName::Memory => {
                let artifacts = resolve_artifacts(tests::memory::register_artifacts)?;
                let resolver = petri::ArtifactResolver::resolver(&artifacts);

                let test = tests::memory::MemoryTest::new(args.profile, args.mem_mb, &resolver)
                    .context("memory prep")?;

                let stats = pal_async::DefaultPool::run_with(async |driver| {
                    harness::run_cold_test(&test, &resolver, &driver, args.iterations).await
                })
                .context("memory test failed")?;
                all_stats.extend(stats);
            }
            TestName::Network => {
                let test = tests::network::NetworkTest {
                    diag: args.diag,
                    nic: args.nic,
                    backend: args.backend,
                    perf_dir: args.perf_dir.clone(),
                };

                let artifacts = resolve_artifacts(tests::network::register_artifacts)?;
                let resolver = petri::ArtifactResolver::resolver(&artifacts);

                let stats = pal_async::DefaultPool::run_with(async |driver| {
                    harness::run_warm_test(&test, &resolver, &driver, args.iterations).await
                })
                .context("network test failed")?;
                all_stats.extend(stats);
            }
            TestName::DiskIo => {
                let test = tests::disk_io::DiskIoTest {
                    diag: args.diag,
                    backend: args.disk_backend,
                    data_disk: args.data_disk.clone(),
                    data_disk_size_gib: args.data_disk_size_gib,
                    perf_dir: args.perf_dir.clone(),
                };

                let artifacts = resolve_artifacts(tests::disk_io::register_artifacts)?;
                let resolver = petri::ArtifactResolver::resolver(&artifacts);

                let stats = pal_async::DefaultPool::run_with(async |driver| {
                    harness::run_warm_test(&test, &resolver, &driver, args.iterations).await
                })
                .context("disk_io test failed")?;
                all_stats.extend(stats);
            }
            TestName::VirtioFs => {
                let test = tests::virtio_fs::VirtioFsTest {
                    diag: args.diag,
                    perf_dir: args.perf_dir.clone(),
                    file_size_mib: args.virtiofs_file_size_mib,
                };

                let artifacts = resolve_artifacts(tests::virtio_fs::register_artifacts)?;
                let resolver = petri::ArtifactResolver::resolver(&artifacts);

                let stats = pal_async::DefaultPool::run_with(async |driver| {
                    harness::run_warm_test(&test, &resolver, &driver, args.iterations).await
                })
                .context("virtio_fs test failed")?;
                all_stats.extend(stats);
            }
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
    let resolver =
        petri_artifact_resolver_openvmm_known_paths::OpenvmmKnownPathsTestArtifactResolver::new("");

    let bundle_name = petri_artifact_resolver_openvmm_known_paths::resolve_bundle_name;

    // Collect the union of all artifacts needed by every test, using the
    // same register_artifacts functions that cmd_run uses. This avoids
    // duplicating artifact lists and automatically adapts to the host arch.
    let all_registers: &[fn(&petri::ArtifactResolver<'_>)] = &[
        tests::boot_time::register_artifacts,
        tests::scale_boot::register_artifacts,
        tests::memory::register_artifacts,
        tests::network::register_artifacts,
        tests::disk_io::register_artifacts,
        tests::virtio_fs::register_artifacts,
    ];

    let mut requirements = petri::TestArtifactRequirements::new();
    for register in all_registers {
        register(&petri::ArtifactResolver::collector(&mut requirements));
    }

    // Deduplicate: required_artifacts may contain repeats across tests.
    let artifact_ids: Vec<_> = {
        let mut seen = std::collections::HashSet::new();
        requirements
            .required_artifacts()
            .filter(|id| seen.insert(*id))
            .collect()
    };

    // Resolve all artifacts at once — reports every missing artifact in
    // a single error rather than failing on the first one.
    let artifacts = requirements
        .resolve(&resolver)
        .context("failed to resolve test artifacts")?;

    let mut files: Vec<(PathBuf, String)> = Vec::new();

    // Add burette itself (not an artifact — it's our own binary).
    let burette_path =
        petri_artifact_resolver_openvmm_known_paths::get_output_executable_path("burette")
            .context("failed to find burette binary")?;
    files.push((burette_path, "burette".into()));

    // Build the file list from resolved artifacts.
    for id in artifact_ids {
        let path = artifacts.get(id).to_path_buf();
        let dest = if let Some(name) = bundle_name(id) {
            name.to_string()
        } else {
            path.file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_else(|| format!("{:?}", id))
        };
        files.push((path, dest));
    }

    // Stage files into a temporary directory.
    let staging = tempfile::tempdir().context("failed to create staging dir")?;
    let bundle = staging.path().join("burette_bundle");

    // Copy files into staging directory, stripping debug symbols from
    // ELF binaries to reduce tarball size.
    for (src, name) in &files {
        let dest = bundle.join(name);
        if let Some(parent) = dest.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("failed to create dir for {name}"))?;
        }
        std::fs::copy(src, &dest).with_context(|| format!("failed to copy {name}"))?;

        // Strip debug symbols from ELF executables to reduce tarball size.
        if !args.no_strip && matches!(name.as_str(), "burette" | "openvmm" | "pipette") {
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
