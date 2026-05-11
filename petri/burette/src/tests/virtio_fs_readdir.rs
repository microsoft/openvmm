// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! virtio-fs directory listing (readdir) performance test.
//!
//! Boots a minimal Linux VM with a virtio-fs device, creates a directory
//! populated with many small files on the host side, and measures how long
//! the guest takes to enumerate the directory. This directly exercises the
//! FUSE readdir / readdirplus paths in the VMM.
//!
//! The PR that removes per-entry `lookup_helper()` calls from the plain
//! readdir path should show a significant improvement in the
//! `virtiofs_readdir_plain_entries_per_sec` metric, while
//! `virtiofs_readdir_plus_entries_per_sec` (which still requires per-entry
//! lookups for dentry pre-population) should remain roughly unchanged.
//!
//! Reported metrics:
//! - `virtiofs_readdir_plain_time` — per-pass plain readdir time (s)
//! - `virtiofs_readdir_plain_entries_per_sec` — plain readdir throughput
//! - `virtiofs_readdir_plus_time` — readdirplus time (s)
//! - `virtiofs_readdir_plus_entries_per_sec` — readdirplus throughput
//!
//! Layout inside the guest (same as the fio-based virtio_fs test):
//!
//! ```text
//! /perf            <- erofs perf rootfs (read-only)
//! /perf/tmp        <- writable tmpfs
//! /perf/tmp/vfs    <- virtio-fs mount, backed by host tempdir
//! ```

use crate::report::MetricResult;
use anyhow::Context as _;
use petri::pipette::cmd;
use petri_artifacts_common::tags::MachineArch;
use std::path::PathBuf;

/// Number of empty files to create in the test directory.
const DEFAULT_FILE_COUNT: u32 = 10_000;

/// Number of times to repeat the plain readdir listing per measurement.
/// The first pass uses READDIRPLUS (priming the READDIRPLUS_AUTO state);
/// subsequent passes use plain READDIR — the path the PR optimizes.
const PLAIN_READDIR_LOOPS: u32 = 20;

/// Tag used for the virtio-fs device.
const VFS_TAG: &str = "readdir_vfs";

/// virtio-fs readdir perf test.
pub struct VirtioFsReaddirTest {
    /// Print guest diagnostics.
    pub diag: bool,
    /// If set, record per-phase perf traces in this directory.
    pub perf_dir: Option<PathBuf>,
    /// Number of files to create in the test directory.
    pub file_count: u32,
}

impl Default for VirtioFsReaddirTest {
    fn default() -> Self {
        Self {
            diag: false,
            perf_dir: None,
            file_count: DEFAULT_FILE_COUNT,
        }
    }
}

/// State kept across warm iterations.
pub struct VirtioFsReaddirState {
    vm: petri::PetriVm<petri::openvmm::OpenVmmPetriBackend>,
    agent: petri::pipette::PipetteClient,
    /// Host tempdir backing the virtio-fs mount. Held alive for the
    /// lifetime of the test; deleted on Drop.
    _vfs_root: tempfile::TempDir,
    /// Number of files in the test directory.
    file_count: u32,
}

fn build_firmware(resolver: &petri::ArtifactResolver<'_>) -> petri::Firmware {
    petri::Firmware::linux_direct(resolver, MachineArch::host())
}

fn require_petritools_erofs(
    resolver: &petri::ArtifactResolver<'_>,
) -> petri_artifacts_core::ResolvedArtifact {
    use petri_artifacts_vmm_test::artifacts::petritools::*;
    match MachineArch::host() {
        MachineArch::X86_64 => resolver.require(PETRITOOLS_EROFS_X64).erase(),
        MachineArch::Aarch64 => resolver.require(PETRITOOLS_EROFS_AARCH64).erase(),
    }
}

/// Register artifacts needed by the readdir test.
pub fn register_artifacts(resolver: &petri::ArtifactResolver<'_>) {
    let firmware = build_firmware(resolver);
    petri::PetriVmArtifacts::<petri::openvmm::OpenVmmPetriBackend>::new(
        resolver,
        firmware,
        MachineArch::host(),
        true,
    );
    require_petritools_erofs(resolver);
}

impl crate::harness::WarmPerfTest for VirtioFsReaddirTest {
    type State = VirtioFsReaddirState;

    fn name(&self) -> &str {
        "virtio_fs_readdir"
    }

    fn warmup_iterations(&self) -> u32 {
        1
    }

