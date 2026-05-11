// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Worker for the prototype gRPC/ttrpc management endpoint.

#![cfg(any(feature = "ttrpc", feature = "grpc"))]

use crate::meshworker::VmmMesh;
use crate::serial_io::bind_serial;
use crate::serial_io::connect_serial;
use crate::vm_controller::InspectTarget;
use crate::vm_controller::VmController;
use crate::vm_controller::VmControllerEvent;
use crate::vm_controller::VmControllerRpc;
use anyhow::Context;
use anyhow::anyhow;
use anyhow::bail;
use futures::FutureExt;
use futures::StreamExt;
use guid::Guid;
use inspect::InspectionBuilder;
use inspect_proto::InspectResponse2;
use inspect_proto::InspectService;
use inspect_proto::UpdateResponse2;
use mesh::CancelReason;
use mesh::MeshPayload;
use mesh::error::RemoteError;
use mesh::rpc::RpcSend;
use mesh_rpc::service::Code;
use mesh_rpc::service::Status;
use mesh_worker::Worker;
use mesh_worker::WorkerId;
use mesh_worker::WorkerRpc;
use netvsp_resources::NetvspHandle;
use openvmm_defs::config::Config;
use openvmm_defs::config::DEFAULT_MMIO_GAPS_X86;
use openvmm_defs::config::DeviceVtl;
use openvmm_defs::config::HypervisorConfig;
use openvmm_defs::config::LoadMode;
use openvmm_defs::config::MemoryConfig;
use openvmm_defs::config::ProcessorTopologyConfig;
use openvmm_defs::config::VirtioBus;
use openvmm_defs::config::VmbusConfig;
use openvmm_defs::config::VpciDeviceConfig;
use openvmm_defs::rpc::VmRpc;
use openvmm_defs::worker::VM_WORKER;
use openvmm_defs::worker::VmWorkerParameters;
use openvmm_helpers::disk::OpenDiskOptions;
use openvmm_helpers::disk::open_disk_type;
use openvmm_ttrpc_vmservice as vmservice;
use pal_async::DefaultDriver;
use pal_async::DefaultPool;
use pal_async::task::Spawn;
use pal_async::task::Task;
use scsidisk_resources::SimpleScsiDiskHandle;
use std::fs::File;
use std::future::Future;
use std::sync::Arc;
use std::time::Duration;
use storvsp_resources::ScsiControllerHandle;
use storvsp_resources::ScsiControllerRequest;
use storvsp_resources::ScsiDeviceAndPath;
use unix_socket::UnixListener;
use virtio_resources::VirtioPciDeviceHandle;
use vm_manifest_builder::VmManifestBuilder;
use vm_resource::IntoResource;
use vm_resource::Resource;
use vm_resource::kind::SerialBackendHandle;
use vm_resource::kind::VirtioDeviceHandle;
use vm_resource::kind::VmbusDeviceHandleKind;

#[derive(mesh::MeshPayload)]
pub struct Parameters {
    pub listener: UnixListener,
    pub transport: RpcTransport,
}

#[derive(Copy, Clone, mesh::MeshPayload)]
pub enum RpcTransport {
    Ttrpc,
    Grpc,
}

impl std::fmt::Display for RpcTransport {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.pad(match self {
            RpcTransport::Ttrpc => "ttrpc",
            RpcTransport::Grpc => "grpc",
        })
    }
}

#[derive(Copy, Clone)]
enum ResolvedTransport {
    #[cfg(feature = "ttrpc")]
    Ttrpc,
    #[cfg(feature = "grpc")]
    Grpc,
}

pub struct TtrpcWorker {
    listener: UnixListener,
    transport: ResolvedTransport,
}

pub const TTRPC_WORKER: WorkerId<Parameters> = WorkerId::new("TtrpcWorker");

impl Worker for TtrpcWorker {
    type Parameters = Parameters;
    type State = ();
    const ID: WorkerId<Self::Parameters> = TTRPC_WORKER;

    fn new(parameters: Self::Parameters) -> anyhow::Result<Self> {
        Ok(Self {
            listener: parameters.listener,
            transport: match parameters.transport {
                #[cfg(feature = "ttrpc")]
                RpcTransport::Ttrpc => ResolvedTransport::Ttrpc,
                #[cfg(feature = "grpc")]
                RpcTransport::Grpc => ResolvedTransport::Grpc,
                #[expect(clippy::allow_attributes)]
                #[allow(unreachable_patterns)]
                transport => bail!("unsupported transport {transport}"),
            },
        })
    }

