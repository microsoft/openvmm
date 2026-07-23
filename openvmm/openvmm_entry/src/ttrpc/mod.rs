// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Worker for the prototype gRPC/ttrpc management endpoint.

#![cfg(any(feature = "ttrpc", feature = "grpc"))]

// The fd-passing protocol relies on `SCM_RIGHTS` and so exists only on unix.
#[cfg(unix)]
mod fd_passing;

#[cfg(unix)]
use fd_passing::FdRegistry;

/// On non-unix platforms the fd-passing protocol does not exist. The registry
/// is still threaded through the shared NIC configuration code, so provide an
/// empty placeholder there; it is never populated or resolved.
#[cfg(not(unix))]
#[derive(Clone, Default)]
struct FdRegistry {}

use crate::meshworker::VmmMesh;
use crate::serial_io::bind_serial;
use crate::serial_io::connect_serial;
use crate::vm_controller::GuestPowerActions;
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
use memory_range::MemoryRange;
use mesh::CancelReason;
use mesh::MeshPayload;
use mesh::error::RemoteError;
use mesh::rpc::RpcSend;
use mesh_rpc::service::Code;
use mesh_rpc::service::Status;
use mesh_worker::Worker;
use mesh_worker::WorkerId;
use mesh_worker::WorkerRpc;
use net_backend_resources::consomme::ConsommeRequest;
use net_backend_resources::consomme::HostPort;
use net_backend_resources::consomme::HostPortConfig;
use net_backend_resources::consomme::HostPortProtocol;
use net_backend_resources::mac_address::MacAddress;
use netvsp_resources::NetvspHandle;
use openvmm_defs::config::Config;
use openvmm_defs::config::DeviceVtl;
use openvmm_defs::config::HypervisorConfig;
use openvmm_defs::config::LoadMode;
use openvmm_defs::config::MemoryConfig;
use openvmm_defs::config::NumaDistance;
use openvmm_defs::config::NumaNode;
use openvmm_defs::config::NumaTopology;
use openvmm_defs::config::PcieDeviceConfig;
use openvmm_defs::config::PcieMmioRangeConfig;
use openvmm_defs::config::PciePortConfig;
use openvmm_defs::config::PcieRootComplexConfig;
use openvmm_defs::config::PcieSwitchConfig;
use openvmm_defs::config::ProcessorTopologyConfig;
use openvmm_defs::config::VirtioBus;
use openvmm_defs::config::VmbusConfig;
use openvmm_defs::config::VpAssignment;
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
use vm_resource::kind::DiskHandleKind;
use vm_resource::kind::NetEndpointHandleKind;
use vm_resource::kind::PciDeviceHandleKind;
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
    /// Auto-detect ttrpc vs. gRPC per connection, based on the first byte of
    /// the stream.
    Auto,
}

impl std::fmt::Display for RpcTransport {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.pad(match self {
            RpcTransport::Ttrpc => "ttrpc",
            RpcTransport::Grpc => "grpc",
            RpcTransport::Auto => "auto",
        })
    }
}

#[derive(Copy, Clone)]
enum ResolvedTransport {
    #[cfg(feature = "ttrpc")]
    Ttrpc,
    #[cfg(feature = "grpc")]
    Grpc,
    Auto,
}

impl ResolvedTransport {
    /// Returns whether ttrpc connections are permitted in this mode.
    #[cfg(feature = "ttrpc")]
    fn allows_ttrpc(self) -> bool {
        match self {
            ResolvedTransport::Ttrpc => true,
            #[cfg(feature = "grpc")]
            ResolvedTransport::Grpc => false,
            ResolvedTransport::Auto => true,
        }
    }

    /// Returns whether gRPC connections are permitted in this mode.
    #[cfg(feature = "grpc")]
    fn allows_grpc(self) -> bool {
        match self {
            #[cfg(feature = "ttrpc")]
            ResolvedTransport::Ttrpc => false,
            ResolvedTransport::Grpc => true,
            ResolvedTransport::Auto => true,
        }
    }
}

/// The RPC server accept loop.
///
/// This owns the accept loop rather than delegating to
/// [`mesh_rpc::Server::run`], so that the protocol used for each connection can
/// be chosen based on the compiled-in features and the configured transport,
/// including auto-detecting ttrpc vs. gRPC from the first byte on the wire.
///
/// Neither ttrpc nor gRPC has an explicit negotiation phase, but their first
/// byte on the wire is distinct, so a single non-consuming peek is enough to
/// classify a connection:
///
/// * ttrpc frames begin with a big-endian `u32` length field whose most
///   significant byte is always zero (messages are capped well under 16 MiB),
///   so the first byte is `0x00`.
/// * gRPC uses HTTP/2 cleartext, whose client connection preface begins with
///   the ASCII bytes `"PRI "`, so the first byte is `b'P'`.
/// * The OpenVMM fd-passing protocol (UNIX only) begins with a handshake whose
///   first byte is `0xFD`, distinct from both of the above.
mod dispatch {
    use super::FdRegistry;
    use super::ResolvedTransport;
    use futures::FutureExt;
    use pal_async::driver::Driver;
    use pal_async::socket::AsSockRef;
    use pal_async::socket::PolledSocket;
    use std::io::Read;
    use std::io::Write;
    use unicycle::FuturesUnordered;
    use unix_socket::UnixListener;
    use unix_socket::UnixStream;

