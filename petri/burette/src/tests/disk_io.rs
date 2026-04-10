// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Block I/O performance test via fio.
//!
//! Boots a minimal Linux VM (linux_direct, pipette as PID 1) with a data disk
//! and a read-only virtio-blk device carrying an erofs image with fio
//! pre-installed. Measures sequential/random read/write bandwidth (MiB/s) and
//! IOPS across multiple iterations. Uses warm mode: the VM is booted once and
//! reused for all iterations.
//!
//! Supports both virtio-blk and storvsc (synthetic SCSI) disk backends.

use crate::report::MetricResult;
use anyhow::Context as _;
use petri::pipette::cmd;
use petri_artifacts_common::tags::MachineArch;
use std::path::PathBuf;
use vm_resource::IntoResource;

/// Which disk backend to use for the fio test.
#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub enum DiskBackend {
    /// Virtio-blk via PCIe (virtio-pci).
    #[value(name = "virtio-blk")]
    VirtioBlk,
    /// Synthetic SCSI (storvsc).
    Storvsc,
}

impl std::fmt::Display for DiskBackend {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        use clap::ValueEnum;
        f.write_str(self.to_possible_value().unwrap().get_name())
    }
}

/// Block I/O test via fio.
pub struct DiskIoTest {
    /// Print guest diagnostics.
    pub diag: bool,
    /// Which disk backend to test.
    pub backend: DiskBackend,
    /// Path to a raw data disk file on the host, or `None` for a RAM-backed
    /// disk. File-backed gives realistic latency on fast storage; RAM-backed
    /// isolates the virtio/storvsc overhead without host filesystem noise.
    pub data_disk: Option<PathBuf>,
    /// Data disk size in GiB.
    pub data_disk_size_gib: u64,
    /// If set, record per-phase perf traces in this directory.
    pub perf_dir: Option<PathBuf>,
}

/// State kept across warm iterations.
pub struct DiskIoTestState {
    vm: petri::PetriVm<petri::openvmm::OpenVmmPetriBackend>,
    agent: petri::pipette::PipetteClient,
    /// Guest device path for the data disk (e.g. "/dev/vda" or "/dev/sdb").
    disk_device: String,
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

/// Register artifacts needed by the disk I/O test.
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

/// GUID for the data disk SCSI controller (used for storvsc backend).
const DATA_DISK_SCSI_CONTROLLER: guid::Guid = guid::guid!("f47ac10b-58cc-4372-a567-0e02b2c3d479");

impl crate::harness::WarmPerfTest for DiskIoTest {
    type State = DiskIoTestState;

    fn name(&self) -> &str {
        match self.backend {
            DiskBackend::VirtioBlk => "disk_io_virtioblk",
            DiskBackend::Storvsc => "disk_io_storvsc",
        }
    }

    fn warmup_iterations(&self) -> u32 {
        1
    }

    async fn setup(
        &self,
        resolver: &petri::ArtifactResolver<'_>,
        driver: &pal_async::DefaultDriver,
    ) -> anyhow::Result<DiskIoTestState> {
        anyhow::ensure!(
            self.data_disk_size_gib > 0,
            "data_disk_size_gib must be greater than 0"
        );
        let disk_size_bytes = self.data_disk_size_gib * 1024 * 1024 * 1024;

        // Create the data disk file if using file-backed storage.
        if let Some(path) = &self.data_disk {
            let file = std::fs::OpenOptions::new()
                .write(true)
                .create(true)
                .truncate(true)
                .open(path)
                .with_context(|| format!("failed to create data disk at {}", path.display()))?;
            file.set_len(disk_size_bytes).with_context(|| {
                format!(
                    "failed to set data disk size to {} GiB",
                    self.data_disk_size_gib
                )
            })?;
            drop(file);
        } else {
            tracing::info!(
                size_gib = self.data_disk_size_gib,
                "using RAM-backed data disk (numbers reflect virtio/storvsc overhead, not host I/O)"
            );
        }

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
            test_name: "disk_io",
            logger: &log_source,
            post_test_hooks: &mut post_test_hooks,
        };

        // Open the perf rootfs erofs image for the virtio-blk device.
        let erofs_path = require_petritools_erofs(resolver);
        let erofs_file = fs_err::File::open(&erofs_path)?;

