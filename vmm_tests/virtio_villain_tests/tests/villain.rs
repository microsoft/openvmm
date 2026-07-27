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
//! a device that is absent makes the guest emit `[SKIP]`, which the harness
//! treats as a failure unless the device is one the kitchen-sink VM
//! deliberately does not attach — see [`supported_devices`]) with
//! `vv.test=<id>` on the kernel command line, waits for the VM to halt, and
//! reads the `[TAG] <id>` verdict from petri's teed serial log. The virtio
//! devices are attached over PCIe, except for villain's virtio-MMIO transport
//! tests (`TEST_FLAG_MMIO` / `M####`), which are booted with the devices on the
//! virtio-MMIO bus so the guest actually exposes an MMIO transport to probe
//! (see [`villain::VillainTest::is_mmio`] and [`run::run_one`]). Villain tests
//! that OpenVMM is known to fail ([`known_failures`]) are marked *ignored*, so
//! CI skips them but they can still be run locally with `--run-ignored`.
//!
//! x86_64/KVM. The villain `initramfs.cpio.gz` and `tests.tsv` are resolved
//! from the `openvmm-deps` release artifact via petri's known-paths resolver
//! (staged into `VMM_TESTS_CONTENT_DIR` by flowey), the same way the guest
//! kernel (the existing OpenVMM linux-direct test `vmlinux`) is. For local
//! development against a custom villain build, `--villain-initramfs` /
//! `--villain-tsv` (or the `VILLAIN_INITRAMFS` / `VILLAIN_TSV` env vars)
//! override the resolved artifact.
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
use virtio_villain_tests::supported_devices;
use virtio_villain_tests::villain;

#[derive(Parser)]
#[command(
    name = "virtio_villain_tests",
    about = "Run virtio-villain against OpenVMM"
)]
struct Cli {
    /// Override the villain `initramfs.cpio.gz` with a local path (for
    /// developing against a custom villain build). Falls back to the
    /// `VILLAIN_INITRAMFS` environment variable; if neither is set, the
    /// initramfs is resolved from the staged `openvmm-deps` artifact.
    #[arg(long)]
    villain_initramfs: Option<PathBuf>,

    /// Override villain's `tests.tsv` (from `init --list-tsv`) with a local
    /// path. Falls back to the `VILLAIN_TSV` environment variable; if neither
    /// is set, the tsv is resolved from the staged `openvmm-deps` artifact.
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

/// Require the host-architecture villain initramfs artifact.
fn require_villain_initrd(resolver: &petri::ArtifactResolver<'_>) -> petri::ResolvedArtifact {
    use petri_artifacts_vmm_test::artifacts::virtio_villain;
    match MachineArch::host() {
        MachineArch::X86_64 => resolver
            .require(virtio_villain::VIRTIO_VILLAIN_INITRD_X64)
            .erase(),
        MachineArch::Aarch64 => resolver
            .require(virtio_villain::VIRTIO_VILLAIN_INITRD_AARCH64)
            .erase(),
    }
}

/// Require the host-architecture villain `tests.tsv` artifact.
fn require_villain_tsv(resolver: &petri::ArtifactResolver<'_>) -> petri::ResolvedArtifact {
    use petri_artifacts_vmm_test::artifacts::virtio_villain;
    match MachineArch::host() {
        MachineArch::X86_64 => resolver
            .require(virtio_villain::VIRTIO_VILLAIN_TSV_X64)
            .erase(),
        MachineArch::Aarch64 => resolver
            .require(virtio_villain::VIRTIO_VILLAIN_TSV_AARCH64)
            .erase(),
    }
}

/// Resolve a single villain artifact path via petri's known-paths resolver
/// (staged under `VMM_TESTS_CONTENT_DIR`). Used for the initramfs and the tsv,
/// each of which can be overridden by a CLI flag / env var for local dev.
fn resolve_villain_file(
    pick: impl Fn(&petri::ArtifactResolver<'_>) -> petri::ResolvedArtifact,
) -> anyhow::Result<PathBuf> {
    let resolver =
        petri_artifact_resolver_openvmm_known_paths::OpenvmmKnownPathsTestArtifactResolver::new("");
    let mut requirements = petri::TestArtifactRequirements::new();
    pick(&petri::ArtifactResolver::collector(&mut requirements));
    let artifacts = requirements
        .resolve(&resolver)
        .context("failed to resolve villain artifact")?;
    Ok(pick(&petri::ArtifactResolver::resolver(&artifacts))
        .get()
        .to_path_buf())
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

fn main() -> anyhow::Result<()> {
    let mut cli = Cli::parse();

    // Local-dev overrides: a custom villain build's initramfs / tsv can be
    // pointed at explicitly (CLI flag, else env var). When unset, the files are
    // resolved from the staged `openvmm-deps` artifact via petri's known-paths
    // resolver.
    let initramfs_override = cli
        .villain_initramfs
        .clone()
        .or_else(|| std::env::var_os("VILLAIN_INITRAMFS").map(PathBuf::from));
    let tsv_override = cli
        .villain_tsv
        .clone()
        .or_else(|| std::env::var_os("VILLAIN_TSV").map(PathBuf::from));

    // The test list comes from the tsv, which is needed for both `--list` and
    // running. Prefer the override; otherwise resolve it from the artifact.
    let tsv = match tsv_override {
        Some(tsv) => tsv,
        None => resolve_villain_file(require_villain_tsv)
            .context("failed to resolve villain tests.tsv")?,
    };
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
        let initramfs = match initramfs_override {
            Some(initramfs) => initramfs,
            None => resolve_villain_file(require_villain_initrd)
                .context("failed to resolve villain initramfs")?,
        };
        (Some(resolve_artifacts()?), initramfs, Some(base_logger))
    };

    let params = run::VmParams {
        initramfs,
        arch: MachineArch::host(),
        mem_bytes: cli
            .mem_mb
            .checked_mul(1024 * 1024)
            .with_context(|| format!("--mem-mb value {} is too large", cli.mem_mb))?,
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
            let version = test.version.clone();
            let spec_section = test.spec_section.clone();
            let device_id = test.device_id;
            let flags = test.flags;
            let required_features = test.required_features;
            let min_queues = test.min_queues;
            let mmio = test.is_mmio();
            let expected_skip = supported_devices::skip_expected(device_id);
            // Ignore (don't even boot a VM for) tests whose target device we
            // don't attach — they would only self-SKIP, so booting ~one VM each
            // is wasted CI time. They report as ignored, not as false passes.
            // Known product failures are ignored too (see `known_failures`).
            let ignored = expected_skip || known_failures::lookup(&name).is_some();
            Trial::test(test.name.clone(), move || -> Result<(), Failed> {
                let artifacts = artifacts
                    .as_ref()
                    .context("artifacts were not resolved (internal error)")
                    .map_err(|e| Failed::from(format!("{e:#}")))?;
                tracing::info!(
                    name,
                    desc,
                    version,
                    spec_section,
                    device_id = format_args!("{device_id:#06x}"),
                    flags,
                    required_features = format_args!("{required_features:#018x}"),
                    min_queues,
                    mmio,
                    "running villain test"
                );
                let test_dir = base_log_dir.join(sanitize(&name));
                fs_err::create_dir_all(&test_dir).map_err(|e| Failed::from(format!("{e:#}")))?;
                let log_source = petri::new_log_source(&test_dir)
                    .context("failed to create per-test log source")
                    .map_err(|e| Failed::from(format!("{e:#}")))?;
                let result = run::run_one(&params, artifacts, &log_source, &name, mmio)
                    .and_then(villain::evaluate);
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
