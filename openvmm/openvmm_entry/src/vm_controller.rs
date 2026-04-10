// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! VM controller task that owns exclusive resources (worker handles,
//! DiagInspector, vtl2_settings) and exposes them to the REPL via mesh RPC.

use crate::DiagInspector;
use crate::meshworker::VmmMesh;
use anyhow::Context;
use futures::FutureExt;
use futures::StreamExt;
use futures_concurrency::stream::Merge;
use get_resources::ged::GuestServicingFlags;
use guid::Guid;
use inspect::InspectMut;
use mesh::rpc::Rpc;
use mesh::rpc::RpcSend;
use mesh_worker::WorkerEvent;
use mesh_worker::WorkerHandle;
use openvmm_defs::rpc::VmRpc;
use std::path::Path;
use std::path::PathBuf;
use std::pin::pin;
use std::sync::Arc;
use std::time::Instant;

/// Inspection target: host-side workers or the paravisor.
#[derive(Clone, Copy, mesh::MeshPayload)]
pub enum InspectTarget {
    Host,
    Paravisor,
}

/// RPC enum for operations requiring exclusive resources.
///
/// All variants derive `MeshPayload` so the boundary is cross-process
/// remotable in the future.
#[derive(mesh::MeshPayload)]
pub enum VmControllerRpc {
    /// Restart the VM worker.
    Restart(Rpc<(), Result<(), mesh::error::RemoteError>>),
    /// Restart the VNC worker.
    RestartVnc(Rpc<(), Result<(), mesh::error::RemoteError>>),
    /// Deferred inspection (commands and tab-completion).
    Inspect(InspectTarget, inspect::Deferred),
    /// Query current VTL2 settings (returned as protobuf-encoded bytes).
    GetVtl2Settings(Rpc<(), Option<Vec<u8>>>),
    /// Add a VTL0 SCSI disk backed by a VTL2 storage device.
    AddVtl0ScsiDisk(Rpc<AddVtl0ScsiDiskParams, Result<(), mesh::error::RemoteError>>),
    /// Remove a VTL0 SCSI disk.
    RemoveVtl0ScsiDisk(Rpc<RemoveVtl0ScsiDiskParams, Result<(), mesh::error::RemoteError>>),
    /// Remove a VTL0 SCSI disk by NVMe namespace ID.
    RemoveVtl0ScsiDiskByNvmeNsid(
        Rpc<RemoveVtl0ScsiDiskByNvmeNsidParams, Result<Option<u32>, mesh::error::RemoteError>>,
    ),
    /// Save a VM snapshot to a directory.
    SaveSnapshot(Rpc<String, Result<(), mesh::error::RemoteError>>),
    /// Service (update) the VTL2 firmware.
    ServiceVtl2(Rpc<ServiceVtl2Params, Result<u64, mesh::error::RemoteError>>),
    /// Stop the VM and quit.
    Quit,
}

#[derive(mesh::MeshPayload)]
pub struct AddVtl0ScsiDiskParams {
    pub controller_guid: Guid,
    pub lun: u32,
    pub device_type: i32,
    pub device_path: Guid,
    pub sub_device_path: u32,
}

#[derive(mesh::MeshPayload)]
pub struct RemoveVtl0ScsiDiskParams {
    pub controller_guid: Guid,
    pub lun: u32,
}

#[derive(mesh::MeshPayload)]
pub struct RemoveVtl0ScsiDiskByNvmeNsidParams {
    pub controller_guid: Guid,
    pub nvme_controller_guid: Guid,
    pub nsid: u32,
}

#[derive(mesh::MeshPayload)]
pub struct ServiceVtl2Params {
    pub user_mode_only: bool,
    pub igvm: Option<String>,
    pub nvme_keepalive: bool,
    pub mana_keepalive: bool,
}

/// Events sent from the VmController to the REPL.
#[derive(mesh::MeshPayload)]
pub enum VmControllerEvent {
    /// The VM worker stopped (normally or with error).
    WorkerStopped { error: Option<String> },
    /// The VNC worker stopped or failed.
    VncWorkerStopped { error: Option<String> },
    /// The guest halted.
    GuestHalt(String),
}