        let mut builder = petri::PetriVmBuilder::minimal(params, artifacts, driver)?
            .with_processor_topology(petri::ProcessorTopology {
                vp_count: 2,
                ..Default::default()
            })
            .with_memory(petri::MemoryConfig {
                startup_bytes: 1024 * 1024 * 1024, // 1 GB
                ..Default::default()
            });

        // Attach erofs + data disk and NIC. Only one modify_backend() call is
        // allowed, so combine all PCIe device setup in a single call.
        let data_disk_path = self.data_disk.clone();
        match self.backend {
            DiskBackend::VirtioBlk => {
                // Build the disk resource before entering the closure (which
                // can't propagate errors).
                let disk = make_disk_resource(&data_disk_path, disk_size_bytes)
                    .context("failed to create data disk resource")?;
                builder = builder.modify_backend(move |b| {
                    b.with_nic()
                        .with_pcie_root_topology(1, 1, 2)
                        .with_custom_config(|c| {
                            use disk_backend_resources::FileDiskHandle;
                            use openvmm_defs::config::PcieDeviceConfig;

                            // erofs image on port 0
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
                            // data disk on port 1
                            c.pcie_devices.push(PcieDeviceConfig {
                                port_name: "s0rc0rp1".into(),
                                resource: virtio_resources::VirtioPciDeviceHandle(
                                    virtio_resources::blk::VirtioBlkHandle {
                                        disk,
                                        read_only: false,
                                    }
                                    .into_resource(),
                                )
                                .into_resource(),
                            });
                        })
                });
            }
            DiskBackend::Storvsc => {
                let disk = match &data_disk_path {
                    Some(p) => petri::Disk::Persistent(p.clone()),
                    None => petri::Disk::Memory(disk_size_bytes),
                };
                builder = builder
                    .modify_backend(move |b| {
                        b.with_nic()
                            .with_pcie_root_topology(1, 1, 1)
                            .with_custom_config(|c| {
                                use disk_backend_resources::FileDiskHandle;
                                use openvmm_defs::config::PcieDeviceConfig;

                                // erofs image on a PCIe root port
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
                            })
                    })
                    .add_vmbus_storage_controller(
                        &DATA_DISK_SCSI_CONTROLLER,
                        petri::Vtl::Vtl0,
                        petri::VmbusStorageType::Scsi,
                    )
                    .add_vmbus_drive(
                        petri::Drive::new(Some(disk), false),
                        &DATA_DISK_SCSI_CONTROLLER,
                        Some(0),
                    );
            }
        }

        if !self.diag {
            builder = builder.without_screenshots();
        } else {
            builder = builder.with_serial_output();
        }

        let (vm, agent) = builder.run().await.context("failed to boot minimal VM")?;

        // Mount the erofs image and prepare chroot (fio is pre-installed).
        agent
            .mount("/dev/vda", "/perf", "erofs", 1 /* MS_RDONLY */, true)
            .await
            .context("failed to mount erofs on /dev/vda")?;
        agent
            .prepare_chroot("/perf")
            .await
            .context("failed to prepare chroot at /perf")?;

        // Discover the data disk device.
        let disk_device = discover_data_disk(&agent, self.backend)
            .await
            .context("failed to discover data disk device")?;
        tracing::info!(disk_device = %disk_device, backend = ?self.backend, "discovered data disk");

        Ok(DiskIoTestState {
            vm,
            agent,
            disk_device,
        })
    }