    fn restart(_state: Self::State) -> anyhow::Result<Self> {
        bail!("not yet supported");
    }

    fn run(self, recv: mesh::Receiver<WorkerRpc<Self::State>>) -> anyhow::Result<()> {
        DefaultPool::run_with(async |driver| {
            let mut service = VmService {
                driver,
                vm: None,
                vm_controller: None,
                vm_controller_events: None,
                controller_task: None,
                wait_vm_response: None,
                halted: false,
                rpc_tasks: Vec::new(),
                transport: self.transport,
            };
            service.run(self.listener, recv).await?;
            Ok(())
        })
    }
}

impl VmService {
    async fn run(
        &mut self,
        listener: UnixListener,
        mut recv: mesh::Receiver<WorkerRpc<()>>,
    ) -> anyhow::Result<()> {
        let mut server = mesh_rpc::Server::new();
        let mut vm_service_recv = server.add_service::<vmservice::Vm>();
        let mut inspect_service_recv = server.add_service::<InspectService>();

        let transport = self.transport;
        let (cancel_send, cancel_recv) = mesh::oneshot();
        let server_task = self.driver.spawn("ttrpc-server", {
            let driver = self.driver.clone();
            async move {
                let r = match transport {
                    #[cfg(feature = "ttrpc")]
                    ResolvedTransport::Ttrpc => server.run(&driver, listener, cancel_recv).await,
                    #[cfg(feature = "grpc")]
                    ResolvedTransport::Grpc => {
                        server.run_grpc(&driver, listener, cancel_recv).await
                    }
                };
                match &r {
                    Ok(()) => tracing::debug!("ttrpc server shutting down"),
                    Err(err) => tracing::error!(
                        error = err.as_ref() as &dyn std::error::Error,
                        "ttrpc server error"
                    ),
                }
                r
            }
        });

        let quit = loop {
            // Take the controller events receiver out of self so it can be
            // polled in the select without borrowing self.
            let mut ctrl_events = self.vm_controller_events.take();
            let ctrl_fut = async {
                match &mut ctrl_events {
                    Some(recv) => recv.next().await,
                    None => std::future::pending().await,
                }
            };

            // Clone the WaitVm cancel context so we can poll it without
            // borrowing self.
            let mut wait_cancel_ctx = self.wait_vm_response.as_mut().map(|(ctx, _)| ctx.clone());
            let wait_cancel_fut = async {
                match &mut wait_cancel_ctx {
                    Some(ctx) => Some(ctx.cancelled().await),
                    None => std::future::pending().await,
                }
            };

            enum Action {
                VmService(Box<Option<(mesh::CancelContext, vmservice::Vm)>>),
                InspectService(Option<(mesh::CancelContext, InspectService)>),
                WorkerRpc(Result<WorkerRpc<()>, mesh::RecvError>),
                ControllerEvent(Option<VmControllerEvent>),
                WaitVmCancelled(CancelReason),
            }

            let action = futures::select! { // merge semantics
                m = vm_service_recv.next() => Action::VmService(Box::new(m)),
                m = inspect_service_recv.next() => Action::InspectService(m),
                r = recv.recv().fuse() => Action::WorkerRpc(r),
                e = ctrl_fut.fuse() => Action::ControllerEvent(e),
                reason = wait_cancel_fut.fuse() => Action::WaitVmCancelled(reason.unwrap()),
            };

            // Restore controller events (unless the channel closed).
            if let Action::ControllerEvent(None) = &action {
                tracing::debug!("controller event channel closed");
            } else {
                self.vm_controller_events = ctrl_events;
            }

            match action {
                Action::VmService(message) => match *message {
                    Some((ctx, message)) => match self.handle(ctx, message).await {
                        HandleAction::None => (),
                        HandleAction::Quit => break true,
                    },
                    None => {
                        tracing::debug!("no more ttrpc requests");
                        break false;
                    }
                },
                Action::InspectService(Some((ctx, message))) => {
                    self.handle_inspect(ctx, message).await;
                }
                Action::InspectService(None) => {
                    tracing::debug!("no more ttrpc requests");
                    break false;
                }
                Action::WorkerRpc(Ok(WorkerRpc::Restart(rpc))) => {
                    rpc.complete(Err(RemoteError::new(anyhow::anyhow!("not supported"))));
                }
                Action::WorkerRpc(Ok(WorkerRpc::Inspect(_))) => (),
                Action::WorkerRpc(Ok(WorkerRpc::Stop)) => {
                    tracing::info!("ttrpc worker stopping");
                    break false;
                }
                Action::WorkerRpc(Err(err)) => {
                    tracing::info!(
                        error = &err as &dyn std::error::Error,
                        "ttrpc worker tearing down"
                    );
                    break false;
                }
                Action::ControllerEvent(Some(event)) => {
                    self.handle_controller_event(event);
                }
                Action::ControllerEvent(None) => {} // handled above
                Action::WaitVmCancelled(reason) => {
                    tracing::debug!("WaitVm client cancelled");
                    if let Some((_, response)) = self.wait_vm_response.take() {
                        response.send(Err(grpc_error(anyhow::Error::new(reason))));
                    }
                }
            }
        };

        // If the controller is still alive (non-Quit exit), shut it down.
        if !quit {
            if let Some(controller) = self.vm_controller.take() {
                controller.send(VmControllerRpc::Quit);
            }
        }
        if let Some(task) = self.controller_task.take() {
            task.await;
        }

        // Complete any pending WaitVm with an error.
        if let Some((_, response)) = self.wait_vm_response.take() {
            response.send(Err(grpc_error(anyhow!("server shutting down"))));
        }

        // Drain any remaining RPCs.
        futures::future::join_all(self.rpc_tasks.drain(..)).await;
        if let Some(vm) = self.vm.take() {
            let _ = Arc::try_unwrap(vm).ok().expect("no more VM references");
        }
        drop(cancel_send);
        server_task.await
    }

