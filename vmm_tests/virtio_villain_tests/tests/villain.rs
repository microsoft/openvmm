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
//! The villain `initramfs.cpio.gz` and `tests.tsv` are resolved from the
//! `openvmm-deps` release artifact via petri's known-paths resolver (staged
//! into `VMM_TESTS_CONTENT_DIR` by flowey), the same way the guest kernel (the
//! existing OpenVMM linux-direct test `vmlinux`) is. For local development
//! against a custom villain build, the `VILLAIN_INITRAMFS` and `VILLAIN_TSV`
//! environment variables override the resolved artifacts.
//!
//! [virtio-villain]: https://github.com/weltling/virtio-villain

mod run {
    pub use virtio_villain_tests::run::*;
}

use anyhow::Context as _;
use libtest_mimic::Failed;
use libtest_mimic::Trial;
use petri_artifacts_common::tags::MachineArch;
use std::path::PathBuf;
use virtio_villain_tests::known_failures;
use virtio_villain_tests::supported_devices;
use virtio_villain_tests::villain;

/// 512 MiB is enough for the linux-direct kernel plus the villain initramfs and
/// keeps the many one-VM-per-test trials lightweight.
const VILLAIN_MEM_BYTES: u64 = 512 * 1024 * 1024;

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
/// (staged under `VMM_TESTS_CONTENT_DIR`). Used for the tsv when it is not
/// overridden by an env var; this does not initialize tracing.
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

fn resolve_trial_artifacts(name: &str) -> anyhow::Result<(petri::TestArtifacts, PathBuf)> {
    let resolver =
        petri_artifact_resolver_openvmm_known_paths::OpenvmmKnownPathsTestArtifactResolver::new(
            name,
        );
    let mut requirements = petri::TestArtifactRequirements::new();
    register_artifacts(&petri::ArtifactResolver::collector(&mut requirements));
    require_villain_initrd(&petri::ArtifactResolver::collector(&mut requirements));
    requirements.require(
        petri_artifacts_common::artifacts::TEST_LOG_DIRECTORY,
        petri::RemoteAccess::LocalOnly,
        false,
    );
    let artifacts = requirements
        .resolve(&resolver)
        .context("failed to resolve test artifacts")?;
    let resolver = petri::ArtifactResolver::resolver(&artifacts);
    register_artifacts(&resolver);
    let initrd = require_villain_initrd(&resolver).get().to_path_buf();
    Ok((artifacts, initrd))
}

fn main() -> anyhow::Result<()> {
    let mut args = libtest_mimic::Arguments::from_args();

    let initramfs_override = std::env::var_os("VILLAIN_INITRAMFS").map(PathBuf::from);
    let tsv_override = std::env::var_os("VILLAIN_TSV").map(PathBuf::from);

    // The test list comes from the tsv, which is needed for both `--list` and
    // running. Prefer the override; otherwise resolve it from the artifact.
    let tsv = match tsv_override {
        Some(tsv) => tsv,
        None => resolve_villain_file(require_villain_tsv)
            .context("failed to resolve villain tests.tsv")?,
    };
    let tests = villain::parse_tsv(&tsv)?;
    let trials: Vec<Trial> = tests
        .into_iter()
        .map(|test| {
            let initramfs_override = initramfs_override.clone();
            let name = test.name.clone();
            let desc = test.desc.clone();
            let version = test.version.clone();
            let spec_section = test.spec_section.clone();
            let device_id = test.device_id;
            let flags = test.flags;
            let required_features = test.required_features;
            let min_queues = test.min_queues;
            let mmio = test.is_mmio();
            // Ignore (don't even boot a VM for) tests that can only self-SKIP on
            // the kitchen-sink VM: a device we don't attach, a feature/queue count
            // we don't offer, or a villain harness limitation. They report as
            // ignored, not as false passes; see `supported_devices::expected_skip`
            // for the rule and the reason string it returns. Known product
            // failures are ignored too (see `known_failures`).
            let expected_skip = supported_devices::expected_skip(
                &name,
                device_id,
                mmio,
                required_features,
                min_queues,
            );
            if let Some(reason) = expected_skip {
                tracing::debug!(name, reason, "ignoring villain test (expected skip)");
            }
            let ignored = expected_skip.is_some() || known_failures::lookup(&name).is_some();
            Trial::test(test.name.clone(), move || -> Result<(), Failed> {
                let (artifacts, initrd) =
                    resolve_trial_artifacts(&name).map_err(|e| Failed::from(format!("{e:#}")))?;
                let output_dir =
                    artifacts.get(petri_artifacts_common::artifacts::TEST_LOG_DIRECTORY);
                let logger =
                    petri::try_init_tracing(output_dir, tracing::level_filters::LevelFilter::INFO)
                        .context("failed to initialize tracing")
                        .map_err(|e| Failed::from(format!("{e:#}")))?;
                logger.log_test_start(&name);
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
                let initramfs = initramfs_override.clone().unwrap_or_else(|| initrd.clone());
                let params = run::VmParams {
                    initramfs,
                    arch: MachineArch::host(),
                    mem_bytes: VILLAIN_MEM_BYTES,
                };
                let result = run::run_one(&params, &artifacts, &logger, &name, mmio)
                    .and_then(villain::evaluate);
                // Write the petri.passed/petri.failed marker (and log the
                // outcome to petri.jsonl) so the logview uploader counts this
                // test. villain tests are never "unstable" — known failures are
                // skipped via the ignored flag instead.
                logger.log_test_result(&result, false);
                result.map_err(|e| Failed::from(format!("{e:#}")))
            })
            .with_ignored_flag(ignored)
        })
        .collect();

    // These VMs are heavy; run them one at a time in-process. Under nextest,
    // each selected test runs in its own process. Running this binary directly
    // with multiple non-ignored trials is not supported because petri tracing is
    // process-global and can only be initialized once.
    if args.test_threads.is_none() {
        args.test_threads = Some(1);
    }

    libtest_mimic::run(&args, trials).exit();
}
