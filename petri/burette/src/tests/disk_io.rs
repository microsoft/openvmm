// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Block I/O performance test via fio.
//!
//! Boots an Alpine Linux VM with a data disk, installs fio, and measures
//! sequential/random read/write bandwidth (MiB/s) and IOPS across multiple
//! iterations. Uses warm mode: the VM is booted once and reused for all
//! iterations.
//!
//! Supports both virtio-blk and storvsc (synthetic SCSI) disk backends.

use crate::report::MetricResult;
use anyhow::Context as _;
use petri::pipette::cmd;
use std::path::PathBuf;
use vm_resource::IntoResource;

const ARCH: petri_artifacts_common::tags::MachineArch =
    petri_artifacts_common::tags::MachineArch::X86_64;

/// Which disk backend to use for the fio test.
#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub enum DiskBackend {
    /// Virtio-blk via MMIO.
    #[value(name = "virtio-blk")]
    VirtioBlk,
    /// Synthetic SCSI (storvsc).
    Storvsc,
}

impl DiskBackend {
    /// Short label used in metric names.
    fn label(self) -> &'static str {
        match self {
            DiskBackend::VirtioBlk => "virtioblk",
            DiskBackend::Storvsc => "storvsc",
        }
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
    use petri_artifacts_vmm_test::artifacts::test_vhd::ALPINE_3_23_X64;

    let vhd = resolver.require(ALPINE_3_23_X64);
    let guest = petri::UefiGuest::Vhd(petri::BootImageConfig::from_vhd(vhd));
    petri::Firmware::uefi(resolver, ARCH, guest)
}

