// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Client driver for the Hyper-V Virtual Machine Bus (VmBus).

#![expect(missing_docs)]
#![forbid(unsafe_code)]

pub mod driver;
pub mod filter;
pub mod saved_state;

pub use self::saved_state::SavedState;
use anyhow::Context as _;
use anyhow::Result;
use futures::FutureExt;
use futures::StreamExt;
use futures::future::OptionFuture;
use futures::stream::SelectAll;
use futures_concurrency::future::Race;
use guid::Guid;
use inspect::Inspect;
use mesh::rpc::FailableRpc;
use mesh::rpc::Rpc;
use mesh::rpc::RpcSend;
use pal_async::task::Spawn;
use pal_async::task::Task;
use pal_event::Event;
use std::collections::HashMap;
use std::collections::VecDeque;
use std::convert::TryInto;
use std::future::Future;
use std::future::poll_fn;
use std::pin::pin;
use std::sync::Arc;
use std::sync::atomic::AtomicU32;
use std::sync::atomic::Ordering;
use std::task::Context;
use std::task::Poll;
use thiserror::Error;
use vmbus_async::async_dgram::AsyncRecv;
use vmbus_async::async_dgram::AsyncRecvExt;
use vmbus_channel::TaggedStream;
use vmbus_channel::bus::GpadlRequest;
use vmbus_channel::bus::ModifyRequest;
use vmbus_channel::bus::OpenData;
use vmbus_channel::gpadl::GpadlId;
use vmbus_core::HvsockConnectRequest;
use vmbus_core::OutgoingMessage;
use vmbus_core::VersionInfo;
use vmbus_core::protocol;
use vmbus_core::protocol::ChannelId;
use vmbus_core::protocol::ConnectionState;
use vmbus_core::protocol::FeatureFlags;
#[cfg(test)]
use vmbus_core::protocol::OpenChannelFlags;
use vmbus_core::protocol::Version;
use vmcore::interrupt::Interrupt;
use vmcore::synic::MonitorPageGpas;

const SINT: u8 = 2;
const VTL: u8 = 0;
const SUPPORTED_VERSIONS: &[Version] = &[Version::Iron, Version::Copper];
const SUPPORTED_FEATURE_FLAGS: FeatureFlags = FeatureFlags::new()
    .with_guest_specified_signal_parameters(true)
    .with_channel_interrupt_redirection(true)
    .with_modify_connection(true)
    .with_client_id(true)
    .with_pause_resume(true);

/// The client interface synic events.
pub trait SynicEventClient: Send + Sync {
    /// Maps an incoming event signal on SINT7 to `event`.
    fn map_event(&self, event_flag: u16, event: &Event) -> std::io::Result<()>;

    /// Unmaps an event previously mapped with `map_event`.
    fn unmap_event(&self, event_flag: u16);

    /// Signals an event on the synic.
    fn signal_event(&self, connection_id: u32, event_flag: u16) -> std::io::Result<()>;
}

/// A stream of vmbus messages that can be paused and resumed.
pub trait VmbusMessageSource: AsyncRecv + Send {
    /// Stop accepting new messages from the synic. After this is called, the message source must
    /// return any pending messages already in the queue, and then return EOF.
    fn pause_message_stream(&mut self) {}

    /// Resume accepting new messages from the synic.
    fn resume_message_stream(&mut self) {}
}

pub trait PollPostMessage: Send {
    fn poll_post_message(
        &mut self,
        cx: &mut Context<'_>,
        connection_id: u32,
        typ: u32,
        msg: &[u8],
    ) -> Poll<()>;
}

#[derive(Inspect)]
pub struct VmbusClient {
    #[inspect(flatten, send = "TaskRequest::Inspect")]
    task_send: mesh::Sender<TaskRequest>,
    #[inspect(skip)]
    access: VmbusClientAccess,
    #[inspect(skip)]
    task: Task<ClientTask>,
}

#[derive(Debug, thiserror::Error)]
pub enum ConnectError {
    #[error("invalid state to connect to the server")]
    InvalidState,
    #[error("no supported protocol versions")]
    NoSupportedVersions,
    #[error("failed to connect to the server: {0:?}")]
    FailedToConnect(ConnectionState),
}

impl From<vmbus_client_core::ConnectError> for ConnectError {
    fn from(error: vmbus_client_core::ConnectError) -> Self {
        match error {
            vmbus_client_core::ConnectError::InvalidState => Self::InvalidState,
            vmbus_client_core::ConnectError::VersionNotSupported => Self::NoSupportedVersions,
            vmbus_client_core::ConnectError::FailedToConnect(status) => {
                Self::FailedToConnect(status)
            }
            _ => Self::InvalidState,
        }
    }
}

#[derive(Clone)]
pub struct VmbusClientAccess {
    client_request_send: mesh::Sender<ClientRequest>,
}

/// A builder for creating a [`VmbusClient`].
pub struct VmbusClientBuilder {
    event_client: Arc<dyn SynicEventClient>,
    msg_source: Box<dyn VmbusMessageSource>,
    msg_client: Box<dyn PollPostMessage>,
}

impl VmbusClientBuilder {
    /// Creates a new instance of the builder with the given synic input.
    pub fn new(
        event_client: impl SynicEventClient + 'static,
        msg_source: impl VmbusMessageSource + 'static,
        msg_client: impl PollPostMessage + 'static,
    ) -> Self {
        Self {
            event_client: Arc::new(event_client),
            msg_source: Box::new(msg_source),
            msg_client: Box::new(msg_client),
        }
    }

    /// Creates a new instance with a receiver for incoming synic messages.
    pub fn build(self, spawner: &impl Spawn) -> VmbusClient {
        let (task_send, task_recv) = mesh::channel();
        let (client_request_send, client_request_recv) = mesh::channel();

        let inner = ClientTaskInner {
            messages: OutgoingMessages {
                poster: self.msg_client,
                queued: VecDeque::new(),
                state: OutgoingMessageState::Paused,
            },
            channel_requests: SelectAll::new(),
            synic: SynicState {
                events: HashMap::new(),
                event_client: self.event_client,
            },
        };

        let mut task = ClientTask {
            inner,
            core: vmbus_client_core::ClientCore::new(vmbus_client_core::Config {
                sint: SINT,
                vtl: VTL,
                supported_versions: SUPPORTED_VERSIONS,
                supported_feature_flags: SUPPORTED_FEATURE_FLAGS,
            }),
            runtime_channels: HashMap::new(),
            dispatched_requests: HashMap::new(),
            offer_send: None,
            next_request_id: 0,
            task_recv,
            running: false,
            paused_via_message: false,
            msg_source: self.msg_source,
            client_request_recv,
        };

        let task = spawner.spawn("vmbus client", async move {
            task.run().await;
            task
        });

        VmbusClient {
            access: VmbusClientAccess {
                client_request_send,
            },
            task_send,
            task,
        }
    }
}

impl VmbusClient {
    /// Connects to the server, negotiating the protocol version and retrieving
    /// the initial list of channel offers.
    pub async fn connect(
        &mut self,
        target_message_vp: u32,
        monitor_page: Option<MonitorPageGpas>,
        client_id: Guid,
    ) -> Result<ConnectResult, ConnectError> {
        let request = ConnectRequest {
            target_message_vp,
            monitor_page,
            client_id,
        };

        self.access
            .client_request_send
            .call(ClientRequest::Connect, request)
            .await
            .unwrap()
    }

    pub async fn unload(self) {
        self.access
            .client_request_send
            .call(ClientRequest::Unload, ())
            .await
            .unwrap();

        self.sever().await;
    }

    pub fn access(&self) -> &VmbusClientAccess {
        &self.access
    }

    pub fn start(&mut self) {
        self.task_send.send(TaskRequest::Start);
    }

    pub async fn stop(&mut self) {
        self.task_send
            .call(TaskRequest::Stop, ())
            .await
            .expect("Failed to send stop request");
    }

    pub async fn save(&self) -> SavedState {
        self.task_send
            .call(TaskRequest::Save, ())
            .await
            .expect("Failed to send save request")
    }

    pub async fn restore(
        &mut self,
        state: SavedState,
    ) -> Result<Option<ConnectResult>, RestoreError> {
        self.task_send
            .call(TaskRequest::Restore, state)
            .await
            .expect("Failed to send restore request")
    }

    pub async fn post_restore(&mut self) {
        self.task_send
            .call(TaskRequest::PostRestore, ())
            .await
            .expect("Failed to send post-restore request");
    }

    async fn sever(self) -> VmbusClientBuilder {
        drop(self.task_send);
        let task = self.task.await;
        VmbusClientBuilder {
            event_client: task.inner.synic.event_client,
            msg_source: task.msg_source,
            msg_client: task.inner.messages.poster,
        }
    }
}

#[derive(Debug)]
pub struct ConnectResult {
    pub version: VersionInfo,
    pub offers: Vec<OfferInfo>,
    pub offer_recv: mesh::Receiver<OfferInfo>,
}

impl VmbusClientAccess {
    pub async fn modify(&self, request: ModifyConnectionRequest) -> ConnectionState {
        self.client_request_send
            .call(ClientRequest::Modify, request)
            .await
            .expect("Failed to send modify request")
    }

    pub fn connect_hvsock(
        &self,
        request: HvsockConnectRequest,
    ) -> impl Future<Output = Option<OfferInfo>> + use<> {
        self.client_request_send
            .call(ClientRequest::HvsockConnect, request)
            .map(|r| r.ok().flatten())
    }
}

#[derive(Debug)]
pub struct OpenRequest {
    pub open_data: OpenData,
    pub incoming_event: Option<Event>,
    pub use_vtl2_connection_id: bool,
}

#[derive(Debug)]
pub struct RestoreRequest {
    pub incoming_event: Option<Event>,
    // FUTURE: move to saved state, don't rely on the caller.
    pub redirected_event_flag: Option<u16>,
    // FUTURE: ditto
    pub connection_id: u32,
}

/// Expresses an operation requested of the client.
pub enum ChannelRequest {
    Open(FailableRpc<OpenRequest, OpenOutput>),
    Restore(FailableRpc<RestoreRequest, OpenOutput>),
    Close(Rpc<(), ()>),
    Gpadl(FailableRpc<GpadlRequest, ()>),
    TeardownGpadl(Rpc<GpadlId, ()>),
    Modify(Rpc<ModifyRequest, i32>),
}

#[derive(Debug)]
pub struct OpenOutput {
    // FUTURE: remove this once it's part of the saved state.
    pub redirected_event_flag: Option<u16>,
}

impl std::fmt::Display for ChannelRequest {
    fn fmt(&self, fmt: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let s = match self {
            ChannelRequest::Open(_) => "Open",
            ChannelRequest::Close(_) => "Close",
            ChannelRequest::Restore(_) => "Restore",
            ChannelRequest::Gpadl(_) => "Gpadl",
            ChannelRequest::TeardownGpadl(_) => "TeardownGpadl",
            ChannelRequest::Modify(_) => "Modify",
        };
        fmt.pad(s)
    }
}

