// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Methods to start a [`PetriVmConfigOpenVmm`] and produce a running [`PetriVmOpenVmm`].

use super::PetriVmConfigOpenVmm;
use super::PetriVmOpenVmm;
use crate::worker::Worker;
use crate::Firmware;
use crate::PetriLogFile;
use crate::PetriLogSource;
use anyhow::Context;
use diag_client::DiagClient;
use disk_backend_resources::FileDiskHandle;
use framebuffer::FramebufferAccess;
use guid::Guid;
use hvlite_defs::config::DeviceVtl;
use image::ColorType;
use mesh_process::Mesh;
use mesh_process::ProcessConfig;
use mesh_worker::WorkerHost;
use pal_async::task::Spawn;
use pal_async::task::Task;
use pal_async::timer::PolledTimer;
use pal_async::DefaultDriver;
use petri_artifacts_common::tags::MachineArch;
use petri_artifacts_common::tags::OsFlavor;
use pipette_client::PipetteClient;
use scsidisk_resources::SimpleScsiDiskHandle;
use std::io::BufRead;
use std::io::Write;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;
use storvsp_resources::ScsiControllerHandle;
use storvsp_resources::ScsiDeviceAndPath;
use storvsp_resources::ScsiPath;
use vm_resource::IntoResource;

impl PetriVmConfigOpenVmm {
    async fn run_core(self) -> anyhow::Result<PetriVmOpenVmm> {
        let Self {
            firmware,
            arch,
            mut config,

            resources,

            openvmm_log_file,

            ged,
            vtl2_settings,
            framebuffer_access,
        } = self;

        // Add the GED and VTL 2 settings.
        if let Some(mut ged) = ged {
            ged.vtl2_settings = Some(prost::Message::encode_to_vec(&vtl2_settings.unwrap()));
            config
                .vmbus_devices
                .push((DeviceVtl::Vtl2, ged.into_resource()));
        }

        let vtl2_vsock_path = config
            .vtl2_vmbus
            .as_ref()
            .and_then(|s| s.vsock_path.as_ref().map(|v| v.into()));

        tracing::debug!(?config, ?firmware, ?arch, "VM config");

        let mesh = Mesh::new("petri_mesh".to_string())?;

        let host = Self::openvmm_host(&mesh, openvmm_log_file, resources.openvmm_path.as_ref())
            .await
            .context("failed to create host process")?;
        let (worker, halt_notif) = Worker::launch(&host, config)
            .await
            .context("failed to launch vm worker")?;

        let worker = Arc::new(worker);
        let watchdog_tasks = Self::start_watchdog_tasks(
            framebuffer_access,
            worker.clone(),
            vtl2_vsock_path,
            &resources.log_source,
            &resources.driver,
        )?;

        let mut vm = PetriVmOpenVmm::new(
            super::runtime::PetriVmInner {
                resources,
                mesh,
                worker,
                watchdog_tasks,
                quirks: firmware.quirks(),
            },
            halt_notif,
        );

        tracing::info!("Resuming VM");
        vm.resume().await?;

        // Run basic save/restore test that should run on every vm
        // TODO: OpenHCL needs virt_whp support
        // TODO: PCAT needs vga device support
        // TODO: arm64 is broken?
        if !firmware.is_openhcl()
            && !matches!(firmware, Firmware::Pcat { .. })
            && !matches!(arch, MachineArch::Aarch64)
        {
            tracing::info!("Testing save/restore");
            vm.verify_save_restore().await?;
        }

        tracing::info!("VM ready");
        Ok(vm)
    }

    /// Build and boot the requested VM. Does not configure and start pipette.
    /// Should only be used for testing platforms that pipette does not support.
    pub async fn run_without_agent(self) -> anyhow::Result<PetriVmOpenVmm> {
        self.run_core().await
    }

    /// Run the VM, launching pipette and returning a client to it.
    pub async fn run(self) -> anyhow::Result<(PetriVmOpenVmm, PipetteClient)> {
        let mut vm = self.run_with_lazy_pipette().await?;
        let client = vm.wait_for_agent().await?;
        Ok((vm, client))
    }

