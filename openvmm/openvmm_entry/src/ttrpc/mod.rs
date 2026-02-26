// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Worker for the prototype gRPC/ttrpc management endpoint.

#![cfg(any(feature = "ttrpc", feature = "grpc"))]

use self::vmservice::nic_config::Backend;
use crate::serial_io::bind_serial;
use anyhow::Context;
use anyhow::anyhow;
use anyhow::bail;
use awaitgroup::WaitGroup;
use futures::FutureExt;
use futures::StreamExt;
use guid::Guid;
use inspect::Inspect;
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
use mesh_worker::RegisteredWorkers;
use mesh_worker::Worker;
use mesh_worker::WorkerId;
use mesh_worker::WorkerRpc;
use netvsp_resources::NetvspHandle;
use openvmm_defs::config::Config;
use openvmm_defs::config::DEFAULT_MMIO_GAPS_X86;
use openvmm_defs::config::DEFAULT_PCIE_ECAM_BASE;
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
use openvmm_helpers::disk::open_disk_type;
use openvmm_ttrpc_vmservice as vmservice;
use pal_async::DefaultDriver;
use pal_async::DefaultPool;
use pal_async::task::Spawn;
use parking_lot::Mutex;
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
use vm_resource::kind::VmbusDeviceHandleKind;
use vmm_core_defs::HaltReason;

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
                worker_handle: None,
                rpc_wait_group: WaitGroup::new(),
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
            futures::select! { // merge semantics
                message = vm_service_recv.next() => match message {
                    Some((ctx, message)) => {
                        match self.handle(ctx, message).await {
                            HandleAction::None => (),
                            HandleAction::Quit(quit) => break Some(quit),
                        }
                    }
                    None => {
                        tracing::debug!("no more ttrpc requests");
                        break None;
                    }
                },
                message = inspect_service_recv.next() => match message {
                    Some((ctx, message)) => {
                        self.handle_inspect(ctx, message).await;
                    }
                    None => {
                        tracing::debug!("no more ttrpc requests");
                        break None;
                    }
                },
                request = recv.recv().fuse() => {
                    match request {
                        Ok(WorkerRpc::Restart(rpc)) => rpc.complete(Err(RemoteError::new(anyhow::anyhow!("not supported")))),
                        Ok(WorkerRpc::Inspect(_)) => (),
                        Ok(WorkerRpc::Stop) => {
                            tracing::info!("ttrpc worker stopping");
                            break None;
                        }
                        Err(err) => {
                            tracing::info!(error = &err as &dyn std::error::Error, "ttrpc worker tearing down");
                            break None;
                        }
                    }
                }
            }
        };

        if let Some(mut worker_handle) = self.worker_handle.take() {
            worker_handle.stop();
            if let Err(err) = worker_handle.join().await {
                tracing::error!(
                    error = err.as_ref() as &dyn std::error::Error,
                    "VM worker failed"
                );
            }
        }

        // Drain any remaining RPCs.
        self.rpc_wait_group.wait().await;
        if let Some(vm) = self.vm.take() {
            let _ = Arc::try_unwrap(vm).ok().expect("no more VM references");
        }
        if let Some(quit) = quit {
            quit.send(Ok(()));
        }
        drop(cancel_send);
        server_task.await
    }

    fn start_rpc<F, R>(
        &self,
        response: mesh::OneshotSender<Result<R, Status>>,
        r: anyhow::Result<F>,
    ) where
        F: 'static + Future<Output = anyhow::Result<R>> + Send,
        R: 'static + MeshPayload + Send,
    {
        match r {
            Ok(fut) => {
                let worker = self.rpc_wait_group.worker();
                self.driver
                    .spawn("ttrpc-rpc", async move {
                        response.send(map_grpc(fut.await));
                        worker.done();
                    })
                    .detach();
            }
            Err(err) => response.send(Err(grpc_error(err))),
        }
    }
}

struct Vm {
    worker_rpc: mesh::Sender<VmRpc>,
    scsi_rpc: Option<mesh::Sender<ScsiControllerRequest>>,
    notify_recv: Mutex<Option<mesh::Receiver<HaltReason>>>,
}

