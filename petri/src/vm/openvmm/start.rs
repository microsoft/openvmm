// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Methods to start a [`PetriVmConfigOpenVmm`] and produce a running [`PetriVmOpenVmm`].

use super::PetriVmConfigOpenVmm;
use super::PetriVmOpenVmm;
use super::PetriVmResourcesOpenVmm;
use crate::OpenvmmLogConfig;
use crate::PetriLogFile;
use crate::PetriVmRuntimeConfig;
use crate::worker::Worker;
use anyhow::Context;
use mesh_process::Mesh;
use mesh_process::ProcessConfig;
use mesh_worker::WorkerHost;
use openvmm_defs::config::DeviceVtl;
use pal_async::pipe::PolledPipe;
use pal_async::task::Spawn;
use petri_artifacts_common::tags::MachineArch;
use petri_artifacts_common::tags::OsFlavor;
use std::collections::BTreeMap;
use std::ffi::OsString;
use std::io::Write;
use std::sync::Arc;
use vm_resource::IntoResource;

impl PetriVmConfigOpenVmm {
    async fn run_core(self) -> anyhow::Result<(PetriVmOpenVmm, PetriVmRuntimeConfig)> {
        let Self {
            runtime_config,
            arch,
            host_log_levels,
            mut config,

            mesh,

            mut resources,

            openvmm_log_file,

            ged,
            framebuffer_view,
        } = self;

        let has_pcie = !config.pcie_root_complexes.is_empty();

        // TODO: OpenHCL needs virt_whp support
        // TODO: PCAT needs vga device support
        // TODO: arm64 is broken?
        // TODO: VPCI and NVMe don't support save/restore
        let supports_save_restore = !resources.properties.is_openhcl
            && !resources.properties.is_pcat
            && !matches!(arch, MachineArch::Aarch64)
            && !resources.properties.using_vpci;

        // Add the GED and VTL 2 settings.
        if let Some(mut ged) = ged {
            ged.vtl2_settings = Some(prost::Message::encode_to_vec(
                runtime_config.vtl2_settings.as_ref().unwrap(),
            ));
            config
                .vmbus_devices
                .push((DeviceVtl::Vtl2, ged.into_resource()));
        }

        tracing::debug!(?config, "OpenVMM config");

        let log_env = match host_log_levels {
            None | Some(OpenvmmLogConfig::TestDefault) => BTreeMap::<OsString, OsString>::from([
                ("OPENVMM_LOG".into(), "debug".into()),
                ("OPENVMM_SHOW_SPANS".into(), "true".into()),
            ]),
            Some(OpenvmmLogConfig::BuiltInDefault) => BTreeMap::new(),
            Some(OpenvmmLogConfig::Custom(levels)) => levels
                .iter()
                .map(|(k, v)| (OsString::from(k), OsString::from(v)))
                .collect::<BTreeMap<OsString, OsString>>(),
        };

        let host = Self::openvmm_host(&mut resources, &mesh, openvmm_log_file, log_env)
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

        // Run basic save/restore test if it is supported
        if supports_save_restore {
            tracing::info!("Testing save/restore");
            vm.verify_save_restore().await?;
        }

        tracing::info!("VM ready");
        Ok((vm, runtime_config))
    }

    /// Run the VM, configuring pipette to automatically start if it is
    /// included in the config
    pub async fn run(mut self) -> anyhow::Result<(PetriVmOpenVmm, PetriVmRuntimeConfig)> {
        let launch_linux_direct_pipette = if self.resources.properties.using_vtl0_pipette {
            if matches!(self.resources.properties.os_flavor, OsFlavor::Windows)
                && !self.resources.properties.is_isolated
            {
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

            self.resources.properties.is_linux_direct
        } else {
            false
        };

        // Start the VM.
        let (mut vm, config) = self.run_core().await?;

        if launch_linux_direct_pipette {
            vm.launch_linux_direct_pipette().await?;
        }

        Ok((vm, config))
    }

    async fn openvmm_host(
        resources: &mut PetriVmResourcesOpenVmm,
        mesh: &Mesh,
        log_file: PetriLogFile,
        vmm_env: BTreeMap<OsString, OsString>,
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
                .stderr(Some(stderr_write))
                .env(vmm_env.into_iter()),
            openvmm_defs::entrypoint::MeshHostParams { runner },
        )
        .await?;
        Ok(host)
    }
}