    fn start_rpc<F, R>(
        &mut self,
        response: mesh::OneshotSender<Result<R, Status>>,
        r: anyhow::Result<F>,
    ) where
        F: 'static + Future<Output = anyhow::Result<R>> + Send,
        R: 'static + MeshPayload + Send,
    {
        match r {
            Ok(fut) => {
                let task = self.driver.spawn("ttrpc-rpc", async move {
                    response.send(map_grpc(fut.await));
                });
                self.rpc_tasks.push(task);
            }
            Err(err) => response.send(Err(grpc_error(err))),
        }
    }
}

struct Vm {
    worker_rpc: mesh::Sender<VmRpc>,
    scsi_rpc: Option<mesh::Sender<ScsiControllerRequest>>,
}

struct VmService {
    driver: DefaultDriver,
    vm: Option<Arc<Vm>>,
    vm_controller: Option<mesh::Sender<VmControllerRpc>>,
    vm_controller_events: Option<mesh::Receiver<VmControllerEvent>>,
    controller_task: Option<Task<()>>,
    wait_vm_response: Option<(mesh::CancelContext, mesh::OneshotSender<Result<(), Status>>)>,
    /// Set when the guest has halted, so that a later `WaitVm` completes
    /// immediately instead of blocking forever. Cleared on `CreateVm`.
    halted: bool,
    rpc_tasks: Vec<Task<()>>,
    transport: ResolvedTransport,
}

fn grpc_error(err: anyhow::Error) -> Status {
    let root_cause = err.root_cause();
    let code = if let Some(code) = root_cause.downcast_ref::<Code>() {
        *code
    } else if let Some(reason) = root_cause.downcast_ref::<CancelReason>() {
        match reason {
            CancelReason::Cancelled => Code::Cancelled,
            CancelReason::DeadlineExceeded => Code::DeadlineExceeded,
        }
    } else {
        Code::Unknown
    };
    Status {
        code: code.into(),
        message: format!("{:#}", err),
        details: vec![],
    }
}

fn map_grpc<T>(r: anyhow::Result<T>) -> Result<T, Status> {
    r.map_err(grpc_error)
}

enum HandleAction {
    None,
    Quit,
}

