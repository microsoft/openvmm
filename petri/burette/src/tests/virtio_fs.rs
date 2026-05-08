// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! virtio-fs file server performance test via fio.
//!
//! Boots a minimal Linux VM (linux_direct, pipette as PID 1) with a virtio-fs
//! device backed by a temporary directory on the host, mounts it inside the
//! guest, and runs fio against a regular file in the mount. Measures
//! sequential/random read/write bandwidth (MiB/s) and IOPS across multiple
//! iterations. Uses warm mode: the VM is booted once and reused for all
//! iterations.
//!
//! Layout inside the guest:
//!
//! ```text
//! /perf            <- erofs perf rootfs (read-only, contains fio)
//! /perf/tmp        <- writable tmpfs (from prepare_chroot)
//! /perf/tmp/vfs    <- virtio-fs mount, backed by host tempdir
//! ```
//!
//! After `chroot /perf`, the guest sees the mount at `/tmp/vfs` and fio
//! reads/writes `/tmp/vfs/test.dat`.

use crate::report::MetricResult;
use anyhow::Context as _;
use petri::pipette::cmd;
use petri_artifacts_common::tags::MachineArch;
use std::path::PathBuf;

/// Test file size in MiB. Must be large enough relative to guest RAM that
/// sequential reads cannot be served entirely from page cache during the
/// measurement window. With 1 GB guest RAM and ramp_time=0, a single
/// sequential pass through 512 MiB takes long enough that the FUSE path
/// dominates.
const TEST_FILE_MIB: u64 = 512;

/// Tag used for the virtio-fs device. Any string matches between host and
/// guest.
const VFS_TAG: &str = "perf_vfs";

/// virtio-fs perf test.
pub struct VirtioFsTest {
    /// Print guest diagnostics.
    pub diag: bool,
    /// If set, record per-phase perf traces in this directory.
    pub perf_dir: Option<PathBuf>,
    /// Test file size in MiB. Default: [`TEST_FILE_MIB`].
    pub file_size_mib: u64,
}

impl Default for VirtioFsTest {
    fn default() -> Self {
        Self {
            diag: false,
            perf_dir: None,
            file_size_mib: TEST_FILE_MIB,
        }
    }
}

/// State kept across warm iterations.
pub struct VirtioFsTestState {
    vm: petri::PetriVm<petri::openvmm::OpenVmmPetriBackend>,
    agent: petri::pipette::PipetteClient,
    /// Host tempdir backing the virtio-fs mount. Held to keep it alive for
    /// the lifetime of the test; deleted on Drop.
    _vfs_root: tempfile::TempDir,
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

/// Register artifacts needed by the virtio-fs test.
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

impl crate::harness::WarmPerfTest for VirtioFsTest {
    type State = VirtioFsTestState;

    fn name(&self) -> &str {
        "virtio_fs"
    }

    fn warmup_iterations(&self) -> u32 {
        1
    }

    async fn setup(
        &self,
        resolver: &petri::ArtifactResolver<'_>,
        driver: &pal_async::DefaultDriver,
    ) -> anyhow::Result<VirtioFsTestState> {
        anyhow::ensure!(
            self.file_size_mib > 0,
            "file_size_mib must be greater than 0"
        );

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
            test_name: "virtio_fs",
            logger: &log_source,
            post_test_hooks: &mut post_test_hooks,
        };

        // Open the perf rootfs erofs image for the virtio-blk device (carries fio).
        let erofs_path = require_petritools_erofs(resolver);
        let erofs_file = fs_err::File::open(&erofs_path)?;

        // Host directory backing the virtio-fs mount.
        let vfs_root = tempfile::Builder::new()
            .prefix("burette-virtiofs-")
            .tempdir()
            .context("failed to create host vfs tempdir")?;
        let vfs_root_path = vfs_root.path().to_string_lossy().into_owned();
        tracing::info!(host_path = %vfs_root_path, "virtio-fs host root");

        let mut builder = petri::PetriVmBuilder::minimal(params, artifacts, driver)?
            .with_processor_topology(petri::ProcessorTopology {
                vp_count: 2,
                ..Default::default()
            })
            .with_memory(petri::MemoryConfig {
                startup_bytes: 1024 * 1024 * 1024, // 1 GB
                ..Default::default()
            });

        // Attach erofs (port 0) + virtio-fs (port 1) and a NIC. Only one
        // modify_backend() call is allowed, so combine all PCIe device setup
        // in a single call.
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

                    // virtio-fs on port 1, backed by the host tempdir.
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

        // Mount the erofs image at /perf so we can run fio.
        agent
            .mount("/dev/vda", "/perf", "erofs", 1 /* MS_RDONLY */, true)
            .await
            .context("failed to mount erofs on /dev/vda")?;
        // prepare_chroot also mounts a writable tmpfs at /perf/tmp.
        agent
            .prepare_chroot("/perf")
            .await
            .context("failed to prepare chroot at /perf")?;

        // Mount the virtio-fs over /perf/tmp/vfs. The underlying /perf/tmp is
        // tmpfs (writable) so mkdir_target works.
        agent
            .mount(VFS_TAG, "/perf/tmp/vfs", "virtiofs", 0, true)
            .await
            .context("failed to mount virtio-fs — is CONFIG_VIRTIO_FS=y in the guest kernel?")?;

        // Pre-allocate the test file on virtio-fs so read tests don't race
        // ahead of the file's existence. Use dd with bs=1M for clarity.
        let size_mib = self.file_size_mib;
        let sh = agent.unix_shell();
        let count = size_mib.to_string();
        cmd!(
            sh,
            "dd if=/dev/zero of=/perf/tmp/vfs/test.dat bs=1048576 count={count} conv=fsync"
        )
        .read()
        .await
        .with_context(|| format!("failed to pre-allocate {size_mib} MiB test file"))?;