/// Owns exclusive VM resources and services RPCs from the REPL.
pub struct VmController {
    pub(crate) mesh: VmmMesh,
    pub(crate) vm_worker: WorkerHandle,
    pub(crate) vnc_worker: Option<WorkerHandle>,
    pub(crate) gdb_worker: Option<WorkerHandle>,
    pub(crate) diag_inspector: DiagInspector,
    pub(crate) vtl2_settings: Option<vtl2_settings_proto::Vtl2Settings>,
    pub(crate) ged_rpc: Option<mesh::Sender<get_resources::ged::GuestEmulationRequest>>,
    pub(crate) vm_rpc: mesh::Sender<VmRpc>,
    pub(crate) paravisor_diag: Arc<diag_client::DiagClient>,
    pub(crate) igvm_path: Option<PathBuf>,
    pub(crate) memory_backing_file: Option<PathBuf>,
    pub(crate) memory: u64,
    pub(crate) processors: u32,
    pub(crate) log_file: Option<PathBuf>,
}

impl VmController {
    /// Run the controller, processing RPCs and worker events until the VM
    /// stops or the REPL sends Quit.
    pub async fn run(
        mut self,
        mut rpc_recv: mesh::Receiver<VmControllerRpc>,
        event_send: mesh::Sender<VmControllerEvent>,
        mut notify_recv: mesh::Receiver<vmm_core_defs::HaltReason>,
    ) {
        enum Event {
            Rpc(VmControllerRpc),
            RpcClosed,
            Worker(WorkerEvent),
            VncWorker(WorkerEvent),
            Halt(vmm_core_defs::HaltReason),
        }

        let mut quit = false;
        loop {
            let event = {
                let rpc = pin!(async {
                    match rpc_recv.next().await {
                        Some(msg) => Event::Rpc(msg),
                        None => Event::RpcClosed,
                    }
                });
                let vm = (&mut self.vm_worker).map(Event::Worker);
                let vnc = futures::stream::iter(self.vnc_worker.as_mut())
                    .flatten()
                    .map(Event::VncWorker);
                let halt = (&mut notify_recv).map(Event::Halt);

                (rpc.into_stream(), vm, vnc, halt)
                    .merge()
                    .next()
                    .await
                    .unwrap()
            };

            match event {
                Event::Rpc(rpc) => {
                    self.handle_rpc(rpc, &mut quit).await;
                }
                Event::RpcClosed => {
                    // REPL disconnected. Stop the VM.
                    tracing::info!("REPL disconnected, stopping VM");
                    self.vm_worker.stop();
                    quit = true;
                }
                Event::Worker(event) => match event {
                    WorkerEvent::Stopped => {
                        if quit {
                            tracing::info!("vm stopped");
                        } else {
                            tracing::error!("vm worker unexpectedly stopped");
                        }
                        event_send.send(VmControllerEvent::WorkerStopped { error: None });
                        break;
                    }
                    WorkerEvent::Failed(err) => {
                        tracing::error!(error = &err as &dyn std::error::Error, "vm worker failed");
                        event_send.send(VmControllerEvent::WorkerStopped {
                            error: Some(format!("{err:#}")),
                        });
                        break;
                    }
                    WorkerEvent::RestartFailed(err) => {
                        tracing::error!(
                            error = &err as &dyn std::error::Error,
                            "vm worker restart failed"
                        );
                    }
                    WorkerEvent::Started => {
                        tracing::info!("vm worker restarted");
                    }
                },
                Event::VncWorker(event) => match event {
                    WorkerEvent::Stopped => {
                        tracing::error!("vnc unexpectedly stopped");
                        event_send.send(VmControllerEvent::VncWorkerStopped { error: None });
                    }
                    WorkerEvent::Failed(err) => {
                        tracing::error!(
                            error = &err as &dyn std::error::Error,
                            "vnc worker failed"
                        );
                        event_send.send(VmControllerEvent::VncWorkerStopped {
                            error: Some(format!("{err:#}")),
                        });
                    }
                    WorkerEvent::RestartFailed(err) => {
                        tracing::error!(
                            error = &err as &dyn std::error::Error,
                            "vnc worker restart failed"
                        );
                    }
                    WorkerEvent::Started => {
                        tracing::info!("vnc worker restarted");
                    }
                },
                Event::Halt(reason) => {
                    tracing::info!(?reason, "guest halted");
                    event_send.send(VmControllerEvent::GuestHalt(format!("{reason:?}")));
                }
            }
        }

        // Ensure all workers are cleaned up before shutting down the mesh.
        self.vm_worker.stop();
        if let Err(err) = self.vm_worker.join().await {
            tracing::error!(
                error = err.as_ref() as &dyn std::error::Error,
                "vm worker join failed"
            );
        }

        if let Some(mut vnc) = self.vnc_worker.take() {
            vnc.stop();
            if let Err(err) = vnc.join().await {
                tracing::error!(
                    error = err.as_ref() as &dyn std::error::Error,
                    "vnc worker join failed"
                );
            }
        }

        if let Some(mut gdb) = self.gdb_worker.take() {
            gdb.stop();
            if let Err(err) = gdb.join().await {
                tracing::error!(
                    error = err.as_ref() as &dyn std::error::Error,
                    "gdb worker join failed"
                );
            }
        }

        self.mesh.shutdown().await;
    }