    async fn setup(
        &self,
        resolver: &petri::ArtifactResolver<'_>,
        driver: &pal_async::DefaultDriver,
    ) -> anyhow::Result<VirtioFsReaddirState> {
        anyhow::ensure!(self.file_count > 0, "file_count must be greater than 0");

        let firmware = build_firmware(resolver);

        let artifacts = petri::PetriVmArtifacts::<petri::openvmm::OpenVmmPetriBackend>::new(
            resolver,
            firmware,
            MachineArch::host(),
            true,
        )
        .context("firmware/arch not compatible with OpenVMM backend")?;

        let mut post_test_hooks = Vec::new();
        let log_source = crate::log_source();
        let params = petri::PetriTestParams {
            test_name: "virtio_fs_readdir",
            logger: &log_source,
            post_test_hooks: &mut post_test_hooks,
        };

        let erofs_path = require_petritools_erofs(resolver);
        let erofs_file = fs_err::File::open(&erofs_path)?;

        let vfs_root = tempfile::Builder::new()
            .prefix("burette-readdir-")
            .tempdir()
            .context("failed to create host vfs tempdir")?;
        let vfs_root_path = vfs_root.path().to_string_lossy().into_owned();
        tracing::info!(host_path = %vfs_root_path, "virtio-fs readdir host root");

        // Create test files on the host side so we don't spend guest CPU
        // time on file creation. This populates the directory before the
        // VM ever touches it.
        let test_dir = vfs_root.path().join("readdir_bench");
        std::fs::create_dir(&test_dir).context("failed to create test dir on host")?;
        for i in 0..self.file_count {
            let path = test_dir.join(format!("file_{i:06}"));
            std::fs::File::create(&path)
                .with_context(|| format!("failed to create host file {i}"))?;
        }
        tracing::info!(file_count = self.file_count, "created test files on host");

        let mut builder = petri::PetriVmBuilder::minimal(params, artifacts, driver)?
            .with_processor_topology(petri::ProcessorTopology {
                vp_count: 2,
                ..Default::default()
            })
            .with_memory(petri::MemoryConfig {
                startup_bytes: 1024 * 1024 * 1024, // 1 GB
                ..Default::default()
            });

        builder = builder.modify_backend(move |b| {
            b.with_nic()
                .with_pcie_root_topology(1, 1, 2)
                .with_custom_config(move |c| {
                    use disk_backend_resources::FileDiskHandle;
                    use openvmm_defs::config::PcieDeviceConfig;
                    use vm_resource::IntoResource;

                    // erofs perf rootfs on port 0 (read-only).
                    c.pcie_devices.push(PcieDeviceConfig {
                        port_name: "s0rc0rp0".into(),
                        resource: virtio_resources::VirtioPciDeviceHandle(
                            virtio_resources::blk::VirtioBlkHandle {
                                disk: FileDiskHandle(erofs_file.into()).into_resource(),
                                read_only: true,
                            }
                            .into_resource(),
                        )
                        .into_resource(),
                    });

                    // virtio-fs on port 1, backed by host tempdir.
                    c.pcie_devices.push(PcieDeviceConfig {
                        port_name: "s0rc0rp1".into(),
                        resource: virtio_resources::VirtioPciDeviceHandle(
                            virtio_resources::fs::VirtioFsHandle {
                                tag: VFS_TAG.into(),
                                fs: virtio_resources::fs::VirtioFsBackend::HostFs {
                                    root_path: vfs_root_path,
                                    mount_options: String::new(),
                                },
                            }
                            .into_resource(),
                        )
                        .into_resource(),
                    });
                })
        });

        if !self.diag {
            builder = builder.without_screenshots();
        } else {
            builder = builder.with_serial_output();
        }

        let (vm, agent) = builder.run().await.context("failed to boot minimal VM")?;

        // Mount erofs and prepare chroot.
        agent
            .mount("/dev/vda", "/perf", "erofs", 1 /* MS_RDONLY */, true)
            .await
            .context("failed to mount erofs on /dev/vda")?;
        agent
            .prepare_chroot("/perf")
            .await
            .context("failed to prepare chroot at /perf")?;

        // Mount virtio-fs.
        agent
            .mount(VFS_TAG, "/perf/tmp/vfs", "virtiofs", 0, true)
            .await
            .context("failed to mount virtio-fs (guest kernel may need CONFIG_VIRTIO_FS=y)")?;

        Ok(VirtioFsReaddirState {
            vm,
            agent,
            _vfs_root: vfs_root,
            file_count: self.file_count,
        })
    }