#[derive(Debug, Error)]
pub enum RestoreError {
    #[error("unsupported protocol version {0:#x}")]
    UnsupportedVersion(u32),

    #[error("unsupported feature flags {0:#x}")]
    UnsupportedFeatureFlags(u32),

    #[error("duplicate channel id {0}")]
    DuplicateChannelId(u32),

    #[error("duplicate gpadl id {0}")]
    DuplicateGpadlId(u32),

    #[error("gpadl for unknown channel id {0}")]
    GpadlForUnknownChannelId(u32),

    #[error("invalid pending message")]
    InvalidPendingMessage(#[source] vmbus_core::MessageTooLarge),

    #[error("failed to offer restored channel")]
    OfferFailed(#[source] anyhow::Error),
}

/// Provides the offer details from the server in addition to both a channel
/// to request client actions and a channel to receive server responses.
#[derive(Debug, Inspect)]
pub struct OfferInfo {
    pub offer: protocol::OfferChannel,
    #[inspect(skip)]
    pub guest_to_host_interrupt: Interrupt,
    #[inspect(skip)]
    pub request_send: mesh::Sender<ChannelRequest>,
    #[inspect(skip)]
    pub revoke_recv: mesh::OneshotReceiver<()>,
}

#[derive(Debug)]
enum ClientRequest {
    Connect(Rpc<ConnectRequest, Result<ConnectResult, ConnectError>>),
    Unload(Rpc<(), ()>),
    Modify(Rpc<ModifyConnectionRequest, ConnectionState>),
    HvsockConnect(Rpc<HvsockConnectRequest, Option<OfferInfo>>),
}

impl std::fmt::Display for ClientRequest {
    fn fmt(&self, fmt: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let s = match self {
            ClientRequest::Connect(..) => "Connect",
            ClientRequest::Unload { .. } => "Unload",
            ClientRequest::Modify(..) => "Modify",
            ClientRequest::HvsockConnect(..) => "HvsockConnect",
        };
        fmt.pad(s)
    }
}

enum TaskRequest {
    Inspect(inspect::Deferred),
    Save(Rpc<(), SavedState>),
    Restore(Rpc<SavedState, Result<Option<ConnectResult>, RestoreError>>),
    PostRestore(Rpc<(), ()>),
    Start,
    Stop(Rpc<(), ()>),
}

#[derive(Copy, Clone, Debug, Default)]
struct ConnectRequest {
    target_message_vp: u32,
    monitor_page: Option<MonitorPageGpas>,
    client_id: Guid,
}

#[derive(Copy, Clone, Debug, Default)]
pub struct ModifyConnectionRequest {
    pub monitor_page: Option<MonitorPageGpas>,
}

impl From<ModifyConnectionRequest> for protocol::ModifyConnection {
    fn from(value: ModifyConnectionRequest) -> Self {
        let monitor_page = value.monitor_page.unwrap_or_default();

        Self {
            parent_to_child_monitor_page_gpa: monitor_page.parent_to_child,
            child_to_parent_monitor_page_gpa: monitor_page.child_to_parent,
        }
    }
}

struct RuntimeChannel {
    revoke_send: Option<mesh::OneshotSender<()>>,
    connection_id: Arc<AtomicU32>,
}

enum DispatchedRequest {
    Connect {
        rpc: Rpc<ConnectRequest, Result<ConnectResult, ConnectError>>,
        version: Option<VersionInfo>,
        offers: Vec<OfferInfo>,
    },
    Unload(Rpc<(), ()>),
    ModifyConnection(Rpc<ModifyConnectionRequest, ConnectionState>),
    HvsockConnect(Rpc<HvsockConnectRequest, Option<OfferInfo>>),
    Open(FailableRpc<(), OpenOutput>),
    ModifyChannel(Rpc<(), i32>),
    EstablishGpadl(FailableRpc<(), ()>),
    TeardownGpadl(Rpc<(), ()>),
}

#[derive(Default)]
struct ActionBuffer {
    actions: Vec<vmbus_client_core::Action>,
}

impl vmbus_client_core::ActionSink for ActionBuffer {
    fn emit(&mut self, action: vmbus_client_core::Action) {
        self.actions.push(action);
    }
}

#[derive(Default)]
struct StepOutcome {
    pause_complete: bool,
}

#[derive(Inspect)]
struct ClientTask {
    #[inspect(flatten)]
    inner: ClientTaskInner,
    #[inspect(skip)]
    core: vmbus_client_core::ClientCore,
    #[inspect(skip)]
    runtime_channels: HashMap<ChannelId, RuntimeChannel>,
    #[inspect(skip)]
    dispatched_requests: HashMap<vmbus_client_core::RequestId, DispatchedRequest>,
    #[inspect(skip)]
    offer_send: Option<mesh::Sender<OfferInfo>>,
    next_request_id: u64,
    running: bool,
    paused_via_message: bool,
    #[inspect(skip)]
    msg_source: Box<dyn VmbusMessageSource>,
    #[inspect(skip)]
    task_recv: mesh::Receiver<TaskRequest>,
    #[inspect(skip)]
    client_request_recv: mesh::Receiver<ClientRequest>,
}

impl ClientTask {
    fn next_request_id(&mut self) -> vmbus_client_core::RequestId {
        loop {
            let request_id = vmbus_client_core::RequestId(self.next_request_id);
            self.next_request_id = self.next_request_id.wrapping_add(1);
            if !self.dispatched_requests.contains_key(&request_id) {
                return request_id;
            }
        }
    }

    fn handle_client_request(&mut self, request: ClientRequest) {
        match request {
            ClientRequest::Connect(rpc) => {
                let params = *rpc.input();
                let request_id = self.next_request_id();
                self.dispatched_requests.insert(
                    request_id,
                    DispatchedRequest::Connect {
                        rpc,
                        version: None,
                        offers: Vec::new(),
                    },
                );
                self.drive_core(vmbus_client_core::Event::Connect {
                    request_id,
                    params: vmbus_client_core::ConnectParams {
                        target_message_vp: params.target_message_vp,
                        monitor_page: params.monitor_page.map(|pages| {
                            vmbus_client_core::MonitorPageGpas {
                                parent_to_child: pages.parent_to_child,
                                child_to_parent: pages.child_to_parent,
                            }
                        }),
                        client_id: params.client_id,
                    },
                });
            }
            ClientRequest::Unload(rpc) => {
                let request_id = self.next_request_id();
                self.dispatched_requests
                    .insert(request_id, DispatchedRequest::Unload(rpc));
                self.drive_core(vmbus_client_core::Event::Unload { request_id });
            }
            ClientRequest::Modify(rpc) => {
                let monitor_page = rpc.input().monitor_page.unwrap_or_default();
                let request_id = self.next_request_id();
                self.dispatched_requests
                    .insert(request_id, DispatchedRequest::ModifyConnection(rpc));
                self.drive_core(vmbus_client_core::Event::ModifyConnection {
                    request_id,
                    monitor_page: vmbus_client_core::MonitorPageGpas {
                        parent_to_child: monitor_page.parent_to_child,
                        child_to_parent: monitor_page.child_to_parent,
                    },
                });
            }
            ClientRequest::HvsockConnect(rpc) => {
                let request = *rpc.input();
                let request_id = self.next_request_id();
                self.dispatched_requests
                    .insert(request_id, DispatchedRequest::HvsockConnect(rpc));
                self.drive_core(vmbus_client_core::Event::HvsockConnect {
                    request_id,
                    request: vmbus_client_core::HvsockConnectRequest {
                        service_id: request.service_id,
                        endpoint_id: request.endpoint_id,
                        silo_id: request.silo_id,
                        hosted_silo_unaware: request.hosted_silo_unaware,
                    },
                });
            }
        }
    }

    fn create_offer_info(&mut self, offer: protocol::OfferChannel) -> Result<OfferInfo> {
        if self.runtime_channels.contains_key(&offer.channel_id) {
            anyhow::bail!("channel {:?} exists", offer.channel_id);
        }
        let (request_send, request_recv) = mesh::channel();
        let (revoke_send, revoke_recv) = mesh::oneshot();

        let connection_id = Arc::new(AtomicU32::new(0));
        self.runtime_channels.insert(
            offer.channel_id,
            RuntimeChannel {
                revoke_send: Some(revoke_send),
                connection_id: connection_id.clone(),
            },
        );

        self.inner
            .channel_requests
            .push(TaggedStream::new(offer.channel_id, request_recv));

        Ok(OfferInfo {
            offer,
            guest_to_host_interrupt: self.inner.synic.guest_to_host_interrupt(connection_id),
            revoke_recv,
            request_send,
        })
    }

    fn handle_open_channel(
        &mut self,
        channel_id: ChannelId,
        rpc: FailableRpc<OpenRequest, OpenOutput>,
    ) {
        let (request, rpc) = rpc.split();
        let supports_interrupt_redirection = self.core.phase().version().is_some_and(|version| {
            version.feature_flags.guest_specified_signal_parameters()
                || version.feature_flags.channel_interrupt_redirection()
        });
        if request.use_vtl2_connection_id && !supports_interrupt_redirection {
            rpc.fail(anyhow::anyhow!(
                "host does not support specifying the connection ID"
            ));
            return;
        }
        let connection_id = if request.use_vtl2_connection_id {
            protocol::ConnectionId::new(channel_id.0, 2.try_into().unwrap(), 7).0
        } else {
            request.open_data.connection_id
        };
        let redirected_event_flag = if let Some(event) = &request.incoming_event {
            match self.allocate_event_flag(event) {
                Ok(flag) => flag,
                Err(err) => {
                    rpc.fail(err.context("failed to allocate event flag"));
                    return;
                }
            }
            .into()
        } else {
            None
        };
        let request_id = self.next_request_id();
        self.dispatched_requests
            .insert(request_id, DispatchedRequest::Open(rpc));
        self.drive_core(vmbus_client_core::Event::OpenChannel {
            request_id,
            channel_id,
            open: vmbus_client_core::OpenChannelParams {
                target_vp: request.open_data.target_vp,
                ring_offset: request.open_data.ring_offset,
                ring_gpadl_id: request.open_data.ring_gpadl_id,
                event_flag: request.open_data.event_flag,
                connection_id,
                redirected_event_flag,
                user_data: request.open_data.user_data,
            },
        });
    }

    fn handle_restore_channel(
        &mut self,
        channel_id: ChannelId,
        rpc: FailableRpc<RestoreRequest, OpenOutput>,
    ) {
        let (request, rpc) = rpc.split();
        if request.incoming_event.is_some() != request.redirected_event_flag.is_some() {
            rpc.fail(anyhow::anyhow!(
                "incoming event and redirected event flag must both be set or unset"
            ));
            return;
        }
        if let Some((flag, event)) = request
            .redirected_event_flag
            .zip(request.incoming_event.as_ref())
        {
            if let Err(err) = self.restore_event_flag(flag, event) {
                rpc.fail(err.context("failed to restore event flag"));
                return;
            }
        }
        let request_id = self.next_request_id();
        self.dispatched_requests
            .insert(request_id, DispatchedRequest::Open(rpc));
        self.drive_core(vmbus_client_core::Event::RestoreChannel {
            request_id,
            channel_id,
            params: vmbus_client_core::RestoreChannelParams {
                redirected_event_flag: request.redirected_event_flag,
                connection_id: request.connection_id,
            },
        });
    }

    fn handle_gpadl(&mut self, channel_id: ChannelId, rpc: FailableRpc<GpadlRequest, ()>) {
        let (request, rpc) = rpc.split();
        let request_id = self.next_request_id();
        self.dispatched_requests
            .insert(request_id, DispatchedRequest::EstablishGpadl(rpc));
        self.drive_core(vmbus_client_core::Event::EstablishGpadl {
            request_id,
            channel_id,
            gpadl_id: request.id,
            request: vmbus_client_core::GpadlRequest {
                id: request.id,
                count: request.count,
                buf: request.buf,
            },
        });
    }

    fn handle_gpadl_teardown(&mut self, channel_id: ChannelId, rpc: Rpc<GpadlId, ()>) {
        let (gpadl_id, rpc) = rpc.split();
        let request_id = self.next_request_id();
        self.dispatched_requests
            .insert(request_id, DispatchedRequest::TeardownGpadl(rpc));
        self.drive_core(vmbus_client_core::Event::TeardownGpadl {
            request_id,
            channel_id,
            gpadl_id,
        });
    }

    fn handle_modify_channel(&mut self, channel_id: ChannelId, rpc: Rpc<ModifyRequest, i32>) {
        let (request, response) = rpc.split();
        let request = match request {
            ModifyRequest::TargetVp { target_vp } => {
                vmbus_client_core::ModifyRequest::TargetVp { target_vp }
            }
        };
        let request_id = self.next_request_id();
        self.dispatched_requests
            .insert(request_id, DispatchedRequest::ModifyChannel(response));
        self.drive_core(vmbus_client_core::Event::ModifyChannel {
            request_id,
            channel_id,
            request,
        });
    }

    fn handle_channel_request(&mut self, channel_id: ChannelId, request: ChannelRequest) {
        match request {
            ChannelRequest::Open(rpc) => self.handle_open_channel(channel_id, rpc),
            ChannelRequest::Restore(rpc) => self.handle_restore_channel(channel_id, rpc),
            ChannelRequest::Gpadl(req) => self.handle_gpadl(channel_id, req),
            ChannelRequest::TeardownGpadl(req) => self.handle_gpadl_teardown(channel_id, req),
            ChannelRequest::Close(req) => {
                self.drive_core(vmbus_client_core::Event::CloseChannel { channel_id });
                req.complete(());
            }
            ChannelRequest::Modify(req) => self.handle_modify_channel(channel_id, req),
        }
    }

    async fn handle_task(&mut self, task: TaskRequest) {
        match task {
            TaskRequest::Inspect(deferred) => {
                deferred.inspect(&*self);
            }
            TaskRequest::Save(rpc) => rpc.handle_sync(|()| self.handle_save()),
            TaskRequest::Restore(rpc) => {
                rpc.handle_sync(|saved_state| self.handle_restore(saved_state))
            }
            TaskRequest::PostRestore(rpc) => rpc.handle_sync(|()| self.handle_post_restore()),
            TaskRequest::Start => self.handle_start(),
            TaskRequest::Stop(rpc) => rpc.handle(async |()| self.handle_stop().await).await,
        }
    }

    fn handle_device_removal(&mut self, channel_id: ChannelId) {
        self.drive_core(vmbus_client_core::Event::ReleaseChannel { channel_id });
    }

    fn handle_start(&mut self) {
        assert!(!self.running);
        self.msg_source.resume_message_stream();
        self.inner.messages.resume();
        self.drive_core(vmbus_client_core::Event::Start);
        if self.paused_via_message {
            self.drive_core(vmbus_client_core::Event::Resume);
            self.paused_via_message = false;
        }
        self.running = true;
    }

    async fn handle_stop(&mut self) {
        assert!(self.running);

        loop {
            // Process messages until there are no more channels waiting for
            // responses. This is necessary to ensure that the saved state does
            // not have to support encoding revoked channels for which we are
            // waiting for GPADL or modify responses.
            while let Some((id, request)) = self.revoked_channel_with_pending_request() {
                tracelimit::info_ratelimited!(
                    channel_id = id.0,
                    request,
                    "waiting for responses for channel"
                );
                assert!(self.process_next_message().await);
            }

            if self.can_pause_resume() {
                self.drive_core(vmbus_client_core::Event::Pause);
                self.inner.messages.pause();
                self.paused_via_message = true;
            } else {
                // Mask the sint to pause the message stream. The host will
                // retry any queued messages after the sint is unmasked.
                self.msg_source.pause_message_stream();
                self.inner.messages.force_pause();
            }

            // Continue processing messages until we hit EOF or get a pause
            // response.
            while self.process_next_message().await {}

            // Ensure there are still no pending requests. If there are, resume
            // and go around again.
            if self.revoked_channel_with_pending_request().is_none() {
                break;
            }
            if !self.can_pause_resume() {
                self.msg_source.resume_message_stream();
            }
            self.inner.messages.resume();
        }

        tracing::debug!("messages drained");
        self.drive_core(vmbus_client_core::Event::Stop);
        self.running = false;
    }

    async fn process_next_message(&mut self) -> bool {
        let mut buf = [0; protocol::MAX_MESSAGE_SIZE];
        let recv = self.msg_source.recv(&mut buf);
        // Concurrently flush until there is no more work to do, since pending
        // messages may be blocking responses from the host.
        let flush = async {
            self.inner.messages.flush_messages().await;
            std::future::pending().await
        };
        let size = (recv, flush)
            .race()
            .await
            .expect("Fatal error reading messages from synic");
        if size == 0 {
            return false;
        }
        !self
            .drive_core(vmbus_client_core::Event::HostMessage(&buf[..size]))
            .pause_complete
    }

    /// Returns whether the server supports in-band messages to pause/resume the
    /// message stream.
    ///
    /// For hosts where this is not supported, we mask the sint to pause new
    /// messages being queued to the sint, then drain the messages. This does
    /// not work with some host implementations, which cannot support draining
    /// the message queue while the sint is masked (due to the use of
    /// HvPostMessageDirect).
    fn can_pause_resume(&self) -> bool {
        self.core
            .phase()
            .version()
            .is_some_and(|version| version.feature_flags.pause_resume())
    }

    async fn run(&mut self) {
        let mut buf = [0; protocol::MAX_MESSAGE_SIZE];
        loop {
            let host_backed_up = !self.inner.messages.is_empty();
            if self.core.host_busy() != host_backed_up {
                self.drive_core(vmbus_client_core::Event::HostBusy {
                    busy: host_backed_up,
                });
            }
            let mut message_recv =
                OptionFuture::from(self.running.then(|| self.msg_source.recv(&mut buf).fuse()));

            // If there are pending outgoing messages, the host is backed up.
            // Try to flush the queue, and in the meantime, stop generating new
            // messages by stopping processing client requests, so as to avoid
            // the outgoing message queue growing without bound.
            //
            // We still need to process incoming messages when in this state,
            // even though they may generate additional outgoing messages, to
            // avoid a deadlock with the host. The host can always DoS the
            // guest, so this is not an attack vector.
            let flush_messages = OptionFuture::from(
                (self.running && host_backed_up)
                    .then(|| self.inner.messages.flush_messages().fuse()),
            );

            let mut client_request_recv = OptionFuture::from(
                (self.running && !host_backed_up).then(|| self.client_request_recv.next()),
            );

            let mut channel_requests = OptionFuture::from(
                (self.running && !host_backed_up)
                    .then(|| self.inner.channel_requests.select_next_some()),
            );

            futures::select! { // merge semantics
                _r = pin!(flush_messages) => {}
                r = self.task_recv.next() => {
                    if let Some(task) = r {
                        self.handle_task(task).await;
                    } else {
                        break;
                    }
                }
                r = client_request_recv => {
                    if let Some(Some(request)) = r {
                        self.handle_client_request(request);
                    } else {
                        break;
                    }
                }
                r = channel_requests => {
                    match r.unwrap() {
                        (id, Some(request)) => self.handle_channel_request(id, request),
                        (id, _) => {
                            self.handle_device_removal(id);
                        }
                    }
                }
                r = message_recv => {
                    match r.unwrap() {
                        Ok(size) => {
                            if size == 0 {
                                panic!("Unexpected end of file reading messages from synic.");
                            }

                            self.drive_core(vmbus_client_core::Event::HostMessage(&buf[..size]));
                        }
                        Err(err) => {
                            panic!("Error reading messages from synic: {err:?}");
                        }
                    }
                }
                complete => break,
            }
        }
    }

    fn drive_core(&mut self, event: vmbus_client_core::Event<'_>) -> StepOutcome {
        let mut sink = ActionBuffer::default();
        self.core.step(event, &mut sink);
        let mut outcome = StepOutcome::default();
        for action in sink.actions {
            self.handle_action(action, &mut outcome);
        }
        self.runtime_channels
            .retain(|channel_id, _| self.core.channels().contains_key(channel_id));
        outcome
    }

    fn handle_action(&mut self, action: vmbus_client_core::Action, outcome: &mut StepOutcome) {
        use vmbus_client_core::Action;
        match action {
            Action::PostMessage(data) => self.inner.messages.send_raw(data),
            Action::SignalEvent {
                connection_id,
                event_flag,
            } => {
                if let Err(err) = self
                    .inner
                    .synic
                    .event_client
                    .signal_event(connection_id, event_flag)
                {
                    tracelimit::warn_ratelimited!(
                        error = &err as &dyn std::error::Error,
                        "failed to signal event"
                    );
                }
            }
            Action::FreeEventFlag(flag) => self.inner.synic.free_event_flag(flag),
            Action::Complete { request_id, result } => {
                self.handle_completion(request_id, result);
            }
            Action::OfferReceived(descriptor) => {
                let offer = descriptor.offer;
                let offer_info = self
                    .create_offer_info(offer)
                    .expect("core rejects duplicate channel offers");
                if let vmbus_client_core::ClientPhase::RequestingOffers { request_id, .. } =
                    self.core.phase()
                {
                    let Some(DispatchedRequest::Connect { offers, .. }) =
                        self.dispatched_requests.get_mut(request_id)
                    else {
                        panic!("missing connect request while collecting offers");
                    };
                    offers.push(offer_info);
                } else if let Some(offer_send) = &mut self.offer_send {
                    offer_send.send(offer_info);
                }
            }
            Action::OfferRescinded { channel_id } => {
                if let Some(channel) = self.runtime_channels.get_mut(&channel_id) {
                    if let Some(revoke_send) = channel.revoke_send.take() {
                        revoke_send.send(());
                    }
                }
            }
            Action::ChannelObservable { channel_id, event } => {
                if let Some(channel) = self.runtime_channels.get(&channel_id) {
                    match event {
                        vmbus_client_core::ChannelObservable::ConnectionIdAssigned(id) => {
                            channel.connection_id.store(id, Ordering::Release);
                        }
                        vmbus_client_core::ChannelObservable::ConnectionIdCleared => {
                            channel.connection_id.store(0, Ordering::Release);
                        }
                        vmbus_client_core::ChannelObservable::Opened
                        | vmbus_client_core::ChannelObservable::Closed
                        | vmbus_client_core::ChannelObservable::Revoked => {}
                        _ => {}
                    }
                }
            }
            Action::PauseComplete => outcome.pause_complete = true,
            _ => {}
        }
    }

    fn handle_completion(
        &mut self,
        request_id: vmbus_client_core::RequestId,
        result: vmbus_client_core::CompletionResult,
    ) {
        use vmbus_client_core::CompletionResult;
        match result {
            CompletionResult::Connect(result) => match result {
                Ok(success) => {
                    let Some(DispatchedRequest::Connect { version, .. }) =
                        self.dispatched_requests.get_mut(&request_id)
                    else {
                        panic!("missing connect request");
                    };
                    *version = Some(success.version);
                    self.drive_core(vmbus_client_core::Event::RequestOffers { request_id });
                }
                Err(error) => {
                    let Some(DispatchedRequest::Connect { rpc, .. }) =
                        self.dispatched_requests.remove(&request_id)
                    else {
                        panic!("missing connect request");
                    };
                    rpc.complete(Err(error.into()));
                }
            },
            CompletionResult::RequestOffers(result) => {
                let Some(DispatchedRequest::Connect {
                    rpc,
                    version,
                    offers,
                }) = self.dispatched_requests.remove(&request_id)
                else {
                    panic!("missing request-offers request");
                };
                match result {
                    Ok(()) => {
                        let (offer_send, offer_recv) = mesh::channel();
                        self.offer_send = Some(offer_send);
                        rpc.complete(Ok(ConnectResult {
                            version: version.expect("connect completed first"),
                            offers,
                            offer_recv,
                        }));
                    }
                    Err(error) => rpc.complete(Err(error.into())),
                }
            }
            CompletionResult::Unload => {
                let Some(DispatchedRequest::Unload(rpc)) =
                    self.dispatched_requests.remove(&request_id)
                else {
                    panic!("missing unload request");
                };
                rpc.complete(());
            }
            CompletionResult::ModifyConnection(result) => {
                let Some(DispatchedRequest::ModifyConnection(rpc)) =
                    self.dispatched_requests.remove(&request_id)
                else {
                    panic!("missing modify-connection request");
                };
                rpc.complete(result);
            }
            CompletionResult::HvsockConnect(descriptor) => {
                let Some(DispatchedRequest::HvsockConnect(rpc)) =
                    self.dispatched_requests.remove(&request_id)
                else {
                    panic!("missing hvsock request");
                };
                let offer = descriptor.map(|descriptor| {
                    self.create_offer_info(descriptor.offer)
                        .expect("core rejects duplicate channel offers")
                });
                rpc.complete(offer);
            }
            CompletionResult::OpenChannel(result) => {
                let Some(DispatchedRequest::Open(rpc)) =
                    self.dispatched_requests.remove(&request_id)
                else {
                    panic!("missing open request");
                };
                match result {
                    Ok(result) => rpc.complete(Ok(OpenOutput {
                        redirected_event_flag: result.redirected_event_flag,
                    })),
                    Err(error) => rpc.fail(anyhow::Error::new(error)),
                }
            }
            CompletionResult::ModifyChannel(result) => {
                let Some(DispatchedRequest::ModifyChannel(rpc)) =
                    self.dispatched_requests.remove(&request_id)
                else {
                    panic!("missing modify-channel request");
                };
                rpc.complete(result);
            }
            CompletionResult::EstablishGpadl(result) => {
                let Some(DispatchedRequest::EstablishGpadl(rpc)) =
                    self.dispatched_requests.remove(&request_id)
                else {
                    panic!("missing establish-gpadl request");
                };
                match result {
                    Ok(()) => rpc.complete(Ok(())),
                    Err(()) => rpc.fail(anyhow::anyhow!("gpadl creation failed")),
                }
            }
            CompletionResult::TeardownGpadl => {
                let Some(DispatchedRequest::TeardownGpadl(rpc)) =
                    self.dispatched_requests.remove(&request_id)
                else {
                    panic!("missing teardown-gpadl request");
                };
                rpc.complete(());
            }
            CompletionResult::ReleaseChannel => {}
            other => {
                // `CompletionResult` is `#[non_exhaustive]`, so adding
                // a new variant to `vmbus_client_core` will silently
                // compile past this match. Rate-limit rather than
                // panic — the wrapper is a trust boundary and a
                // future protocol addition should not kill the vmbus
                // client task for the whole VM.
                tracelimit::warn_ratelimited!(
                    ?other,
                    request_id = request_id.0,
                    "unhandled completion from vmbus_client_core; ignoring"
                );
            }
        }
    }

    fn allocate_event_flag(&mut self, event: &Event) -> Result<u16> {
        let flag = self
            .core
            .allocate_event_flag()
            .map_err(anyhow::Error::new)?;
        if let Err(err) = self.inner.synic.map_event(flag, event) {
            self.core.free_event_flag(flag);
            return Err(err);
        }
        Ok(flag)
    }

    fn restore_event_flag(&mut self, flag: u16, event: &Event) -> Result<()> {
        self.core
            .reserve_event_flag(flag)
            .map_err(anyhow::Error::new)?;
        if let Err(err) = self.inner.synic.map_event(flag, event) {
            self.core.free_event_flag(flag);
            return Err(err);
        }
        Ok(())
    }

    fn revoked_channel_with_pending_request(&self) -> Option<(ChannelId, &'static str)> {
        self.core
            .channels()
            .iter()
            .find_map(|(&channel_id, entry)| {
                if !matches!(entry.phase, vmbus_client_core::ChannelPhase::Revoked) {
                    return None;
                }
                if entry.modify_request_id.is_some() {
                    return Some((channel_id, "modify"));
                }
                entry.gpadls.values().find_map(|phase| match phase {
                    vmbus_client_core::GpadlPhase::Offered { .. } => {
                        Some((channel_id, "creating gpadl"))
                    }
                    vmbus_client_core::GpadlPhase::TearingDown { .. } => {
                        Some((channel_id, "tearing down gpadl"))
                    }
                    vmbus_client_core::GpadlPhase::Created => None,
                })
            })
    }
}

#[derive(Inspect)]
struct OutgoingMessages {
    #[inspect(skip)]
    poster: Box<dyn PollPostMessage>,
    #[inspect(with = "|x| x.len()")]
    queued: VecDeque<OutgoingMessage>,
    state: OutgoingMessageState,
}

#[derive(Inspect, PartialEq, Eq, Debug)]
enum OutgoingMessageState {
    Running,
    SendingPauseMessage,
    Paused,
}

impl OutgoingMessages {
    fn send_raw(&mut self, data: Vec<u8>) {
        let msg = OutgoingMessage::from_message(&data)
            .expect("vmbus_client_core emitted an invalid outgoing message");
        if self.queued.is_empty() && self.state == OutgoingMessageState::Running {
            let r = self.poster.poll_post_message(
                &mut Context::from_waker(std::task::Waker::noop()),
                protocol::VMBUS_MESSAGE_REDIRECT_CONNECTION_ID,
                1,
                msg.data(),
            );
            if let Poll::Ready(()) = r {
                return;
            }
        }
        tracing::trace!("queueing message");
        self.queued.push_back(msg);
    }

    async fn flush_messages(&mut self) {
        let mut send = async |msg: &OutgoingMessage| {
            poll_fn(|cx| {
                self.poster.poll_post_message(
                    cx,
                    protocol::VMBUS_MESSAGE_REDIRECT_CONNECTION_ID,
                    1,
                    msg.data(),
                )
            })
            .await
        };
        match self.state {
            OutgoingMessageState::Running => {
                while let Some(msg) = self.queued.front() {
                    send(msg).await;
                    tracing::trace!("sent queued message");
                    self.queued.pop_front();
                }
            }
            OutgoingMessageState::SendingPauseMessage => {
                while let Some(msg) = self.queued.front() {
                    send(msg).await;
                    tracing::trace!("sent queued message while pausing");
                    self.queued.pop_front();
                }
                self.state = OutgoingMessageState::Paused;
            }
            OutgoingMessageState::Paused => {}
        }
    }

    /// Pause by sending a pause message to the host. This will cause the host
    /// to stop sending messages after sending a pause response.
    fn pause(&mut self) {
        assert_eq!(self.state, OutgoingMessageState::Running);
        self.state = OutgoingMessageState::SendingPauseMessage;
    }

    /// Force a pause by setting the state to Paused. This is used when the
    /// host does not support in-band pause/resume messages, in which case
    /// the SINT is masked to force the host to stop sending messages.
    fn force_pause(&mut self) {
        assert_eq!(self.state, OutgoingMessageState::Running);
        self.state = OutgoingMessageState::Paused;
    }

    fn resume(&mut self) {
        assert_eq!(self.state, OutgoingMessageState::Paused);
        self.state = OutgoingMessageState::Running;
    }

    fn is_empty(&self) -> bool {
        self.queued.is_empty()
    }
}

#[derive(Inspect)]
struct ClientTaskInner {
    messages: OutgoingMessages,
    #[inspect(skip)]
    channel_requests: SelectAll<TaggedStream<ChannelId, mesh::Receiver<ChannelRequest>>>,
    synic: SynicState,
}

#[derive(Inspect)]
struct SynicState {
    #[inspect(skip)]
    event_client: Arc<dyn SynicEventClient>,
    #[inspect(with = "|x| x.len()")]
    events: HashMap<u16, Event>,
}

impl SynicState {
    fn guest_to_host_interrupt(&self, connection_id: Arc<AtomicU32>) -> Interrupt {
        Interrupt::from_fn({
            let event_client = self.event_client.clone();
            move || {
                let connection_id = connection_id.load(Ordering::Acquire);
                if connection_id == 0 {
                    tracing::debug!("interrupt signal after close");
                    return;
                }

                if let Err(err) = event_client.signal_event(connection_id, 0) {
                    tracelimit::warn_ratelimited!(
                        error = &err as &dyn std::error::Error,
                        "failed to signal event"
                    );
                }
            }
        })
    }

    fn map_event(&mut self, event_flag: u16, event: &Event) -> Result<()> {
        self.event_client
            .map_event(event_flag, event)
            .context("failed to map event")?;
        assert!(self.events.insert(event_flag, event.clone()).is_none());
        Ok(())
    }

    fn free_event_flag(&mut self, flag: u16) {
        assert!(self.events.remove(&flag).is_some());
        self.event_client.unmap_event(flag);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures_concurrency::future::Join;
    use guid::Guid;
    use pal_async::DefaultDriver;
    use pal_async::async_test;
    use pal_async::timer::PolledTimer;
    use protocol::TargetInfo;
    use std::fmt::Debug;
    use std::task::ready;
    use std::time::Duration;
    use test_with_tracing::test;
    use vmbus_core::protocol::MessageHeader;
    use vmbus_core::protocol::MessageType;
    use vmbus_core::protocol::OfferFlags;
    use vmbus_core::protocol::UserDefinedData;
    use vmbus_core::protocol::VmbusMessage;
    use zerocopy::FromBytes;
    use zerocopy::FromZeros;
    use zerocopy::Immutable;
    use zerocopy::IntoBytes;
    use zerocopy::KnownLayout;

    const VMBUS_TEST_CLIENT_ID: Guid = guid::guid!("e6e6e6e6-e6e6-e6e6-e6e6-e6e6e6e6e6e6");

    fn in_msg<T: IntoBytes + Immutable + KnownLayout>(message_type: MessageType, t: T) -> Vec<u8> {
        let mut data = Vec::new();
        data.extend_from_slice(&message_type.0.to_ne_bytes());
        data.extend_from_slice(&0u32.to_ne_bytes());
        data.extend_from_slice(t.as_bytes());
        data
    }

    #[track_caller]
    fn check_message<T>(msg: OutgoingMessage, chk: T)
    where
        T: IntoBytes + FromBytes + Immutable + KnownLayout + Debug + VmbusMessage,
    {
        check_message_with_data(msg, chk, &[]);
    }

    #[track_caller]
    fn check_message_with_data<T>(msg: OutgoingMessage, chk: T, data: &[u8])
    where
        T: IntoBytes + FromBytes + Immutable + KnownLayout + Debug + VmbusMessage,
    {
        let chk_data = OutgoingMessage::with_data(&chk, data);
        if msg.data() != chk_data.data() {
            let (header, rest) = MessageHeader::read_from_prefix(msg.data()).unwrap();
            assert_eq!(header.message_type(), <T as VmbusMessage>::MESSAGE_TYPE);
            let (msg, rest) = T::read_from_prefix(rest).expect("incorrect message size");
            if msg.as_bytes() != chk.as_bytes() {
                panic!("mismatched messages, expected {:#?}, got {:#?}", chk, msg);
            }
            if rest != data {
                panic!("mismatched data, expected {:#?}, got {:#?}", data, rest);
            }
        }
    }

    struct TestServer {
        messages: mesh::Receiver<OutgoingMessage>,
        send: mesh::Sender<Vec<u8>>,
    }

    impl TestServer {
        async fn next(&mut self) -> Option<OutgoingMessage> {
            self.messages.next().await
        }

        fn send(&self, msg: Vec<u8>) {
            self.send.send(msg);
        }

        async fn connect(&mut self, client: &mut VmbusClient) -> ConnectResult {
            self.connect_with_channels(client, |_| {}).await
        }

        async fn connect_with_channels(
            &mut self,
            client: &mut VmbusClient,
            send_offers: impl FnOnce(&mut Self),
        ) -> ConnectResult {
            let client_connect = client.connect(0, None, Guid::ZERO);

            let server_connect = async {
                let _ = self.next().await.unwrap();

                self.send(in_msg(
                    MessageType::VERSION_RESPONSE,
                    protocol::VersionResponse2 {
                        version_response: protocol::VersionResponse {
                            version_supported: 1,
                            connection_state: ConnectionState::SUCCESSFUL,
                            padding: 0,
                            selected_version_or_connection_id: 0,
                        },
                        supported_features: SUPPORTED_FEATURE_FLAGS.into(),
                    },
                ));

                check_message(self.next().await.unwrap(), protocol::RequestOffers {});

                send_offers(self);
                self.send(in_msg(MessageType::ALL_OFFERS_DELIVERED, [0x00]));
            };

            let (connection, ()) = (client_connect, server_connect).join().await;

            let connection = connection.unwrap();
            assert_eq!(connection.version.version, Version::Copper);
            assert_eq!(connection.version.feature_flags, SUPPORTED_FEATURE_FLAGS);
            connection
        }

        async fn get_channel(&mut self, client: &mut VmbusClient) -> OfferInfo {
            let [channel] = self
                .get_channels(client, 1)
                .await
                .offers
                .try_into()
                .unwrap();
            channel
        }

        async fn get_channels(&mut self, client: &mut VmbusClient, count: usize) -> ConnectResult {
            self.connect_with_channels(client, |this| {
                for i in 0..count {
                    let offer = protocol::OfferChannel {
                        interface_id: Guid::new_random(),
                        instance_id: Guid::new_random(),
                        rsvd: [0; 4],
                        flags: OfferFlags::new(),
                        mmio_megabytes: 0,
                        user_defined: UserDefinedData::new_zeroed(),
                        subchannel_index: 0,
                        mmio_megabytes_optional: 0,
                        channel_id: ChannelId(i as u32),
                        monitor_id: 0,
                        monitor_allocated: 0,
                        is_dedicated: 0,
                        connection_id: 0,
                    };

                    this.send(in_msg(MessageType::OFFER_CHANNEL, offer));
                }
            })
            .await
        }

        async fn stop_client(&mut self, client: &mut VmbusClient) {
            let client_stop = client.stop();
            let server_stop = async {
                check_message(self.next().await.unwrap(), protocol::Pause);
                self.send(in_msg(MessageType::PAUSE_RESPONSE, protocol::PauseResponse));
            };
            (client_stop, server_stop).join().await;
        }

        async fn start_client(&mut self, client: &mut VmbusClient) {
            client.start();
            check_message(self.next().await.unwrap(), protocol::Resume);
        }
    }

    struct TestServerClient {
        sender: mesh::Sender<OutgoingMessage>,
        timer: PolledTimer,
        deadline: Option<pal_async::timer::Instant>,
    }

    impl PollPostMessage for TestServerClient {
        fn poll_post_message(
            &mut self,
            cx: &mut Context<'_>,
            _connection_id: u32,
            _typ: u32,
            msg: &[u8],
        ) -> Poll<()> {
            loop {
                if let Some(deadline) = self.deadline {
                    ready!(self.timer.poll_until(cx, deadline));
                    self.deadline = None;
                }
                // Randomly choose whether to delay the message.
                //
                // FUTURE: use some kind of deterministic test framework for this to
                // allow for reproducible tests.
                let mut b = [0];
                getrandom::fill(&mut b).unwrap();
                if b[0] % 4 == 0 {
                    self.deadline =
                        Some(pal_async::timer::Instant::now() + Duration::from_millis(10));
                } else {
                    let msg = OutgoingMessage::from_message(msg).unwrap();
                    tracing::info!(
                        msg = ?MessageHeader::read_from_prefix(msg.data()),
                        "sending message"
                    );
                    self.sender.send(msg);
                    break Poll::Ready(());
                }
            }
        }
    }

    struct NoopSynicEvents;

    impl SynicEventClient for NoopSynicEvents {
        fn map_event(&self, _event_flag: u16, _event: &Event) -> std::io::Result<()> {
            Ok(())
        }

        fn unmap_event(&self, _event_flag: u16) {}

        fn signal_event(&self, _connection_id: u32, _event_flag: u16) -> std::io::Result<()> {
            Err(std::io::ErrorKind::Unsupported.into())
        }
    }

    struct TestMessageSource {
        msg_recv: mesh::Receiver<Vec<u8>>,
        paused: bool,
    }

    impl AsyncRecv for TestMessageSource {
        fn poll_recv(
            &mut self,
            cx: &mut Context<'_>,
            mut bufs: &mut [std::io::IoSliceMut<'_>],
        ) -> Poll<std::io::Result<usize>> {
            let value = match self.msg_recv.poll_recv(cx) {
                Poll::Ready(v) => v.unwrap(),
                Poll::Pending => {
                    if self.paused {
                        return Poll::Ready(Ok(0));
                    } else {
                        return Poll::Pending;
                    }
                }
            };
            let mut remaining = value.as_slice();
            let mut total_size = 0;
            while !remaining.is_empty() && !bufs.is_empty() {
                let size = bufs[0].len().min(remaining.len());
                bufs[0][..size].copy_from_slice(&remaining[..size]);
                remaining = &remaining[size..];
                bufs = &mut bufs[1..];
                total_size += size;
            }

            Ok(total_size).into()
        }
    }

    impl VmbusMessageSource for TestMessageSource {
        fn pause_message_stream(&mut self) {
            self.paused = true;
        }

        fn resume_message_stream(&mut self) {
            self.paused = false;
        }
    }

    fn test_init(driver: &DefaultDriver) -> (TestServer, VmbusClient) {
        let (msg_send, msg_recv) = mesh::channel();
        let (synic_send, synic_recv) = mesh::channel();
        let server = TestServer {
            messages: synic_recv,
            send: msg_send,
        };
        let mut client = VmbusClientBuilder::new(
            NoopSynicEvents,
            TestMessageSource {
                msg_recv,
                paused: false,
            },
            TestServerClient {
                sender: synic_send,
                deadline: None,
                timer: PolledTimer::new(driver),
            },
        )
        .build(driver);
        client.start();
        (server, client)
    }

    #[async_test]
    async fn test_initiate_contact_success(driver: DefaultDriver) {
        let (mut server, client) = test_init(&driver);
        let _recv = client
            .access
            .client_request_send
            .call(ClientRequest::Connect, ConnectRequest::default());
        check_message(
            server.next().await.unwrap(),
            protocol::InitiateContact2 {
                initiate_contact: protocol::InitiateContact {
                    version_requested: Version::Copper as u32,
                    target_message_vp: 0,
                    interrupt_page_or_target_info: TargetInfo::new()
                        .with_sint(2)
                        .with_vtl(0)
                        .with_feature_flags(SUPPORTED_FEATURE_FLAGS.into())
                        .into(),
                    parent_to_child_monitor_page_gpa: 0,
                    child_to_parent_monitor_page_gpa: 0,
                },
                ..FromZeros::new_zeroed()
            },
        );
    }

    #[async_test]
    async fn test_connect_success(driver: DefaultDriver) {
        let (mut server, mut client) = test_init(&driver);
        let client_connect = client.connect(0, None, Guid::ZERO);

        let server_connect = async {
            check_message(
                server.next().await.unwrap(),
                protocol::InitiateContact2 {
                    initiate_contact: protocol::InitiateContact {
                        version_requested: Version::Copper as u32,
                        target_message_vp: 0,
                        interrupt_page_or_target_info: TargetInfo::new()
                            .with_sint(2)
                            .with_vtl(0)
                            .with_feature_flags(SUPPORTED_FEATURE_FLAGS.into())
                            .into(),
                        parent_to_child_monitor_page_gpa: 0,
                        child_to_parent_monitor_page_gpa: 0,
                    },
                    ..FromZeros::new_zeroed()
                },
            );

            server.send(in_msg(
                MessageType::VERSION_RESPONSE,
                protocol::VersionResponse2 {
                    version_response: protocol::VersionResponse {
                        version_supported: 1,
                        connection_state: ConnectionState::SUCCESSFUL,
                        padding: 0,
                        selected_version_or_connection_id: 0,
                    },
                    supported_features: SUPPORTED_FEATURE_FLAGS.into_bits(),
                },
            ));

            check_message(server.next().await.unwrap(), protocol::RequestOffers {});
            server.send(in_msg(MessageType::ALL_OFFERS_DELIVERED, [0x00]));
        };

        let (connection, ()) = (client_connect, server_connect).join().await;
        let connection = connection.unwrap();

        assert_eq!(connection.version.version, Version::Copper);
        assert_eq!(connection.version.feature_flags, SUPPORTED_FEATURE_FLAGS);
    }

    #[async_test]
    async fn test_feature_flags(driver: DefaultDriver) {
        let (mut server, mut client) = test_init(&driver);
        let client_connect = client.connect(0, None, Guid::ZERO);

        let server_connect = async {
            check_message(
                server.next().await.unwrap(),
                protocol::InitiateContact2 {
                    initiate_contact: protocol::InitiateContact {
                        version_requested: Version::Copper as u32,
                        target_message_vp: 0,
                        interrupt_page_or_target_info: TargetInfo::new()
                            .with_sint(2)
                            .with_vtl(0)
                            .with_feature_flags(SUPPORTED_FEATURE_FLAGS.into())
                            .into(),
                        parent_to_child_monitor_page_gpa: 0,
                        child_to_parent_monitor_page_gpa: 0,
                    },
                    ..FromZeros::new_zeroed()
                },
            );

            // Report the server doesn't support some of the feature flags, and make
            // sure this is reflected in the returned version.
            server.send(in_msg(
                MessageType::VERSION_RESPONSE,
                protocol::VersionResponse2 {
                    version_response: protocol::VersionResponse {
                        version_supported: 1,
                        connection_state: ConnectionState::SUCCESSFUL,
                        padding: 0,
                        selected_version_or_connection_id: 0,
                    },
                    supported_features: 2,
                },
            ));

            check_message(server.next().await.unwrap(), protocol::RequestOffers {});
            server.send(in_msg(MessageType::ALL_OFFERS_DELIVERED, [0x00]));
        };

        let (connection, ()) = (client_connect, server_connect).join().await;
        let connection = connection.unwrap();

        assert_eq!(connection.version.version, Version::Copper);
        assert_eq!(
            connection.version.feature_flags,
            FeatureFlags::new().with_channel_interrupt_redirection(true)
        );
    }

    #[async_test]
    async fn test_client_id(driver: DefaultDriver) {
        let (mut server, client) = test_init(&driver);
        let initiate_contact = ConnectRequest {
            client_id: VMBUS_TEST_CLIENT_ID,
            ..Default::default()
        };
        let _recv = client
            .access
            .client_request_send
            .call(ClientRequest::Connect, initiate_contact);

        check_message(
            server.next().await.unwrap(),
            protocol::InitiateContact2 {
                initiate_contact: protocol::InitiateContact {
                    version_requested: Version::Copper as u32,
                    target_message_vp: 0,
                    interrupt_page_or_target_info: TargetInfo::new()
                        .with_sint(2)
                        .with_vtl(0)
                        .with_feature_flags(SUPPORTED_FEATURE_FLAGS.into())
                        .into(),
                    parent_to_child_monitor_page_gpa: 0,
                    child_to_parent_monitor_page_gpa: 0,
                },
                client_id: VMBUS_TEST_CLIENT_ID,
            },
        );
    }

    #[async_test]
    async fn test_version_negotiation(driver: DefaultDriver) {
        let (mut server, mut client) = test_init(&driver);
        let client_connect = client.connect(0, None, Guid::ZERO);

        let server_connect = async {
            check_message(
                server.next().await.unwrap(),
                protocol::InitiateContact2 {
                    initiate_contact: protocol::InitiateContact {
                        version_requested: Version::Copper as u32,
                        target_message_vp: 0,
                        interrupt_page_or_target_info: TargetInfo::new()
                            .with_sint(2)
                            .with_vtl(0)
                            .with_feature_flags(SUPPORTED_FEATURE_FLAGS.into())
                            .into(),
                        parent_to_child_monitor_page_gpa: 0,
                        child_to_parent_monitor_page_gpa: 0,
                    },
                    ..FromZeros::new_zeroed()
                },
            );

            server.send(in_msg(
                MessageType::VERSION_RESPONSE,
                protocol::VersionResponse {
                    version_supported: 0,
                    connection_state: ConnectionState::SUCCESSFUL,
                    padding: 0,
                    selected_version_or_connection_id: 0,
                },
            ));

            check_message(
                server.next().await.unwrap(),
                protocol::InitiateContact {
                    version_requested: Version::Iron as u32,
                    target_message_vp: 0,
                    interrupt_page_or_target_info: TargetInfo::new()
                        .with_sint(2)
                        .with_vtl(0)
                        .with_feature_flags(FeatureFlags::new().into())
                        .into(),
                    parent_to_child_monitor_page_gpa: 0,
                    child_to_parent_monitor_page_gpa: 0,
                },
            );

            server.send(in_msg(
                MessageType::VERSION_RESPONSE,
                protocol::VersionResponse {
                    version_supported: 1,
                    connection_state: ConnectionState::SUCCESSFUL,
                    padding: 0,
                    selected_version_or_connection_id: 0,
                },
            ));

            check_message(server.next().await.unwrap(), protocol::RequestOffers {});
            server.send(in_msg(MessageType::ALL_OFFERS_DELIVERED, [0x00]));
        };

        let (connection, ()) = (client_connect, server_connect).join().await;
        let connection = connection.unwrap();

        assert_eq!(connection.version.version, Version::Iron);
        assert_eq!(connection.version.feature_flags, FeatureFlags::new());
    }

    #[async_test]
    async fn test_open_channel_success(driver: DefaultDriver) {
        let (mut server, mut client) = test_init(&driver);
        let channel = server.get_channel(&mut client).await;

        let recv = channel.request_send.call(
            ChannelRequest::Open,
            OpenRequest {
                open_data: OpenData {
                    target_vp: Some(0),
                    ring_offset: 0,
                    ring_gpadl_id: GpadlId(0),
                    event_flag: 0,
                    connection_id: 0,
                    user_data: UserDefinedData::new_zeroed(),
                },
                incoming_event: None,
                use_vtl2_connection_id: false,
            },
        );

        check_message(
            server.next().await.unwrap(),
            protocol::OpenChannel2 {
                open_channel: protocol::OpenChannel {
                    channel_id: ChannelId(0),
                    open_id: 0,
                    ring_buffer_gpadl_id: GpadlId(0),
                    target_vp: 0,
                    downstream_ring_buffer_page_offset: 0,
                    user_data: UserDefinedData::new_zeroed(),
                },
                connection_id: 0,
                event_flag: 0,
                flags: Default::default(),
            },
        );

        server.send(in_msg(
            MessageType::OPEN_CHANNEL_RESULT,
            protocol::OpenResult {
                channel_id: ChannelId(0),
                open_id: 0,
                status: protocol::STATUS_SUCCESS as u32,
            },
        ));

        recv.await.unwrap().unwrap();
    }

    #[async_test]
    async fn test_open_channel_fail(driver: DefaultDriver) {
        let (mut server, mut client) = test_init(&driver);
        let channel = server.get_channel(&mut client).await;

        let recv = channel.request_send.call(
            ChannelRequest::Open,
            OpenRequest {
                open_data: OpenData {
                    target_vp: Some(0),
                    ring_offset: 0,
                    ring_gpadl_id: GpadlId(0),
                    event_flag: 0,
                    connection_id: 0,
                    user_data: UserDefinedData::new_zeroed(),
                },
                incoming_event: None,
                use_vtl2_connection_id: false,
            },
        );

        check_message(
            server.next().await.unwrap(),
            protocol::OpenChannel2 {
                open_channel: protocol::OpenChannel {
                    channel_id: ChannelId(0),
                    open_id: 0,
                    ring_buffer_gpadl_id: GpadlId(0),
                    target_vp: 0,
                    downstream_ring_buffer_page_offset: 0,
                    user_data: UserDefinedData::new_zeroed(),
                },
                connection_id: 0,
                event_flag: 0,
                flags: Default::default(),
            },
        );

        server.send(in_msg(
            MessageType::OPEN_CHANNEL_RESULT,
            protocol::OpenResult {
                channel_id: ChannelId(0),
                open_id: 0,
                status: protocol::STATUS_UNSUCCESSFUL as u32,
            },
        ));

        recv.await.unwrap().unwrap_err();
    }

    #[async_test]
    async fn test_vtl2_connection_id_requires_interrupt_redirection(driver: DefaultDriver) {
        let (mut server, mut client) = test_init(&driver);
        let client_connect = client.connect(0, None, Guid::ZERO);
        let server_connect = async {
            let _ = server.next().await.unwrap();
            server.send(in_msg(
                MessageType::VERSION_RESPONSE,
                protocol::VersionResponse2 {
                    version_response: protocol::VersionResponse {
                        version_supported: 1,
                        connection_state: ConnectionState::SUCCESSFUL,
                        padding: 0,
                        selected_version_or_connection_id: 0,
                    },
                    supported_features: 0,
                },
            ));
            check_message(server.next().await.unwrap(), protocol::RequestOffers {});
            server.send(in_msg(
                MessageType::OFFER_CHANNEL,
                protocol::OfferChannel {
                    interface_id: Guid::new_random(),
                    instance_id: Guid::new_random(),
                    rsvd: [0; 4],
                    flags: OfferFlags::new(),
                    mmio_megabytes: 0,
                    user_defined: UserDefinedData::new_zeroed(),
                    subchannel_index: 0,
                    mmio_megabytes_optional: 0,
                    channel_id: ChannelId(0),
                    monitor_id: 0,
                    monitor_allocated: 0,
                    is_dedicated: 0,
                    connection_id: 0,
                },
            ));
            server.send(in_msg(MessageType::ALL_OFFERS_DELIVERED, [0x00]));
        };
        let (connection, ()) = (client_connect, server_connect).join().await;
        let [channel] = connection.unwrap().offers.try_into().unwrap();

        let recv = channel.request_send.call(
            ChannelRequest::Open,
            OpenRequest {
                open_data: OpenData {
                    target_vp: Some(0),
                    ring_offset: 0,
                    ring_gpadl_id: GpadlId(0),
                    event_flag: 0,
                    connection_id: 0,
                    user_data: UserDefinedData::new_zeroed(),
                },
                incoming_event: None,
                use_vtl2_connection_id: true,
            },
        );

        recv.await.unwrap().unwrap_err();
    }

    #[async_test]
    async fn test_modify_channel(driver: DefaultDriver) {
        let (mut server, mut client) = test_init(&driver);
        let channel = server.get_channel(&mut client).await;

        // N.B. A real server requires the channel to be open before sending this, but the test
        //      server doesn't care.
        let recv = channel.request_send.call(
            ChannelRequest::Modify,
            ModifyRequest::TargetVp { target_vp: 1 },
        );

        check_message(
            server.next().await.unwrap(),
            protocol::ModifyChannel {
                channel_id: ChannelId(0),
                target_vp: 1,
            },
        );

        server.send(in_msg(
            MessageType::MODIFY_CHANNEL_RESPONSE,
            protocol::ModifyChannelResponse {
                channel_id: ChannelId(0),
                status: protocol::STATUS_SUCCESS,
            },
        ));

        let status = recv.await.unwrap();
        assert_eq!(status, protocol::STATUS_SUCCESS);
    }

    #[async_test]
    async fn test_save_restore_connected(driver: DefaultDriver) {
        let (mut server, mut client) = test_init(&driver);
        server.connect(&mut client).await;
        server.stop_client(&mut client).await;
        let s0 = client.save().await;
        let builder = client.sever().await;
        let mut client = builder.build(&driver);
        client.restore(s0.clone()).await.unwrap();

        let s1 = client.save().await;

        assert_eq!(s0, s1);
    }

    #[async_test]
    async fn test_restore_disconnected_discards_stale_protocol_state(driver: DefaultDriver) {
        let (_server, client) = test_init(&driver);
        let builder = client.sever().await;
        let mut client = builder.build(&driver);
        let offer = protocol::OfferChannel {
            interface_id: Guid::new_random(),
            instance_id: Guid::new_random(),
            rsvd: [0; 4],
            flags: OfferFlags::new(),
            mmio_megabytes: 0,
            user_defined: UserDefinedData::new_zeroed(),
            subchannel_index: 0,
            mmio_megabytes_optional: 0,
            channel_id: ChannelId(7),
            monitor_id: 0,
            monitor_allocated: 0,
            is_dedicated: 0,
            connection_id: 0,
        };
        let state = SavedState {
            client_state: saved_state::ClientState::Disconnected,
            channels: vec![saved_state::Channel {
                id: 7,
                state: saved_state::ChannelState::Offered,
                offer: offer.into(),
            }],
            gpadls: vec![saved_state::Gpadl {
                gpadl_id: 1,
                channel_id: 7,
                state: saved_state::GpadlState::Created,
            }],
            pending_messages: vec![saved_state::PendingMessage { data: vec![0xff] }],
        };

        assert!(client.restore(state).await.unwrap().is_none());
        let restored = client.save().await;
        assert!(restored.channels.is_empty());
        assert!(restored.gpadls.is_empty());
        assert!(restored.pending_messages.is_empty());
    }

    #[async_test]
    async fn test_save_restore_connected_with_channel(driver: DefaultDriver) {
        let (mut server, mut client) = test_init(&driver);
        let c0 = server.get_channel(&mut client).await;
        server.stop_client(&mut client).await;
        let s0 = client.save().await;
        let builder = client.sever().await;
        let mut client = builder.build(&driver);
        let connection = client.restore(s0.clone()).await.unwrap().unwrap();
        let s1 = client.save().await;
        assert_eq!(s0, s1);
        assert_eq!(connection.offers[0].offer, c0.offer);
    }

    #[async_test]
    async fn test_save_restore_connected_with_revoked_channel(driver: DefaultDriver) {
        let (mut server, mut client) = test_init(&driver);
        let c0 = server.get_channel(&mut client).await;
        server.send(in_msg(
            MessageType::RESCIND_CHANNEL_OFFER,
            protocol::RescindChannelOffer {
                channel_id: ChannelId(0),
            },
        ));
        c0.revoke_recv.await.unwrap();
        let rpc = c0.request_send.call(
            ChannelRequest::Modify,
            ModifyRequest::TargetVp { target_vp: 1 },
        );

        check_message(
            server.next().await.unwrap(),
            protocol::ModifyChannel {
                channel_id: ChannelId(0),
                target_vp: 1,
            },
        );

        let client_stop = client.stop();
        let server_stop = async {
            server.send(in_msg(
                MessageType::MODIFY_CHANNEL_RESPONSE,
                protocol::ModifyChannelResponse {
                    channel_id: ChannelId(0),
                    status: protocol::STATUS_SUCCESS,
                },
            ));
            check_message(server.next().await.unwrap(), protocol::Pause);
            server.send(in_msg(MessageType::PAUSE_RESPONSE, protocol::PauseResponse));
        };
        (client_stop, server_stop).join().await;

        rpc.await.unwrap();

        let s0 = client.save().await;
        let builder = client.sever().await;
        let mut client = builder.build(&driver);
        let connection = client.restore(s0.clone()).await.unwrap().unwrap();
        let s1 = client.save().await;
        assert_eq!(s0, s1);
        assert!(connection.offers.is_empty());
        server.start_client(&mut client).await;
        check_message(
            server.next().await.unwrap(),
            protocol::RelIdReleased {
                channel_id: ChannelId(0),
            },
        );
    }

    #[async_test]
    async fn test_connect_fails_on_incorrect_state(driver: DefaultDriver) {
        let (mut server, mut client) = test_init(&driver);
        server.connect(&mut client).await;
        let err = client.connect(0, None, Guid::ZERO).await.unwrap_err();
        assert!(matches!(err, ConnectError::InvalidState), "{:?}", err);
    }

    #[async_test]
    async fn test_hot_add_remove(driver: DefaultDriver) {
        let (mut server, mut client) = test_init(&driver);

        let mut connection = server.connect(&mut client).await;
        let offer = protocol::OfferChannel {
            interface_id: Guid::new_random(),
            instance_id: Guid::new_random(),
            rsvd: [0; 4],
            flags: OfferFlags::new(),
            mmio_megabytes: 0,
            user_defined: UserDefinedData::new_zeroed(),
            subchannel_index: 0,
            mmio_megabytes_optional: 0,
            channel_id: ChannelId(5),
            monitor_id: 0,
            monitor_allocated: 0,
            is_dedicated: 0,
            connection_id: 0,
        };

        server.send(in_msg(MessageType::OFFER_CHANNEL, offer));
        let info = connection.offer_recv.next().await.unwrap();

        assert_eq!(offer, info.offer);

        server.send(in_msg(
            MessageType::RESCIND_CHANNEL_OFFER,
            protocol::RescindChannelOffer {
                channel_id: ChannelId(5),
            },
        ));

        info.revoke_recv.await.unwrap();
        drop(info.request_send);

        check_message(
            server.next().await.unwrap(),
            protocol::RelIdReleased {
                channel_id: ChannelId(5),
            },
        );
    }

    #[async_test]
    async fn test_gpadl_success(driver: DefaultDriver) {
        let (mut server, mut client) = test_init(&driver);
        let channel = server.get_channel(&mut client).await;
        let recv = channel.request_send.call(
            ChannelRequest::Gpadl,
            GpadlRequest {
                id: GpadlId(1),
                count: 1,
                buf: vec![5],
            },
        );

        check_message_with_data(
            server.next().await.unwrap(),
            protocol::GpadlHeader {
                channel_id: ChannelId(0),
                gpadl_id: GpadlId(1),
                len: 8,
                count: 1,
            },
            0x5u64.as_bytes(),
        );

        server.send(in_msg(
            MessageType::GPADL_CREATED,
            protocol::GpadlCreated {
                channel_id: ChannelId(0),
                gpadl_id: GpadlId(1),
                status: protocol::STATUS_SUCCESS,
            },
        ));

        recv.await.unwrap().unwrap();

        let rpc = channel
            .request_send
            .call(ChannelRequest::TeardownGpadl, GpadlId(1));

        check_message(
            server.next().await.unwrap(),
            protocol::GpadlTeardown {
                channel_id: ChannelId(0),
                gpadl_id: GpadlId(1),
            },
        );

        server.send(in_msg(
            MessageType::GPADL_TORNDOWN,
            protocol::GpadlTorndown {
                gpadl_id: GpadlId(1),
            },
        ));

        rpc.await.unwrap();
    }

    #[async_test]
    async fn test_gpadl_fail(driver: DefaultDriver) {
        let (mut server, mut client) = test_init(&driver);
        let channel = server.get_channel(&mut client).await;
        let recv = channel.request_send.call(
            ChannelRequest::Gpadl,
            GpadlRequest {
                id: GpadlId(1),
                count: 1,
                buf: vec![7],
            },
        );

        check_message_with_data(
            server.next().await.unwrap(),
            protocol::GpadlHeader {
                channel_id: ChannelId(0),
                gpadl_id: GpadlId(1),
                len: 8,
                count: 1,
            },
            0x7u64.as_bytes(),
        );

        server.send(in_msg(
            MessageType::GPADL_CREATED,
            protocol::GpadlCreated {
                channel_id: ChannelId(0),
                gpadl_id: GpadlId(1),
                status: protocol::STATUS_UNSUCCESSFUL,
            },
        ));

        recv.await.unwrap().unwrap_err();
    }

    #[async_test]
    async fn test_gpadl_with_revoke(driver: DefaultDriver) {
        let (mut server, mut client) = test_init(&driver);
        let channel = server.get_channel(&mut client).await;
        let channel_id = ChannelId(0);
        for gpadl_id in [1, 2, 3].map(GpadlId) {
            let recv = channel.request_send.call(
                ChannelRequest::Gpadl,
                GpadlRequest {
                    id: gpadl_id,
                    count: 1,
                    buf: vec![3],
                },
            );

            check_message_with_data(
                server.next().await.unwrap(),
                protocol::GpadlHeader {
                    channel_id,
                    gpadl_id,
                    len: 8,
                    count: 1,
                },
                0x3u64.as_bytes(),
            );

            server.send(in_msg(
                MessageType::GPADL_CREATED,
                protocol::GpadlCreated {
                    channel_id,
                    gpadl_id,
                    status: protocol::STATUS_SUCCESS,
                },
            ));

            recv.await.unwrap().unwrap();
        }

        let rpc = channel
            .request_send
            .call(ChannelRequest::TeardownGpadl, GpadlId(1));

        check_message(
            server.next().await.unwrap(),
            protocol::GpadlTeardown {
                channel_id,
                gpadl_id: GpadlId(1),
            },
        );

        server.send(in_msg(
            MessageType::RESCIND_CHANNEL_OFFER,
            protocol::RescindChannelOffer { channel_id },
        ));

        let recv = channel.request_send.call_failable(
            ChannelRequest::Gpadl,
            GpadlRequest {
                id: GpadlId(4),
                count: 1,
                buf: vec![3],
            },
        );

        check_message_with_data(
            server.next().await.unwrap(),
            protocol::GpadlHeader {
                channel_id,
                gpadl_id: GpadlId(4),
                len: 8,
                count: 1,
            },
            0x3u64.as_bytes(),
        );

        server.send(in_msg(
            MessageType::GPADL_CREATED,
            protocol::GpadlCreated {
                channel_id,
                gpadl_id: GpadlId(4),
                status: protocol::STATUS_UNSUCCESSFUL,
            },
        ));

        server.send(in_msg(
            MessageType::GPADL_TORNDOWN,
            protocol::GpadlTorndown {
                gpadl_id: GpadlId(1),
            },
        ));

        rpc.await.unwrap();
        recv.await.unwrap_err();

        channel.revoke_recv.await.unwrap();

        let rpc = channel
            .request_send
            .call(ChannelRequest::TeardownGpadl, GpadlId(2));
        drop(channel.request_send);

        check_message(
            server.next().await.unwrap(),
            protocol::GpadlTeardown {
                channel_id,
                gpadl_id: GpadlId(2),
            },
        );

        server.send(in_msg(
            MessageType::GPADL_TORNDOWN,
            protocol::GpadlTorndown {
                gpadl_id: GpadlId(2),
            },
        ));

        rpc.await.unwrap();

        check_message(
            server.next().await.unwrap(),
            protocol::RelIdReleased { channel_id },
        );
    }

    #[async_test]
    async fn test_modify_connection(driver: DefaultDriver) {
        let (mut server, mut client) = test_init(&driver);
        server.connect(&mut client).await;
        let call = client.access.client_request_send.call(
            ClientRequest::Modify,
            ModifyConnectionRequest {
                monitor_page: Some(MonitorPageGpas {
                    child_to_parent: 5,
                    parent_to_child: 6,
                }),
            },
        );

        check_message(
            server.next().await.unwrap(),
            protocol::ModifyConnection {
                child_to_parent_monitor_page_gpa: 5,
                parent_to_child_monitor_page_gpa: 6,
            },
        );

        server.send(in_msg(
            MessageType::MODIFY_CONNECTION_RESPONSE,
            protocol::ModifyConnectionResponse {
                connection_state: ConnectionState::FAILED_LOW_RESOURCES,
            },
        ));

        let result = call.await.unwrap();
        assert_eq!(ConnectionState::FAILED_LOW_RESOURCES, result);
    }

    #[async_test]
    async fn test_hvsock(driver: DefaultDriver) {
        let (mut server, mut client) = test_init(&driver);
        server.connect(&mut client).await;
        let request = HvsockConnectRequest {
            service_id: Guid::new_random(),
            endpoint_id: Guid::new_random(),
            silo_id: Guid::new_random(),
            hosted_silo_unaware: false,
        };

        let resp = client.access().connect_hvsock(request);
        check_message(
            server.next().await.unwrap(),
            protocol::TlConnectRequest2 {
                base: protocol::TlConnectRequest {
                    service_id: request.service_id,
                    endpoint_id: request.endpoint_id,
                },
                silo_id: request.silo_id,
            },
        );

        // Now send a failure result.
        server.send(in_msg(
            MessageType::TL_CONNECT_REQUEST_RESULT,
            protocol::TlConnectResult {
                service_id: request.service_id,
                endpoint_id: request.endpoint_id,
                status: protocol::STATUS_CONNECTION_REFUSED,
            },
        ));

        let result = resp.await;
        assert!(result.is_none());
    }

    #[async_test]
    async fn test_synic_event_flags(driver: DefaultDriver) {
        let (mut server, mut client) = test_init(&driver);
        let connection = server.get_channels(&mut client, 5).await;
        let event = Event::new();

        for _ in 0..5 {
            for (i, channel) in connection.offers.iter().enumerate() {
                let recv = channel.request_send.call(
                    ChannelRequest::Open,
                    OpenRequest {
                        open_data: OpenData {
                            target_vp: Some(0),
                            ring_offset: 0,
                            ring_gpadl_id: GpadlId(0),
                            event_flag: 0,
                            connection_id: 0,
                            user_data: UserDefinedData::new_zeroed(),
                        },
                        incoming_event: Some(event.clone()),
                        use_vtl2_connection_id: false,
                    },
                );

                let expected_event_flag = i as u16 + 1;

                check_message(
                    server.next().await.unwrap(),
                    protocol::OpenChannel2 {
                        open_channel: protocol::OpenChannel {
                            channel_id: channel.offer.channel_id,
                            open_id: 0,
                            ring_buffer_gpadl_id: GpadlId(0),
                            target_vp: 0,
                            downstream_ring_buffer_page_offset: 0,
                            user_data: UserDefinedData::new_zeroed(),
                        },
                        connection_id: 0,
                        event_flag: expected_event_flag,
                        flags: OpenChannelFlags::new().with_redirect_interrupt(true),
                    },
                );

                server.send(in_msg(
                    MessageType::OPEN_CHANNEL_RESULT,
                    protocol::OpenResult {
                        channel_id: channel.offer.channel_id,
                        open_id: 0,
                        status: protocol::STATUS_SUCCESS as u32,
                    },
                ));

                let output = recv.await.unwrap().unwrap();
                assert_eq!(output.redirected_event_flag, Some(expected_event_flag));
            }

            for (i, channel) in connection.offers.iter().enumerate() {
                // Close the channel to prepare for the next iteration of the loop.
                // The event flag should be the same each time.
                channel
                    .request_send
                    .call(ChannelRequest::Close, ())
                    .await
                    .unwrap();

                check_message(
                    server.next().await.unwrap(),
                    protocol::CloseChannel {
                        channel_id: ChannelId(i as u32),
                    },
                );
            }
        }
    }

    #[async_test]
    async fn test_revoke(driver: DefaultDriver) {
        let (mut server, mut client) = test_init(&driver);
        let channel = server.get_channel(&mut client).await;

        server.send(in_msg(
            MessageType::RESCIND_CHANNEL_OFFER,
            protocol::RescindChannelOffer {
                channel_id: ChannelId(0),
            },
        ));

        channel.revoke_recv.await.unwrap();

        channel
            .request_send
            .call_failable(
                ChannelRequest::Open,
                OpenRequest {
                    open_data: OpenData {
                        target_vp: Some(0),
                        ring_offset: 0,
                        ring_gpadl_id: GpadlId(0),
                        event_flag: 0,
                        connection_id: 0,
                        user_data: UserDefinedData::new_zeroed(),
                    },
                    incoming_event: None,
                    use_vtl2_connection_id: false,
                },
            )
            .await
            .unwrap_err();
    }

    #[async_test]
    async fn test_reoffer_in_use_rel_id_is_dropped(driver: DefaultDriver) {
        let (mut server, mut client) = test_init(&driver);
        let mut connection = server.get_channels(&mut client, 1).await;
        let [channel] = connection.offers.try_into().unwrap();

        server.send(in_msg(
            MessageType::RESCIND_CHANNEL_OFFER,
            protocol::RescindChannelOffer {
                channel_id: ChannelId(0),
            },
        ));

        channel.revoke_recv.await.unwrap();

        // This offer is invalid because the rel id is still in use.
        let duplicate_offer = protocol::OfferChannel {
            interface_id: Guid::new_random(),
            instance_id: Guid::new_random(),
            rsvd: [0; 4],
            flags: OfferFlags::new(),
            mmio_megabytes: 0,
            user_defined: UserDefinedData::new_zeroed(),
            subchannel_index: 0,
            mmio_megabytes_optional: 0,
            channel_id: ChannelId(0),
            monitor_id: 0,
            monitor_allocated: 0,
            is_dedicated: 0,
            connection_id: 0,
        };

        server.send(in_msg(MessageType::OFFER_CHANNEL, duplicate_offer));

        let valid_offer = protocol::OfferChannel {
            channel_id: ChannelId(1),
            ..duplicate_offer
        };
        server.send(in_msg(MessageType::OFFER_CHANNEL, valid_offer));

        let received = connection.offer_recv.next().await.unwrap();
        assert_eq!(received.offer, valid_offer);
    }

    #[async_test]
    async fn test_revoke_release_and_reoffer(driver: DefaultDriver) {
        let (mut server, mut client) = test_init(&driver);
        let mut connection = server.get_channels(&mut client, 1).await;
        let [channel] = connection.offers.try_into().unwrap();

        server.send(in_msg(
            MessageType::RESCIND_CHANNEL_OFFER,
            protocol::RescindChannelOffer {
                channel_id: ChannelId(0),
            },
        ));

        channel.revoke_recv.await.unwrap();
        drop(channel.request_send);

        check_message(
            server.next().await.unwrap(),
            protocol::RelIdReleased {
                channel_id: ChannelId(0),
            },
        );

        let offer = protocol::OfferChannel {
            interface_id: Guid::new_random(),
            instance_id: Guid::new_random(),
            rsvd: [0; 4],
            flags: OfferFlags::new(),
            mmio_megabytes: 0,
            user_defined: UserDefinedData::new_zeroed(),
            subchannel_index: 0,
            mmio_megabytes_optional: 0,
            channel_id: ChannelId(0),
            monitor_id: 0,
            monitor_allocated: 0,
            is_dedicated: 0,
            connection_id: 0,
        };

        server.send(in_msg(MessageType::OFFER_CHANNEL, offer));

        connection.offer_recv.next().await.unwrap();
    }

    #[async_test]
    async fn test_release_revoke_and_reoffer(driver: DefaultDriver) {
        let (mut server, mut client) = test_init(&driver);
        let mut connection = server.get_channels(&mut client, 1).await;
        let [channel] = connection.offers.try_into().unwrap();

        let open = channel.request_send.call_failable(
            ChannelRequest::Open,
            OpenRequest {
                open_data: OpenData {
                    target_vp: Some(0),
                    ring_offset: 0,
                    ring_gpadl_id: GpadlId(0),
                    event_flag: 0,
                    connection_id: 0,
                    user_data: UserDefinedData::new_zeroed(),
                },
                incoming_event: None,
                use_vtl2_connection_id: false,
            },
        );

        let server_open = async {
            check_message(
                server.next().await.unwrap(),
                protocol::OpenChannel2 {
                    open_channel: protocol::OpenChannel {
                        channel_id: ChannelId(0),
                        open_id: 0,
                        ring_buffer_gpadl_id: GpadlId(0),
                        target_vp: 0,
                        downstream_ring_buffer_page_offset: 0,
                        user_data: UserDefinedData::new_zeroed(),
                    },
                    connection_id: 0,
                    event_flag: 0,
                    flags: Default::default(),
                },
            );
            server.send(in_msg(
                MessageType::OPEN_CHANNEL_RESULT,
                protocol::OpenResult {
                    channel_id: ChannelId(0),
                    open_id: 0,
                    status: protocol::STATUS_SUCCESS as u32,
                },
            ));
        };

        (open, server_open).join().await.0.unwrap();

        // This will close the channel but won't release it yet.
        drop(channel);

        check_message(
            server.next().await.unwrap(),
            protocol::CloseChannel {
                channel_id: ChannelId(0),
            },
        );

        server.send(in_msg(
            MessageType::RESCIND_CHANNEL_OFFER,
            protocol::RescindChannelOffer {
                channel_id: ChannelId(0),
            },
        ));

        // Should be released.
        check_message(
            server.next().await.unwrap(),
            protocol::RelIdReleased {
                channel_id: ChannelId(0),
            },
        );

        let offer = protocol::OfferChannel {
            interface_id: Guid::new_random(),
            instance_id: Guid::new_random(),
            rsvd: [0; 4],
            flags: OfferFlags::new(),
            mmio_megabytes: 0,
            user_defined: UserDefinedData::new_zeroed(),
            subchannel_index: 0,
            mmio_megabytes_optional: 0,
            channel_id: ChannelId(0),
            monitor_id: 0,
            monitor_allocated: 0,
            is_dedicated: 0,
            connection_id: 0,
        };

        server.send(in_msg(MessageType::OFFER_CHANNEL, offer));

        // New offer should come through.
        connection.offer_recv.next().await.unwrap();
    }
}