impl VmService {
    async fn handle(&mut self, ctx: mesh::CancelContext, request: vmservice::Vm) -> HandleAction {
        tracing::debug!(?request, "request");
        match request {
            vmservice::Vm::CreateVm(request, response) => {
                response.send(map_grpc(self.create_vm(request).await))
            }
            vmservice::Vm::TeardownVm((), response) => {
                response.send(map_grpc(self.teardown_vm().await))
            }
            vmservice::Vm::Quit((), response) => {
                // Shut down the controller (which stops and joins the worker).
                if let Some(controller) = self.vm_controller.take() {
                    controller.send(VmControllerRpc::Quit);
                }
                if let Some(task) = self.controller_task.take() {
                    task.await;
                }
                self.vm.take();
                self.vm_controller_events.take();
                if let Some((_, wait_response)) = self.wait_vm_response.take() {
                    wait_response.send(Err(grpc_error(anyhow!("VM quit"))));
                }
                response.send(Ok(()));
                return HandleAction::Quit;
            }
            request => {
                let vm = match &self.vm {
                    Some(vm) => vm.clone(),
                    None => {
                        request.fail(grpc_error(anyhow!("VM not created yet")));
                        return HandleAction::None;
                    }
                };
                match request {
                    vmservice::Vm::PauseVm((), response) => {
                        let r = Ok(self.pause_vm(&vm));
                        self.start_rpc(response, r);
                    }
                    vmservice::Vm::ResumeVm((), response) => {
                        let r = Ok(self.resume_vm(&vm));
                        self.start_rpc(response, r);
                    }
                    vmservice::Vm::WaitVm((), response) => {
                        if self.wait_vm_response.is_some() {
                            response.send(Err(grpc_error(anyhow!("wait VM already in flight"))));
                        } else if self.halted {
                            // Guest already halted before WaitVm was called;
                            // complete immediately.
                            response.send(Ok(()));
                        } else {
                            self.wait_vm_response = Some((ctx.clone(), response));
                        }
                    }
                    vmservice::Vm::ModifyResource(request, response) => {
                        let r = self.modify_resource(&vm, request);
                        self.start_rpc(response, r);
                    }

                    r @ vmservice::Vm::CapabilitiesVm(_, _)
                    | r @ vmservice::Vm::PropertiesVm(_, _) => {
                        r.fail(grpc_error(anyhow!("not supported")))
                    }

                    vmservice::Vm::CreateVm(_, _)
                    | vmservice::Vm::TeardownVm(_, _)
                    | vmservice::Vm::Quit(_, _) => unreachable!(),
                };
            }
        }
        HandleAction::None
    }

    async fn handle_inspect(&mut self, ctx: mesh::CancelContext, request: InspectService) {
        match request {
            InspectService::Inspect(request, response) => {
                self.start_rpc(response, Ok(self.inspect(ctx, request)))
            }
            InspectService::Update(request, response) => {
                self.start_rpc(response, Ok(self.update(ctx, request)))
            }
        }
    }

    fn inspect(
        &self,
        ctx: mesh::CancelContext,
        request: inspect_proto::InspectRequest,
    ) -> impl Future<Output = anyhow::Result<InspectResponse2>> + use<> {
        let mut inspection = InspectionBuilder::new(&request.path)
            .depth(Some(request.depth as usize))
            .inspect(inspect::adhoc(|req| {
                if let Some(controller) = &self.vm_controller {
                    controller.send(VmControllerRpc::Inspect(InspectTarget::Host, req.defer()));
                }
            }));
        async move {
            let _ = ctx
                .with_timeout(Duration::from_secs(1))
                .until_cancelled(inspection.resolve())
                .await;
            let result = inspection.results();
            let response = InspectResponse2 { result };
            Ok(response)
        }
    }

    fn update(
        &self,
        ctx: mesh::CancelContext,
        request: inspect_proto::UpdateRequest,
    ) -> impl Future<Output = anyhow::Result<UpdateResponse2>> + use<> {
        let update = inspect::update(
            &request.path,
            &request.value,
            inspect::adhoc(|req| {
                if let Some(controller) = &self.vm_controller {
                    controller.send(VmControllerRpc::Inspect(InspectTarget::Host, req.defer()));
                }
            }),
        );
        async move {
            let new_value = ctx
                .with_timeout(Duration::from_secs(1))
                .until_cancelled(update)
                .await??;
            let response = UpdateResponse2 { new_value };
            Ok(response)
        }
    }

