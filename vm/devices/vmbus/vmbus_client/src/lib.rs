// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Client driver for the Hyper-V Virtual Machine Bus (VmBus).

#![expect(missing_docs)]
#![forbid(unsafe_code)]

pub mod driver;
pub mod filter;
mod hvsock;
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
use std::collections::hash_map;
use std::convert::TryInto;
use std::future::Future;
use std::future::poll_fn;
use std::ops::Deref;
use std::ops::DerefMut;
use std::pin::pin;
use std::sync::Arc;
use std::sync::atomic::AtomicU32;
use std::sync::atomic::Ordering;
use std::task::Context;
use std::task::Poll;
use thiserror::Error;
use vmbus_async::async_dgram::AsyncRecv;
use vmbus_async::async_dgram::AsyncRecvExt;
use vmbus_channel::bus::GpadlRequest;
use vmbus_channel::bus::ModifyRequest;
use vmbus_channel::bus::OfferKey;
use vmbus_channel::bus::OpenData;
use vmbus_channel::gpadl::GpadlId;
use vmbus_core::HvsockConnectRequest;
use vmbus_core::OutgoingMessage;
use vmbus_core::TaggedStream;
use vmbus_core::VersionInfo;
use vmbus_core::protocol;
use vmbus_core::protocol::ChannelId;
use vmbus_core::protocol::ConnectionState;
use vmbus_core::protocol::FeatureFlags;
use vmbus_core::protocol::Message;
use vmbus_core::protocol::OpenChannelFlags;
use vmbus_core::protocol::Version;
use vmcore::interrupt::Interrupt;
use vmcore::synic::MonitorPageGpas;
use zerocopy::Immutable;
use zerocopy::IntoBytes;
use zerocopy::KnownLayout;

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
            teardown_gpadls: HashMap::new(),
            channel_requests: SelectAll::new(),
            synic: SynicState {
                event_flag_state: Vec::new(),
                event_client: self.event_client,
            },
        };

        let mut task = ClientTask {
            inner,
            channels: ChannelList::default(),
            task_recv,
            running: false,
            msg_source: self.msg_source,
            client_request_recv,
            state: ClientState::Disconnected,
            modify_request: None,
            hvsock_tracker: hvsock::HvsockRequestTracker::new(),
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

/// The overall state machine used to drive which actions the client can legally
/// take. This primarily pertains to overall client activity but has a
/// side-effect of limiting whether or not channels can perform actions.
#[derive(Inspect)]
#[inspect(external_tag)]
enum ClientState {
    /// The client has yet to connect to the server.
    Disconnected,
    /// The client has initiated contact with the server.
    Connecting {
        version: Version,
        #[inspect(skip)]
        rpc: Rpc<ConnectRequest, Result<ConnectResult, ConnectError>>,
    },
    /// The client has negotiated the protocol version with the server.
    Connected {
        version: VersionInfo,
        #[inspect(skip)]
        offer_send: mesh::Sender<OfferInfo>,
    },
    /// The client has requested offers from the server.
    RequestingOffers {
        version: VersionInfo,
        #[inspect(skip)]
        rpc: Rpc<(), Result<ConnectResult, ConnectError>>,
        #[inspect(skip)]
        offers: Vec<OfferInfo>,
    },
    /// The client has initiated an unload from the server.
    Disconnecting {
        version: VersionInfo,
        #[inspect(skip)]
        rpc: Rpc<(), ()>,
    },
}

impl ClientState {
    fn get_version(&self) -> Option<VersionInfo> {
        match self {
            ClientState::Connected { version, .. } => Some(*version),
            ClientState::RequestingOffers { version, .. } => Some(*version),
            ClientState::Disconnecting { version, .. } => Some(*version),
            ClientState::Disconnected | ClientState::Connecting { .. } => None,
        }
    }
}

impl std::fmt::Display for ClientState {
    fn fmt(&self, fmt: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let s = match self {
            ClientState::Disconnected => "Disconnected",
            ClientState::Connecting { .. } => "Connecting",
            ClientState::Connected { .. } => "Connected",
            ClientState::RequestingOffers { .. } => "RequestingOffers",
            ClientState::Disconnecting { .. } => "Disconnecting",
        };
        fmt.pad(s)
    }
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

/// The per-channel state which dictates which whether or not a channel can
/// request an Open/Close. As GPADLs can happen outside this loop there is no
/// state tied to GPADL actions.
#[derive(Debug, Inspect)]
#[inspect(external_tag)]
enum ChannelState {
    /// The channel has been offered to the client.
    Offered,
    /// The channel has requested the server to be opened.
    Opening {
        redirected_event_flag: Option<u16>,
        #[inspect(skip)]
        redirected_event: Option<Event>,
        #[inspect(skip)]
        rpc: FailableRpc<(), OpenOutput>,
    },
    /// The channel has been restored but not claimed.
    Restored,
    /// The channel has been successfully opened.
    Opened {
        redirected_event_flag: Option<u16>,
        #[inspect(skip)]
        redirected_event: Option<Event>,
    },
    /// The channel has been revoked by the server.
    Revoked,
}

impl std::fmt::Display for ChannelState {
    fn fmt(&self, fmt: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let s = match self {
            ChannelState::Opening { .. } => "Opening",
            ChannelState::Offered => "Offered",
            ChannelState::Opened { .. } => "Opened",
            ChannelState::Restored => "Restored",
            ChannelState::Revoked => "Revoked",
        };
        fmt.pad(s)
    }
}

#[derive(Debug, Inspect)]
struct Channel {
    offer: protocol::OfferChannel,
    // When dropped, notifies the caller the channel has been revoked.
    #[inspect(skip)]
    revoke_send: Option<mesh::OneshotSender<()>>,
    state: ChannelState,
    #[inspect(with = "|x| x.is_some()")]
    modify_response_send: Option<Rpc<(), i32>>,
    #[inspect(with = "|x| inspect::iter_by_key(x).map_key(|x| x.0)")]
    gpadls: HashMap<GpadlId, GpadlState>,
    is_client_released: bool,
    connection_id: Arc<AtomicU32>,
}

impl Channel {
    fn pending_request(&self) -> Option<&'static str> {
        if self.modify_response_send.is_some() {
            return Some("modify");
        }
        self.gpadls.iter().find_map(|(_, gpadl)| match gpadl {
            GpadlState::Offered(_) => Some("creating gpadl"),
            GpadlState::Created => None,
            GpadlState::TearingDown { .. } => Some("tearing down gpadl"),
        })
    }
}

#[derive(Inspect)]
struct ClientTask {
    #[inspect(flatten)]
    inner: ClientTaskInner,
    channels: ChannelList,
    state: ClientState,
    hvsock_tracker: hvsock::HvsockRequestTracker,
    running: bool,
    #[inspect(with = "|x| x.is_some()")]
    modify_request: Option<Rpc<ModifyConnectionRequest, ConnectionState>>,
    #[inspect(skip)]
    msg_source: Box<dyn VmbusMessageSource>,
    #[inspect(skip)]
    task_recv: mesh::Receiver<TaskRequest>,
    #[inspect(skip)]
    client_request_recv: mesh::Receiver<ClientRequest>,
}

impl ClientTask {
    fn handle_initiate_contact(
        &mut self,
        rpc: Rpc<ConnectRequest, Result<ConnectResult, ConnectError>>,
        version: Version,
    ) {
        let ClientState::Disconnected = self.state else {
            tracing::warn!(client_state = %self.state, "invalid client state for InitiateContact");
            rpc.complete(Err(ConnectError::InvalidState));
            return;
        };
        let feature_flags = if version >= Version::Copper {
            SUPPORTED_FEATURE_FLAGS
        } else {
            FeatureFlags::new()
        };

        let request = rpc.input();

        tracing::debug!(version = ?version, ?feature_flags, "VmBus client connecting");
        let target_info = protocol::TargetInfo::new()
            .with_sint(SINT)
            .with_vtl(VTL)
            .with_feature_flags(feature_flags.into());
        let monitor_page = request.monitor_page.unwrap_or_default();
        let msg = protocol::InitiateContact2 {
            initiate_contact: protocol::InitiateContact {
                version_requested: version as u32,
                target_message_vp: request.target_message_vp,
                interrupt_page_or_target_info: target_info.into(),
                parent_to_child_monitor_page_gpa: monitor_page.parent_to_child,
                child_to_parent_monitor_page_gpa: monitor_page.child_to_parent,
            },
            client_id: request.client_id,
        };

        self.state = ClientState::Connecting { version, rpc };
        if version < Version::Copper {
            self.inner.messages.send(&msg.initiate_contact)
        } else {
            self.inner.messages.send(&msg);
        }
    }

    fn handle_unload(&mut self, rpc: Rpc<(), ()>) {
        tracing::debug!(%self.state, "VmBus client disconnecting");
        self.state = ClientState::Disconnecting {
            version: self.state.get_version().expect("invalid state for unload"),
            rpc,
        };

        self.inner.messages.send(&protocol::Unload {});
    }

    fn handle_modify(&mut self, request: Rpc<ModifyConnectionRequest, ConnectionState>) {
        if !matches!(self.state, ClientState::Connected { version, .. }
            if version.feature_flags.modify_connection())
        {
            tracing::warn!("ModifyConnection not supported");
            request.complete(ConnectionState::FAILED_UNKNOWN_FAILURE);
            return;
        }

        if self.modify_request.is_some() {
            tracing::warn!("Duplicate ModifyConnection request");
            request.complete(ConnectionState::FAILED_UNKNOWN_FAILURE);
            return;
        }

        let message = protocol::ModifyConnection::from(*request.input());
        self.modify_request = Some(request);
        self.inner.messages.send(&message);
    }

    fn handle_tl_connect(&mut self, rpc: Rpc<HvsockConnectRequest, Option<OfferInfo>>) {
        // The client only supports protocol versions which use the newer message format.
        // The host will not send a TlConnectRequestResult message on success, so a response to this
        // message is not guaranteed.
        let message = protocol::TlConnectRequest2::from(*rpc.input());
        self.hvsock_tracker.add_request(rpc);
        self.inner.messages.send(&message);
    }

    fn handle_client_request(&mut self, request: ClientRequest) {
        match request {
            ClientRequest::Connect(rpc) => {
                self.handle_initiate_contact(rpc, *SUPPORTED_VERSIONS.last().unwrap());
            }
            ClientRequest::Unload(rpc) => {
                self.handle_unload(rpc);
            }
            ClientRequest::Modify(request) => self.handle_modify(request),
            ClientRequest::HvsockConnect(request) => self.handle_tl_connect(request),
        }
    }

    fn handle_version_response(&mut self, msg: protocol::VersionResponse2) {
        let old_state = std::mem::replace(&mut self.state, ClientState::Disconnected);
        let ClientState::Connecting { version, rpc } = old_state else {
            self.state = old_state;
            tracing::warn!(
                client_state = %self.state,
                "invalid client state to handle VersionResponse"
            );
            return;
        };
        if msg.version_response.version_supported > 0 {
            if msg.version_response.connection_state != ConnectionState::SUCCESSFUL {
                rpc.complete(Err(ConnectError::FailedToConnect(
                    msg.version_response.connection_state,
                )));
                return;
            }

            let feature_flags = if version >= Version::Copper {
                FeatureFlags::from(msg.supported_features)
            } else {
                FeatureFlags::new()
            };

            let version = VersionInfo {
                version,
                feature_flags,
            };

            self.inner.messages.send(&protocol::RequestOffers {});
            self.state = ClientState::RequestingOffers {
                version,
                rpc: rpc.split().1,
                offers: Vec::new(),
            };
            tracing::info!(?version, "VmBus client connected, requesting offers");
        } else {
            let index = SUPPORTED_VERSIONS
                .iter()
                .position(|v| *v == version)
                .unwrap();

            if index == 0 {
                rpc.complete(Err(ConnectError::NoSupportedVersions));
                return;
            }
            let next_version = SUPPORTED_VERSIONS[index - 1];
            tracing::debug!(
                version = version as u32,
                next_version = next_version as u32,
                "Unsupported version, retrying"
            );
            self.handle_initiate_contact(rpc, next_version);
        }
    }

    fn create_channel(&mut self, offer: protocol::OfferChannel) -> Result<OfferInfo> {
        self.create_channel_core(offer, ChannelState::Offered)
    }

    fn create_channel_core(
        &mut self,
        offer: protocol::OfferChannel,
        state: ChannelState,
    ) -> Result<OfferInfo> {
        if self.channels.0.contains_key(&offer.channel_id) {
            anyhow::bail!("channel {:?} exists", offer.channel_id);
        }
        let (request_send, request_recv) = mesh::channel();
        let (revoke_send, revoke_recv) = mesh::oneshot();

        let connection_id = Arc::new(AtomicU32::new(0));
        self.channels.0.insert(
            offer.channel_id,
            Channel {
                revoke_send: Some(revoke_send),
                offer,
                state,
                modify_response_send: None,
                gpadls: HashMap::new(),
                is_client_released: false,
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

    fn handle_offer(&mut self, offer: protocol::OfferChannel) {
        let offer_info = self
            .create_channel(offer)
            .expect("channel should not exist");

        tracing::info!(
                state = %self.state,
                channel_id = offer.channel_id.0,
                interface_id = %offer.interface_id,
                instance_id = %offer.instance_id,
                subchannel_index = offer.subchannel_index,
                "received offer");

        if let Some(offer) = self.hvsock_tracker.check_offer(&offer_info.offer) {
            offer.complete(Some(offer_info));
        } else {
            match &mut self.state {
                ClientState::Connected { offer_send, .. } => {
                    offer_send.send(offer_info);
                }
                ClientState::RequestingOffers { offers, .. } => {
                    offers.push(offer_info);
                }
                state => unreachable!("invalid client state for OfferChannel: {state}"),
            }
        }
    }

    fn handle_rescind(&mut self, rescind: protocol::RescindChannelOffer) -> TriedRelease {
        let mut channel = self.channels.get_mut(rescind.channel_id);
        tracing::info!(
            state = %self.state,
            channel_id = rescind.channel_id.0,
            key = %OfferKey::from(&channel.offer),
            "received rescind"
        );
        let event_flag = match std::mem::replace(&mut channel.state, ChannelState::Revoked) {
            ChannelState::Offered => None,
            ChannelState::Opening {
                redirected_event_flag,
                redirected_event: _,
                rpc,
            } => {
                rpc.fail(anyhow::anyhow!("channel revoked"));
                redirected_event_flag
            }
            ChannelState::Restored => None,
            ChannelState::Opened {
                redirected_event_flag,
                redirected_event: _,
            } => redirected_event_flag,
            ChannelState::Revoked => {
                panic!("channel id {:?} already revoked", rescind.channel_id);
            }
        };
        if let Some(event_flag) = event_flag {
            self.inner.synic.free_event_flag(event_flag);
        }

        // Drop the channel and send the revoked message to the client.
        channel.revoke_send.take().unwrap().send(());

        channel.try_release(&mut self.inner.messages)
    }

    fn handle_offers_delivered(&mut self) {
        match std::mem::replace(&mut self.state, ClientState::Disconnected) {
            ClientState::RequestingOffers {
                version,
                rpc,
                offers,
            } => {
                tracing::info!(version = ?version, "VmBus client connected, offers delivered");
                let (offer_send, offer_recv) = mesh::channel();
                self.state = ClientState::Connected {
                    version,
                    offer_send,
                };
                rpc.complete(Ok(ConnectResult {
                    version,
                    offers,
                    offer_recv,
                }));
            }
            state => {
                tracing::warn!(client_state = %state, "invalid client state for OffersDelivered");
                self.state = state;
            }
        }
    }

    fn handle_gpadl_created(&mut self, request: protocol::GpadlCreated) -> TriedRelease {
        let mut channel = self.channels.get_mut(request.channel_id);
        let Some(gpadl_state) = channel.gpadls.get_mut(&request.gpadl_id) else {
            panic!("GpadlCreated for unknown gpadl {:#x}", request.gpadl_id.0);
        };

        let rpc = match std::mem::replace(gpadl_state, GpadlState::Created) {
            GpadlState::Offered(rpc) => rpc,
            old_state => {
                panic!(
                    "invalid state {old_state:?} for gpadl {:#x}:{:#x}",
                    request.channel_id.0, request.gpadl_id.0
                );
            }
        };

        let gpadl_created = request.status == protocol::STATUS_SUCCESS;
        if gpadl_created {
            rpc.complete(Ok(()));
        } else {
            channel.gpadls.remove(&request.gpadl_id).unwrap();
            rpc.fail(anyhow::anyhow!(
                "gpadl creation failed: {:#x}",
                request.status
            ));
        };
        channel.try_release(&mut self.inner.messages)
    }

    fn handle_open_result(&mut self, result: protocol::OpenResult) {
        let mut channel = self.channels.get_mut(result.channel_id);
        tracing::debug!(
            channel_id = result.channel_id.0,
            key = %OfferKey::from(&channel.offer),
            result = result.status,
            "received open result"
        );

        let channel_opened = result.status == protocol::STATUS_SUCCESS as u32;
        let old_state = std::mem::replace(&mut channel.state, ChannelState::Offered);
        let ChannelState::Opening {
            redirected_event_flag,
            redirected_event,
            rpc,
        } = old_state
        else {
            tracing::warn!(
                key = %OfferKey::from(&channel.offer),
                old_state = ?channel.state,
                channel_opened,
                "invalid state for open result"
            );
            channel.state = old_state;
            return;
        };

        if !channel_opened {
            if let Some(event_flag) = redirected_event_flag {
                self.inner.synic.free_event_flag(event_flag);
            }
            rpc.fail(anyhow::anyhow!("open failed: {:#x}", result.status));
            return;
        }

        channel.state = ChannelState::Opened {
            redirected_event_flag,
            redirected_event,
        };

        rpc.complete(Ok(OpenOutput {
            redirected_event_flag,
        }));
    }

    fn handle_gpadl_torndown(&mut self, request: protocol::GpadlTorndown) -> TriedRelease {
        let Some(channel_id) = self.inner.teardown_gpadls.remove(&request.gpadl_id) else {
            panic!("gpadl {:#x} not in teardown list", request.gpadl_id.0);
        };

        let mut channel = self.channels.get_mut(channel_id);
        tracing::debug!(
            gpadl_id = request.gpadl_id.0,
            channel_id = channel_id.0,
            key = %OfferKey::from(&channel.offer),
            "Received GpadlTorndown"
        );

        let gpadl_state = channel
            .gpadls
            .remove(&request.gpadl_id)
            .expect("gpadl validated above");

        let GpadlState::TearingDown { rpcs } = gpadl_state else {
            panic!("gpadl should be tearing down if in teardown list, state = {gpadl_state:?}");
        };

        for rpc in rpcs {
            rpc.complete(());
        }
        channel.try_release(&mut self.inner.messages)
    }

    fn handle_unload_complete(&mut self) {
        match std::mem::replace(&mut self.state, ClientState::Disconnected) {
            ClientState::Disconnecting { version: _, rpc } => {
                tracing::info!("VmBus client disconnected");
                rpc.complete(());
            }
            state => {
                tracing::warn!(client_state = %state, "invalid client state for UnloadComplete");
            }
        }
    }

    fn handle_modify_complete(&mut self, response: protocol::ModifyConnectionResponse) {
        if let Some(request) = self.modify_request.take() {
            request.complete(response.connection_state)
        } else {
            tracing::warn!("Unexpected modify complete request");
        }
    }

    fn handle_modify_channel_response(
        &mut self,
        response: protocol::ModifyChannelResponse,
    ) -> TriedRelease {
        let mut channel = self.channels.get_mut(response.channel_id);
        let Some(sender) = channel.modify_response_send.take() else {
            panic!(
                "unexpected modify channel response for channel {:#x}",
                response.channel_id.0
            );
        };

        sender.complete(response.status);
        channel.try_release(&mut self.inner.messages)
    }

    fn handle_tl_connect_result(&mut self, response: protocol::TlConnectResult) {
        if let Some(rpc) = self.hvsock_tracker.check_result(&response) {
            rpc.complete(None);
        }
    }

    /// Returns false if the message was a pause complete message.
    fn handle_synic_message(&mut self, data: &[u8]) -> bool {
        let msg = Message::parse(data, self.state.get_version()).unwrap();
        tracing::trace!(?msg, "received client message from synic");

        match msg {
            Message::VersionResponse3(version_response, ..) => {
                // The client never sends the server-specified monitor pages feature flag, but
                // since version response messages are distinguished only by size, the response can
                // still look like `VersionResponse3` if the size was not set exactly by the server.
                // Since the feature flag can't be set, the extra data can be ignored.
                self.handle_version_response(version_response.version_response2);
            }
            Message::VersionResponse2(version_response, ..) => {
                self.handle_version_response(version_response);
            }
            Message::VersionResponse(version_response, ..) => {
                self.handle_version_response(version_response.into());
            }
            Message::OfferChannel(offer, ..) => {
                self.handle_offer(offer);
            }
            Message::AllOffersDelivered(..) => {
                self.handle_offers_delivered();
            }
            Message::UnloadComplete(..) => {
                self.handle_unload_complete();
            }
            Message::ModifyConnectionResponse(response, ..) => {
                self.handle_modify_complete(response);
            }
            Message::GpadlCreated(gpadl, ..) => {
                self.handle_gpadl_created(gpadl);
            }
            Message::OpenResult(result, ..) => {
                self.handle_open_result(result);
            }
            Message::GpadlTorndown(gpadl, ..) => {
                self.handle_gpadl_torndown(gpadl);
            }
            Message::RescindChannelOffer(rescind, ..) => {
                self.handle_rescind(rescind);
            }
            Message::ModifyChannelResponse(response, ..) => {
                self.handle_modify_channel_response(response);
            }
            Message::TlConnectResult(response, ..) => self.handle_tl_connect_result(response),
            // Unsupported messages.
            Message::CloseReservedChannelResponse(..) => {
                todo!("Unsupported message {msg:?}")
            }
            Message::PauseResponse(..) => {
                return false;
            }
            // Messages that should only be received by a vmbus server.
            Message::RequestOffers(..)
            | Message::OpenChannel2(..)
            | Message::OpenChannel(..)
            | Message::CloseChannel(..)
            | Message::GpadlHeader(..)
            | Message::GpadlBody(..)
            | Message::GpadlTeardown(..)
            | Message::RelIdReleased(..)
            | Message::InitiateContact(..)
            | Message::InitiateContact2(..)
            | Message::Unload(..)
            | Message::OpenReservedChannel(..)
            | Message::CloseReservedChannel(..)
            | Message::TlConnectRequest2(..)
            | Message::TlConnectRequest(..)
            | Message::ModifyChannel(..)
            | Message::ModifyConnection(..)
            | Message::Pause(..)
            | Message::Resume(..) => {
                unreachable!("Client received server message {msg:?}");
            }
        }
        true
    }

    fn handle_open_channel(
        &mut self,
        channel_id: ChannelId,
        rpc: FailableRpc<OpenRequest, OpenOutput>,
    ) {
        let mut channel = self.channels.get_mut(channel_id);
        match &channel.state {
            ChannelState::Offered => {}
            ChannelState::Revoked => {
                rpc.fail(anyhow::anyhow!("channel revoked"));
                return;
            }
            state => {
                rpc.fail(anyhow::anyhow!("invalid channel state: {}", state));
                return;
            }
        }

        tracing::info!(
            channel_id = channel_id.0,
            key = %OfferKey::from(&channel.offer),
            "opening channel on host"
        );

        let (request, rpc) = rpc.split();
        let open_data = &request.open_data;

        let supports_interrupt_redirection =
            if let ClientState::Connected { version, .. } = self.state {
                version.feature_flags.guest_specified_signal_parameters()
                    || version.feature_flags.channel_interrupt_redirection()
            } else {
                false
            };

        if !supports_interrupt_redirection && open_data.event_flag != channel_id.0 as u16 {
            rpc.fail(anyhow::anyhow!(
                "host does not support specifying the event flag"
            ));
            return;
        }

        let open_channel = protocol::OpenChannel {
            channel_id,
            open_id: 0,
            ring_buffer_gpadl_id: open_data.ring_gpadl_id,
            target_vp: open_data.target_vp,
            downstream_ring_buffer_page_offset: open_data.ring_offset,
            user_data: open_data.user_data,
        };

        let connection_id = if request.use_vtl2_connection_id {
            if !supports_interrupt_redirection {
                rpc.fail(anyhow::anyhow!(
                    "host does not support specfiying the connection ID"
                ));
                return;
            }
            protocol::ConnectionId::new(channel_id.0, 2.try_into().unwrap(), 7).0
        } else {
            open_data.connection_id
        };

        // No failure paths after the one for allocating the event flag, since
        // otherwise we would need to free the event flag.
        let mut flags = OpenChannelFlags::new();
        let event_flag = if let Some(event) = &request.incoming_event {
            if !supports_interrupt_redirection {
                rpc.fail(anyhow::anyhow!(
                    "host does not support redirecting interrupts"
                ));
                return;
            }

            flags.set_redirect_interrupt(true);
            match self.inner.synic.allocate_event_flag(event) {
                Ok(flag) => flag,
                Err(err) => {
                    rpc.fail(err.context("failed to allocate event flag"));
                    return;
                }
            }
        } else {
            open_data.event_flag
        };

        if supports_interrupt_redirection {
            self.inner.messages.send(&protocol::OpenChannel2 {
                open_channel,
                connection_id,
                event_flag,
                flags,
            });
        } else {
            self.inner.messages.send(&open_channel);
        }

        channel
            .connection_id
            .store(connection_id, Ordering::Release);
        channel.state = ChannelState::Opening {
            redirected_event_flag: (request.incoming_event.is_some()).then_some(event_flag),
            redirected_event: request.incoming_event,
            rpc,
        }
    }

    fn handle_restore_channel(
        &mut self,
        channel_id: ChannelId,
        request: RestoreRequest,
    ) -> Result<OpenOutput> {
        let mut channel = self.channels.get_mut(channel_id);
        if !matches!(channel.state, ChannelState::Restored) {
            anyhow::bail!("invalid channel state: {}", channel.state);
        }

        if request.incoming_event.is_some() != request.redirected_event_flag.is_some() {
            anyhow::bail!("incoming event and redirected event flag must both be set or unset");
        }

        if let Some((flag, event)) = request
            .redirected_event_flag
            .zip(request.incoming_event.as_ref())
        {
            self.inner.synic.restore_event_flag(flag, event)?;
        }

        channel
            .connection_id
            .store(request.connection_id, Ordering::Release);
        channel.state = ChannelState::Opened {
            redirected_event_flag: request.redirected_event_flag,
            redirected_event: request.incoming_event,
        };
        Ok(OpenOutput {
            redirected_event_flag: request.redirected_event_flag,
        })
    }

    fn handle_gpadl(&mut self, channel_id: ChannelId, rpc: FailableRpc<GpadlRequest, ()>) {
        let (request, rpc) = rpc.split();
        let mut channel = self.channels.get_mut(channel_id);
        if channel
            .gpadls
            .insert(request.id, GpadlState::Offered(rpc))
            .is_some()
        {
            panic!(
                "duplicate gpadl ID {:?} for channel {:?}.",
                request.id, channel_id
            );
        }

        tracing::trace!(
            channel_id = channel_id.0,
            key = %OfferKey::from(&channel.offer),
            gpadl_id = request.id.0,
            count = request.count,
            len = request.buf.len(),
            "received gpadl request"
        );

        // Split off the values that fit in the header.
        let (first, remaining) = if request.buf.len() > protocol::GpadlHeader::MAX_DATA_VALUES {
            request.buf.split_at(protocol::GpadlHeader::MAX_DATA_VALUES)
        } else {
            (request.buf.as_slice(), [].as_slice())
        };

        let message = protocol::GpadlHeader {
            channel_id,
            gpadl_id: request.id,
            len: (request.buf.len() * size_of::<u64>())
                .try_into()
                .expect("Too many GPA values"),
            count: request.count,
        };

        self.inner
            .messages
            .send_with_data(&message, first.as_bytes());

        // Send GpadlBody messages for the remaining values.
        let message = protocol::GpadlBody {
            rsvd: 0,
            gpadl_id: request.id,
        };
        for chunk in remaining.chunks(protocol::GpadlBody::MAX_DATA_VALUES) {
            self.inner
                .messages
                .send_with_data(&message, chunk.as_bytes());
        }
    }

    fn handle_gpadl_teardown(&mut self, channel_id: ChannelId, rpc: Rpc<GpadlId, ()>) {
        let (gpadl_id, rpc) = rpc.split();
        let mut channel = self.channels.get_mut(channel_id);
        let Some(gpadl_state) = channel.gpadls.get_mut(&gpadl_id) else {
            tracing::warn!(
                gpadl_id = gpadl_id.0,
                channel_id = channel_id.0,
                key = %OfferKey::from(&channel.offer),
                "Gpadl teardown for unknown gpadl or revoked channel"
            );
            return;
        };

        match gpadl_state {
            GpadlState::Offered(_) => {
                tracing::warn!(
                    gpadl_id = gpadl_id.0,
                    channel_id = channel_id.0,
                    key = %OfferKey::from(&channel.offer),
                    "gpadl teardown for offered gpadl"
                );
            }
            GpadlState::Created => {
                *gpadl_state = GpadlState::TearingDown { rpcs: vec![rpc] };
                // The caller must guarantee that GPADL teardown requests are only made
                // for unique GPADL IDs. This is currently enforced in vmbus_server by
                // blocking GPADL teardown messages for reserved channels.
                assert!(
                    self.inner
                        .teardown_gpadls
                        .insert(gpadl_id, channel_id)
                        .is_none(),
                    "Gpadl state validated above"
                );

                self.inner.messages.send(&protocol::GpadlTeardown {
                    channel_id,
                    gpadl_id,
                });
            }
            GpadlState::TearingDown { rpcs } => {
                rpcs.push(rpc);
            }
        }
    }

    fn handle_close_channel(&mut self, channel_id: ChannelId) {
        let mut channel = self.channels.get_mut(channel_id);
        self.inner.close_channel(channel_id, &mut channel);
    }

    fn handle_modify_channel(&mut self, channel_id: ChannelId, rpc: Rpc<ModifyRequest, i32>) {
        // The client doesn't support versions below Iron, so we always expect the host to send a
        // ModifyChannelResponse. This means we don't need to worry about sending a ChannelResponse
        // if that weren't supported.
        assert!(self.check_version(Version::Iron));
        let mut channel = self.channels.get_mut(channel_id);
        if channel.modify_response_send.is_some() {
            panic!("duplicate channel modify request {channel_id:?}");
        }

        let (request, response) = rpc.split();
        channel.modify_response_send = Some(response);
        let payload = match request {
            ModifyRequest::TargetVp { target_vp } => protocol::ModifyChannel {
                channel_id,
                target_vp,
            },
        };

        self.inner.messages.send(&payload);
    }

    fn handle_channel_request(&mut self, channel_id: ChannelId, request: ChannelRequest) {
        match request {
            ChannelRequest::Open(rpc) => self.handle_open_channel(channel_id, rpc),
            ChannelRequest::Restore(rpc) => {
                rpc.handle_failable_sync(|request| self.handle_restore_channel(channel_id, request))
            }
            ChannelRequest::Gpadl(req) => self.handle_gpadl(channel_id, req),
            ChannelRequest::TeardownGpadl(req) => self.handle_gpadl_teardown(channel_id, req),
            ChannelRequest::Close(req) => {
                req.handle_sync(|()| self.handle_close_channel(channel_id))
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

    /// Makes sure a channel is closed if the channel request stream was dropped.
    fn handle_device_removal(&mut self, channel_id: ChannelId) -> TriedRelease {
        let mut channel = self.channels.get_mut(channel_id);
        channel.is_client_released = true;
        // Close the channel if it is still open.
        if let ChannelState::Opened { .. } = channel.state {
            tracing::warn!(
                channel_id = channel_id.0,
                key = %OfferKey::from(&channel.offer),
                "Channel dropped without closing first"
            );
            self.inner.close_channel(channel_id, &mut channel);
        }
        channel.try_release(&mut self.inner.messages)
    }

    /// Determines if the client is connected with at least the specified version.
    fn check_version(&self, version: Version) -> bool {
        matches!(self.state, ClientState::Connected { version: v, .. } if v.version >= version)
    }

    fn handle_start(&mut self) {
        assert!(!self.running);
        self.msg_source.resume_message_stream();
        self.inner.messages.resume();
        self.running = true;
    }

    async fn handle_stop(&mut self) {
        assert!(self.running);

        loop {
            // Process messages until there are no more channels waiting for
            // responses. This is necessary to ensure that the saved state does
            // not have to support encoding revoked channels for which we are
            // waiting for GPADL or modify responses.
            while let Some((id, request)) = self.channels.revoked_channel_with_pending_request() {
                tracelimit::info_ratelimited!(
                    channel_id = id.0,
                    request,
                    "waiting for responses for channel"
                );
                assert!(self.process_next_message().await);
            }

            if self.can_pause_resume() {
                self.inner.messages.pause();
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
            if self
                .channels
                .revoked_channel_with_pending_request()
                .is_none()
            {
                break;
            }
            if !self.can_pause_resume() {
                self.msg_source.resume_message_stream();
            }
            self.inner.messages.resume();
        }

        tracing::debug!("messages drained");
        // Because the run loop awaits all async operations, there is no need for rundown.
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
        self.handle_synic_message(&buf[..size])
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
        if let ClientState::Connected { version, .. } = self.state {
            version.feature_flags.pause_resume()
        } else {
            false
        }
    }

    async fn run(&mut self) {
        let mut buf = [0; protocol::MAX_MESSAGE_SIZE];
        loop {
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
            let host_backed_up = !self.inner.messages.is_empty();
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

                            self.handle_synic_message(&buf[..size]);
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
}

impl ClientTaskInner {
    fn close_channel(&mut self, channel_id: ChannelId, channel: &mut Channel) {
        if let ChannelState::Opened {
            redirected_event_flag,
            ..
        } = channel.state
        {
            if let Some(flag) = redirected_event_flag {
                self.synic.free_event_flag(flag);
            }
            tracing::info!(
                channel_id = channel_id.0,
                key = %OfferKey::from(&channel.offer),
                "closing channel on host"
            );

            self.messages.send(&protocol::CloseChannel { channel_id });
            channel.state = ChannelState::Offered;
            channel.connection_id.store(0, Ordering::Release);
        } else {
            tracing::warn!(
                channel_id = channel_id.0,
                key = %OfferKey::from(&channel.offer),
                channel_state = %channel.state,
                "invalid channel state for close channel"
            );
        }
    }
}

#[derive(Debug, Inspect)]
#[inspect(external_tag)]
enum GpadlState {
    /// GpadlHeader has been sent to the host.
    Offered(#[inspect(skip)] FailableRpc<(), ()>),
    /// Host has responded with GpadlCreated.
    Created,
    /// GpadlTeardown message has been sent to the host.
    TearingDown {
        #[inspect(skip)]
        rpcs: Vec<Rpc<(), ()>>,
    },
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
    fn send<T: IntoBytes + protocol::VmbusMessage + std::fmt::Debug + Immutable + KnownLayout>(
        &mut self,
        msg: &T,
    ) {
        self.send_with_data(msg, &[])
    }

    fn send_with_data<
        T: IntoBytes + protocol::VmbusMessage + std::fmt::Debug + Immutable + KnownLayout,
    >(
        &mut self,
        msg: &T,
        data: &[u8],
    ) {
        tracing::trace!(typ = ?T::MESSAGE_TYPE, "Sending message to host");
        let msg = OutgoingMessage::with_data(msg, data);
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
                send(&OutgoingMessage::new(&protocol::Pause)).await;
                tracing::trace!("sent pause message");
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
        // Queue a resume message to be sent later.
        self.queued
            .push_front(OutgoingMessage::new(&protocol::Resume));
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
    #[inspect(with = "|x| inspect::iter_by_key(x).map_key(|id| id.0)")]
    teardown_gpadls: HashMap<GpadlId, ChannelId>,
    #[inspect(skip)]
    channel_requests: SelectAll<TaggedStream<ChannelId, mesh::Receiver<ChannelRequest>>>,
    synic: SynicState,
}

#[derive(Inspect)]
struct SynicState {
    #[inspect(skip)]
    event_client: Arc<dyn SynicEventClient>,
    #[inspect(iter_by_index)]
    event_flag_state: Vec<bool>,
}

#[derive(Inspect, Default)]
#[inspect(transparent)]
struct ChannelList(
    #[inspect(with = "|x| inspect::iter_by_key(x).map_key(|id| id.0)")] HashMap<ChannelId, Channel>,
);

/// A reference to a channel that can be used to remove the channel from the map
/// as well.
struct ChannelRef<'a>(hash_map::OccupiedEntry<'a, ChannelId, Channel>);

/// A tag value used to indicate that [`ChannelRef::try_release`] has been called.
/// This is useful as a return value for methods that might transition a channel
/// into a fully released state.
struct TriedRelease(());

impl ChannelRef<'_> {
    /// If the channel has been fully released (revoked, released by the client,
    /// no pending requests), notifies the server and removes this channel from
    /// the map.
    fn try_release(self, messages: &mut OutgoingMessages) -> TriedRelease {
        if self.is_client_released
            && matches!(self.state, ChannelState::Revoked)
            && self.pending_request().is_none()
        {
            let channel_id = *self.0.key();
            tracelimit::info_ratelimited!(
                channel_id = channel_id.0,
                key = %OfferKey::from(&self.offer),
                "releasing channel"
            );

            messages.send(&protocol::RelIdReleased { channel_id });
            self.0.remove();
        }
        TriedRelease(())
    }
}

impl Deref for ChannelRef<'_> {
    type Target = Channel;

    fn deref(&self) -> &Self::Target {
        self.0.get()
    }
}

impl DerefMut for ChannelRef<'_> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        self.0.get_mut()
    }
}

impl ChannelList {
    fn revoked_channel_with_pending_request(&self) -> Option<(ChannelId, &'static str)> {
        self.0.iter().find_map(|(&id, channel)| {
            if !matches!(channel.state, ChannelState::Revoked) {
                return None;
            }
            Some((id, channel.pending_request()?))
        })
    }

    #[track_caller]
    fn get_mut(&mut self, channel_id: ChannelId) -> ChannelRef<'_> {
        match self.0.entry(channel_id) {
            hash_map::Entry::Occupied(entry) => ChannelRef(entry),
            hash_map::Entry::Vacant(_) => {
                panic!("channel {:?} not found", channel_id);
            }
        }
    }
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

    const MAX_EVENT_FLAGS: u16 = 2047;

    fn allocate_event_flag(&mut self, event: &Event) -> Result<u16> {
        let i = self
            .event_flag_state
            .iter()
            .position(|&used| !used)
            .ok_or(())
            .or_else(|()| {
                if self.event_flag_state.len() >= Self::MAX_EVENT_FLAGS as usize {
                    anyhow::bail!("out of event flags");
                }
                self.event_flag_state.push(false);
                Ok(self.event_flag_state.len() - 1)
            })?;

        let event_flag = (i + 1) as u16;
        self.event_client
            .map_event(event_flag, event)
            .context("failed to map event")?;
        self.event_flag_state[i] = true;
        Ok(event_flag)
    }

    fn restore_event_flag(&mut self, flag: u16, event: &Event) -> Result<()> {
        let i = (flag as usize)
            .checked_sub(1)
            .context("invalid event flag")?;
        if i >= Self::MAX_EVENT_FLAGS as usize {
            anyhow::bail!("invalid event flag");
        }
        if self.event_flag_state.len() <= i {
            self.event_flag_state.resize(i + 1, false);
        }
        if self.event_flag_state[i] {
            anyhow::bail!("event flag already in use");
        }
        self.event_client
            .map_event(flag, event)
            .context("failed to map event")?;
        self.event_flag_state[i] = true;
        Ok(())
    }

    fn free_event_flag(&mut self, flag: u16) {
        let i = flag as usize - 1;
        assert!(i < self.event_flag_state.len());
        self.event_flag_state[i] = false;
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
                    target_vp: 0,
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
                    target_vp: 0,
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
                            target_vp: 0,
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
                        target_vp: 0,
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
    #[should_panic(expected = "channel should not exist")]
    async fn test_reoffer_in_use_rel_id(driver: DefaultDriver) {
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

        // This offer will cause a panic since the rel id is still in use.
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

        connection.offer_recv.next().await;
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
                    target_vp: 0,
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