        tracing::info!(size_mib, "virtio-fs test file allocated");

        Ok(VirtioFsTestState {
            vm,
            agent,
            _vfs_root: vfs_root,
        })
    }

    async fn run_once(&self, state: &mut VirtioFsTestState) -> anyhow::Result<Vec<MetricResult>> {
        let mut metrics = Vec::new();
        let pid = state.vm.backend().pid();
        let mut recorder = crate::harness::PerfRecorder::new(self.perf_dir.as_deref(), pid)?;
        let size_mib = self.file_size_mib;

        // Each fio job: 10s runtime, no ramp (ramp_time=0 so reads start
        // cold against the FUSE path rather than warming the page cache).
        // Sequential tests use 128k blocks to exercise zero-copy and
        // max-pages; random tests use 4k blocks for IOPS measurement.
        let fio_jobs: &[(&str, &str, &str)] = &[
            // (fio_rw_mode, primary_field, block_size)
            ("read", "read", "128k"),
            ("write", "write", "128k"),
            ("randread", "read", "4k"),
            ("randwrite", "write", "4k"),
        ];

        for &(rw_mode, field, bs) in fio_jobs {
            let is_random = rw_mode.starts_with("rand");
            let phase = if is_random {
                rw_mode.strip_prefix("rand").unwrap()
            } else {
                rw_mode
            };
            let prefix = if is_random { "rand" } else { "seq" };

            let perf_label = format!("fio_virtiofs_{prefix}_{phase}");
            recorder.start(&perf_label)?;

            let json = run_fio_job(&state.agent, rw_mode, bs, size_mib)
                .await
                .with_context(|| format!("fio {rw_mode} failed"))?;

            recorder.stop()?;

            let bw_name = format!("fio_virtiofs_{prefix}_{phase}_bw");
            metrics.push(parse_fio_bw(&json, &bw_name, field)?);

            if is_random {
                let iops_name = format!("fio_virtiofs_{prefix}_{phase}_iops");
                metrics.push(parse_fio_iops(&json, &iops_name, field)?);
            }
        }

        Ok(metrics)
    }

    async fn teardown(&self, state: VirtioFsTestState) -> anyhow::Result<()> {
        state.agent.power_off().await?;
        state.vm.wait_for_clean_teardown().await?;
        Ok(())
    }
}

/// Run a single fio job against the virtio-fs test file and return the raw
/// JSON output.
///
/// Before each job we invalidate the guest page cache so that reads exercise
/// the full FUSE request path rather than being served from guest RAM.
/// `ramp_time=0` ensures the measurement window starts cold; the harness's
/// warmup iteration handles VM-level warm-up. `--end_fsync=1` flushes
/// buffered writes through FUSE before fio reports results.
///
/// Unlike `disk_io` which uses `--direct=1` (O_DIRECT on a block device),
/// we use `--direct=0` because Linux FUSE does not support O_DIRECT unless
/// the server negotiates `FOPEN_DIRECT_IO`. Instead we rely on explicit
/// cache invalidation to ensure I/O hits the FUSE path.
async fn run_fio_job(
    agent: &petri::pipette::PipetteClient,
    rw_mode: &str,
    bs: &str,
    size_mib: u64,
) -> anyhow::Result<String> {
    // Flush dirty pages then drop guest page caches so reads go through
    // the FUSE path. `sync` first ensures writeback is complete, making
    // cache state deterministic between fio jobs.
    let drop_sh = agent.unix_shell();
    let drop_script = "sync; echo 3 > /proc/sys/vm/drop_caches";
    cmd!(drop_sh, "sh -c {drop_script}")
        .read()
        .await
        .context("failed to drop guest page caches")?;

    let mut sh = agent.unix_shell();
    sh.chroot("/perf");
    let size_arg = format!("{size_mib}M");
    let output: String = cmd!(sh, "fio --name=test --filename=/tmp/vfs/test.dat --rw={rw_mode} --bs={bs} --ioengine=io_uring --direct=0 --runtime=10 --time_based=1 --ramp_time=0 --iodepth=32 --numjobs=1 --size={size_arg} --invalidate=1 --end_fsync=1 --output-format=json")
        .read()
        .await
        .with_context(|| format!("fio {rw_mode} on virtio-fs failed"))?;

    Ok(output)
}

/// Parse bandwidth (MiB/s) from fio JSON output.
fn parse_fio_bw(json: &str, metric_name: &str, field: &str) -> anyhow::Result<MetricResult> {
    let v: serde_json::Value = serde_json::from_str(json).context("failed to parse fio JSON")?;

    let bw_bytes = v["jobs"][0][field]["bw_bytes"].as_f64().with_context(|| {
        tracing::error!(json = %json, "failed to find {field}.bw_bytes in fio output");
        format!("missing {field}.bw_bytes in fio output for {metric_name}")
    })?;

    let mib_s = bw_bytes / (1024.0 * 1024.0);
    Ok(MetricResult {
        name: metric_name.to_string(),
        unit: "MiB/s".to_string(),
        value: mib_s,
    })
}

/// Parse IOPS from fio JSON output.
fn parse_fio_iops(json: &str, metric_name: &str, field: &str) -> anyhow::Result<MetricResult> {
    let v: serde_json::Value = serde_json::from_str(json).context("failed to parse fio JSON")?;

    let iops = v["jobs"][0][field]["iops"].as_f64().with_context(|| {
        tracing::error!(json = %json, "failed to find {field}.iops in fio output");
        format!("missing {field}.iops in fio output for {metric_name}")
    })?;

    Ok(MetricResult {
        name: metric_name.to_string(),
        unit: "IOPS".to_string(),
        value: iops,
    })
}