    async fn create_vm(&mut self, request: vmservice::CreateVmRequest) -> anyhow::Result<()> {
        let req_config = request.config.context("missing configuration")?;

        if self.vm.is_some() {
            bail!("VM already created");
        }

        // Reset halt state for the new VM.
        self.halted = false;

        let load_mode = match req_config
            .boot_config
            .context("missing boot configuration")?
        {
            vmservice::vm_config::BootConfig::DirectBoot(boot) => {
                let kernel = File::open(boot.kernel_path).context("failed to open kernel")?;
                let initrd = if boot.initrd_path.is_empty() {
                    None
                } else {
                    Some(File::open(boot.initrd_path).context("failed to open initrd")?)
                };
                LoadMode::Linux {
                    kernel,
                    initrd,
                    cmdline: boot.kernel_cmdline,
                    custom_dsdt: None,
                    enable_serial: true,
                    boot_mode: openvmm_defs::config::LinuxDirectBootMode::Acpi,
                }
            }
            vmservice::vm_config::BootConfig::Uefi(_) => {
                anyhow::bail!("uefi not yet supported")
            }
        };

        let mut ports = [(); 4].map(|_| None);
        for port in req_config.serial_config.iter().flat_map(|c| &c.ports) {
            let pc = ports
                .get_mut(port.port as usize)
                .context("invalid serial port")?;
            let (serial_fn, action) = open_socket_backend(port.connect);
            *pc = Some(serial_fn(port.socket_path.as_ref()).with_context(|| {
                format!("failed to {} serial socket: {}", action, port.socket_path)
            })?);
        }

        let chipset = VmManifestBuilder::new(
            vm_manifest_builder::BaseChipsetType::HyperVGen2LinuxDirect,
            vm_manifest_builder::MachineArch::X86_64,
        )
        .with_serial(ports)
        .build()
        .context("failed to build vm configuration")?;

        // Extract memory and processor counts for the VmController.
        let config_mem_size = req_config
            .memory_config
            .as_ref()
            .context("missing memory configuration")?
            .memory_mb
            .checked_mul(0x100000)
            .context("invalid memory configuration")?;
        let config_proc_count = req_config
            .processor_config
            .as_ref()
            .map(|c| c.processor_count)
            .unwrap_or(1);

        let mut config = Config {
            // TODO: devices, other stuff
            load_mode,
            ide_disks: vec![],
            floppy_disks: vec![],
            pcie_root_complexes: vec![],
            pcie_devices: vec![],
            pcie_switches: vec![],
            vpci_devices: vec![],
            memory: MemoryConfig {
                mem_size: config_mem_size,
                mmio_gaps: DEFAULT_MMIO_GAPS_X86.into(),
                pci_ecam_gaps: vec![],
                pci_mmio_gaps: vec![],
                prefetch_memory: false,
                private_memory: false,
                transparent_hugepages: false,
                hugepages: false,
                hugepage_size: None,
                numa_mem_sizes: None,
            },
            chipset: chipset.chipset,
            processor_topology: ProcessorTopologyConfig {
                proc_count: config_proc_count,
                vps_per_socket: None,
                enable_smt: None,
                arch: Default::default(),
            },
            hypervisor: HypervisorConfig {
                with_hv: true,
                ..Default::default()
            },
            #[cfg(windows)]
            kernel_vmnics: vec![],
            input: mesh::Receiver::new(),
            framebuffer: None,
            vga_firmware: None,
            vtl2_gfx: false,
            virtio_devices: vec![],
            vmbus: Some(VmbusConfig::default()),
            vtl2_vmbus: None,
            vmbus_devices: vec![],
            #[cfg(windows)]
            vpci_resources: vec![],
            vmgs: None,
            secure_boot_enabled: false,
            custom_uefi_vars: Default::default(),
            firmware_event_send: None,
            debugger_rpc: None,
            chipset_devices: chipset.chipset_devices,
            pci_chipset_devices: chipset.pci_chipset_devices,
            chipset_capabilities: chipset.capabilities,
            generation_id_recv: None,
            rtc_delta_milliseconds: 0,
            automatic_guest_reset: true,
            efi_diagnostics_log_level: Default::default(),
        };

        let mut scsi_rpc = None;
        if let Some(devices_config) = req_config.devices_config {
            if !devices_config.scsi_disks.is_empty() {
                let mut devices = Vec::new();
                for disk in devices_config.scsi_disks {
                    devices.push(make_disk_config(disk).await?);
                }
                let (send, recv) = mesh::channel();
                config.vmbus_devices.push((
                    DeviceVtl::Vtl0,
                    ScsiControllerHandle {
                        instance_id: guid::guid!("ba6163d9-04a1-4d29-b605-72e2ffb1dc7f"),
                        max_sub_channel_count: 0,
                        devices,
                        io_queue_depth: None,
                        requests: Some(recv),
                        poll_mode_queue_depth: None,
                    }
                    .into_resource(),
                ));
                scsi_rpc = Some(send);
            }

            for nic in devices_config.nic_config {
                config.vmbus_devices.push(parse_nic_config(nic)?);
            }

            for virtiofs in devices_config.virtiofs_config {
                let resource = virtio_resources::fs::VirtioFsHandle {
                    tag: virtiofs.tag,
                    fs: virtio_resources::fs::VirtioFsBackend::HostFs {
                        root_path: virtiofs.root_path,
                        mount_options: String::new(),
                    },
                }
                .into_resource();
                // Use VPCI when possible (currently only on Windows and macOS due
                // to KVM backend limitations).
                if cfg!(windows) || cfg!(target_os = "macos") {
                    config.vpci_devices.push(VpciDeviceConfig {
                        vtl: DeviceVtl::Vtl0,
                        instance_id: Guid::new_random(),
                        resource: VirtioPciDeviceHandle(resource).into_resource(),
                    });
                } else {
                    config.virtio_devices.push((VirtioBus::Mmio, resource));
                }
            }

            if let Some(virtio_console) = devices_config.virtio_console {
                if !virtio_console.socket_path.is_empty() {
                    let (serial_fn, action) = open_socket_backend(virtio_console.connect);
                    let backend =
                        serial_fn(virtio_console.socket_path.as_ref()).with_context(|| {
                            format!(
                                "failed to {} virtio console socket: {}",
                                action, virtio_console.socket_path
                            )
                        })?;
                    let resource: Resource<VirtioDeviceHandle> =
                        virtio_resources::console::VirtioConsoleHandle { backend }.into_resource();
                    if cfg!(windows) || cfg!(target_os = "macos") {
                        config.vpci_devices.push(VpciDeviceConfig {
                            vtl: DeviceVtl::Vtl0,
                            instance_id: Guid::new_random(),
                            resource: VirtioPciDeviceHandle(resource).into_resource(),
                        });
                    } else {
                        config.virtio_devices.push((VirtioBus::Mmio, resource));
                    }
                }
            }
        }

        if let Some(hvsocket_config) = req_config.hvsocket_config {
            let listener = UnixListener::bind(&hvsocket_config.path).with_context(|| {
                format!("failed to bind hvsocket path: {}", &hvsocket_config.path)
            })?;
            config.vmbus.as_mut().unwrap().vsock_listener = Some(listener);
            config.vmbus.as_mut().unwrap().vsock_path = Some(hvsocket_config.path);
        }

        let (send, recv) = mesh::channel();
        let (notify_send, notify_recv) = mesh::channel();

        // Create a VmmMesh for local/in-process workers.
        let mesh = VmmMesh::new(&self.driver, true)?;
        let vm_host = mesh
            .make_host("vm", None)
            .await
            .context("spawning vm process failed")?;

        let worker = vm_host
            .launch_worker(
                VM_WORKER,
                VmWorkerParameters {
                    hypervisor: openvmm_helpers::hypervisor::choose_hypervisor()?,
                    cfg: config,
                    saved_state: None,
                    shared_memory: None,
                    rpc: recv,
                    notify: notify_send,
                },
            )
            .await?;

        let memory = config_mem_size;
        let processors = config_proc_count;

        // Create channels for VmController.
        let (vm_controller_send, vm_controller_recv) = mesh::channel();
        let (event_send, event_recv) = mesh::channel();

        // Build VmController with no paravisor-specific fields.
        let controller = VmController {
            mesh,
            vm_worker: worker,
            vnc_worker: None,
            gdb_worker: None,
            diag_inspector: None,
            vtl2_settings: None,
            ged_rpc: None,
            vm_rpc: send.clone(),
            paravisor_diag: None,
            igvm_path: None,
            memory_backing_file: None,
            memory,
            processors,
            log_file: None,
        };

        // Spawn the controller task.
        let controller_task = self.driver.spawn(
            "vm-controller",
            controller.run(vm_controller_recv, event_send, notify_recv),
        );

        self.vm_controller = Some(vm_controller_send);
        self.vm_controller_events = Some(event_recv);
        self.controller_task = Some(controller_task);
        self.vm = Some(Arc::new(Vm {
            scsi_rpc,
            worker_rpc: send,
        }));
        Ok(())
    }

