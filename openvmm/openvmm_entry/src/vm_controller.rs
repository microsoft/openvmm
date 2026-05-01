// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! VM controller task that owns exclusive resources (worker handles,
//! DiagInspector, vtl2_settings) and exposes them to the REPL via mesh RPC.

use crate::DiagInspector;
use crate::meshworker::VmmMesh;
use crate::vm_connect;
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
use openvmm_defs::worker::VM_WORKER;
use openvmm_defs::worker::VmWorkerParameters;
use pal_async::DefaultDriver;
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
    /// Connect an attach client to the currently running VM.
    Connect(Rpc<(), Result<vm_connect::VmConnectResponse, mesh::error::RemoteError>>),
    /// Create and start a VM worker in server mode.
    CreateVm(Rpc<Box<ServerVmStartParams>, Result<ServerVmHandles, mesh::error::RemoteError>>),
    /// Stop and drop the current VM worker in server mode.
    TeardownVm(Rpc<(), Result<(), mesh::error::RemoteError>>),
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
pub struct ServerVmStartParams {
    pub worker_params: VmWorkerParameters,
    pub vm_rpc: mesh::Sender<VmRpc>,
    pub notify_recv: mesh::Receiver<vmm_core_defs::HaltReason>,
    pub scsi_rpc: Option<mesh::Sender<storvsp_resources::ScsiControllerRequest>>,
    pub memory: u64,
    pub processors: u32,
}

#[derive(Clone, mesh::MeshPayload)]
pub struct ServerVmHandles {
    pub vm_rpc: mesh::Sender<VmRpc>,
    pub scsi_rpc: Option<mesh::Sender<storvsp_resources::ScsiControllerRequest>>,
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

pub struct CurrentVm {
    pub(crate) vm_worker: WorkerHandle,
    pub(crate) vnc_worker: Option<WorkerHandle>,
    pub(crate) gdb_worker: Option<WorkerHandle>,
    pub(crate) diag_inspector: Option<DiagInspector>,
    pub(crate) vtl2_settings: Option<vtl2_settings_proto::Vtl2Settings>,
    pub(crate) ged_rpc: Option<mesh::Sender<get_resources::ged::GuestEmulationRequest>>,
    pub(crate) vm_rpc: mesh::Sender<VmRpc>,
    pub(crate) scsi_rpc: Option<mesh::Sender<storvsp_resources::ScsiControllerRequest>>,
    pub(crate) nvme_vtl2_rpc: Option<mesh::Sender<nvme_resources::NvmeControllerRequest>>,
    pub(crate) shutdown_ic: Option<mesh::Sender<hyperv_ic_resources::shutdown::ShutdownRpc>>,
    pub(crate) kvp_ic: Option<mesh::Sender<hyperv_ic_resources::kvp::KvpConnectRpc>>,
    pub(crate) paravisor_diag: Option<Arc<diag_client::DiagClient>>,
    pub(crate) igvm_path: Option<PathBuf>,
    pub(crate) memory_backing_file: Option<PathBuf>,
    pub(crate) memory: u64,
    pub(crate) processors: u32,
    pub(crate) log_file: Option<PathBuf>,
    pub(crate) notify_recv: mesh::Receiver<vmm_core_defs::HaltReason>,
}

/// Owns exclusive VM resources and services RPCs from the REPL.
pub struct VmController {
    pub(crate) driver: DefaultDriver,
    pub(crate) mesh: VmmMesh,
    pub(crate) vm_controller: mesh::Sender<VmControllerRpc>,
    pub(crate) current_vm: Option<CurrentVm>,
    pub(crate) attach_path: Option<PathBuf>,
    pub(crate) attach_listener: Option<vm_connect::AttachListener>,
    pub(crate) exit_on_vm_stop: bool,
}

impl VmController {
    /// Run the controller, processing RPCs and worker events until the VM
    /// stops or the caller (REPL or ttrpc server) sends Quit.
    pub async fn run(
        mut self,
        mut rpc_recv: mesh::Receiver<VmControllerRpc>,
        event_send: mesh::Sender<VmControllerEvent>,
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
                if let Some(current_vm) = &mut self.current_vm {
                    let vm = (&mut current_vm.vm_worker).map(Event::Worker);
                    let vnc = futures::stream::iter(current_vm.vnc_worker.as_mut())
                        .flatten()
                        .map(Event::VncWorker);
                    let halt = (&mut current_vm.notify_recv).map(Event::Halt);

                    (rpc.into_stream(), vm, vnc, halt)
                        .merge()
                        .next()
                        .await
                        .unwrap()
                } else {
                    rpc.into_stream().next().await.unwrap()
                }
            };