    /// Runs the RPC server, listening on `listener` and servicing connections
    /// until `cancel`, dispatching each connection according to `transport`.
    pub(super) async fn run(
        server: &mesh_rpc::Server,
        driver: &(impl Driver + ?Sized),
        listener: UnixListener,
        cancel: mesh::OneshotReceiver<()>,
        transport: ResolvedTransport,
        registry: FdRegistry,
    ) -> anyhow::Result<()> {
        let mut listener = PolledSocket::new(driver, listener)?;
        let mut tasks = FuturesUnordered::new();
        let mut cancel = cancel.fuse();
        loop {
            let conn = futures::select! { // merge semantics
                r = listener.accept().fuse() => r,
                _ = tasks.next() => continue,
                _ = cancel => break,
            };
            if let Ok(conn) = conn.and_then(|(conn, _)| PolledSocket::new(driver, conn)) {
                let registry = registry.clone();
                tasks.push(async move {
                    let _ = serve(server, conn, transport, &registry)
                        .await
                        .map_err(|err| {
                            tracing::error!(
                                error = err.as_ref() as &dyn std::error::Error,
                                "connection error"
                            )
                        });
                });
            }
        }
        Ok(())
    }

    /// Services a single connection.
    ///
    /// The protocol is always determined by peeking the first byte of the
    /// stream; the configured `transport` only restricts which protocols are
    /// permitted (e.g. a ttrpc-only server rejects a gRPC client).
    async fn serve(
        server: &mesh_rpc::Server,
        mut conn: PolledSocket<UnixStream>,
        transport: ResolvedTransport,
        registry: &FdRegistry,
    ) -> anyhow::Result<()> {
        // Wait for the client to send data (returning early if it hangs up
        // first) and classify the protocol from its first byte.
        let Some(first_byte) = peek_first_byte(&mut conn).await? else {
            return Ok(());
        };

        // The fd-passing protocol (UNIX only) is allowed in every transport
        // mode; it shares the socket with ttrpc/gRPC and is selected by its
        // distinct magic first byte.
        #[cfg(not(unix))]
        let _ = registry;

        match first_byte {
            #[cfg(feature = "ttrpc")]
            0x00 if transport.allows_ttrpc() => server.serve_connection(conn).await,
            #[cfg(feature = "grpc")]
            b'P' if transport.allows_grpc() => server.serve_connection_grpc(conn).await,
            #[cfg(unix)]
            super::fd_passing::MAGIC_FIRST_BYTE => super::fd_passing::serve(conn, registry).await,
            byte => {
                anyhow::bail!("unrecognized or disallowed rpc protocol (first byte {byte:#04x})")
            }
        }
    }

