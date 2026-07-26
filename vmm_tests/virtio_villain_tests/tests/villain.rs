// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Standalone test runner that drives [virtio-villain] against OpenVMM.
//!
//! virtio-villain is a guest-side virtio protocol fault-injection / conformance
//! suite: a static musl `init` (PID 1) that walks the virtio transports itself
//! and injects out-of-spec virtqueue inputs, printing a verdict marker per test
//! on the serial console before powering off.
//!
//! This binary uses **petri as a library** (like `burette`) plus
//! **libtest-mimic** to expose one test case per villain test. Each case boots
//! a single "kitchen-sink" OpenVMM VM (every supported virtio device attached;
//! absent devices self-SKIP in the guest) with `vv.test=<id>` on the kernel
//! command line, waits for the VM to halt, and reads the `[TAG] <id>` verdict
//! from petri's teed serial log. Villain tests that OpenVMM is known to fail
//! ([`known_failures`]) are marked *ignored*, so CI skips them but they can
//! still be run locally with `--run-ignored`.
//!
//! Phase 1: PCI transport, x86_64/KVM. The villain `initramfs.cpio.gz` and
//! `tests.tsv` are supplied locally via `--villain-initramfs`/`--villain-tsv`
//! (or the `VILLAIN_INITRAMFS`/`VILLAIN_TSV` env vars); the guest kernel is the
//! existing OpenVMM linux-direct test `vmlinux`. A later phase resolves these
//! from the `openvmm-deps` release artifact via flowey.
//!
//! [virtio-villain]: https://github.com/weltling/virtio-villain

mod run {
    pub use virtio_villain_tests::run::*;
}

use anyhow::Context as _;
use clap::Parser;
use libtest_mimic::Failed;
use libtest_mimic::Trial;
use petri_artifacts_common::tags::MachineArch;
use std::path::PathBuf;
use virtio_villain_tests::known_failures;
use virtio_villain_tests::villain;
use virtio_villain_tests::villain::VerdictScan;

#[derive(Parser)]
#[command(
    name = "virtio_villain_tests",
    about = "Run virtio-villain against OpenVMM"
)]
struct Cli {
    /// Path to villain's `initramfs.cpio.gz`. Falls back to the
    /// `VILLAIN_INITRAMFS` environment variable.
    #[arg(long)]
    villain_initramfs: Option<PathBuf>,

    /// Path to villain's `tests.tsv` (from `init --list-tsv`). Falls back to
    /// the `VILLAIN_TSV` environment variable.
    #[arg(long)]
    villain_tsv: Option<PathBuf>,

    /// Base directory for per-test petri logs. Defaults to `TEST_OUTPUT_PATH`
    /// (the same env var the petri known-paths resolver honors, so CI can point
    /// all runners at one publishable directory) if set, else
    /// `vmm_test_results/virtio_villain`.
    #[arg(long)]
    log_dir: Option<PathBuf>,

    /// Guest RAM in MiB.
    #[arg(long, default_value_t = 512)]
    mem_mb: u64,

    #[command(flatten)]
    inner: libtest_mimic::Arguments,
}

/// Two-pass artifact resolution (the petri-tool / burette pattern): resolve the
/// reused OpenVMM linux-direct kernel for the host architecture.
fn register_artifacts(resolver: &petri::ArtifactResolver<'_>) {
    let firmware = petri::Firmware::linux_direct(resolver, MachineArch::host());
    petri::PetriVmArtifacts::<petri::openvmm::OpenVmmPetriBackend>::new(
        resolver,
        firmware,
        MachineArch::host(),
        false,
    );
}

fn resolve_artifacts() -> anyhow::Result<petri::TestArtifacts> {
    let resolver =
        petri_artifact_resolver_openvmm_known_paths::OpenvmmKnownPathsTestArtifactResolver::new("");
    let mut requirements = petri::TestArtifactRequirements::new();
    register_artifacts(&petri::ArtifactResolver::collector(&mut requirements));
    let artifacts = requirements
        .resolve(&resolver)
        .context("failed to resolve test artifacts")?;
    register_artifacts(&petri::ArtifactResolver::resolver(&artifacts));
    Ok(artifacts)
}

/// Sanitize a villain test name into a filesystem-safe directory component.
fn sanitize(name: &str) -> String {
    name.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_') {
                c
            } else {
                '_'
            }
        })
        .collect()
}