    /// Run the VM, configuring pipette to automatically start, but do not wait
    /// for it to connect. This is useful for tests where the first boot attempt
    /// is expected to not succeed, but pipette functionality is still desired.
    pub async fn run_with_lazy_pipette(mut self) -> anyhow::Result<PetriVmOpenVmm> {
        const CIDATA_SCSI_INSTANCE: Guid =
            Guid::from_static_str("766e96f8-2ceb-437e-afe3-a93169e48a7b");

        // Construct the agent disk.
        let agent_disk = self
            .resources
            .agent_image
            .build()
            .context("failed to build agent image")?;

        // Add a SCSI controller to contain the agent disk. Don't reuse an
        // existing controller so that we can avoid interfering with
        // test-specific configuration.
        self.config.vmbus_devices.push((
            DeviceVtl::Vtl0,
            ScsiControllerHandle {
                instance_id: CIDATA_SCSI_INSTANCE,
                max_sub_channel_count: 1,
                io_queue_depth: None,
                devices: vec![ScsiDeviceAndPath {
                    path: ScsiPath {
                        path: 0,
                        target: 0,
                        lun: 0,
                    },
                    device: SimpleScsiDiskHandle {
                        read_only: true,
                        parameters: Default::default(),
                        disk: FileDiskHandle(agent_disk.into_file()).into_resource(),
                    }
                    .into_resource(),
                }],
                requests: None,
            }
            .into_resource(),
        ));

        if matches!(self.firmware.os_flavor(), OsFlavor::Windows) {
            // Make a file for the IMC hive. It's not guaranteed to be at a fixed
            // location at runtime.
            let mut imc_hive_file = tempfile::tempfile().context("failed to create temp file")?;
            imc_hive_file
                .write_all(include_bytes!("../../../guest-bootstrap/imc.hiv"))
                .context("failed to write imc hive")?;

            // Add the IMC device.
            self.config.vmbus_devices.push((
                DeviceVtl::Vtl0,
                vmbfs_resources::VmbfsImcDeviceHandle {
                    file: imc_hive_file,
                }
                .into_resource(),
            ));
        }

        if self.firmware.is_openhcl() {
            // Add a second pipette disk for VTL 2
            const UH_CIDATA_SCSI_INSTANCE: Guid =
                Guid::from_static_str("766e96f8-2ceb-437e-afe3-a93169e48a7c");

            let uh_agent_disk = self
                .resources
                .openhcl_agent_image
                .as_ref()
                .unwrap()
                .build()
                .context("failed to build agent image")?;

            self.config.vmbus_devices.push((
                DeviceVtl::Vtl2,
                ScsiControllerHandle {
                    instance_id: UH_CIDATA_SCSI_INSTANCE,
                    max_sub_channel_count: 1,
                    io_queue_depth: None,
                    devices: vec![ScsiDeviceAndPath {
                        path: ScsiPath {
                            path: 0,
                            target: 0,
                            lun: 0,
                        },
                        device: SimpleScsiDiskHandle {
                            read_only: true,
                            parameters: Default::default(),
                            disk: FileDiskHandle(uh_agent_disk.into_file()).into_resource(),
                        }
                        .into_resource(),
                    }],
                    requests: None,
                }
                .into_resource(),
            ));
        }

        let is_linux_direct = self.firmware.is_linux_direct();

        // Start the VM.
        let mut vm = self.run_core().await?;

        if is_linux_direct {
            vm.launch_linux_direct_pipette().await?;
        }

        Ok(vm)
    }