    async fn teardown_vm(&mut self) -> anyhow::Result<()> {
        let controller = self.vm_controller.take().context("vm not created")?;
        controller.send(VmControllerRpc::Quit);
        drop(controller);
        if let Some(task) = self.controller_task.take() {
            task.await;
        }
        self.vm.take();
        self.vm_controller_events.take();
        if let Some((_, response)) = self.wait_vm_response.take() {
            response.send(Err(grpc_error(anyhow!("VM torn down"))));
        }
        Ok(())
    }

    fn pause_vm(&mut self, vm: &Vm) -> impl Future<Output = anyhow::Result<()>> + use<> {
        let recv = vm.worker_rpc.call(VmRpc::Pause, ());
        async move { recv.await.map(drop).context("pause failed") }
    }

    fn resume_vm(&mut self, vm: &Vm) -> impl Future<Output = anyhow::Result<()>> + use<> {
        let recv = vm.worker_rpc.call(VmRpc::Resume, ());
        async move { recv.await.map(drop).context("resume failed") }
    }

    fn handle_controller_event(&mut self, event: VmControllerEvent) {
        match event {
            VmControllerEvent::GuestHalt(reason) => {
                tracing::info!(%reason, "guest halted (via controller)");
                self.halted = true;
                if let Some((_, response)) = self.wait_vm_response.take() {
                    response.send(Ok(()));
                }
            }
            VmControllerEvent::WorkerStopped { error } => {
                if let Some(err) = &error {
                    tracing::error!(error = %err, "VM worker stopped with error");
                } else {
                    tracing::info!("VM worker stopped");
                }
                if let Some((_, response)) = self.wait_vm_response.take() {
                    let status = if let Some(err) = &error {
                        grpc_error(anyhow!("VM worker stopped: {}", err))
                    } else {
                        grpc_error(anyhow!("VM worker stopped"))
                    };
                    response.send(Err(status));
                }
                // Clear VM state since the worker is gone. The controller
                // task will be awaited during final cleanup.
                self.vm.take();
                self.vm_controller.take();
            }
            VmControllerEvent::VncWorkerStopped { error } => {
                if let Some(err) = &error {
                    tracing::error!(error = %err, "VNC worker stopped unexpectedly");
                }
            }
        }
    }