/// Turn a scan result into a test outcome. Known-failing tests are marked
/// ignored at trial-construction time (see [`known_failures`]) rather than
/// inverted here, so this is a straight good→Ok / bad→Err mapping.
fn evaluate(scan: VerdictScan) -> anyhow::Result<()> {
    let (good, detail) = match scan {
        VerdictScan::Found(v) => (v.is_good(), format!("{v:?}")),
        VerdictScan::MarkerMissing => (false, "WEDGED (no verdict marker for this test)".into()),
        VerdictScan::NoMarkers => (false, "FAIL (guest emitted no villain markers)".into()),
    };

    if good {
        Ok(())
    } else {
        anyhow::bail!("{detail}")
    }
}

fn main() -> anyhow::Result<()> {
    let mut cli = Cli::parse();

    let initramfs = cli
        .villain_initramfs
        .clone()
        .or_else(|| std::env::var_os("VILLAIN_INITRAMFS").map(PathBuf::from));
    let tsv = cli
        .villain_tsv
        .clone()
        .or_else(|| std::env::var_os("VILLAIN_TSV").map(PathBuf::from))
        .context("villain tsv not specified (--villain-tsv or VILLAIN_TSV)")?;

    let tests = villain::parse_tsv(&tsv)?;

    // Resolve the base log dir: explicit `--log-dir`, else `TEST_OUTPUT_PATH`
    // (petri convention, so CI publishes all runners' logs uniformly), else a
    // repo-relative default.
    let log_dir = cli
        .log_dir
        .clone()
        .or_else(|| std::env::var_os("TEST_OUTPUT_PATH").map(PathBuf::from))
        .unwrap_or_else(|| PathBuf::from("vmm_test_results/virtio_villain"));

    // Listing (used by nextest for discovery) must emit *only* the libtest
    // `<name>: test` lines on stdout — so skip tracing init (which prints an
    // `[[ATTACHMENT]]` line), the log dir, and VM artifact/initramfs resolution.
    // Those are only needed when we actually run VMs.
    let (artifacts, initramfs, _base_logger) = if cli.inner.list {
        (None, PathBuf::new(), None)
    } else {
        // Install the global tracing subscriber once, rooted at the base log
        // dir. Per-test log sources (each with their own serial `linux.log`)
        // are created below via `petri::new_log_source`.
        fs_err::create_dir_all(&log_dir)?;
        let base_logger =
            petri::try_init_tracing(&log_dir, tracing::level_filters::LevelFilter::INFO)
                .context("failed to initialize tracing")?;
        let initramfs = initramfs.context(
            "villain initramfs not specified (--villain-initramfs or VILLAIN_INITRAMFS)",
        )?;
        (Some(resolve_artifacts()?), initramfs, Some(base_logger))
    };

    let params = run::VmParams {
        initramfs,
        arch: MachineArch::host(),
        mem_bytes: cli.mem_mb * 1024 * 1024,
    };

    let base_log_dir = log_dir.clone();
    let trials: Vec<Trial> = tests
        .into_iter()
        .map(|test| {
            let params = params.clone();
            let artifacts = artifacts.clone();
            let base_log_dir = base_log_dir.clone();
            let name = test.name.clone();
            let desc = test.desc.clone();
            let device_id = test.device_id;
            let ignored = known_failures::lookup(&name).is_some();
            Trial::test(test.name.clone(), move || -> Result<(), Failed> {
                let artifacts = artifacts
                    .as_ref()
                    .context("artifacts were not resolved (internal error)")
                    .map_err(|e| Failed::from(format!("{e:#}")))?;
                tracing::info!(name, desc, device_id, "running villain test");
                let test_dir = base_log_dir.join(sanitize(&name));
                fs_err::create_dir_all(&test_dir).map_err(|e| Failed::from(format!("{e:#}")))?;
                let log_source = petri::new_log_source(&test_dir)
                    .context("failed to create per-test log source")
                    .map_err(|e| Failed::from(format!("{e:#}")))?;
                let result =
                    run::run_one(&params, artifacts, &log_source, &name).and_then(evaluate);
                // Write the petri.passed/petri.failed marker (and log the
                // outcome to petri.jsonl) so the logview uploader counts this
                // test. villain tests are never "unstable" — known failures are
                // skipped via the ignored flag instead.
                log_source.log_test_result(&name, &result, false);
                result.map_err(|e| Failed::from(format!("{e:#}")))
            })
            .with_ignored_flag(ignored)
        })
        .collect();

    // These VMs are heavy; run them one at a time in-process. Under nextest,
    // each test runs in its own process regardless.
    if cli.inner.test_threads.is_none() {
        cli.inner.test_threads = Some(1);
    }

    libtest_mimic::run(&cli.inner, trials).exit();
}