    fn start_watchdog_tasks(
        framebuffer_access: Option<FramebufferAccess>,
        worker: Arc<Worker>,
        vtl2_vsock_path: Option<PathBuf>,
        log_source: &PetriLogSource,
        driver: &DefaultDriver,
    ) -> anyhow::Result<Vec<Task<()>>> {
        // Our CI environment will kill tests after some time. We want to save
        // some information about the VM if it's still running at that point.
        const TIMEOUT_DURATION_MINUTES: u64 = 6;
        const TIMER_DURATION: Duration = Duration::from_secs(TIMEOUT_DURATION_MINUTES * 60 - 10);

        let mut tasks = Vec::new();

        let mut timer = PolledTimer::new(driver);
        tasks.push(driver.spawn("petri-watchdog-inspect", {
            let log_source = log_source.clone();
            async move {
                timer.sleep(TIMER_DURATION).await;
                tracing::warn!(
                    "Test has been running for almost {TIMEOUT_DURATION_MINUTES} minutes,
                     saving inspect details."
                );

                if let Err(e) =
                    log_source.write_attachment("timeout_inspect.log", worker.inspect_all().await)
                {
                    tracing::error!(?e, "Failed to save inspect log");
                    return;
                }
                tracing::info!("Watchdog inspect task finished.");
            }
        }));

        if let Some(fba) = framebuffer_access {
            let mut view = fba.view()?;
            let mut timer = PolledTimer::new(driver);
            let log_source = log_source.clone();
            tasks.push(driver.spawn("petri-watchdog-screenshot", async move {
                let mut count = 0;
                loop {
                    timer.sleep(Duration::from_secs(2)).await;
                    count += 1;
                    tracing::info!(count, "Taking screenshot.");

                    // Our framebuffer uses 4 bytes per pixel, approximating an
                    // BGRA image, however it only actually contains BGR data.
                    // The fourth byte is effectively noise. We can set the 'alpha'
                    // value to 0xFF to make the image opaque, while we also
                    // convert it to RGB to output it as a PNG.
                    const BYTES_PER_PIXEL: usize = 4;
                    let (width, height) = view.resolution();
                    let (widthsize, heightsize) = (width as usize, height as usize);
                    let len = widthsize * heightsize * BYTES_PER_PIXEL;

                    let mut image = vec![0; len];
                    for (i, line) in
                        (0..height).zip(image.chunks_exact_mut(widthsize * BYTES_PER_PIXEL))
                    {
                        view.read_line(i, line);
                        for pixel in line.chunks_exact_mut(BYTES_PER_PIXEL) {
                            pixel.swap(0, 2);
                            pixel[3] = 0xFF;
                        }
                    }

                    let r = log_source
                        .create_attachment("screenshot.png")
                        .and_then(|mut f| {
                            image::write_buffer_with_format(
                                &mut f,
                                &image,
                                width.into(),
                                height.into(),
                                ColorType::Rgba8,
                                image::ImageFormat::Png,
                            )
                            .map_err(Into::into)
                        });

                    if let Err(e) = r {
                        tracing::error!(?e, "Failed to save screenshot");
                    } else {
                        tracing::info!(count, "Screenshot saved.");
                    }
                }
            }));
        }

        if let Some(vtl2_vsock_path) = vtl2_vsock_path {
            let mut timer = PolledTimer::new(driver);
            let driver2 = driver.clone();
            let log_source = log_source.clone();
            tasks.push(driver.spawn("petri-watchdog-inspect-vtl2", async move {
                timer.sleep(TIMER_DURATION).await;
                tracing::warn!(
                    "Test has been running for almost {TIMEOUT_DURATION_MINUTES} minutes, saving openhcl inspect details."
                );

                let diag_client =
                     DiagClient::from_hybrid_vsock(driver2, &vtl2_vsock_path);

                let output = match diag_client.inspect("", None, None).await {
                    Err(e) => {
                        tracing::error!(?e, "Failed to inspect vtl2");
                        return;
                    }
                    Ok(output) => output,
                };

                let formatted_output = format!("{output:#}");
                if let Err(e) = log_source.write_attachment("timeout_openhcl_inspect.log", formatted_output) {
                    tracing::error!(?e, "Failed to save ohcldiag-dev inspect log");
                    return;
                }

                tracing::info!("Watchdog OpenHCL inspect task finished.");
            }));
        }

        Ok(tasks)
    }

    async fn openvmm_host(
        mesh: &Mesh,
        log_file: PetriLogFile,
        path: &Path,
    ) -> anyhow::Result<WorkerHost> {
        // Copy the child's stderr to this process's, since internally this is
        // wrapped by the test harness.
        let (stderr_read, stderr_write) = pal::pipe_pair()?;
        std::thread::spawn(move || {
            let read = std::io::BufReader::new(stderr_read);
            for line in read.lines() {
                match line {
                    Ok(line) => {
                        log_file.write_entry(line);
                    }
                    Err(err) => {
                        tracing::warn!(
                            error = &err as &dyn std::error::Error,
                            "error reading hvlite stderr"
                        );
                    }
                }
            }
        });

        let (host, runner) = mesh_worker::worker_host();
        mesh.launch_host(
            ProcessConfig::new("vmm")
                .process_name(path)
                .stderr(Some(stderr_write)),
            hvlite_defs::entrypoint::MeshHostParams { runner },
        )
        .await?;
        Ok(host)
    }
}