    fn modify_resource(
        &mut self,
        vm: &Vm,
        request: vmservice::ModifyResourceRequest,
    ) -> anyhow::Result<impl Future<Output = anyhow::Result<()>> + use<>> {
        use vmservice::modify_resource_request::Resource;
        match request.resource.context("missing resource")? {
            Resource::ScsiDisk(disk) => {
                let scsi_path = storvsp_resources::ScsiPath {
                    path: 0,
                    target: 0,
                    lun: disk.lun.try_into().ok().context("lun value out of range")?,
                };

                if request.r#type == vmservice::ModifyType::Add as i32 {
                    if disk.controller != 0 {
                        anyhow::bail!("controller must be 0");
                    }
                    let scsi_rpc = vm.scsi_rpc.as_ref().context("no scsi controller")?.clone();
                    Ok(async move {
                        let config = make_disk_config(disk).await?;
                        scsi_rpc
                            .call_failable(ScsiControllerRequest::AddDevice, config)
                            .await
                            .map_err(anyhow::Error::from)
                    }
                    .boxed())
                } else if request.r#type == vmservice::ModifyType::Remove as i32 {
                    let recv = vm
                        .scsi_rpc
                        .as_ref()
                        .context("no scsi controller")?
                        .call_failable(ScsiControllerRequest::RemoveDevice, scsi_path);
                    Ok(async move { recv.await.map_err(anyhow::Error::from) }.boxed())
                } else {
                    anyhow::bail!("unsupported request type {}", request.r#type);
                }
            }
            Resource::NicConfig(nic) => {
                if request.r#type != vmservice::ModifyType::Add as i32 {
                    anyhow::bail!("not supported yet");
                }
                let config = parse_nic_config(nic)?;
                let recv = vm.worker_rpc.call_failable(VmRpc::AddVmbusDevice, config);
                Ok(async move { recv.await.map_err(anyhow::Error::from) }.boxed())
            }
            Resource::VpmemDisk(_) => anyhow::bail!("vpmem not supported"),
            Resource::WindowsDevice(_) => anyhow::bail!("device assignment not supported"),
            Resource::Processor(_) | Resource::ProcessorConfig(_) | Resource::Memory(_) => {
                anyhow::bail!("processor and memory resources not supported")
            }
        }
    }
}