    async fn handle_rpc(&mut self, rpc: VmControllerRpc, quit: &mut bool) {
        match rpc {
            VmControllerRpc::Restart(req) => {
                let result = self.handle_restart().await;
                req.complete(result.map_err(mesh::error::RemoteError::new));
            }
            VmControllerRpc::RestartVnc(req) => {
                let result = self.handle_restart_vnc().await;
                req.complete(result.map_err(mesh::error::RemoteError::new));
            }
            VmControllerRpc::Inspect(target, deferred) => {
                self.handle_inspect(target, deferred);
            }
            VmControllerRpc::GetVtl2Settings(req) => {
                let bytes = self
                    .vtl2_settings
                    .as_ref()
                    .map(prost::Message::encode_to_vec);
                req.complete(bytes);
            }
            VmControllerRpc::AddVtl0ScsiDisk(req) => {
                let (params, req) = req.split();
                let result = self.handle_add_vtl0_scsi_disk(params).await;
                req.complete(result.map_err(mesh::error::RemoteError::new));
            }
            VmControllerRpc::RemoveVtl0ScsiDisk(req) => {
                let (params, req) = req.split();
                let result = self.handle_remove_vtl0_scsi_disk(params).await;
                req.complete(result.map_err(mesh::error::RemoteError::new));
            }
            VmControllerRpc::RemoveVtl0ScsiDiskByNvmeNsid(req) => {
                let (params, req) = req.split();
                let result = self.handle_remove_vtl0_scsi_disk_by_nvme_nsid(params).await;
                req.complete(result.map_err(mesh::error::RemoteError::new));
            }
            VmControllerRpc::SaveSnapshot(req) => {
                let (dir, req) = req.split();
                let result = self.handle_save_snapshot(Path::new(&dir)).await;
                req.complete(result.map_err(mesh::error::RemoteError::new));
            }
            VmControllerRpc::ServiceVtl2(req) => {
                let (params, req) = req.split();
                let result = self.handle_service_vtl2(params).await;
                req.complete(result.map_err(mesh::error::RemoteError::new));
            }
            VmControllerRpc::Quit => {
                tracing::info!("quitting");
                self.vm_worker.stop();
                *quit = true;
            }
        }
    }

    async fn handle_restart(&mut self) -> anyhow::Result<()> {
        let vm_host = self
            .mesh
            .make_host("vm", self.log_file.clone())
            .await
            .context("spawning vm process failed")?;
        self.vm_worker.restart(&vm_host);
        Ok(())
    }