    async fn run_once(&self, state: &mut DiskIoTestState) -> anyhow::Result<Vec<MetricResult>> {
        let mut metrics = Vec::new();
        let label = self.backend;
        let pid = state.vm.backend().pid();
        let mut recorder = crate::harness::PerfRecorder::new(self.perf_dir.as_deref(), pid)?;
        let dev = &state.disk_device;

        // Each fio job: 10s runtime + 5s ramp = 15s.
        // For sequential modes we only extract BW; for random modes we extract
        // both BW and IOPS from a single fio run to avoid redundant work.
        let fio_jobs: &[(&str, &str)] = &[
            // (fio_rw_mode, primary_field)
            ("read", "read"),
            ("write", "write"),
            ("randread", "read"),
            ("randwrite", "write"),
        ];

        for &(rw_mode, field) in fio_jobs {
            let is_random = rw_mode.starts_with("rand");
            let phase = if is_random {
                rw_mode.strip_prefix("rand").unwrap()
            } else {
                rw_mode
            };
            let prefix = if is_random { "rand" } else { "seq" };

            let perf_label = format!("fio_{label}_{prefix}_{phase}");
            recorder.start(&perf_label)?;

            let json = run_fio_job(&state.agent, dev, rw_mode)
                .await
                .with_context(|| format!("fio {rw_mode} failed"))?;

            recorder.stop()?;

            let bw_name = format!("fio_{label}_{prefix}_{phase}_bw");
            metrics.push(parse_fio_bw(&json, &bw_name, field)?);

            if is_random {
                let iops_name = format!("fio_{label}_{prefix}_{phase}_iops");
                metrics.push(parse_fio_iops(&json, &iops_name, field)?);
            }
        }

        Ok(metrics)
    }

    async fn teardown(&self, state: DiskIoTestState) -> anyhow::Result<()> {
        state.agent.power_off().await?;
        state.vm.wait_for_clean_teardown().await?;
        Ok(())
    }
}

/// Create a disk resource from either a file path or a RAM-backed disk.
fn make_disk_resource(
    path: &Option<PathBuf>,
    size_bytes: u64,
) -> anyhow::Result<vm_resource::Resource<vm_resource::kind::DiskHandleKind>> {
    match path {
        Some(p) => openvmm_helpers::disk::open_disk_type(p, false)
            .with_context(|| format!("failed to open data disk at {}", p.display())),
        None => {
            use disk_backend_resources::LayeredDiskHandle;
            use disk_backend_resources::layer::RamDiskLayerHandle;
            Ok(LayeredDiskHandle::single_layer(RamDiskLayerHandle {
                len: Some(size_bytes),
                sector_size: None,
            })
            .into_resource())
        }
    }
}

/// Discover the data disk device path in the guest.
///
/// For virtio-blk, the device appears as /dev/vda (first virtio-blk device).
/// For storvsc, the device is located via the controller's VMBus instance GUID
/// in sysfs, which is stable regardless of device enumeration order.
async fn discover_data_disk(
    agent: &petri::pipette::PipetteClient,
    backend: DiskBackend,
) -> anyhow::Result<String> {
    let sh = agent.unix_shell();

    match backend {
        DiskBackend::VirtioBlk => {
            // /dev/vda is the erofs image, the data disk is /dev/vdb.
            let blocks = cmd!(sh, "ls /sys/block")
                .read()
                .await
                .context("failed to list /sys/block")?;

            tracing::debug!(blocks = %blocks, "guest block devices");

            // The data disk is the second virtio-blk device (vdb).
            if blocks.split_whitespace().any(|d| d == "vdb") {
                Ok("/dev/vdb".to_string())
            } else {
                anyhow::bail!("data disk /dev/vdb not found in guest; found: {blocks}")
            }
        }
        DiskBackend::Storvsc => {
            // Discover the data disk by controller GUID via sysfs, which is
            // stable regardless of device enumeration order.
            let guid = DATA_DISK_SCSI_CONTROLLER;
            let list_cmd =
                format!("ls -d /sys/bus/vmbus/devices/{guid}/host*/target*/*:0:0:0/block/sd*");
            let path = cmd!(sh, "sh -c {list_cmd}")
                .read()
                .await
                .with_context(|| format!("no SCSI data disk found for controller {guid}"))?;
            let dev = path
                .lines()
                .next()
                .and_then(|l| l.rsplit('/').next())
                .context("failed to parse device name from sysfs")?;
            Ok(format!("/dev/{dev}"))
        }
    }
}

/// Run a single fio job and return the raw JSON output.
async fn run_fio_job(
    agent: &petri::pipette::PipetteClient,
    device: &str,
    rw_mode: &str,
) -> anyhow::Result<String> {
    let mut sh = agent.unix_shell();
    sh.chroot("/perf");
    let output: String = cmd!(sh, "fio --name=test --filename={device} --rw={rw_mode} --bs=4k --ioengine=io_uring --direct=1 --runtime=10 --ramp_time=5 --iodepth=32 --numjobs=1 --output-format=json")
        .read()
        .await
        .with_context(|| format!("fio {rw_mode} on {device} failed"))?;

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