struct VmService {
    driver: DefaultDriver,
    vm: Option<Arc<Vm>>,
    worker_handle: Option<mesh_worker::WorkerHandle>,
    rpc_wait_group: WaitGroup,
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
    Quit(mesh::OneshotSender<Result<(), Status>>),
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
            vmservice::Vm::Quit((), response) => return HandleAction::Quit(response),
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
                        let r = self.wait_vm(ctx, vm);
                        self.start_rpc(response, r);
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
                if let Some(worker) = &self.worker_handle {
                    worker.inspect(req)
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
                if let Some(worker) = &self.worker_handle {
                    worker.inspect(req)
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

        let load_mode = match req_config
            .boot_config
            .context("missing boot configuration")?
        {
            vmservice::vm_config::BootConfig::DirectBoot(boot) => {
                let kernel = File::open(boot.kernel_path).context("failed to open kernel")?;
                let initrd_file = File::open(boot.initrd_path).context("failed to open initrd")?;
                LoadMode::Linux {
                    kernel,
                    initrd: Some(initrd_file),
                    cmdline: boot.kernel_cmdline,
                    custom_dsdt: None,
                    enable_serial: true,
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
            *pc = Some(bind_serial(port.socket_path.as_ref()).with_context(|| {
                format!("failed to bind to serial socket: {}", port.socket_path)
            })?);
        }

        let chipset = VmManifestBuilder::new(
            vm_manifest_builder::BaseChipsetType::HyperVGen2LinuxDirect,
            vm_manifest_builder::MachineArch::X86_64,
        )
        .with_serial(ports)
        .build()
        .context("failed to build vm configuration")?;

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
                mem_size: req_config
                    .memory_config
                    .as_ref()
                    .context("missing memory configuration")?
                    .memory_mb
                    .checked_mul(0x100000)
                    .context("invalid memory configuration")?,
                mmio_gaps: DEFAULT_MMIO_GAPS_X86.into(),
                prefetch_memory: false,
                pcie_ecam_base: DEFAULT_PCIE_ECAM_BASE,
            },
            chipset: chipset.chipset,
            processor_topology: ProcessorTopologyConfig {
                proc_count: req_config
                    .processor_config
                    .as_ref()
                    .map(|c| c.processor_count)
                    .unwrap_or(1),
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
                    devices.push(make_disk_config(disk)?);
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
                    config.virtio_devices.push((VirtioBus::Pci, resource));
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

        let (host, runner) = mesh_worker::worker_host();
        self.driver
            .spawn("worker-host", runner.run(RegisteredWorkers))
            .detach();

        let worker = host
            .launch_worker(
                VM_WORKER,
                VmWorkerParameters {
                    hypervisor: None,
                    cfg: config,
                    saved_state: None,
                    rpc: recv,
                    notify: notify_send,
                },
            )
            .await?;

        self.worker_handle = Some(worker);
        self.vm = Some(Arc::new(Vm {
            scsi_rpc,
            notify_recv: Mutex::new(Some(notify_recv)),
            worker_rpc: send,
        }));
        Ok(())
    }

    async fn teardown_vm(&mut self) -> anyhow::Result<()> {
        let mut worker_handle = self.worker_handle.take().context("vm not created")?;
        worker_handle.stop();
        worker_handle.join().await?;
        let _ = self.vm.take();
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

    fn wait_vm(
        &mut self,
        mut ctx: mesh::CancelContext,
        vm: Arc<Vm>,
    ) -> anyhow::Result<impl Future<Output = anyhow::Result<()>> + use<>> {
        let mut notify_recv = vm
            .notify_recv
            .lock()
            .take()
            .context("wait VM already in flight")?;
        Ok(async move {
            let r = futures::select! { // race semantics
                r = notify_recv.recv().fuse() => {
                    r.context("VM worker communication failure")
                }
                reason = ctx.cancelled().fuse() => {
                    Err(anyhow::Error::new(reason))
                }
            };
            *vm.notify_recv.lock() = Some(notify_recv);
            r?;
            Ok(())
        })
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
                    let config = make_disk_config(disk)?;
                    let recv = vm
                        .scsi_rpc
                        .as_ref()
                        .context("no scsi controller")?
                        .call_failable(ScsiControllerRequest::AddDevice, config);
                    Ok(async move { recv.await.map_err(anyhow::Error::from) }.boxed())
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

fn parse_nic_config(
    nic: vmservice::NicConfig,
) -> anyhow::Result<(DeviceVtl, Resource<VmbusDeviceHandleKind>)> {
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
        #[cfg(unix)]
        Backend::Tap(tap) => {
            net_backend_resources::tap::TapHandle { name: tap.name }.into_resource()
        }
        _ => anyhow::bail!("unsupported backend"),
    };
    let cfg = NetvspHandle {
        instance_id: nic.nic_id.parse().context("invalid instance ID")?,
        mac_address: nic
            .mac_address
            .parse::<macaddr::MacAddr6>()
            .context("invalid mac address")?
            .into_array()
            .into(),
        endpoint,
        max_queues: None,
    };
    Ok((DeviceVtl::Vtl0, cfg.into_resource()))
}

fn make_disk_config(disk: vmservice::ScsiDisk) -> anyhow::Result<ScsiDeviceAndPath> {
    Ok(ScsiDeviceAndPath {
        path: storvsp_resources::ScsiPath {
            path: 0,
            target: 0,
            lun: disk.lun.try_into().ok().context("lun value out of range")?,
        },
        device: SimpleScsiDiskHandle {
            disk: open_disk_type(disk.host_path.as_ref(), disk.read_only)
                .with_context(|| format!("failed to open {}", disk.host_path))?,
            read_only: disk.read_only,
            parameters: Default::default(),
        }
        .into_resource(),
    })
}