    async fn handle_restart_vnc(&mut self) -> anyhow::Result<()> {
        if let Some(vnc) = &mut self.vnc_worker {
            let vnc_host = self
                .mesh
                .make_host("vnc", None)
                .await
                .context("spawning vnc process failed")?;
            vnc.restart(&vnc_host);
            Ok(())
        } else {
            anyhow::bail!("no VNC server running")
        }
    }

    fn handle_inspect(&mut self, target: InspectTarget, deferred: inspect::Deferred) {
        let obj = inspect::adhoc_mut(|req| match target {
            InspectTarget::Host => {
                let mut resp = req.respond();
                resp.field("mesh", &self.mesh)
                    .field("vm", &self.vm_worker)
                    .field("vnc", self.vnc_worker.as_ref())
                    .field("gdb", self.gdb_worker.as_ref());
            }
            InspectTarget::Paravisor => {
                self.diag_inspector.inspect_mut(req);
            }
        });
        deferred.inspect(obj);
    }

    async fn handle_save_snapshot(&self, dir: &Path) -> anyhow::Result<()> {
        let memory_file_path = self
            .memory_backing_file
            .as_ref()
            .context("save-snapshot requires --memory-backing-file")?;

        // Pause the VM.
        self.vm_rpc
            .call(VmRpc::Pause, ())
            .await
            .context("failed to pause VM")?;

        // Get device state via existing VmRpc::Save.
        let saved_state_msg = self
            .vm_rpc
            .call_failable(VmRpc::Save, ())
            .await
            .context("failed to save state")?;

        // Serialize the ProtobufMessage to bytes for writing to disk.
        let saved_state_bytes = mesh::payload::encode(saved_state_msg);

        // Fsync the memory backing file.
        let memory_file = fs_err::File::open(memory_file_path)?;
        memory_file
            .sync_all()
            .context("failed to fsync memory backing file")?;

        // Build manifest.
        let manifest = openvmm_helpers::snapshot::SnapshotManifest {
            version: openvmm_helpers::snapshot::MANIFEST_VERSION,
            created_at: std::time::SystemTime::now().into(),
            openvmm_version: env!("CARGO_PKG_VERSION").to_string(),
            memory_size_bytes: self.memory,
            vp_count: self.processors,
            page_size: crate::system_page_size(),
            architecture: crate::GUEST_ARCH.to_string(),
        };

        // Write snapshot directory.
        openvmm_helpers::snapshot::write_snapshot(
            dir,
            &manifest,
            &saved_state_bytes,
            memory_file_path,
        )?;

        // VM stays paused. Do NOT resume.
        Ok(())
    }

    async fn handle_service_vtl2(&self, params: ServiceVtl2Params) -> anyhow::Result<u64> {
        let start;
        if params.user_mode_only {
            start = Instant::now();
            self.paravisor_diag.restart().await?;
        } else {
            let igvm = params
                .igvm
                .map(PathBuf::from)
                .or_else(|| self.igvm_path.clone())
                .context("no igvm file loaded")?;
            let file = fs_err::File::open(igvm)?;
            start = Instant::now();
            let ged_rpc = self.ged_rpc.as_ref().context("no GED")?;
            openvmm_helpers::underhill::save_underhill(
                &self.vm_rpc,
                ged_rpc,
                GuestServicingFlags {
                    nvme_keepalive: params.nvme_keepalive,
                    mana_keepalive: params.mana_keepalive,
                },
                file.into(),
            )
            .await?;
            openvmm_helpers::underhill::restore_underhill(&self.vm_rpc, ged_rpc).await?;
        }
        let elapsed = Instant::now() - start;
        Ok(elapsed.as_millis() as u64)
    }

    async fn modify_vtl2_settings(
        &mut self,
        f: impl FnOnce(&mut vtl2_settings_proto::Vtl2Settings),
    ) -> anyhow::Result<()> {
        let mut settings_copy = self
            .vtl2_settings
            .clone()
            .context("vtl2 settings not configured")?;

        f(&mut settings_copy);

        let ged_rpc = self.ged_rpc.as_ref().context("no GED configured")?;

        ged_rpc
            .call_failable(
                get_resources::ged::GuestEmulationRequest::ModifyVtl2Settings,
                prost::Message::encode_to_vec(&settings_copy),
            )
            .await?;

        self.vtl2_settings = Some(settings_copy);
        Ok(())
    }