    async fn run_once(
        &self,
        state: &mut VirtioFsReaddirState,
    ) -> anyhow::Result<Vec<MetricResult>> {
        let mut metrics = Vec::new();
        let pid = state.vm.backend().pid();
        let mut recorder = crate::harness::PerfRecorder::new(self.perf_dir.as_deref(), pid)?;
        let file_count = state.file_count as f64;

        // Drop all caches before each iteration so readdir goes through FUSE.
        let sh = state.agent.unix_shell();
        let drop_cmd = "sync; echo 3 > /proc/sys/vm/drop_caches";
        cmd!(sh, "sh -c {drop_cmd}")
            .read()
            .await
            .context("failed to drop caches")?;

        // --- Plain readdir (ls -f via chroot) ---
        // Use `ls -f` from the petritools erofs (GNU coreutils), which
        // lists directory entries without sorting or stat'ing. This is
        // the workload that triggers FUSE READDIR (not READDIRPLUS) via
        // the kernel's READDIRPLUS_AUTO fallback after the first pass.
        //
        // Do one unmeasured priming pass first: the kernel starts with
        // READDIRPLUS and switches to plain READDIR once it sees the
        // entries aren't being stat'd. Then measure `loops` iterations
        // of pure plain READDIR.
        let loops = PLAIN_READDIR_LOOPS;
        {
            let mut sh = state.agent.unix_shell();
            sh.chroot("/perf");
            cmd!(sh, "ls -f /tmp/vfs/readdir_bench")
                .read()
                .await
                .context("plain readdir priming pass failed")?;
        }
        recorder.start("virtiofs_readdir_plain")?;
        let plain_output = {
            let mut sh = state.agent.unix_shell();
            sh.chroot("/perf");
            let loops_str = loops.to_string();
            // Use `date +%s.%N` (GNU coreutils) for nanosecond precision.
            let script = concat!(
                "START=$(date +%s.%N); ",
                "i=0; while [ $i -lt $LOOPS ]; do ",
                "ls -f /tmp/vfs/readdir_bench > /dev/null; ",
                "i=$((i+1)); ",
                "done; ",
                "END=$(date +%s.%N); ",
                "echo elapsed $(awk \"BEGIN {print $END - $START}\")",
            );
            let full_script = format!("LOOPS={loops_str}; {script}");
            cmd!(sh, "sh -c {full_script}")
                .read()
                .await
                .context("plain readdir (ls -f) failed")?
        };
        recorder.stop()?;

        let total_plain_secs = parse_elapsed(&plain_output, "plain readdir")?;
        let plain_secs = total_plain_secs / loops as f64;
        let plain_entries_per_sec = file_count / plain_secs;
        tracing::info!(
            total_plain_secs,
            loops,
            plain_secs,
            plain_entries_per_sec,
            "plain readdir complete"
        );
        metrics.push(MetricResult {
            name: "virtiofs_readdir_plain_time".to_string(),
            unit: "s".to_string(),
            value: plain_secs,
        });
        metrics.push(MetricResult {
            name: "virtiofs_readdir_plain_entries_per_sec".to_string(),
            unit: "entries/s".to_string(),
            value: plain_entries_per_sec,
        });

        // Drop caches before the readdirplus test.
        let sh = state.agent.unix_shell();
        cmd!(sh, "sh -c {drop_cmd}")
            .read()
            .await
            .context("failed to drop caches")?;

        // --- Readdirplus (ls -l via chroot) ---
        // `ls -l` stat's every entry, which keeps the kernel using
        // readdirplus for dentry pre-population. This serves as a
        // control: the PR should not affect this path.
        recorder.start("virtiofs_readdir_plus")?;
        let plus_output = {
            let mut sh = state.agent.unix_shell();
            sh.chroot("/perf");
            let script = concat!(
                "START=$(date +%s.%N); ",
                "ls -l /tmp/vfs/readdir_bench > /dev/null; ",
                "END=$(date +%s.%N); ",
                "echo elapsed $(awk \"BEGIN {print $END - $START}\")",
            );
            cmd!(sh, "sh -c {script}")
                .read()
                .await
                .context("readdirplus (ls -l) failed")?
        };
        recorder.stop()?;

        let plus_secs = parse_elapsed(&plus_output, "readdirplus")?;
        let plus_entries_per_sec = file_count / plus_secs;
        tracing::info!(plus_secs, plus_entries_per_sec, "readdirplus complete");
        metrics.push(MetricResult {
            name: "virtiofs_readdir_plus_time".to_string(),
            unit: "s".to_string(),
            value: plus_secs,
        });
        metrics.push(MetricResult {
            name: "virtiofs_readdir_plus_entries_per_sec".to_string(),
            unit: "entries/s".to_string(),
            value: plus_entries_per_sec,
        });

        Ok(metrics)
    }

    async fn teardown(&self, state: VirtioFsReaddirState) -> anyhow::Result<()> {
        state.agent.power_off().await?;
        state.vm.wait_for_clean_teardown().await?;
        Ok(())
    }
}

/// Parse the elapsed time from the shell script output.
///
/// Expects a line of the form `elapsed <seconds>` somewhere in the output.
fn parse_elapsed(output: &str, label: &str) -> anyhow::Result<f64> {
    for line in output.lines() {
        let line = line.trim();
        if let Some(rest) = line.strip_prefix("elapsed ") {
            let secs: f64 = rest
                .trim()
                .parse()
                .with_context(|| format!("{label}: failed to parse elapsed time: {rest:?}"))?;
            anyhow::ensure!(
                secs > 0.0,
                "{label}: elapsed time must be positive, got {secs}"
            );
            return Ok(secs);
        }
    }
    anyhow::bail!("{label}: no 'elapsed' line found in output: {output:?}")
}