/// Returns the appropriate serial backend open function and a human-readable
/// action verb for error messages, based on whether we should connect to an
/// existing socket or bind a new listener.
fn open_socket_backend(
    connect: bool,
) -> (
    fn(&std::path::Path) -> std::io::Result<Resource<SerialBackendHandle>>,
    &'static str,
) {
    if connect {
        (connect_serial, "connect to")
    } else {
        (bind_serial, "bind")
    }
}

fn parse_nic_config(
    nic: vmservice::NicConfig,
) -> anyhow::Result<(DeviceVtl, Resource<VmbusDeviceHandleKind>)> {
    use self::vmservice::nic_config::Backend;

    let endpoint = match nic.backend.context("missing backend")? {
        #[cfg(windows)]
        Backend::LegacyPortId(port_id) => net_backend_resources::dio::WindowsDirectIoHandle {
            switch_port_id: net_backend_resources::dio::SwitchPortId {
                switch: nic.legacy_switch_id.parse().context("invalid switch ID")?,
                port: port_id.parse().context("invalid port ID")?,
            },
        }
        .into_resource(),
        #[cfg(windows)]
        Backend::Dio(dio) => net_backend_resources::dio::WindowsDirectIoHandle {
            switch_port_id: net_backend_resources::dio::SwitchPortId {
                switch: dio.switch_id.parse().context("invalid switch ID")?,
                port: dio.port_id.parse().context("invalid port ID")?,
            },
        }
        .into_resource(),
        #[cfg(target_os = "linux")]
        Backend::Tap(tap) => {
            let fd = net_tap::tap::open_tap(&tap.name)
                .with_context(|| format!("failed to open TAP device '{}'", tap.name))?;
            net_backend_resources::tap::TapHandle { fd }.into_resource()
        }
        Backend::Consomme(consomme) => net_backend_resources::consomme::ConsommeHandle {
            cidr: if consomme.cidr.is_empty() {
                None
            } else {
                Some(consomme.cidr)
            },
            ports: Vec::new(),
        }
        .into_resource(),
        _ => anyhow::bail!("unsupported backend"),
    };
    let cfg = NetvspHandle {
        instance_id: nic.nic_id.parse().context("invalid instance ID")?,
        mac_address: nic
            .mac_address
            .parse::<net_backend_resources::mac_address::MacAddress>()
            .context("invalid mac address")?,
        endpoint,
        max_queues: None,
    };
    Ok((DeviceVtl::Vtl0, cfg.into_resource()))
}

async fn make_disk_config(disk: vmservice::ScsiDisk) -> anyhow::Result<ScsiDeviceAndPath> {
    Ok(ScsiDeviceAndPath {
        path: storvsp_resources::ScsiPath {
            path: 0,
            target: 0,
            lun: disk.lun.try_into().ok().context("lun value out of range")?,
        },
        device: SimpleScsiDiskHandle {
            disk: open_disk_type(
                disk.host_path.as_ref(),
                OpenDiskOptions {
                    read_only: disk.read_only,
                    direct: false,
                },
            )
            .await
            .with_context(|| format!("failed to open {}", disk.host_path))?,
            read_only: disk.read_only,
            parameters: Default::default(),
        }
        .into_resource(),
    })
}