    /// Waits for the first byte of the stream to be available and returns it
    /// without consuming it, so the chosen protocol handler sees a pristine
    /// stream.
    ///
    /// Returns `None` if the peer closed the connection before sending any data.
    async fn peek_first_byte(
        conn: &mut PolledSocket<impl AsSockRef + Read + Write>,
    ) -> std::io::Result<Option<u8>> {
        let mut buf = [0u8; 1];
        let n = conn.peek(&mut buf).await?;
        Ok((n != 0).then_some(buf[0]))
    }
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
                RpcTransport::Auto => ResolvedTransport::Auto,
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
                lifecycle: VmLifecycle::Uninitialized,
                rpc_tasks: Vec::new(),
                transport: self.transport,
                registry: FdRegistry::default(),
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
        let registry = self.registry.clone();
        let (cancel_send, cancel_recv) = mesh::oneshot();
        let server_task = self.driver.spawn("ttrpc-server", {
            let driver = self.driver.clone();
            async move {
                let r = dispatch::run(&server, &driver, listener, cancel_recv, transport, registry)
                    .await;
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
    consomme_rpc: Option<mesh::Sender<ConsommeRequest>>,
}

enum VmLifecycle {
    Uninitialized,
    Running,
    Paused,
    Halted(String),
}

impl From<&VmLifecycle> for vmservice::VmState {
    fn from(lifecycle: &VmLifecycle) -> Self {
        match lifecycle {
            VmLifecycle::Uninitialized => vmservice::VmState::Uninitialized,
            VmLifecycle::Running => vmservice::VmState::Running,
            VmLifecycle::Paused => vmservice::VmState::Paused,
            VmLifecycle::Halted(_) => vmservice::VmState::Halted,
        }
    }
}

struct VmService {
    driver: DefaultDriver,
    vm: Option<Arc<Vm>>,
    vm_controller: Option<mesh::Sender<VmControllerRpc>>,
    vm_controller_events: Option<mesh::Receiver<VmControllerEvent>>,
    controller_task: Option<Task<()>>,
    wait_vm_response: Option<(mesh::CancelContext, mesh::OneshotSender<Result<(), Status>>)>,
    lifecycle: VmLifecycle,
    rpc_tasks: Vec<Task<()>>,
    transport: ResolvedTransport,
    /// Registry of file descriptors passed in over the fd-passing protocol,
    /// resolvable by name (e.g. for tap NIC backends).
    registry: FdRegistry,
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
            vmservice::Vm::CapabilitiesVm((), response) => {
                response.send(Ok(self.build_capabilities()));
            }
            vmservice::Vm::PropertiesVm(_request, response) => {
                response.send(Ok(self.build_properties()));
            }
            vmservice::Vm::PauseVm((), response) => {
                response.send(map_grpc(self.pause_vm().await));
            }
            vmservice::Vm::ResumeVm((), response) => {
                response.send(map_grpc(self.resume_vm().await));
            }
            vmservice::Vm::WaitVm((), response) => {
                if self.vm.is_none() {
                    response.send(Err(grpc_error(anyhow!("VM not created yet"))));
                } else if self.wait_vm_response.is_some() {
                    response.send(Err(grpc_error(anyhow!("wait VM already in flight"))));
                } else if matches!(self.lifecycle, VmLifecycle::Halted(_)) {
                    response.send(Ok(()));
                } else {
                    self.wait_vm_response = Some((ctx.clone(), response));
                }
            }
            vmservice::Vm::ModifyResource(request, response) => {
                let r = self.modify_resource(request);
                self.start_rpc(response, r);
            }
            vmservice::Vm::AddPcieDevice(request, response) => {
                let r = self.add_pcie_device(request);
                self.start_rpc(response, r);
            }
            vmservice::Vm::RemovePcieDevice(request, response) => {
                let r = self.remove_pcie_device(request);
                self.start_rpc(response, r);
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
        let mut req_config = request.config.context("missing configuration")?;

        if self.vm.is_some() {
            bail!("VM already created");
        }

        // Snapshot the fd registry so tap NIC backends can resolve descriptors
        // passed in over the fd-passing protocol.
        let registry = self.registry.clone();

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

        #[cfg(guest_arch = "aarch64")]
        let arch = vm_manifest_builder::MachineArch::Aarch64;
        #[cfg(guest_arch = "x86_64")]
        let arch = vm_manifest_builder::MachineArch::X86_64;

        let chipset_builder = VmManifestBuilder::new(
            vm_manifest_builder::BaseChipsetType::HyperVGen2LinuxDirect,
            arch,
        )
        .with_serial(ports);
        let layout_config = chipset_builder.layout_config();
        let chipset = chipset_builder
            .build()
            .context("failed to build vm configuration")?;

        // Build the NUMA topology. A `MemoryConfig` and an explicit
        // `NumaConfig` are mutually exclusive (mirrors the CLI `--memory` vs
        // `--numa` conflict). `config_mem_size` is the total guest memory
        // reported to the `VmController`.
        let (numa, config_mem_size) = if let Some(numa_config) = req_config.numa_config.take() {
            if req_config.memory_config.is_some() {
                bail!("memory_config and numa_config are mutually exclusive");
            }
            build_numa_topology(numa_config)?
        } else {
            let mem_size = req_config
                .memory_config
                .as_ref()
                .context("missing memory configuration")?
                .memory_mb
                .checked_mul(0x100000)
                .context("invalid memory configuration")?;
            let numa = NumaTopology {
                nodes: vec![NumaNode {
                    mem: Some(MemoryConfig {
                        mem_size,
                        prefetch_memory: false,
                        private_memory: false,
                        transparent_hugepages: true,
                        hugepages: false,
                        hugepage_size: None,
                        host_numa_node: None,
                    }),
                    vps: VpAssignment::FromTopology,
                }],
                distances: vec![],
            };
            (numa, mem_size)
        };

        let config_proc_count = req_config
            .processor_config
            .as_ref()
            .map(|c| c.processor_count)
            .unwrap_or(1);

        // Build the PCIe topology (root complexes, switches, and the devices
        // attached behind their ports).
        let (pcie_root_complexes, pcie_switches, pcie_devices) =
            if let Some(pcie) = req_config.pcie.take() {
                build_pcie_topology(pcie, &registry).await?
            } else {
                (Vec::new(), Vec::new(), Vec::new())
            };

        let mut config = Config {
            // TODO: devices, other stuff
            load_mode,
            ide_disks: vec![],
            floppy_disks: vec![],
            pcie_root_complexes,
            pcie_devices,
            pcie_switches,
            pcie_generic_initiators: vec![],
            vpci_devices: vec![],
            numa,
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
            firmware_event_send: None,
            debugger_rpc: None,
            chipset_devices: chipset.chipset_devices,
            pci_chipset_devices: chipset.pci_chipset_devices,
            isa_dma_controller: chipset.isa_dma_controller,
            chipset_capabilities: chipset.capabilities,
            layout: layout_config,
            rtc_delta_milliseconds: 0,
            automatic_guest_reset: true,
        };

        let mut scsi_rpc = None;
        let mut consomme_rpc = None;
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
                let is_consomme = matches!(
                    &nic.backend,
                    Some(vmservice::nic_config::Backend::Consomme(_))
                );
                // Only wire the bind/unbind RPC channel to the first consomme
                // NIC. Additional consomme NICs work but cannot be targeted by
                // runtime bind/unbind commands.
                let recv = if is_consomme && consomme_rpc.is_none() {
                    let (send, recv) = mesh::channel();
                    consomme_rpc = Some(send);
                    Some(recv)
                } else {
                    None
                };
                config
                    .vmbus_devices
                    .push(parse_nic_config(nic, recv, &registry)?);
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
                        vnode: None,
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
                            vnode: None,
                        });
                    } else {
                        config.virtio_devices.push((VirtioBus::Mmio, resource));
                    }
                }
            }
        }

        if let Some(hvsocket_config) = req_config.hvsocket_config {
            let listener = UnixListener::bind(&hvsocket_config.path).with_context(|| {
                format!("failed to bind hvsocket path: {}", hvsocket_config.path)
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
            // The ttrpc/grpc server never exits on a guest power event; it uses
            // the historical defaults (none of which is Exit), so the
            // ExitRequested event handled below is unreachable here.
            guest_power_actions: GuestPowerActions::default(),
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
            consomme_rpc,
            worker_rpc: send,
        }));
        self.lifecycle = VmLifecycle::Paused;
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
        self.lifecycle = VmLifecycle::Uninitialized;
        if let Some((_, response)) = self.wait_vm_response.take() {
            response.send(Err(grpc_error(anyhow!("VM torn down"))));
        }
        Ok(())
    }

    fn build_properties(&self) -> vmservice::PropertiesVmResponse {
        let halt_reason = match &self.lifecycle {
            VmLifecycle::Halted(reason) => Some(reason.clone()),
            _ => None,
        };
        vmservice::PropertiesVmResponse {
            memory_stats: None,
            processor_stats: None,
            state: vmservice::VmState::from(&self.lifecycle) as i32,
            halt_reason,
        }
    }

    fn build_capabilities(&self) -> vmservice::CapabilitiesVmResponse {
        use vmservice::capabilities_vm_response::Resource;
        use vmservice::capabilities_vm_response::SupportedGuestOs;
        use vmservice::capabilities_vm_response::SupportedResource;

        vmservice::CapabilitiesVmResponse {
            supported_resources: vec![
                SupportedResource {
                    resource: Resource::Scsi as i32,
                    add: true,
                    remove: true,
                    update: false,
                },
                SupportedResource {
                    resource: Resource::Vpci as i32,
                    add: true,
                    remove: true,
                    update: false,
                },
                SupportedResource {
                    resource: Resource::VmNic as i32,
                    add: true,
                    remove: true,
                    update: true,
                },
            ],
            supported_guest_os: vec![SupportedGuestOs::Linux as i32],
        }
    }

    async fn pause_vm(&mut self) -> anyhow::Result<()> {
        let vm = self.vm.clone().context("VM not created yet")?;
        vm.worker_rpc
            .call(VmRpc::Pause, ())
            .await
            .map(drop)
            .context("pause failed")?;
        if !matches!(self.lifecycle, VmLifecycle::Halted(_)) {
            self.lifecycle = VmLifecycle::Paused;
        }
        Ok(())
    }

    async fn resume_vm(&mut self) -> anyhow::Result<()> {
        let vm = self.vm.clone().context("VM not created yet")?;
        vm.worker_rpc
            .call(VmRpc::Resume, ())
            .await
            .map(drop)
            .context("resume failed")?;
        if !matches!(self.lifecycle, VmLifecycle::Halted(_)) {
            self.lifecycle = VmLifecycle::Running;
        }
        Ok(())
    }

    fn handle_controller_event(&mut self, event: VmControllerEvent) {
        match event {
            VmControllerEvent::GuestHalt(reason) => {
                tracing::info!(%reason, "guest halted (via controller)");
                self.lifecycle = VmLifecycle::Halted(reason);
                if let Some((_, response)) = self.wait_vm_response.take() {
                    response.send(Ok(()));
                }
            }
            VmControllerEvent::ExitRequested { code } => {
                // The server leaves the guest power actions at their defaults
                // (none is `exit`), so this should not occur in ttrpc/grpc mode;
                // log rather than exiting the server out from under its clients.
                tracing::warn!(code, "unexpected exit request in server mode");
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
                self.lifecycle = VmLifecycle::Uninitialized;
            }
            VmControllerEvent::VncWorkerStopped { error } => {
                if let Some(err) = &error {
                    tracing::error!(error = %err, "VNC worker stopped unexpectedly");
                }
            }
        }
    }

    fn add_pcie_device(
        &self,
        request: vmservice::AddPcieDeviceRequest,
    ) -> anyhow::Result<impl Future<Output = anyhow::Result<()>> + use<>> {
        let worker_rpc = self
            .vm
            .as_ref()
            .context("VM not created yet")?
            .worker_rpc
            .clone();
        let registry = self.registry.clone();
        Ok(async move {
            let vmservice::AddPcieDeviceRequest { port_name, device } = request;
            let resource = build_pcie_device(device.context("missing device")?, &registry).await?;
            worker_rpc
                .call_failable(VmRpc::AddPcieDevice, (port_name, resource))
                .await
                .map_err(anyhow::Error::from)
        })
    }

    fn remove_pcie_device(
        &self,
        request: vmservice::RemovePcieDeviceRequest,
    ) -> anyhow::Result<impl Future<Output = anyhow::Result<()>> + use<>> {
        let recv = self
            .vm
            .as_ref()
            .context("VM not created yet")?
            .worker_rpc
            .call_failable(VmRpc::RemovePcieDevice, request.port_name);
        Ok(async move { recv.await.map_err(anyhow::Error::from) })
    }

    fn modify_resource(
        &self,
        request: vmservice::ModifyResourceRequest,
    ) -> anyhow::Result<impl Future<Output = anyhow::Result<()>> + use<>> {
        use vmservice::modify_resource_request::Resource;
        let vm = self.vm.as_ref().context("VM not created yet")?;
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
                if request.r#type == vmservice::ModifyType::Add as i32 {
                    if matches!(
                        &nic.backend,
                        Some(vmservice::nic_config::Backend::Consomme(_))
                    ) {
                        anyhow::bail!(
                            "adding a consomme NIC via ModifyResource is not supported; \
                             configure it at VM creation time"
                        );
                    }
                    let config = parse_nic_config(nic, None, &self.registry)?;
                    let recv = vm.worker_rpc.call_failable(VmRpc::AddVmbusDevice, config);
                    Ok(async move { recv.await.map_err(anyhow::Error::from) }.boxed())
                } else if request.r#type == vmservice::ModifyType::Update as i32 {
                    let consomme = match nic.backend.context("missing backend")? {
                        vmservice::nic_config::Backend::Consomme(c) => c,
                        _ => anyhow::bail!("port update only supported for consomme backend"),
                    };
                    let consomme_rpc = vm
                        .consomme_rpc
                        .as_ref()
                        .context("no consomme port channel")?
                        .clone();
                    Ok(async move {
                        for port in consomme.ports {
                            let cfg = parse_port_config(port)?;
                            consomme_rpc
                                .call_failable(ConsommeRequest::Bind, cfg)
                                .await
                                .map_err(anyhow::Error::from)?;
                        }
                        Ok(())
                    }
                    .boxed())
                } else if request.r#type == vmservice::ModifyType::Remove as i32 {
                    let consomme = match nic.backend.context("missing backend")? {
                        vmservice::nic_config::Backend::Consomme(c) => c,
                        _ => anyhow::bail!("port remove only supported for consomme backend"),
                    };
                    let consomme_rpc = vm
                        .consomme_rpc
                        .as_ref()
                        .context("no consomme port channel")?
                        .clone();
                    Ok(async move {
                        for port in consomme.ports {
                            let cfg = parse_port_config(port)?;
                            consomme_rpc
                                .call_failable(ConsommeRequest::Unbind, cfg)
                                .await
                                .map_err(anyhow::Error::from)?;
                        }
                        Ok(())
                    }
                    .boxed())
                } else {
                    anyhow::bail!("unsupported NIC modify type {}", request.r#type);
                }
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

/// Convert a ttrpc `PortConfig` (untrusted input) into a `HostPortConfig`,
/// validating the protocol and port ranges. The host port is always treated as
/// a fixed port; the unbind path ignores it.
fn parse_port_config(port: vmservice::PortConfig) -> anyhow::Result<HostPortConfig> {
    let vmservice::PortConfig {
        host_port,
        guest_port,
        protocol,
    } = port;
    let protocol = if protocol == vmservice::IpProtocol::Tcp as i32 {
        HostPortProtocol::Tcp
    } else if protocol == vmservice::IpProtocol::Udp as i32 {
        HostPortProtocol::Udp
    } else {
        anyhow::bail!("invalid protocol {protocol}");
    };
    Ok(HostPortConfig {
        protocol,
        host_address: None,
        host_port: HostPort::Fixed(host_port.try_into().context("host port out of range")?),
        guest_port: guest_port.try_into().context("guest port out of range")?,
    })
}

fn parse_nic_config(
    nic: vmservice::NicConfig,
    recv: Option<mesh::Receiver<ConsommeRequest>>,
    registry: &FdRegistry,
) -> anyhow::Result<(DeviceVtl, Resource<VmbusDeviceHandleKind>)> {
    use self::vmservice::nic_config::Backend;
    #[cfg(not(target_os = "linux"))]
    let _ = registry;
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
        Backend::Tap(tap) => build_tap_backend(tap, registry)?,
        Backend::Consomme(consomme) => net_backend_resources::consomme::ConsommeHandle {
            cidr: if consomme.cidr.is_empty() {
                None
            } else {
                Some(consomme.cidr)
            },
            ports: consomme
                .ports
                .into_iter()
                .map(parse_port_config)
                .collect::<anyhow::Result<_>>()?,
            recv,
        }
        .into_resource(),
        _ => anyhow::bail!("unsupported backend"),
    };
    let cfg = NetvspHandle {
        instance_id: nic.nic_id.parse().context("invalid instance ID")?,
        mac_address: nic
            .mac_address
            .parse::<MacAddress>()
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

/// Builds a [`NumaTopology`] from the proto `NumaConfig`, returning the
/// topology and the total guest memory in bytes (summed across the nodes).
fn build_numa_topology(numa: vmservice::NumaConfig) -> anyhow::Result<(NumaTopology, u64)> {
    let vmservice::NumaConfig {
        nodes: proto_nodes,
        distances: proto_distances,
    } = numa;
    let mut total_mem = 0u64;
    let mut nodes = Vec::new();
    for node in proto_nodes {
        let vmservice::NumaNode { memory, vps } = node;
        let mem = if let Some(mem) = memory {
            let vmservice::NodeMemoryConfig {
                memory_mb,
                host_numa_node,
                prefetch,
                private_memory,
                transparent_hugepages,
                hugepages,
                hugepage_size_bytes,
            } = mem;
            let mem_size = memory_mb
                .checked_mul(0x100000)
                .context("invalid node memory size")?;
            total_mem = total_mem
                .checked_add(mem_size)
                .context("total memory overflow")?;
            Some(MemoryConfig {
                mem_size,
                prefetch_memory: prefetch,
                private_memory,
                transparent_hugepages: transparent_hugepages.unwrap_or(true),
                hugepages,
                hugepage_size: hugepage_size_bytes,
                host_numa_node,
            })
        } else {
            None
        };
        // Absent => `FromTopology`; present-but-empty => `Empty` (CPU-less);
        // present-and-non-empty => explicit VP indices.
        let vps = match vps {
            None => VpAssignment::FromTopology,
            Some(vmservice::VpAssignment { vp_index }) if vp_index.is_empty() => {
                VpAssignment::Empty
            }
            Some(vmservice::VpAssignment { vp_index }) => VpAssignment::Explicit(vp_index),
        };
        nodes.push(NumaNode { mem, vps });
    }

    let distances = proto_distances
        .into_iter()
        .map(|d| {
            let vmservice::NumaDistance { src, dst, distance } = d;
            Ok(NumaDistance {
                src,
                dst,
                distance: distance.try_into().context("distance out of range")?,
            })
        })
        .collect::<anyhow::Result<Vec<_>>>()?;

    Ok((NumaTopology { nodes, distances }, total_mem))
}

/// Converts a proto MMIO window (a size plus an optional pinned base) into a
/// [`PcieMmioRangeConfig`].
fn pcie_mmio_range_config(size: u64, base: Option<u64>) -> anyhow::Result<PcieMmioRangeConfig> {
    Ok(if let Some(base) = base {
        let end = base
            .checked_add(size)
            .context("MMIO base + size overflows")?;
        PcieMmioRangeConfig::Fixed(MemoryRange::try_new(base..end).context("invalid MMIO range")?)
    } else {
        PcieMmioRangeConfig::Dynamic { size }
    })
}

/// Flattens the nested proto PCIe topology into the flat config representation:
/// a list of root complexes (each carrying its root ports), a list of switches
/// (each referencing its parent port), and a list of devices (each referencing
/// the port it sits behind).
async fn build_pcie_topology(
    topology: vmservice::PcieTopologyConfig,
    registry: &FdRegistry,
) -> anyhow::Result<(
    Vec<PcieRootComplexConfig>,
    Vec<PcieSwitchConfig>,
    Vec<PcieDeviceConfig>,
)> {
    let vmservice::PcieTopologyConfig {
        root_complexes: proto_root_complexes,
    } = topology;
    let mut root_complexes = Vec::new();
    let mut switches = Vec::new();
    // Devices are built after the topology walk so that the (async) device
    // construction does not need to recurse.
    let mut pending_devices: Vec<(String, vmservice::PcieDeviceKind)> = Vec::new();

    for (index, rc) in proto_root_complexes.into_iter().enumerate() {
        let vmservice::PcieRootComplex {
            name,
            segment,
            start_bus,
            end_bus,
            low_mmio,
            high_mmio,
            low_mmio_base,
            high_mmio_base,
            preserve_bars,
            node,
            root_ports,
        } = rc;
        let mut ports = Vec::new();
        for root_port in root_ports {
            let vmservice::PciePort {
                name: port_name,
                hotplug,
                attached,
                devfn,
            } = root_port;
            ports.push(PciePortConfig {
                name: port_name.clone(),
                devfn: devfn
                    .map(|d| d.try_into().context("devfn out of range"))
                    .transpose()?,
                hotplug,
                acs_capabilities_supported: None,
                cxl: false,
                pasid: false,
            });
            if let Some(attached) = attached {
                walk_pcie_attachment(port_name, attached, &mut switches, &mut pending_devices)?;
            }
        }

        root_complexes.push(PcieRootComplexConfig {
            index: index as u32,
            name,
            segment: segment.try_into().context("segment out of range")?,
            start_bus: start_bus.try_into().context("start_bus out of range")?,
            end_bus: end_bus.try_into().context("end_bus out of range")?,
            low_mmio: pcie_mmio_range_config(low_mmio, low_mmio_base)?,
            high_mmio: pcie_mmio_range_config(high_mmio, high_mmio_base)?,
            ports,
            cxl: None,
            iommu: None,
            vnode: node,
            preserve_bars,
        });
    }

    let mut devices = Vec::new();
    for (port_name, device) in pending_devices {
        let resource = build_pcie_device(device, registry).await?;
        devices.push(PcieDeviceConfig {
            port_name,
            resource,
        });
    }

    Ok((root_complexes, switches, devices))
}

/// Walks a single proto `PcieAttachment` (the thing behind one port): either an
/// endpoint device (queued in `pending_devices`) or a nested switch (appended
/// to `switches`, recursing into its downstream ports).
fn walk_pcie_attachment(
    port_name: String,
    attachment: vmservice::PcieAttachment,
    switches: &mut Vec<PcieSwitchConfig>,
    pending_devices: &mut Vec<(String, vmservice::PcieDeviceKind)>,
) -> anyhow::Result<()> {
    match attachment.kind.context("missing attachment kind")? {
        vmservice::pcie_attachment::Kind::Device(device) => {
            pending_devices.push((port_name, device));
        }
        vmservice::pcie_attachment::Kind::Switch(switch) => {
            let vmservice::PcieSwitch {
                name: switch_name,
                downstream_ports,
            } = switch;
            let mut ports = Vec::new();
            let mut children = Vec::new();
            for downstream in downstream_ports {
                let vmservice::PciePort {
                    name: downstream_name,
                    hotplug,
                    attached,
                    devfn,
                } = downstream;
                ports.push(PciePortConfig {
                    name: downstream_name.clone(),
                    devfn: devfn
                        .map(|d| d.try_into().context("devfn out of range"))
                        .transpose()?,
                    hotplug,
                    acs_capabilities_supported: None,
                    cxl: false,
                    pasid: false,
                });
                if let Some(attached) = attached {
                    children.push((downstream_name, attached));
                }
            }
            switches.push(PcieSwitchConfig {
                name: switch_name,
                parent_port: port_name,
                ports,
            });
            for (downstream_name, attached) in children {
                walk_pcie_attachment(downstream_name, attached, switches, pending_devices)?;
            }
        }
    }
    Ok(())
}

/// Builds the resource for a single endpoint PCIe device function (a virtio
/// function, an NVMe controller, or a VFIO-assigned host device).
async fn build_pcie_device(
    device: vmservice::PcieDeviceKind,
    registry: &FdRegistry,
) -> anyhow::Result<Resource<PciDeviceHandleKind>> {
    use vmservice::pcie_device_kind::Kind;
    let vmservice::PcieDeviceKind { kind } = device;
    Ok(match kind.context("missing PCIe device kind")? {
        Kind::Virtio(virtio) => {
            let resource = build_virtio_device(virtio, registry).await?;
            VirtioPciDeviceHandle(resource).into_resource()
        }
        Kind::Nvme(nvme) => build_nvme_controller(nvme).await?,
        Kind::Vfio(vfio) => build_vfio_device(vfio)?,
    })
}

/// Builds a VFIO-assigned host PCI device resource from the proto `VfioDevice`.
///
/// Uses the legacy VFIO group/container path: the device's IOMMU group is
/// resolved from sysfs and the corresponding `/dev/vfio/<group_id>` file is
/// opened. The device must already be bound to `vfio-pci` on the host.
#[cfg(target_os = "linux")]
fn build_vfio_device(vfio: vmservice::VfioDevice) -> anyhow::Result<Resource<PciDeviceHandleKind>> {
    let vmservice::VfioDevice { host_pci_address } = vfio;
    // The address is joined into a sysfs path below; reject path separators so
    // it cannot escape `/sys/bus/pci/devices` (an absolute path or `..` would
    // otherwise redirect the join).
    if host_pci_address.contains('/') || host_pci_address.contains("..") {
        anyhow::bail!("PCI address must not contain path separators");
    }
    let sysfs_path = std::path::Path::new("/sys/bus/pci/devices").join(&host_pci_address);
    let iommu_group_link =
        std::fs::read_link(sysfs_path.join("iommu_group")).with_context(|| {
            format!("failed to read IOMMU group for {host_pci_address} (is it bound to vfio-pci?)")
        })?;
    let group_id: u64 = iommu_group_link
        .file_name()
        .and_then(|s| s.to_str())
        .context("invalid iommu_group symlink")?
        .parse()
        .context("failed to parse IOMMU group ID")?;
    let group = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(format!("/dev/vfio/{group_id}"))
        .with_context(|| format!("failed to open /dev/vfio/{group_id}"))?;
    Ok(vfio_assigned_device_resources::VfioDeviceHandle {
        pci_id: host_pci_address,
        group,
        bar_pt: [false; 6],
    }
    .into_resource())
}

#[cfg(not(target_os = "linux"))]
fn build_vfio_device(
    _vfio: vmservice::VfioDevice,
) -> anyhow::Result<Resource<PciDeviceHandleKind>> {
    anyhow::bail!("VFIO device assignment is only supported on Linux")
}

/// Builds an NVMe controller resource from the proto `NvmeConfig`.
async fn build_nvme_controller(
    nvme: vmservice::NvmeConfig,
) -> anyhow::Result<Resource<PciDeviceHandleKind>> {
    let vmservice::NvmeConfig {
        controller_id,
        namespaces: proto_namespaces,
    } = nvme;
    let mut namespaces = Vec::new();
    for ns in proto_namespaces {
        let vmservice::NvmeNamespace {
            nsid,
            backend,
            read_only,
        } = ns;
        let disk =
            build_disk_backend(backend.context("missing namespace backend")?, read_only).await?;
        namespaces.push(nvme_resources::NamespaceDefinition {
            nsid,
            read_only,
            disk,
        });
    }
    Ok(nvme_resources::NvmeControllerHandle {
        subsystem_id: crate::storage_builder::deterministic_guid(&controller_id),
        msix_count: 64,
        max_io_queues: 64,
        namespaces,
        requests: None,
    }
    .into_resource())
}

/// Builds a transport-independent virtio device function from the proto
/// `VirtioDevice`.
async fn build_virtio_device(
    device: vmservice::VirtioDevice,
    registry: &FdRegistry,
) -> anyhow::Result<Resource<VirtioDeviceHandle>> {
    use vmservice::virtio_device::Kind;
    let vmservice::VirtioDevice { kind } = device;
    Ok(match kind.context("missing virtio device kind")? {
        Kind::Blk(vmservice::VirtioBlk { backend, read_only }) => {
            let disk =
                build_disk_backend(backend.context("missing blk backend")?, read_only).await?;
            virtio_resources::blk::VirtioBlkHandle { disk, read_only }.into_resource()
        }
        Kind::Net(vmservice::VirtioNet {
            max_queues,
            backend,
            mac_address,
        }) => {
            let endpoint = build_nic_backend(backend.context("missing net backend")?, registry)?;
            virtio_resources::net::VirtioNetHandle {
                max_queues: max_queues
                    .map(|q| q.try_into().context("max_queues out of range"))
                    .transpose()?,
                mac_address: mac_address
                    .parse::<MacAddress>()
                    .context("invalid mac address")?,
                endpoint,
            }
            .into_resource()
        }
        Kind::Rng(vmservice::VirtioRng {}) => {
            virtio_resources::rng::VirtioRngHandle.into_resource()
        }
        Kind::Vsock(vmservice::VirtioVsock { socket_path }) => {
            let listener = UnixListener::bind(&socket_path)
                .with_context(|| format!("failed to bind virtio-vsock socket: {socket_path}"))?;
            virtio_resources::vsock::VirtioVsockHandle {
                // The guest CID does not matter for the UDS relay; it just needs
                // to be a non-reserved value.
                guest_cid: 0x3,
                base_path: socket_path,
                listener,
            }
            .into_resource()
        }
        Kind::Console(vmservice::VirtioConsole { backend }) => {
            let backend = build_serial_backend(backend.context("missing console backend")?)?;
            virtio_resources::console::VirtioConsoleHandle { backend }.into_resource()
        }
        Kind::VhostUser(vhost_user) => build_vhost_user_device(vhost_user)?,
    })
}

/// Builds a disk backend resource from the proto `DiskBackend`.
async fn build_disk_backend(
    backend: vmservice::DiskBackend,
    read_only: bool,
) -> anyhow::Result<Resource<DiskHandleKind>> {
    let vmservice::DiskBackend { kind } = backend;
    match kind.context("missing disk backend kind")? {
        vmservice::disk_backend::Kind::File(vmservice::FileDisk { path, direct }) => {
            open_disk_type(path.as_ref(), OpenDiskOptions { read_only, direct })
                .await
                .with_context(|| format!("failed to open {path}"))
        }
    }
}

/// Builds a host network endpoint resource from the proto `NicBackend`.
fn build_nic_backend(
    backend: vmservice::NicBackend,
    registry: &FdRegistry,
) -> anyhow::Result<Resource<NetEndpointHandleKind>> {
    use vmservice::nic_backend::Kind;
    #[cfg(not(target_os = "linux"))]
    let _ = registry;
    let vmservice::NicBackend { kind } = backend;
    Ok(match kind.context("missing network backend")? {
        Kind::Consomme(vmservice::ConsommeBackend { cidr, ports }) => {
            net_backend_resources::consomme::ConsommeHandle {
                cidr: (!cidr.is_empty()).then_some(cidr),
                ports: ports
                    .into_iter()
                    .map(parse_port_config)
                    .collect::<anyhow::Result<_>>()?,
                recv: None,
            }
            .into_resource()
        }
        #[cfg(target_os = "linux")]
        Kind::Tap(tap) => build_tap_backend(tap, registry)?,
        #[cfg(windows)]
        Kind::Dio(vmservice::DioBackend { switch_id, port_id }) => {
            net_backend_resources::dio::WindowsDirectIoHandle {
                switch_port_id: net_backend_resources::dio::SwitchPortId {
                    switch: switch_id.parse().context("invalid switch ID")?,
                    port: port_id.parse().context("invalid port ID")?,
                },
            }
            .into_resource()
        }
        _ => anyhow::bail!("unsupported network backend"),
    })
}

/// Resolves a proto `TapBackend` into a tap NIC endpoint resource, either by
/// opening `/dev/net/tun` by device name or by resolving a descriptor
/// registered via the fd-passing protocol.
#[cfg(target_os = "linux")]
fn build_tap_backend(
    tap: vmservice::TapBackend,
    registry: &FdRegistry,
) -> anyhow::Result<Resource<NetEndpointHandleKind>> {
    use vmservice::tap_backend::Source;
    let fd = match tap.source.context("missing tap source")? {
        Source::Name(name) => net_tap::tap::open_tap(&name)
            .with_context(|| format!("failed to open TAP device '{name}'"))?,
        Source::FdName(fd_name) => registry
            .resolve(&fd_name)
            .with_context(|| format!("failed to resolve tap fd '{fd_name}'"))?,
    };
    Ok(net_backend_resources::tap::TapHandle { fd }.into_resource())
}

/// Builds a serial backend resource from the proto `SerialBackend`.
fn build_serial_backend(
    backend: vmservice::SerialBackend,
) -> anyhow::Result<Resource<SerialBackendHandle>> {
    let vmservice::SerialBackend { kind } = backend;
    match kind.context("missing serial backend kind")? {
        vmservice::serial_backend::Kind::Relay(vmservice::SerialRelay {
            socket_path,
            connect,
        }) => {
            let (serial_fn, action) = open_socket_backend(connect);
            serial_fn(socket_path.as_ref())
                .with_context(|| format!("failed to {action} serial socket: {socket_path}"))
        }
    }
}

/// Builds a vhost-user-backed virtio device. Only supported on unix, where the
/// backend is reached over a Unix domain socket.
#[cfg(unix)]
fn build_vhost_user_device(
    vhost_user: vmservice::VhostUser,
) -> anyhow::Result<Resource<VirtioDeviceHandle>> {
    use vmservice::vhost_user_device::Kind;

    let vmservice::VhostUser {
        socket_path,
        device,
    } = vhost_user;
    let stream = unix_socket::UnixStream::connect(&socket_path)
        .with_context(|| format!("failed to connect to vhost-user socket: {socket_path}"))?;
    let vmservice::VhostUserDevice { kind } = device.context("missing vhost-user device")?;
    let to_u16 =
        |v: u32| -> anyhow::Result<u16> { v.try_into().context("queue value out of range") };
    Ok(match kind.context("missing vhost-user device kind")? {
        Kind::Blk(vmservice::VhostUserBlk {
            num_queues,
            queue_size,
        }) => virtio_resources::vhost_user::VhostUserBlkHandle {
            socket: stream.into(),
            num_queues: num_queues.map(to_u16).transpose()?,
            queue_size: queue_size.map(to_u16).transpose()?,
        }
        .into_resource(),
        Kind::Fs(vmservice::VhostUserFs {
            tag,
            num_queues,
            queue_size,
        }) => virtio_resources::vhost_user::VhostUserFsHandle {
            socket: stream.into(),
            tag,
            num_queues: num_queues.map(to_u16).transpose()?,
            queue_size: queue_size.map(to_u16).transpose()?,
        }
        .into_resource(),
        Kind::Other(vmservice::VhostUserGeneric {
            device_id,
            queue_sizes,
        }) => virtio_resources::vhost_user::VhostUserGenericHandle {
            socket: stream.into(),
            device_id: to_u16(device_id)?,
            queue_sizes: queue_sizes
                .into_iter()
                .map(to_u16)
                .collect::<anyhow::Result<Vec<_>>>()?,
        }
        .into_resource(),
    })
}

#[cfg(not(unix))]
fn build_vhost_user_device(
    _vhost_user: vmservice::VhostUser,
) -> anyhow::Result<Resource<VirtioDeviceHandle>> {
    anyhow::bail!("vhost-user is only supported on unix hosts")
}