    async fn handle_add_vtl0_scsi_disk(
        &mut self,
        params: AddVtl0ScsiDiskParams,
    ) -> anyhow::Result<()> {
        let mut not_found = false;
        self.modify_vtl2_settings(|settings| {
            let dynamic = settings.dynamic.get_or_insert_with(Default::default);

            let scsi_controller = dynamic.storage_controllers.iter_mut().find(|c| {
                c.instance_id == params.controller_guid.to_string()
                    && c.protocol
                        == vtl2_settings_proto::storage_controller::StorageProtocol::Scsi as i32
            });

            let Some(scsi_controller) = scsi_controller else {
                not_found = true;
                return;
            };

            scsi_controller.luns.push(vtl2_settings_proto::Lun {
                location: params.lun,
                device_id: Guid::new_random().to_string(),
                vendor_id: "OpenVMM".to_string(),
                product_id: "Disk".to_string(),
                product_revision_level: "1.0".to_string(),
                serial_number: "0".to_string(),
                model_number: "1".to_string(),
                physical_devices: Some(vtl2_settings_proto::PhysicalDevices {
                    r#type: vtl2_settings_proto::physical_devices::BackingType::Single.into(),
                    device: Some(vtl2_settings_proto::PhysicalDevice {
                        device_type: params.device_type,
                        device_path: params.device_path.to_string(),
                        sub_device_path: params.sub_device_path,
                    }),
                    devices: Vec::new(),
                }),
                is_dvd: false,
                ..Default::default()
            });
        })
        .await?;

        if not_found {
            anyhow::bail!("SCSI controller {} not found", params.controller_guid);
        }
        Ok(())
    }

    async fn handle_remove_vtl0_scsi_disk(
        &mut self,
        params: RemoveVtl0ScsiDiskParams,
    ) -> anyhow::Result<()> {
        self.modify_vtl2_settings(|settings| {
            let dynamic = settings.dynamic.as_mut();
            if let Some(dynamic) = dynamic {
                if let Some(scsi_controller) = dynamic.storage_controllers.iter_mut().find(|c| {
                    c.instance_id == params.controller_guid.to_string()
                        && c.protocol
                            == vtl2_settings_proto::storage_controller::StorageProtocol::Scsi as i32
                }) {
                    scsi_controller.luns.retain(|l| l.location != params.lun);
                }
            }
        })
        .await
    }

    async fn handle_remove_vtl0_scsi_disk_by_nvme_nsid(
        &mut self,
        params: RemoveVtl0ScsiDiskByNvmeNsidParams,
    ) -> anyhow::Result<Option<u32>> {
        let mut removed_lun = None;
        self.modify_vtl2_settings(|settings| {
            let dynamic = settings.dynamic.as_mut();
            if let Some(dynamic) = dynamic {
                if let Some(scsi_controller) = dynamic.storage_controllers.iter_mut().find(|c| {
                    c.instance_id == params.controller_guid.to_string()
                        && c.protocol
                            == vtl2_settings_proto::storage_controller::StorageProtocol::Scsi as i32
                }) {
                    let nvme_controller_str = params.nvme_controller_guid.to_string();
                    scsi_controller.luns.retain(|l| {
                        let dominated_by_nsid = l.physical_devices.as_ref().is_some_and(|pd| {
                            pd.device.as_ref().is_some_and(|d| {
                                d.device_type
                                    == vtl2_settings_proto::physical_device::DeviceType::Nvme as i32
                                    && d.device_path == nvme_controller_str
                                    && d.sub_device_path == params.nsid
                            })
                        });
                        if dominated_by_nsid {
                            removed_lun = Some(l.location);
                            false
                        } else {
                            true
                        }
                    });
                }
            }
        })
        .await?;
        Ok(removed_lun)
    }
}
