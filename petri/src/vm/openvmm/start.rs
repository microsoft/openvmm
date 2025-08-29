// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Methods to start a [`PetriVmConfigOpenVmm`] and produce a running [`PetriVmOpenVmm`].

use super::PetriVmConfigOpenVmm;
use super::PetriVmOpenVmm;
use super::PetriVmResourcesOpenVmm;
use crate::Firmware;
use crate::PetriLogFile;
use crate::worker::Worker;
use anyhow::Context;
use disk_backend_resources::FileDiskHandle;
use guid::Guid;
use hvlite_defs::config::DeviceVtl;
use mesh_process::Mesh;
use mesh_process::ProcessConfig;
use mesh_worker::WorkerHost;
use pal_async::pipe::PolledPipe;
use pal_async::task::Spawn;
use petri_artifacts_common::tags::MachineArch;
use petri_artifacts_common::tags::OsFlavor;
use scsidisk_resources::SimpleScsiDiskHandle;
use std::io::Write;
use std::sync::Arc;
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

            mut resources,

            openvmm_log_file,

            ged,
            framebuffer_view,
        } = self;

        if firmware.is_openhcl() {
            // Add a pipette disk for VTL 2
            const UH_CIDATA_SCSI_INSTANCE: Guid =
                guid::guid!("766e96f8-2ceb-437e-afe3-a93169e48a7c");

            if let Some(openhcl_agent_disk) = resources
                .openhcl_agent_image
                .as_ref()
                .unwrap()
                .build()
                .context("failed to build agent image")?
            {
                config.vmbus_devices.push((
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
                                disk: FileDiskHandle(openhcl_agent_disk.into_file())
                                    .into_resource(),
                            }
                            .into_resource(),
                        }],
                        requests: None,
                    }
                    .into_resource(),
                ));
            }
        }

        // Add the GED and VTL 2 settings.
        if let Some(mut ged) = ged {
            ged.vtl2_settings = Some(prost::Message::encode_to_vec(
                resources.vtl2_settings.as_ref().unwrap(),
            ));
            config
                .vmbus_devices
                .push((DeviceVtl::Vtl2, ged.into_resource()));
        }

        tracing::debug!(?config, ?firmware, ?arch, "VM config");

        let mesh = Mesh::new("petri_mesh".to_string())?;

        let host = Self::openvmm_host(&mut resources, &mesh, openvmm_log_file)
            .await
            .context("failed to create host process")?;
        let (worker, halt_notif) = Worker::launch(&host, config)
            .await
            .context("failed to launch vm worker")?;

        let worker = Arc::new(worker);

        let mut vm = PetriVmOpenVmm::new(
            super::runtime::PetriVmInner {
                resources,
                mesh,
                worker,
                framebuffer_view,
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

    /// Run the VM, configuring pipette to automatically start if it is
    /// included in the config
    pub async fn run(mut self) -> anyhow::Result<PetriVmOpenVmm> {
        let launch_linux_direct_pipette = if let Some(agent_image) = &self.resources.agent_image {
            const CIDATA_SCSI_INSTANCE: Guid = guid::guid!("766e96f8-2ceb-437e-afe3-a93169e48a7b");

            // Construct the agent disk.
            if let Some(agent_disk) = agent_image.build().context("failed to build agent image")? {
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
            }

            if matches!(self.firmware.os_flavor(), OsFlavor::Windows) {
                // Make a file for the IMC hive. It's not guaranteed to be at a fixed
                // location at runtime.
                let mut imc_hive_file =
                    tempfile::tempfile().context("failed to create temp file")?;
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

            self.firmware.is_linux_direct() && agent_image.contains_pipette()
        } else {
            false
        };

        // Start the VM.
        let mut vm = self.run_core().await?;

        if launch_linux_direct_pipette {
            vm.launch_linux_direct_pipette().await?;
        }

        Ok(vm)
    }

    async fn openvmm_host(
        resources: &mut PetriVmResourcesOpenVmm,
        mesh: &Mesh,
        log_file: PetriLogFile,
    ) -> anyhow::Result<WorkerHost> {
        // Copy the child's stderr to this process's, since internally this is
        // wrapped by the test harness.
        let (stderr_read, stderr_write) = pal::pipe_pair()?;
        let task = resources.driver.spawn(
            "serial log",
            crate::log_task(
                log_file,
                PolledPipe::new(&resources.driver, stderr_read)
                    .context("failed to create polled pipe")?,
                "openvmm stderr",
            ),
        );
        resources.log_stream_tasks.push(task);

        let (host, runner) = mesh_worker::worker_host();
        mesh.launch_host(
            ProcessConfig::new("vmm")
                .process_name(&resources.openvmm_path)
                .stderr(Some(stderr_write)),
            hvlite_defs::entrypoint::MeshHostParams { runner },
        )
        .await?;
        Ok(host)
    }
}