            match event {
                Event::Rpc(rpc) => {
                    self.handle_rpc(rpc, &mut quit).await;
                    if quit {
                        break;
                    }
                }
                Event::RpcClosed => {
                    // Controller RPC channel closed (REPL/ttrpc disconnected).
                    // Stop the VM.
                    tracing::info!("controller RPC channel closed, stopping VM");
                    break;
                }
                Event::Worker(event) => match event {
                    WorkerEvent::Stopped => {
                        if quit {
                            tracing::info!("vm stopped");
                        } else {
                            tracing::error!("vm worker unexpectedly stopped");
                        }
                        event_send.send(VmControllerEvent::WorkerStopped { error: None });
                        self.teardown_current_vm_after_worker_stop().await;
                        if self.exit_on_vm_stop {
                            break;
                        }
                    }
                    WorkerEvent::Failed(err) => {
                        tracing::error!(error = &err as &dyn std::error::Error, "vm worker failed");
                        event_send.send(VmControllerEvent::WorkerStopped {
                            error: Some(format!("{err:#}")),
                        });
                        self.teardown_current_vm_after_worker_stop().await;
                        if self.exit_on_vm_stop {
                            break;
                        }
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

        self.stop_attach_listener().await;
        self.stop_current_vm().await;
        self.mesh.shutdown().await;
    }

    pub(crate) async fn start_attach_listener(&mut self) -> anyhow::Result<()> {
        let Some(path) = &self.attach_path else {
            return Ok(());
        };
        let listener = vm_connect::start_attach_listener(
            self.mesh.process_mesh()?,
            &self.driver,
            path,
            self.vm_controller.clone(),
        )
        .await?;
        self.attach_listener = Some(listener);
        Ok(())
    }

    async fn stop_attach_listener(&mut self) {
        if let Some(listener) = self.attach_listener.take() {
            listener.shutdown().await;
        }
    }

    fn attach_resources(&self) -> anyhow::Result<vm_connect::AttachResources> {
        let current_vm = self.current_vm.as_ref().context("VM not created")?;
        Ok(vm_connect::AttachResources {
            vm_rpc: current_vm.vm_rpc.clone(),
            vm_controller: self.vm_controller.clone(),
            scsi_rpc: current_vm.scsi_rpc.clone(),
            nvme_vtl2_rpc: current_vm.nvme_vtl2_rpc.clone(),
            shutdown_ic: current_vm.shutdown_ic.clone(),
            kvp_ic: current_vm.kvp_ic.clone(),
            has_vtl2: current_vm.vtl2_settings.is_some(),
        })
    }

    fn current_vm(&self) -> anyhow::Result<&CurrentVm> {
        self.current_vm.as_ref().context("VM not created")
    }

    fn current_vm_mut(&mut self) -> anyhow::Result<&mut CurrentVm> {
        self.current_vm.as_mut().context("VM not created")
    }

    async fn stop_current_vm(&mut self) {
        let Some(mut current_vm) = self.current_vm.take() else {
            return;
        };

        current_vm.vm_worker.stop();
        if let Err(err) = current_vm.vm_worker.join().await {
            tracing::error!(
                error = err.as_ref() as &dyn std::error::Error,
                "vm worker join failed"
            );
        }

        if let Some(mut vnc) = current_vm.vnc_worker.take() {
            vnc.stop();
            if let Err(err) = vnc.join().await {
                tracing::error!(
                    error = err.as_ref() as &dyn std::error::Error,
                    "vnc worker join failed"
                );
            }
        }

        if let Some(mut gdb) = current_vm.gdb_worker.take() {
            gdb.stop();
            if let Err(err) = gdb.join().await {
                tracing::error!(
                    error = err.as_ref() as &dyn std::error::Error,
                    "gdb worker join failed"
                );
            }
        }
    }

    async fn teardown_current_vm_after_worker_stop(&mut self) {
        if let Some(mut current_vm) = self.current_vm.take() {
            if let Some(mut vnc) = current_vm.vnc_worker.take() {
                vnc.stop();
                let _ = vnc.join().await;
            }
            if let Some(mut gdb) = current_vm.gdb_worker.take() {
                gdb.stop();
                let _ = gdb.join().await;
            }
        }
    }

    async fn handle_rpc(&mut self, rpc: VmControllerRpc, quit: &mut bool) {
        match rpc {
            VmControllerRpc::Connect(req) => {
                let result = self.attach_resources().map(Into::into);
                req.complete(result.map_err(mesh::error::RemoteError::new));
            }
            VmControllerRpc::CreateVm(req) => {
                let (params, req) = req.split();
                let result = self.handle_create_vm(*params).await;
                req.complete(result.map_err(mesh::error::RemoteError::new));
            }
            VmControllerRpc::TeardownVm(req) => {
                let result = self.handle_teardown_vm().await;
                req.complete(result.map_err(mesh::error::RemoteError::new));
            }
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
                    .current_vm
                    .as_ref()
                    .and_then(|vm| vm.vtl2_settings.as_ref())
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
                *quit = true;
            }
        }
    }

    async fn handle_create_vm(
        &mut self,
        params: ServerVmStartParams,
    ) -> anyhow::Result<ServerVmHandles> {
        if self.current_vm.is_some() {
            anyhow::bail!("VM already created");
        }

        let vm_host = self
            .mesh
            .make_host("vm", None)
            .await
            .context("spawning vm process failed")?;

        let worker = vm_host
            .launch_worker(VM_WORKER, params.worker_params)
            .await?;
        let handles = ServerVmHandles {
            vm_rpc: params.vm_rpc.clone(),
            scsi_rpc: params.scsi_rpc.clone(),
        };
        self.current_vm = Some(CurrentVm {
            vm_worker: worker,
            vnc_worker: None,
            gdb_worker: None,
            diag_inspector: None,
            vtl2_settings: None,
            ged_rpc: None,
            vm_rpc: params.vm_rpc,
            scsi_rpc: params.scsi_rpc,
            nvme_vtl2_rpc: None,
            shutdown_ic: None,
            kvp_ic: None,
            paravisor_diag: None,
            igvm_path: None,
            memory_backing_file: None,
            memory: params.memory,
            processors: params.processors,
            log_file: None,
            notify_recv: params.notify_recv,
        });

        Ok(handles)
    }

    async fn handle_teardown_vm(&mut self) -> anyhow::Result<()> {
        self.current_vm.as_ref().context("VM not created")?;
        self.stop_current_vm().await;
        Ok(())
    }

    async fn handle_restart(&mut self) -> anyhow::Result<()> {
        let log_file = self.current_vm()?.log_file.clone();
        let vm_host = self
            .mesh
            .make_host("vm", log_file)
            .await
            .context("spawning vm process failed")?;
        self.current_vm_mut()?.vm_worker.restart(&vm_host);
        Ok(())
    }

    async fn handle_restart_vnc(&mut self) -> anyhow::Result<()> {
        if self.current_vm()?.vnc_worker.is_some() {
            let vnc_host = self
                .mesh
                .make_host("vnc", None)
                .await
                .context("spawning vnc process failed")?;
            self.current_vm_mut()?
                .vnc_worker
                .as_mut()
                .expect("checked above")
                .restart(&vnc_host);
            Ok(())
        } else {
            anyhow::bail!("no VNC server running")
        }
    }

    fn handle_inspect(&mut self, target: InspectTarget, deferred: inspect::Deferred) {
        let obj = inspect::adhoc_mut(|req| match target {
            InspectTarget::Host => {
                let mut resp = req.respond();
                let current_vm = self.current_vm.as_ref();
                resp.field("mesh", &self.mesh)
                    .field("vm", current_vm.map(|vm| &vm.vm_worker))
                    .field("vnc", current_vm.and_then(|vm| vm.vnc_worker.as_ref()))
                    .field("gdb", current_vm.and_then(|vm| vm.gdb_worker.as_ref()));
            }
            InspectTarget::Paravisor => {
                if let Some(inspector) = self
                    .current_vm
                    .as_mut()
                    .and_then(|vm| vm.diag_inspector.as_mut())
                {
                    inspector.inspect_mut(req);
                }
            }
        });
        deferred.inspect(obj);
    }

    async fn handle_save_snapshot(&self, dir: &Path) -> anyhow::Result<()> {
        let current_vm = self.current_vm()?;
        let memory_file_path = current_vm
            .memory_backing_file
            .as_ref()
            .context("save-snapshot requires --memory-backing-file")?;

        // Pause the VM.
        current_vm
            .vm_rpc
            .call(VmRpc::Pause, ())
            .await
            .context("failed to pause VM")?;

        // Get device state via existing VmRpc::Save.
        let saved_state_msg = current_vm
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
            memory_size_bytes: current_vm.memory,
            vp_count: current_vm.processors,
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
        let current_vm = self.current_vm()?;
        let start;
        if params.user_mode_only {
            start = Instant::now();
            current_vm
                .paravisor_diag
                .as_ref()
                .context("no paravisor diagnostics client")?
                .restart()
                .await?;
        } else {
            let igvm = params
                .igvm
                .map(PathBuf::from)
                .or_else(|| current_vm.igvm_path.clone())
                .context("no igvm file loaded")?;
            let file = fs_err::File::open(igvm)?;
            start = Instant::now();
            let ged_rpc = current_vm.ged_rpc.as_ref().context("no GED")?;
            openvmm_helpers::underhill::save_underhill(
                &current_vm.vm_rpc,
                ged_rpc,
                GuestServicingFlags {
                    nvme_keepalive: params.nvme_keepalive,
                    mana_keepalive: params.mana_keepalive,
                },
                file.into(),
            )
            .await?;
            openvmm_helpers::underhill::restore_underhill(&current_vm.vm_rpc, ged_rpc).await?;
        }
        let elapsed = Instant::now() - start;
        Ok(elapsed.as_millis() as u64)
    }

    async fn modify_vtl2_settings(
        &mut self,
        f: impl FnOnce(&mut vtl2_settings_proto::Vtl2Settings),
    ) -> anyhow::Result<()> {
        let mut settings_copy = self
            .current_vm()?
            .vtl2_settings
            .clone()
            .context("vtl2 settings not configured")?;

        f(&mut settings_copy);

        let ged_rpc = self
            .current_vm()?
            .ged_rpc
            .as_ref()
            .context("no GED configured")?;

        ged_rpc
            .call_failable(
                get_resources::ged::GuestEmulationRequest::ModifyVtl2Settings,
                prost::Message::encode_to_vec(&settings_copy),
            )
            .await?;

        self.current_vm_mut()?.vtl2_settings = Some(settings_copy);
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