/// Register artifacts needed by the disk I/O test.
pub fn register_artifacts(resolver: &petri::ArtifactResolver<'_>) {
    let firmware = build_firmware(resolver);
    petri::PetriVmArtifacts::<petri::openvmm::OpenVmmPetriBackend>::new(
        resolver, firmware, ARCH, true,
    );
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
            resolver, firmware, ARCH, true,
        )
        .context("firmware/arch not compatible with OpenVMM backend")?;

        let mut post_test_hooks = Vec::new();
        let log_source = crate::log_source();
        let params = petri::PetriTestParams {
            test_name: "disk_io",
            logger: &log_source,
            post_test_hooks: &mut post_test_hooks,
        };

        let mut builder = petri::PetriVmBuilder::new(params, artifacts, driver)?
            .with_processor_topology(petri::ProcessorTopology {
                vp_count: 2,
                ..Default::default()
            })
            .with_memory(petri::MemoryConfig {
                startup_bytes: 2 * 1024 * 1024 * 1024,
                ..Default::default()
            });

        // Attach data disk and NIC. Only one modify_backend() call is
        // allowed, so combine disk + NIC setup in a single call.
        let data_disk_path = self.data_disk.clone();
        match self.backend {
            DiskBackend::VirtioBlk => {
                builder = builder.modify_backend(move |b| {
                    // Add NETVSP NIC for package installation.
                    let b = b.with_nic();
                    // Add virtio-blk on PCIe (UEFI guests need PCIe, not MMIO).
                    b.with_custom_config(|c| {
                        use openvmm_defs::config::PcieDeviceConfig;
                        use openvmm_defs::config::PcieRootComplexConfig;
                        use openvmm_defs::config::PcieRootPortConfig;

                        let disk = make_disk_resource(&data_disk_path, disk_size_bytes);

                        // Set up PCIe topology for the virtio-blk device.
                        let low_mmio_start = c.memory.mmio_gaps[0].start();
                        let high_mmio_end = c.memory.mmio_gaps[1].end();

                        const ECAM_SIZE: u64 = 256 * 1024 * 1024;
                        const LOW_MMIO_SIZE: u64 = 64 * 1024 * 1024;
                        const HIGH_MMIO_SIZE: u64 = 1024 * 1024 * 1024;

                        let pcie_low = memory_range::MemoryRange::new(
                            low_mmio_start - LOW_MMIO_SIZE..low_mmio_start,
                        );
                        let pcie_high = memory_range::MemoryRange::new(
                            high_mmio_end..high_mmio_end + HIGH_MMIO_SIZE,
                        );
                        let ecam_range = memory_range::MemoryRange::new(
                            pcie_low.start() - ECAM_SIZE..pcie_low.start(),
                        );

                        c.memory.pci_ecam_gaps.push(ecam_range);
                        c.memory.pci_mmio_gaps.push(pcie_low);
                        c.memory.pci_mmio_gaps.push(pcie_high);
                        c.pcie_root_complexes.push(PcieRootComplexConfig {
                            index: 0,
                            name: "rc0".into(),
                            segment: 0,
                            start_bus: 0,
                            end_bus: 255,
                            ecam_range,
                            low_mmio: pcie_low,
                            high_mmio: pcie_high,
                            ports: vec![PcieRootPortConfig {
                                name: "rp0".into(),
                                hotplug: false,
                            }],
                        });

                        c.pcie_devices.push(PcieDeviceConfig {
                            port_name: "rp0".into(),
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
                    .modify_backend(|b| b.with_nic())
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
        }

        let (vm, agent) = builder.run().await.context("failed to boot Alpine VM")?;

        // Bring up networking for package installation.
        let sh = agent.unix_shell();
        cmd!(sh, "ifconfig eth0 up").run().await?;
        cmd!(sh, "udhcpc eth0").run().await?;

        // Install fio.
        cmd!(sh, "apk add fio")
            .run()
            .await
            .context("failed to install fio — host may need internet access")?;

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
        let label = self.backend.label();
        let pid = state.vm.backend().pid();
        let mut recorder = crate::harness::PerfRecorder::new(self.perf_dir.as_deref(), pid)?;
        let dev = &state.disk_device;

        // Each fio job: 10s runtime + 5s ramp = 15s.
        let fio_jobs: &[(&str, &str, &str)] = &[
            // (metric_suffix, fio_rw_mode, primary_field)
            ("seq_read_bw", "read", "read"),
            ("seq_write_bw", "write", "write"),
            ("rand_read_iops", "randread", "read"),
            ("rand_write_iops", "randwrite", "write"),
            ("rand_read_bw", "randread", "read"),
            ("rand_write_bw", "randwrite", "write"),
        ];

        for &(suffix, rw_mode, field) in fio_jobs {
            let metric_name = format!("fio_{label}_{suffix}");
            recorder.start(&metric_name)?;

            let json = run_fio_job(&state.agent, dev, rw_mode)
                .await
                .with_context(|| format!("fio {rw_mode} failed"))?;

            recorder.stop()?;

            let m = if suffix.ends_with("_iops") {
                parse_fio_iops(&json, &metric_name, field)?
            } else {
                parse_fio_bw(&json, &metric_name, field)?
            };
            metrics.push(m);
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
) -> vm_resource::Resource<vm_resource::kind::DiskHandleKind> {
    match path {
        Some(p) => {
            openvmm_helpers::disk::open_disk_type(p, false).expect("failed to open data disk")
        }
        None => {
            use disk_backend_resources::LayeredDiskHandle;
            use disk_backend_resources::layer::RamDiskLayerHandle;
            LayeredDiskHandle::single_layer(RamDiskLayerHandle {
                len: Some(size_bytes),
                sector_size: None,
            })
            .into_resource()
        }
    }
}

/// Discover the data disk device path in the guest.
///
/// For virtio-blk, the device appears as /dev/vda (first virtio-blk device).
/// For storvsc, it appears as an additional /dev/sd* device.
async fn discover_data_disk(
    agent: &petri::pipette::PipetteClient,
    backend: DiskBackend,
) -> anyhow::Result<String> {
    let sh = agent.unix_shell();

    // List block devices via /sys/block (always available, no extra packages).
    let blocks = cmd!(sh, "ls /sys/block")
        .read()
        .await
        .context("failed to list /sys/block")?;

    tracing::debug!(blocks = %blocks, "guest block devices");

    let devices: Vec<&str> = blocks.split_whitespace().collect();

    match backend {
        DiskBackend::VirtioBlk => {
            // Find the first vd* device.
            for dev in &devices {
                if dev.starts_with("vd") {
                    return Ok(format!("/dev/{dev}"));
                }
            }
            anyhow::bail!("no virtio-blk device (vd*) found in guest; found: {blocks}")
        }
        DiskBackend::Storvsc => {
            // Find sd* devices that aren't the boot disk.
            // The boot disk is sda; agent disk is sdb; data disk is the last one.
            let mut sd_devices: Vec<&&str> =
                devices.iter().filter(|n| n.starts_with("sd")).collect();
            sd_devices.sort();
            let data_dev = sd_devices
                .last()
                .context("no SCSI data disk (sd*) found in guest")?;
            Ok(format!("/dev/{data_dev}"))
        }
    }
}

/// Run a single fio job and return the raw JSON output.
async fn run_fio_job(
    agent: &petri::pipette::PipetteClient,
    device: &str,
    rw_mode: &str,
) -> anyhow::Result<String> {
    let sh = agent.unix_shell();
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
