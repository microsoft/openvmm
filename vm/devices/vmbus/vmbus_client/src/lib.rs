// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

#![forbid(unsafe_code)]

mod hvsock;
mod saved_state;

pub use self::saved_state::SavedState;
use anyhow::Result;
use futures::future::OptionFuture;
use futures::stream::SelectAll;
use futures::FutureExt;
use futures::StreamExt;
use guid::Guid;
use inspect::Inspect;
use mesh::rpc::Rpc;
use mesh::rpc::RpcSend;
use pal_async::task::Spawn;
use pal_async::task::Task;
use std::collections::HashMap;
use std::convert::TryInto;
use std::future::Future;
use std::sync::Arc;
use thiserror::Error;
use vmbus_async::async_dgram::AsyncRecv;
use vmbus_async::async_dgram::AsyncRecvExt;
use vmbus_channel::bus::GpadlRequest;
use vmbus_channel::bus::ModifyRequest;
use vmbus_channel::bus::OpenData;
use vmbus_channel::gpadl::GpadlId;
use vmbus_core::protocol;
use vmbus_core::protocol::ChannelId;
use vmbus_core::protocol::ConnectionState;
use vmbus_core::protocol::FeatureFlags;
use vmbus_core::protocol::Message;
use vmbus_core::protocol::OpenChannelFlags;
use vmbus_core::protocol::Version;
use vmbus_core::HvsockConnectRequest;
use vmbus_core::MonitorPageGpas;
use vmbus_core::OutgoingMessage;
use vmbus_core::TaggedStream;
use vmbus_core::VersionInfo;
use zerocopy::Immutable;
use zerocopy::IntoBytes;
use zerocopy::KnownLayout;

const SINT: u8 = 2;
const VTL: u8 = 0;
const SUPPORTED_VERSIONS: &[Version] = &[Version::Iron, Version::Copper];
const SUPPORTED_FEATURE_FLAGS: FeatureFlags = FeatureFlags::all();

/// The client interface to the synic.
pub trait SynicClient: Send + Sync {
    fn post_message(&self, connection_id: u32, typ: u32, msg: &[u8]);
    /// Maps an incoming event signal on SINT7 to `event`.
    fn map_event(&self, event_flag: u16, event: &pal_event::Event) -> std::io::Result<()>;
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

pub struct VmbusClient {
    task_send: mesh::Sender<TaskRequest>,
    access: VmbusClientAccess,
    _thread: Task<()>,
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
    client_request_send: Arc<mesh::Sender<ClientRequest>>,
}

impl VmbusClient {
    /// Creates a new instance with a receiver for incoming synic messages.
    pub fn new(
        synic: Arc<dyn SynicClient>,
        offer_send: mesh::Sender<OfferInfo>,
        msg_source: impl VmbusMessageSource + 'static,
        spawner: &impl Spawn,
    ) -> Self {
        let (task_send, task_recv) = mesh::channel();
        let (client_request_send, client_request_recv) = mesh::channel();

        let inner = ClientTaskInner {
            synic,
            channels: HashMap::new(),
            gpadls: HashMap::new(),
            teardown_gpadls: HashMap::new(),
            channel_requests: SelectAll::new(),
        };

        let mut task = ClientTask {
            inner,
            task_recv,
            running: false,
            offer_send,
            msg_source,
            client_request_recv,
            state: ClientState::Disconnected,
            modify_request: None,
            hvsock_tracker: hvsock::HvsockRequestTracker::new(),
        };

        let thread = spawner.spawn("vmbus client", async move { task.run().await });

        Self {
            access: VmbusClientAccess {
                client_request_send: Arc::new(client_request_send),
            },
            task_send,
            _thread: thread,
        }
    }

    /// Send the InitiateContact message to the server.
    pub async fn connect(
        &mut self,
        target_message_vp: u32,
        monitor_page: Option<MonitorPageGpas>,
        client_id: Guid,
    ) -> Result<VersionInfo, ConnectError> {
        let request = InitiateContactRequest {
            target_message_vp,
            monitor_page,
            client_id,
        };

        self.access
            .client_request_send
            .call(ClientRequest::InitiateContact, request)
            .await
            .unwrap()
    }

    /// Send the RequestOffers message to the server, providing a sender to
    /// which the client can forward received offers to.
    pub async fn request_offers(&mut self) -> Vec<OfferInfo> {
        let (send, recv) = mesh::channel();
        self.access
            .client_request_send
            .send(ClientRequest::RequestOffers(send));
        recv.collect().await
    }

    /// Send the Unload message to the server.
    pub async fn unload(&mut self) {
        self.access
            .client_request_send
            .call(ClientRequest::Unload, ())
            .await
            .unwrap();
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
    ) -> Result<(Option<VersionInfo>, Vec<RestoredChannel>), RestoreError> {
        self.task_send
            .call(TaskRequest::Restore, state)
            .await
            .expect("Failed to send restore request")
    }
}

impl Inspect for VmbusClient {
    fn inspect(&self, req: inspect::Request<'_>) {
        self.task_send.send(TaskRequest::Inspect(req.defer()));
    }
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
    pub flags: OpenChannelFlags,
}

/// Expresses an operation requested of the client.
pub enum ChannelRequest {
    Open(Rpc<OpenRequest, bool>),
    Close,
    Gpadl(Rpc<GpadlRequest, bool>),
    TeardownGpadl(GpadlId),
    Modify(Rpc<ModifyRequest, i32>),
}

impl std::fmt::Display for ChannelRequest {
    fn fmt(&self, fmt: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ChannelRequest::Open(_) => write!(fmt, "Open"),
            ChannelRequest::Close => write!(fmt, "Close"),
            ChannelRequest::Gpadl(_) => write!(fmt, "Gpadl"),
            ChannelRequest::TeardownGpadl(_) => write!(fmt, "TeardownGpadl"),
            ChannelRequest::Modify(_) => write!(fmt, "Modify"),
        }
    }
}

/// Expresses a response sent from the server.
#[derive(Debug)]
pub enum ChannelResponse {
    TeardownGpadl(GpadlId),
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
}

/// Provides the offer details from the server in addition to both a channel
/// to request client actions and a channel to receive server responses.
#[derive(Debug, Inspect)]
pub struct OfferInfo {
    pub offer: protocol::OfferChannel,
    #[inspect(skip)]
    pub request_send: mesh::Sender<ChannelRequest>,
    #[inspect(skip)]
    pub response_recv: mesh::Receiver<ChannelResponse>,
}

#[derive(Debug)]
enum ClientRequest {
    InitiateContact(Rpc<InitiateContactRequest, Result<VersionInfo, ConnectError>>),
    RequestOffers(mesh::Sender<OfferInfo>),
    Unload(Rpc<(), ()>),
    Modify(Rpc<ModifyConnectionRequest, ConnectionState>),
    HvsockConnect(Rpc<HvsockConnectRequest, Option<OfferInfo>>),
}

impl std::fmt::Display for ClientRequest {
    fn fmt(&self, fmt: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ClientRequest::InitiateContact(..) => write!(fmt, "InitiateContact"),
            ClientRequest::RequestOffers { .. } => write!(fmt, "RequestOffers"),
            ClientRequest::Unload { .. } => write!(fmt, "Unload"),
            ClientRequest::Modify(..) => write!(fmt, "Modify"),
            ClientRequest::HvsockConnect(..) => write!(fmt, "HvsockConnect"),
        }
    }
}

enum TaskRequest {
    Inspect(inspect::Deferred),
    Save(Rpc<(), SavedState>),
    Restore(Rpc<SavedState, Result<(Option<VersionInfo>, Vec<RestoredChannel>), RestoreError>>),
    Start,
    Stop(Rpc<(), ()>),
}

/// Information about a restored channel.
#[derive(Debug)]
pub struct RestoredChannel {
    /// The channel offer.
    pub offer: OfferInfo,
    /// Whether the channel was open at save time.
    pub open: bool,
}

/// The overall state machine used to drive which actions the client can legally
/// take. This primarily pertains to overall client activity but has a
/// side-effect of limiting whether or not channels can perform actions.
enum ClientState {
    /// The client has yet to connect to the server.
    Disconnected,
    /// The client has initiated contact with the server.
    Connecting(
        Version,
        Rpc<InitiateContactRequest, Result<VersionInfo, ConnectError>>,
    ),
    /// The client has negotiated the protocol version with the server.
    Connected(VersionInfo),
    /// The client has requested offers from the server.
    RequestingOffers(VersionInfo, mesh::Sender<OfferInfo>),
    /// The client has initiated an unload from the server.
    Disconnecting(VersionInfo, Rpc<(), ()>),
}

impl ClientState {
    fn get_version(&self) -> Option<VersionInfo> {
        match self {
            ClientState::Connected(version) => Some(*version),
            ClientState::RequestingOffers(version, _) => Some(*version),
            ClientState::Disconnecting(version, _) => Some(*version),
            ClientState::Disconnected | ClientState::Connecting(..) => None,
        }
    }
}

impl std::fmt::Display for ClientState {
    fn fmt(&self, fmt: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ClientState::Disconnected => write!(fmt, "Disconnected"),
            ClientState::Connecting(..) => write!(fmt, "Connecting"),
            ClientState::Connected(_) => write!(fmt, "Connected"),
            ClientState::RequestingOffers(..) => write!(fmt, "RequestingOffers"),
            ClientState::Disconnecting(..) => write!(fmt, "Disconnecting"),
        }
    }
}

#[derive(Copy, Clone, Debug, Default)]
pub struct InitiateContactRequest {
    pub target_message_vp: u32,
    pub monitor_page: Option<MonitorPageGpas>,
    pub client_id: Guid,
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
#[derive(Debug)]
enum ChannelState {
    /// The channel has been offered to the client.
    Offered,
    /// The channel has requested the server to be opened.
    Opening(Rpc<(), bool>),
    /// The channel has been successfully opened.
    Opened,
}

impl std::fmt::Display for ChannelState {
    fn fmt(&self, fmt: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ChannelState::Opening(..) => write!(fmt, "Opening"),
            ChannelState::Offered => write!(fmt, "Offered"),
            ChannelState::Opened => write!(fmt, "Opened"),
        }
    }
}

struct Channel {
    offer: protocol::OfferChannel,
    response_send: mesh::Sender<ChannelResponse>,
    state: ChannelState,
    modify_response_send: Option<Rpc<(), i32>>,
}

impl std::fmt::Debug for Channel {
    fn fmt(&self, fmt: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        fmt.debug_struct("Channel")
            .field("offer", &self.offer)
            .field("state", &self.state)
            .finish()
    }
}

impl Channel {
    /// Convert the `interface_id` of this channel to a human readable string.
    fn interface_id_to_string(&self) -> &str {
        // TODO: There doesn't exist a single crate that has all these interface
        // IDs. Today they're defined in each individual crate, but we don't
        // want to include all those crates as dependencies here.
        //
        // In the future, it might make sense to have a common protocol crate
        // that has all of these defined, but for now just redefine the most
        // common ones here. Add more as needed.

        const SHUTDOWN_IC: Guid = Guid::from_static_str("0e0b6031-5213-4934-818b-38d90ced39db");
        const KVP_IC: Guid = Guid::from_static_str("a9a0f4e7-5a45-4d96-b827-8a841e8c03e6");
        const VSS_IC: Guid = Guid::from_static_str("35fa2e29-ea23-4236-96ae-3a6ebacba440");
        const TIMESYNC_IC: Guid = Guid::from_static_str("9527e630-d0ae-497b-adce-e80ab0175caf");
        const HEARTBEAT_IC: Guid = Guid::from_static_str("57164f39-9115-4e78-ab55-382f3bd5422d");
        const RDV_IC: Guid = Guid::from_static_str("276aacf4-ac15-426c-98dd-7521ad3f01fe");

        const INHERITED_ACTIVATION: Guid =
            Guid::from_static_str("3375baf4-9e15-4b30-b765-67acb10d607b");

        const NET: Guid = Guid::from_static_str("f8615163-df3e-46c5-913f-f2d2f965ed0e");
        const SCSI: Guid = Guid::from_static_str("ba6163d9-04a1-4d29-b605-72e2ffb1dc7f");
        const VPCI: Guid = Guid::from_static_str("44c4f61d-4444-4400-9d52-802e27ede19f");

        match self.offer.interface_id {
            SHUTDOWN_IC => "shutdown_ic",
            KVP_IC => "kvp_ic",
            VSS_IC => "vss_ic",
            TIMESYNC_IC => "timesync_ic",
            HEARTBEAT_IC => "heartbeat_ic",
            RDV_IC => "rdv_ic",
            INHERITED_ACTIVATION => "inherited_activation",
            NET => "net",
            SCSI => "scsi",
            VPCI => "vpci",
            _ => "unknown",
        }
    }

    fn inspect(&self, resp: &mut inspect::Response<'_>) {
        resp.display("state", &self.state)
            .field_with("interface_name", || self.interface_id_to_string())
            .display("instance_id", &self.offer.instance_id)
            .display("interface_id", &self.offer.interface_id)
            .field("mmio_megabytes", self.offer.mmio_megabytes)
            .field("monitor_allocated", self.offer.monitor_allocated != 0)
            .field("monitor_id", self.offer.monitor_id)
            .field("connection_id", self.offer.connection_id)
            .field("is_dedicated", self.offer.is_dedicated != 0);
    }
}

struct ClientTask<T: VmbusMessageSource> {
    inner: ClientTaskInner,
    state: ClientState,
    hvsock_tracker: hvsock::HvsockRequestTracker,
    running: bool,
    modify_request: Option<Rpc<ModifyConnectionRequest, ConnectionState>>,
    msg_source: T,
    offer_send: mesh::Sender<OfferInfo>,
    task_recv: mesh::Receiver<TaskRequest>,
    client_request_recv: mesh::Receiver<ClientRequest>,
}

impl<T: VmbusMessageSource> ClientTask<T> {
    fn handle_initiate_contact(
        &mut self,
        rpc: Rpc<InitiateContactRequest, Result<VersionInfo, ConnectError>>,
        version: Version,
    ) {
        if let ClientState::Disconnected = self.state {
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

            self.state = ClientState::Connecting(version, rpc);
            if version < Version::Copper {
                self.inner.send(&msg.initiate_contact)
            } else {
                self.inner.send(&msg);
            }
        } else {
            tracing::warn!(client_state = %self.state, "invalid client state for InitiateContact");
            rpc.complete(Err(ConnectError::InvalidState));
        }
    }

    fn handle_request_offers(&mut self, send: mesh::Sender<OfferInfo>) {
        if let ClientState::Connected(version) = self.state {
            self.state = ClientState::RequestingOffers(version, send);
            self.inner.send(&protocol::RequestOffers {});
        } else {
            tracing::warn!(client_state = %self.state, "invalid client state for RequestOffers");
        }
    }

    fn handle_unload(&mut self, rpc: Rpc<(), ()>) {
        tracing::debug!(%self.state, "VmBus client disconnecting");
        self.state = ClientState::Disconnecting(
            self.state.get_version().expect("invalid state for unload"),
            rpc,
        );

        self.inner.send(&protocol::Unload {});
    }

    fn handle_modify(&mut self, request: Rpc<ModifyConnectionRequest, ConnectionState>) {
        if !matches!(self.state, ClientState::Connected(version) if version.feature_flags.modify_connection())
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
        self.inner.send(&message);
    }

    fn handle_tl_connect(&mut self, rpc: Rpc<HvsockConnectRequest, Option<OfferInfo>>) {
        // The client only supports protocol versions which use the newer message format.
        // The host will not send a TlConnectRequestResult message on success, so a response to this
        // message is not guaranteed.
        let message = protocol::TlConnectRequest2::from(*rpc.input());
        self.hvsock_tracker.add_request(rpc);
        self.inner.send(&message);
    }

    fn handle_client_request(&mut self, request: ClientRequest) {
        match request {
            ClientRequest::InitiateContact(rpc) => {
                self.handle_initiate_contact(rpc, *SUPPORTED_VERSIONS.last().unwrap());
            }
            ClientRequest::RequestOffers(send) => {
                self.handle_request_offers(send);
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
        if let ClientState::Connecting(version, rpc) = old_state {
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

                self.state = ClientState::Connected(version);
                tracing::info!(?version, "VmBus client connected");
                rpc.complete(Ok(version));
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
        } else {
            self.state = old_state;
            tracing::warn!(client_state = %self.state, "invalid client state to handle VersionResponse");
        }
    }

    fn create_channel(&mut self, offer: protocol::OfferChannel) -> Option<OfferInfo> {
        self.create_channel_core(offer, ChannelState::Offered)
    }

    fn create_channel_core(
        &mut self,
        offer: protocol::OfferChannel,
        state: ChannelState,
    ) -> Option<OfferInfo> {
        if let Some(channel) = self.inner.channels.get_mut(&offer.channel_id) {
            channel.state = ChannelState::Offered;
            tracing::debug!(channel_id = %offer.channel_id.0, "client channel exists");
            return None;
        }
        let (request_send, request_recv) = mesh::channel();
        let (response_send, response_recv) = mesh::channel();

        self.inner.channels.insert(
            offer.channel_id,
            Channel {
                response_send,
                offer,
                state,
                modify_response_send: None,
            },
        );

        self.inner
            .channel_requests
            .push(TaggedStream::new(offer.channel_id, request_recv));

        Some(OfferInfo {
            offer,
            response_recv,
            request_send,
        })
    }

    fn handle_offer(&mut self, offer: protocol::OfferChannel) {
        if let Some(offer_info) = self.create_channel(offer) {
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
                if let ClientState::RequestingOffers(_, send) = &self.state {
                    send.send(offer_info);
                } else {
                    self.offer_send.send(offer_info);
                }
            }
        }
    }

    fn handle_rescind(&mut self, rescind: protocol::RescindChannelOffer) {
        tracing::info!(state = %self.state, channel_id = rescind.channel_id.0, "received rescind");

        let channel = &self.inner.channels[&rescind.channel_id];

        // Teardown all remaining gpadls for this channel. We don't care about GpadlTorndown
        // responses at this point.
        self.inner
            .gpadls
            .retain(|&(channel_id, gpadl_id), gpadl_state| {
                if channel_id != rescind.channel_id {
                    return true;
                }

                // If the gpadl was already tearing down, send a response now.
                if matches!(gpadl_state, GpadlState::TearingDown) {
                    channel
                        .response_send
                        .send(ChannelResponse::TeardownGpadl(gpadl_id));
                } else {
                    // TODO: is this really necessary? The host should have
                    // already unmapped all GPADLs. Remove if possible.
                    send_message(
                        self.inner.synic.as_ref(),
                        &protocol::GpadlTeardown {
                            channel_id,
                            gpadl_id,
                        },
                        &[],
                    );
                }

                self.inner.teardown_gpadls.insert(gpadl_id, None);

                false
            });

        // Drop the channel, which will close the response channel, which will
        // cause the client to know the channel has been revoked.
        //
        // TODO: this is wrong--client requests can still come in after this,
        // and they will fail to find the channel by channel ID and panic (or
        // worse, the channel ID will get reused). Either find and drop the
        // associated incoming request channel here, or keep this channel object
        // around until the client is done with it.
        self.inner.channels.remove(&rescind.channel_id);

        // Tell the host we're not referencing the client ID anymore.
        self.inner.send(&protocol::RelIdReleased {
            channel_id: rescind.channel_id,
        });
    }

    fn handle_offers_delivered(&mut self) {
        if let ClientState::RequestingOffers(version, _send) = &self.state {
            // This will drop the sender and cause the client to know the offers are done.
            self.state = ClientState::Connected(*version);
        } else {
            tracing::warn!(client_state = %self.state, "invalid client state to handle AllOffersDelivered");
        }
    }

    fn handle_gpadl_created(&mut self, request: protocol::GpadlCreated) {
        let Some(gpadl_state) = self
            .inner
            .gpadls
            .get_mut(&(request.channel_id, request.gpadl_id))
        else {
            tracing::warn!(
                gpadl_id = request.gpadl_id.0,
                "GpadlCreated for unknown gpadl"
            );

            return;
        };

        if !matches!(gpadl_state, GpadlState::Offered(..)) {
            tracing::warn!(
                gpadl_id = request.gpadl_id.0,
                channel_id = request.channel_id.0,
                ?gpadl_state,
                "Invalid state for GpadlCreated"
            );

            return;
        };

        let gpadl_created = request.status == protocol::STATUS_SUCCESS;
        let old_state = if gpadl_created {
            std::mem::replace(gpadl_state, GpadlState::Created)
        } else {
            self.inner
                .gpadls
                .remove(&(request.channel_id, request.gpadl_id))
                .unwrap()
        };

        let GpadlState::Offered(sender) = old_state else {
            unreachable!("validated above");
        };

        sender.complete(gpadl_created)
    }

    fn handle_open_result(&mut self, result: protocol::OpenResult) {
        tracing::debug!(
            channel_id = result.channel_id.0,
            result = result.status,
            "received open result"
        );

        let channel = self
            .inner
            .channels
            .get_mut(&result.channel_id)
            .expect("channel should exist");

        let channel_opened = result.status == protocol::STATUS_SUCCESS as u32;
        let new_state = if channel_opened {
            ChannelState::Opened
        } else {
            ChannelState::Offered
        };

        // Even if the old state is wrong, we still update to the state the host thinks we're in.
        let old_state = std::mem::replace(&mut channel.state, new_state);
        let ChannelState::Opening(rpc) = old_state else {
            tracing::warn!(?old_state, channel_opened, "invalid state for open result");
            return;
        };

        rpc.complete(channel_opened);
    }

    fn handle_gpadl_torndown(&mut self, request: protocol::GpadlTorndown) {
        let channel_id = match self.inner.teardown_gpadls.remove(&request.gpadl_id) {
            Some(Some(channel_id)) => channel_id,
            Some(None) => {
                tracing::debug!(
                    gpadl_id = request.gpadl_id.0,
                    "GpadlTorndown for gpadl torn down by rescind"
                );
                return;
            }
            None => {
                tracing::warn!(
                    gpadl_id = request.gpadl_id.0,
                    "Unknown ID or invalid state for GpadlTorndown"
                );
                return;
            }
        };

        tracing::debug!(
            gpadl_id = request.gpadl_id.0,
            channel_id = channel_id.0,
            "Received GpadlTorndown"
        );

        let gpadl_state = self
            .inner
            .gpadls
            .remove(&(channel_id, request.gpadl_id))
            .expect("gpadl validated above");

        assert!(
            matches!(gpadl_state, GpadlState::TearingDown),
            "gpadl should be tearing down if in teardown list, state = {gpadl_state:?}"
        );

        let channel = &self.inner.channels[&channel_id];

        channel
            .response_send
            .send(ChannelResponse::TeardownGpadl(request.gpadl_id));
    }

    fn handle_unload_complete(&mut self) {
        match std::mem::replace(&mut self.state, ClientState::Disconnected) {
            ClientState::Disconnecting(_, rpc) => {
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

    fn handle_modify_channel_response(&mut self, response: protocol::ModifyChannelResponse) {
        let Some(sender) = self
            .inner
            .channels
            .get_mut(&response.channel_id)
            .expect("modify response for unknown channel")
            .modify_response_send
            .take()
        else {
            tracing::warn!(
                channel_id = response.channel_id.0,
                "unexpected modify channel response"
            );
            return;
        };

        sender.complete(response.status);
    }

    fn handle_tl_connect_result(&mut self, response: protocol::TlConnectResult) {
        if let Some(rpc) = self.hvsock_tracker.check_result(&response) {
            rpc.complete(None);
        }
    }

    fn handle_synic_message(&mut self, data: &[u8]) {
        let msg = Message::parse(data, self.state.get_version()).unwrap();
        tracing::trace!(?msg, "received client message from synic");

        match msg {
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
                self.handle_modify_channel_response(response)
            }
            Message::TlConnectResult(response, ..) => self.handle_tl_connect_result(response),
            // Unsupported messages.
            Message::CloseReservedChannelResponse(..) => {
                todo!("Unsupported message {msg:?}")
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
            | Message::ModifyConnection(..) => {
                unreachable!("Client received server message {msg:?}");
            }
        }
    }

    fn handle_open_channel(&mut self, channel_id: ChannelId, rpc: Rpc<OpenRequest, bool>) {
        let channel = self
            .inner
            .channels
            .get_mut(&channel_id)
            .expect("invalid channel");

        if !matches!(channel.state, ChannelState::Offered) {
            tracing::warn!(id = %channel_id.0, channel_state = %self.inner.channel_state(channel_id).unwrap(), "invalid channel state for OpenChannel");
            rpc.complete(false);
            return;
        }

        tracing::info!(channel_id = channel_id.0, "opening channel on host");
        let request = rpc.input();
        let open_data = &request.open_data;

        let open_channel = protocol::OpenChannel {
            channel_id,
            open_id: 0,
            ring_buffer_gpadl_id: open_data.ring_gpadl_id,
            target_vp: open_data.target_vp,
            downstream_ring_buffer_page_offset: open_data.ring_offset,
            user_data: open_data.user_data,
        };

        if matches!(self.state, ClientState::Connected(version) if version.feature_flags.guest_specified_signal_parameters() || version.feature_flags.channel_interrupt_redirection())
        {
            // N.B. The open_data will contain the server's event
            // flag/connection ID if the VTL0 guest doesn't use alternate
            // values (it normally won't), so we can communicate those to
            // the host if they differ.
            self.inner.send(&protocol::OpenChannel2 {
                open_channel,
                connection_id: open_data.connection_id,
                event_flag: open_data.event_flag,
                flags: request.flags.into(),
            });
        } else {
            assert_eq!(
                open_data.event_flag, channel_id.0 as u16,
                "Trying to use guest-specified event flag when the host doesn't support it."
            );

            self.inner.send(&open_channel);
        }

        self.inner.channels.get_mut(&channel_id).unwrap().state =
            ChannelState::Opening(rpc.split().1);
    }

    fn handle_gpadl(&mut self, channel_id: ChannelId, rpc: Rpc<GpadlRequest, bool>) {
        let (request, rpc) = rpc.split();
        if self
            .inner
            .gpadls
            .insert((channel_id, request.id), GpadlState::Offered(rpc))
            .is_some()
        {
            panic!(
                "duplicate gpadl ID {:?} for channel {:?}.",
                request.id, channel_id
            );
        }

        tracing::trace!(
            channel_id = channel_id.0,
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

        self.inner.send_with_data(&message, first.as_bytes());

        // Send GpadlBody messages for the remaining values.
        let message = protocol::GpadlBody {
            rsvd: 0,
            gpadl_id: request.id,
        };
        for chunk in remaining.chunks(protocol::GpadlBody::MAX_DATA_VALUES) {
            self.inner.send_with_data(&message, chunk.as_bytes());
        }
    }

    fn handle_gpadl_teardown(&mut self, channel_id: ChannelId, gpadl_id: GpadlId) {
        let Some(gpadl_state) = self.inner.gpadls.get_mut(&(channel_id, gpadl_id)) else {
            tracing::warn!(
                gpadl_id = gpadl_id.0,
                channel_id = channel_id.0,
                "Gpadl teardown for unknown gpadl or revoked channel"
            );
            return;
        };

        if matches!(gpadl_state, GpadlState::TearingDown) {
            tracing::warn!(
                gpadl_id = gpadl_id.0,
                channel_id = channel_id.0,
                "Gpadl already tearing down"
            );
            return;
        }

        *gpadl_state = GpadlState::TearingDown;
        // The caller must guarantee that GPADL teardown requests are only made
        // for unique GPADL IDs. This is currently enforced in vmbus_server by
        // blocking GPADL teardown messages for reserved channels.
        assert!(
            self.inner
                .teardown_gpadls
                .insert(gpadl_id, Some(channel_id))
                .is_none(),
            "Gpadl state validated above"
        );

        self.inner.send(&protocol::GpadlTeardown {
            channel_id,
            gpadl_id,
        });
    }

    fn handle_close_channel(&mut self, channel_id: ChannelId) {
        if let ChannelState::Opened = self.inner.channel_state(channel_id).unwrap() {
            tracing::info!(channel_id = channel_id.0, "closing channel on host");
            self.inner.send(&protocol::CloseChannel { channel_id });
            self.inner.channels.get_mut(&channel_id).unwrap().state = ChannelState::Offered;
        } else {
            tracing::warn!(id = %channel_id.0, channel_state = %self.inner.channel_state(channel_id).unwrap(), "invalid channel state for close channel");
        }
    }

    fn handle_modify_channel(&mut self, channel_id: ChannelId, rpc: Rpc<ModifyRequest, i32>) {
        // The client doesn't support versions below Iron, so we always expect the host to send a
        // ModifyChannelResponse. This means we don't need to worry about sending a ChannelResponse
        // if that weren't supported.
        assert!(self.check_version(Version::Iron));
        let channel = self
            .inner
            .channels
            .get_mut(&channel_id)
            .unwrap_or_else(|| panic!("modify request for unknown channel {channel_id:?}"));

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

        self.inner.send(&payload);
    }

    fn handle_channel_request(&mut self, channel_id: ChannelId, request: ChannelRequest) {
        if let Some(state) = self.inner.channel_state(channel_id) {
            tracing::trace!(id = %channel_id.0, request = %request, %state, "received client request");
        } else {
            tracing::warn!(id = %channel_id.0, request = %request, "received client request for unknown channel");
            return;
        }

        match request {
            ChannelRequest::Open(rpc) => self.handle_open_channel(channel_id, rpc),
            ChannelRequest::Gpadl(req) => self.handle_gpadl(channel_id, req),
            ChannelRequest::TeardownGpadl(req) => self.handle_gpadl_teardown(channel_id, req),
            ChannelRequest::Close => self.handle_close_channel(channel_id),
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
            TaskRequest::Start => self.handle_start(),
            TaskRequest::Stop(rpc) => rpc.handle(|()| self.handle_stop()).await,
        }
    }

    /// Makes sure a channel is closed if the channel request stream was dropped.
    fn handle_device_removal(&mut self, channel_id: ChannelId) {
        if let Some(ChannelState::Opened) = self.inner.channel_state(channel_id) {
            tracing::warn!(
                channel_id = channel_id.0,
                "Channel dropped without closing first"
            );

            self.handle_close_channel(channel_id);
        }
    }

    /// Determines if the client is connected with at least the specified version.
    fn check_version(&self, version: Version) -> bool {
        matches!(self.state, ClientState::Connected(v) if v.version >= version)
    }

    fn handle_start(&mut self) {
        assert!(!self.running);
        self.msg_source.resume_message_stream();
        self.running = true;
    }

    async fn handle_stop(&mut self) {
        assert!(self.running);
        self.msg_source.pause_message_stream();

        // Process messages until we hit EOF.
        tracing::debug!("draining messages");
        let mut buf = [0; protocol::MAX_MESSAGE_SIZE];
        loop {
            let size = self
                .msg_source
                .recv(&mut buf)
                .await
                .expect("Fatal error reading messages from synic");

            if size == 0 {
                break;
            }

            self.handle_synic_message(&buf[..size]);
        }

        tracing::debug!("messages drained");
        // Because the run loop awaits all async operations, there is no need for rundown.
        self.running = false;
    }

    async fn run(&mut self) {
        let mut buf = [0; protocol::MAX_MESSAGE_SIZE];
        loop {
            let mut message_recv =
                OptionFuture::from(self.running.then(|| self.msg_source.recv(&mut buf).fuse()));

            let mut client_request_recv =
                OptionFuture::from(self.running.then(|| self.client_request_recv.next()));

            let mut channel_requests = OptionFuture::from(
                self.running
                    .then(|| self.inner.channel_requests.select_next_some()),
            );

            futures::select! { // merge semantics
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
                        (id, _) => self.handle_device_removal(id),
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

impl<T: VmbusMessageSource> Inspect for ClientTask<T> {
    fn inspect(&self, req: inspect::Request<'_>) {
        let mut resp = req.respond();
        resp.display("state", &self.state);
        let version = match self.state {
            ClientState::Connected(version) => Some(version),
            ClientState::RequestingOffers(version, ..) => Some(version),
            _ => None,
        };

        if let Some(version) = version {
            resp.field(
                "protocol",
                format!(
                    "{}.{}",
                    version.version as u32 >> 16,
                    version.version as u32 & 0xffff
                ),
            );
            resp.binary("feature_flags", u32::from(version.feature_flags));
        }

        for (id, channel) in self.inner.channels.iter() {
            resp.child(&channel.offer.instance_id.to_string(), |req| {
                let mut resp = req.respond();
                resp.field("id", id.0);
                channel.inspect(&mut resp);
            });
        }
    }
}

#[derive(Debug)]
enum GpadlState {
    /// GpadlHeader has been sent to the host.
    Offered(Rpc<(), bool>),
    /// Host has responded with GpadlCreated.
    Created,
    /// GpadlTeardown message has been sent to the host.
    TearingDown,
}

struct ClientTaskInner {
    synic: Arc<dyn SynicClient>,
    channels: HashMap<ChannelId, Channel>,
    gpadls: HashMap<(ChannelId, GpadlId), GpadlState>,
    teardown_gpadls: HashMap<GpadlId, Option<ChannelId>>,
    channel_requests: SelectAll<TaggedStream<ChannelId, mesh::Receiver<ChannelRequest>>>,
}

impl ClientTaskInner {
    fn send<T: IntoBytes + protocol::VmbusMessage + std::fmt::Debug + Immutable + KnownLayout>(
        &self,
        msg: &T,
    ) {
        send_message(self.synic.as_ref(), msg, &[])
    }

    fn send_with_data<
        T: IntoBytes + protocol::VmbusMessage + std::fmt::Debug + Immutable + KnownLayout,
    >(
        &self,
        msg: &T,
        data: &[u8],
    ) {
        send_message(self.synic.as_ref(), msg, data)
    }

    fn channel_state(&self, channel_id: ChannelId) -> Option<&ChannelState> {
        self.channels.get(&channel_id).map(|c| &c.state)
    }
}

fn send_message<
    T: IntoBytes + protocol::VmbusMessage + std::fmt::Debug + Immutable + KnownLayout,
>(
    synic: &dyn SynicClient,
    msg: &T,
    data: &[u8],
) {
    tracing::trace!(typ = ?T::MESSAGE_TYPE, "Sending message to host");
    synic.post_message(
        protocol::VMBUS_MESSAGE_REDIRECT_CONNECTION_ID,
        1,
        OutgoingMessage::with_data(msg, data).data(),
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use guid::Guid;
    use pal_async::async_test;
    use pal_async::DefaultPool;
    use parking_lot::Mutex;
    use protocol::TargetInfo;
    use std::sync::Arc;
    use std::task::ready;
    use vmbus_core::protocol::MessageType;
    use vmbus_core::protocol::OfferFlags;
    use vmbus_core::protocol::UserDefinedData;
    use zerocopy::FromZeros;
    use zerocopy::Immutable;
    use zerocopy::IntoBytes;
    use zerocopy::KnownLayout;

    const VMBUS_TEST_CLIENT_ID: Guid =
        Guid::from_static_str("e6e6e6e6-e6e6-e6e6-e6e6-e6e6e6e6e6e6");

    fn in_msg<T: IntoBytes + Immutable + KnownLayout>(message_type: MessageType, t: T) -> Vec<u8> {
        let mut data = Vec::new();
        data.extend_from_slice(&message_type.0.to_ne_bytes());
        data.extend_from_slice(&0u32.to_ne_bytes());
        data.extend_from_slice(t.as_bytes());
        data
    }

    struct TestServer {
        messages: Mutex<Vec<OutgoingMessage>>,
        send: mesh::Sender<Vec<u8>>,
    }

    impl TestServer {
        fn next(&self) -> Option<OutgoingMessage> {
            let mut tries = 0;
            loop {
                if let Some(msg) = self.messages.lock().pop() {
                    return Some(msg);
                }
                if tries > 50 {
                    return None;
                }
                tries += 1;
                std::thread::sleep(std::time::Duration::from_millis(10));
            }
        }

        fn send(&self, msg: Vec<u8>) {
            self.send.send(msg);
        }

        async fn connect(&self, client: &mut VmbusClient) {
            let recv = client.access.client_request_send.call(
                ClientRequest::InitiateContact,
                InitiateContactRequest::default(),
            );

            let _ = self.next().unwrap();

            self.send(in_msg(
                MessageType::VERSION_RESPONSE,
                protocol::VersionResponse2 {
                    version_response: protocol::VersionResponse {
                        version_supported: 1,
                        connection_state: ConnectionState::SUCCESSFUL,
                        padding: 0,
                        selected_version_or_connection_id: 0,
                    },
                    supported_features: FeatureFlags::all().into(),
                },
            ));

            let version = recv.await.unwrap().unwrap();
            assert_eq!(version.version, Version::Copper);
            assert_eq!(version.feature_flags, FeatureFlags::all());
        }

        async fn get_channel(&self, client: &mut VmbusClient) -> OfferInfo {
            self.connect(client).await;

            let (send, mut recv) = mesh::channel();
            client
                .access
                .client_request_send
                .send(ClientRequest::RequestOffers(send));

            let _ = self.next().unwrap();

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

            self.send(in_msg(MessageType::OFFER_CHANNEL, offer));

            let received_offer = recv.next().await.unwrap();

            self.send(in_msg(MessageType::ALL_OFFERS_DELIVERED, [0x00]));

            assert!(recv.next().await.is_none());

            received_offer
        }
    }

    impl SynicClient for Arc<TestServer> {
        fn post_message(&self, _channel: u32, _typ: u32, msg: &[u8]) {
            self.messages
                .lock()
                .push(OutgoingMessage::from_message(msg));
        }

        fn map_event(&self, _event_flag: u16, _event: &pal_event::Event) -> std::io::Result<()> {
            Err(std::io::ErrorKind::Unsupported.into())
        }

        fn unmap_event(&self, _event_flag: u16) {
            unreachable!()
        }

        fn signal_event(&self, _connection_id: u32, _event_flag: u16) -> std::io::Result<()> {
            Err(std::io::ErrorKind::Unsupported.into())
        }
    }

    struct TestMessageSource {
        msg_recv: mesh::Receiver<Vec<u8>>,
    }

    impl AsyncRecv for TestMessageSource {
        fn poll_recv(
            &mut self,
            cx: &mut std::task::Context<'_>,
            mut bufs: &mut [std::io::IoSliceMut<'_>],
        ) -> std::task::Poll<std::io::Result<usize>> {
            let value = ready!(self.msg_recv.poll_recv(cx)).unwrap();
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

    impl VmbusMessageSource for TestMessageSource {}

    fn test_init() -> (Arc<TestServer>, VmbusClient, mesh::Receiver<OfferInfo>) {
        let pool = DefaultPool::new();
        let driver = pool.driver();
        let (msg_send, msg_recv) = mesh::channel();
        let server = Arc::new(TestServer {
            messages: Mutex::new(Vec::new()),
            send: msg_send,
        });
        let (offer_send, offer_recv) = mesh::channel();

        let mut client = VmbusClient::new(
            Arc::new(server.clone()),
            offer_send,
            TestMessageSource { msg_recv },
            &driver,
        );
        client.start();
        let _ = std::thread::Builder::new()
            .spawn(move || pool.run())
            .unwrap();

        (server, client, offer_recv)
    }

    #[async_test]
    async fn test_initiate_contact_success() {
        let (server, client, _) = test_init();
        let _recv = client.access.client_request_send.call(
            ClientRequest::InitiateContact,
            InitiateContactRequest::default(),
        );

        assert_eq!(
            server.next().unwrap(),
            OutgoingMessage::new(&protocol::InitiateContact2 {
                initiate_contact: protocol::InitiateContact {
                    version_requested: Version::Copper as u32,
                    target_message_vp: 0,
                    interrupt_page_or_target_info: TargetInfo::new()
                        .with_sint(2)
                        .with_vtl(0)
                        .with_feature_flags(FeatureFlags::all().into())
                        .into(),
                    parent_to_child_monitor_page_gpa: 0,
                    child_to_parent_monitor_page_gpa: 0,
                },
                ..FromZeros::new_zeroed()
            })
        );
    }

    #[async_test]
    async fn test_connect_success() {
        let (server, client, _) = test_init();
        let recv = client.access.client_request_send.call(
            ClientRequest::InitiateContact,
            InitiateContactRequest::default(),
        );

        assert_eq!(
            server.next().unwrap(),
            OutgoingMessage::new(&protocol::InitiateContact2 {
                initiate_contact: protocol::InitiateContact {
                    version_requested: Version::Copper as u32,
                    target_message_vp: 0,
                    interrupt_page_or_target_info: TargetInfo::new()
                        .with_sint(2)
                        .with_vtl(0)
                        .with_feature_flags(FeatureFlags::all().into())
                        .into(),
                    parent_to_child_monitor_page_gpa: 0,
                    child_to_parent_monitor_page_gpa: 0,
                },
                ..FromZeros::new_zeroed()
            })
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
                supported_features: FeatureFlags::all().into_bits(),
            },
        ));

        let version = recv.await.unwrap().unwrap();

        assert_eq!(version.version, Version::Copper);
        assert_eq!(version.feature_flags, FeatureFlags::all());
    }

    #[async_test]
    async fn test_feature_flags() {
        let (server, client, _) = test_init();
        let recv = client.access.client_request_send.call(
            ClientRequest::InitiateContact,
            InitiateContactRequest::default(),
        );

        assert_eq!(
            server.next().unwrap(),
            OutgoingMessage::new(&protocol::InitiateContact2 {
                initiate_contact: protocol::InitiateContact {
                    version_requested: Version::Copper as u32,
                    target_message_vp: 0,
                    interrupt_page_or_target_info: TargetInfo::new()
                        .with_sint(2)
                        .with_vtl(0)
                        .with_feature_flags(FeatureFlags::all().into())
                        .into(),
                    parent_to_child_monitor_page_gpa: 0,
                    child_to_parent_monitor_page_gpa: 0,
                },
                ..FromZeros::new_zeroed()
            })
        );

        // Report the server doesn't support some of the feature flags, and make sure this is reflected in
        // the returned version.
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

        let version = recv.await.unwrap().unwrap();

        assert_eq!(version.version, Version::Copper);
        assert_eq!(
            version.feature_flags,
            FeatureFlags::new().with_channel_interrupt_redirection(true)
        );
    }

    #[test]
    fn test_client_id() {
        let (server, client, _) = test_init();
        let initiate_contact = InitiateContactRequest {
            client_id: VMBUS_TEST_CLIENT_ID,
            ..Default::default()
        };
        let _recv = client
            .access
            .client_request_send
            .call(ClientRequest::InitiateContact, initiate_contact);

        assert_eq!(
            server.next().unwrap(),
            OutgoingMessage::new(&protocol::InitiateContact2 {
                initiate_contact: protocol::InitiateContact {
                    version_requested: Version::Copper as u32,
                    target_message_vp: 0,
                    interrupt_page_or_target_info: TargetInfo::new()
                        .with_sint(2)
                        .with_vtl(0)
                        .with_feature_flags(FeatureFlags::all().into())
                        .into(),
                    parent_to_child_monitor_page_gpa: 0,
                    child_to_parent_monitor_page_gpa: 0,
                },
                client_id: VMBUS_TEST_CLIENT_ID,
            })
        )
    }

    #[async_test]
    async fn test_version_negotiation() {
        let (server, client, _) = test_init();
        let recv = client.access.client_request_send.call(
            ClientRequest::InitiateContact,
            InitiateContactRequest::default(),
        );

        assert_eq!(
            server.next().unwrap(),
            OutgoingMessage::new(&protocol::InitiateContact2 {
                initiate_contact: protocol::InitiateContact {
                    version_requested: Version::Copper as u32,
                    target_message_vp: 0,
                    interrupt_page_or_target_info: TargetInfo::new()
                        .with_sint(2)
                        .with_vtl(0)
                        .with_feature_flags(FeatureFlags::all().into())
                        .into(),
                    parent_to_child_monitor_page_gpa: 0,
                    child_to_parent_monitor_page_gpa: 0,
                },
                ..FromZeros::new_zeroed()
            })
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

        assert_eq!(
            server.next().unwrap(),
            OutgoingMessage::new(&protocol::InitiateContact {
                version_requested: Version::Iron as u32,
                target_message_vp: 0,
                interrupt_page_or_target_info: TargetInfo::new()
                    .with_sint(2)
                    .with_vtl(0)
                    .with_feature_flags(FeatureFlags::new().into())
                    .into(),
                parent_to_child_monitor_page_gpa: 0,
                child_to_parent_monitor_page_gpa: 0,
            })
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

        let version = recv.await.unwrap().unwrap();

        assert_eq!(version.version, Version::Iron);
        assert_eq!(version.feature_flags, FeatureFlags::new());
    }

    #[async_test]
    async fn test_request_offers_success() {
        let (server, mut client, _) = test_init();

        server.connect(&mut client).await;

        let (send, mut recv) = mesh::channel();
        client
            .access
            .client_request_send
            .send(ClientRequest::RequestOffers(send));

        assert_eq!(
            server.next().unwrap(),
            OutgoingMessage::new(&protocol::RequestOffers {})
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

        let received_offer = recv.next().await.unwrap();

        assert_eq!(received_offer.offer, offer);

        server.send(in_msg(MessageType::ALL_OFFERS_DELIVERED, [0x00]));

        assert!(recv.next().await.is_none());
    }

    #[async_test]
    async fn test_open_channel_success() {
        let (server, mut client, _) = test_init();
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
                flags: OpenChannelFlags::new(),
            },
        );

        assert_eq!(
            server.next().unwrap(),
            OutgoingMessage::new(&protocol::OpenChannel2 {
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
                flags: 0,
            })
        );

        server.send(in_msg(
            MessageType::OPEN_CHANNEL_RESULT,
            protocol::OpenResult {
                channel_id: ChannelId(0),
                open_id: 0,
                status: protocol::STATUS_SUCCESS as u32,
            },
        ));

        let opened = recv.await.unwrap();
        assert!(opened);
    }

    #[async_test]
    async fn test_open_channel_fail() {
        let (server, mut client, _) = test_init();
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
                flags: OpenChannelFlags::new(),
            },
        );

        assert_eq!(
            server.next().unwrap(),
            OutgoingMessage::new(&protocol::OpenChannel2 {
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
                flags: 0,
            })
        );

        server.send(in_msg(
            MessageType::OPEN_CHANNEL_RESULT,
            protocol::OpenResult {
                channel_id: ChannelId(0),
                open_id: 0,
                status: protocol::STATUS_UNSUCCESSFUL as u32,
            },
        ));

        let opened = recv.await.unwrap();
        assert!(!opened);
    }

    #[async_test]
    async fn test_modify_channel() {
        let (server, mut client, _) = test_init();
        let channel = server.get_channel(&mut client).await;

        // N.B. A real server requires the channel to be open before sending this, but the test
        //      server doesn't care.
        let recv = channel.request_send.call(
            ChannelRequest::Modify,
            ModifyRequest::TargetVp { target_vp: 1 },
        );

        assert_eq!(
            server.next().unwrap(),
            OutgoingMessage::new(&protocol::ModifyChannel {
                channel_id: ChannelId(0),
                target_vp: 1,
            })
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
    async fn test_save_restore_connected() {
        let s0;
        {
            let (server, mut client, _) = test_init();
            server.connect(&mut client).await;
            s0 = client.save().await;
        }
        let (_, mut client, _) = test_init();
        client.restore(s0.clone()).await.unwrap();

        let s1 = client.save().await;

        assert_eq!(s0, s1);
    }

    #[async_test]
    async fn test_save_restore_connected_with_channel() {
        let s0;
        let c0;
        {
            let (server, mut client, _) = test_init();
            c0 = server.get_channel(&mut client).await;
            s0 = client.save().await;
        }
        let (_, mut client, _) = test_init();
        let (_, channels) = client.restore(s0.clone()).await.unwrap();

        let s1 = client.save().await;
        assert_eq!(s0, s1);
        assert_eq!(channels[0].offer.offer, c0.offer);
    }

    #[async_test]
    async fn test_connect_fails_on_incorrect_state() {
        let (server, mut client, _) = test_init();
        server.connect(&mut client).await;
        let err = client.connect(0, None, Guid::ZERO).await.unwrap_err();
        assert!(matches!(err, ConnectError::InvalidState), "{:?}", err);
    }

    #[async_test]
    async fn test_hot_add_remove() {
        let (server, mut client, mut offer_recv) = test_init();

        server.connect(&mut client).await;
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
        let mut info = offer_recv.next().await.unwrap();

        assert_eq!(offer, info.offer);

        server.send(in_msg(
            MessageType::RESCIND_CHANNEL_OFFER,
            protocol::RescindChannelOffer {
                channel_id: ChannelId(5),
            },
        ));

        assert_eq!(
            server.next().unwrap(),
            OutgoingMessage::new(&protocol::RelIdReleased {
                channel_id: ChannelId(5)
            })
        );

        assert!(info.response_recv.next().await.is_none());
    }

    #[async_test]
    async fn test_gpadl_success() {
        let (server, mut client, _) = test_init();
        let mut channel = server.get_channel(&mut client).await;
        let recv = channel.request_send.call(
            ChannelRequest::Gpadl,
            GpadlRequest {
                id: GpadlId(1),
                count: 1,
                buf: vec![5],
            },
        );

        assert_eq!(
            server.next().unwrap(),
            OutgoingMessage::with_data(
                &protocol::GpadlHeader {
                    channel_id: ChannelId(0),
                    gpadl_id: GpadlId(1),
                    len: 8,
                    count: 1,
                },
                0x5u64.as_bytes()
            )
        );

        server.send(in_msg(
            MessageType::GPADL_CREATED,
            protocol::GpadlCreated {
                channel_id: ChannelId(0),
                gpadl_id: GpadlId(1),
                status: protocol::STATUS_SUCCESS,
            },
        ));

        let created = recv.await.unwrap();
        assert!(created);

        channel
            .request_send
            .send(ChannelRequest::TeardownGpadl(GpadlId(1)));

        assert_eq!(
            server.next().unwrap(),
            OutgoingMessage::new(&protocol::GpadlTeardown {
                channel_id: ChannelId(0),
                gpadl_id: GpadlId(1),
            })
        );

        server.send(in_msg(
            MessageType::GPADL_TORNDOWN,
            protocol::GpadlTorndown {
                gpadl_id: GpadlId(1),
            },
        ));

        let ChannelResponse::TeardownGpadl(gpadl_id) = channel.response_recv.next().await.unwrap();

        assert_eq!(gpadl_id, GpadlId(1));
    }

    #[async_test]
    async fn test_gpadl_fail() {
        let (server, mut client, _) = test_init();
        let channel = server.get_channel(&mut client).await;
        let recv = channel.request_send.call(
            ChannelRequest::Gpadl,
            GpadlRequest {
                id: GpadlId(1),
                count: 1,
                buf: vec![7],
            },
        );

        assert_eq!(
            server.next().unwrap(),
            OutgoingMessage::with_data(
                &protocol::GpadlHeader {
                    channel_id: ChannelId(0),
                    gpadl_id: GpadlId(1),
                    len: 8,
                    count: 1,
                },
                0x7u64.as_bytes()
            )
        );

        server.send(in_msg(
            MessageType::GPADL_CREATED,
            protocol::GpadlCreated {
                channel_id: ChannelId(0),
                gpadl_id: GpadlId(1),
                status: protocol::STATUS_UNSUCCESSFUL,
            },
        ));

        let created = recv.await.unwrap();
        assert!(!created);
    }

    #[async_test]
    async fn test_gpadl_with_revoke() {
        let (server, mut client, _offer_recv) = test_init();
        let mut channel = server.get_channel(&mut client).await;
        let channel_id = ChannelId(0);
        let gpadl_id = GpadlId(1);
        let recv = channel.request_send.call(
            ChannelRequest::Gpadl,
            GpadlRequest {
                id: gpadl_id,
                count: 1,
                buf: vec![3],
            },
        );

        assert_eq!(
            server.next().unwrap(),
            OutgoingMessage::with_data(
                &protocol::GpadlHeader {
                    channel_id,
                    gpadl_id,
                    len: 8,
                    count: 1,
                },
                0x3u64.as_bytes()
            )
        );

        server.send(in_msg(
            MessageType::GPADL_CREATED,
            protocol::GpadlCreated {
                channel_id,
                gpadl_id,
                status: protocol::STATUS_SUCCESS,
            },
        ));

        let created = recv.await.unwrap();
        assert!(created);

        channel
            .request_send
            .send(ChannelRequest::TeardownGpadl(gpadl_id));

        assert_eq!(
            server.next().unwrap(),
            OutgoingMessage::new(&protocol::GpadlTeardown {
                channel_id,
                gpadl_id,
            })
        );

        server.send(in_msg(
            MessageType::RESCIND_CHANNEL_OFFER,
            protocol::RescindChannelOffer { channel_id },
        ));

        let ChannelResponse::TeardownGpadl(id) = channel.response_recv.next().await.unwrap();

        assert_eq!(id, gpadl_id);

        assert_eq!(
            server.next().unwrap(),
            OutgoingMessage::new(&protocol::RelIdReleased { channel_id })
        );

        assert!(channel.response_recv.next().await.is_none());
    }

    #[async_test]
    async fn test_modify_connection() {
        let (server, mut client, _) = test_init();
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

        assert_eq!(
            server.next().unwrap(),
            OutgoingMessage::new(&protocol::ModifyConnection {
                child_to_parent_monitor_page_gpa: 5,
                parent_to_child_monitor_page_gpa: 6,
            })
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
    async fn test_hvsock() {
        let (server, mut client, _offer_recv) = test_init();
        server.connect(&mut client).await;
        let request = HvsockConnectRequest {
            service_id: Guid::new_random(),
            endpoint_id: Guid::new_random(),
            silo_id: Guid::new_random(),
        };

        let resp = client.access().connect_hvsock(request);
        assert_eq!(
            server.next().unwrap(),
            OutgoingMessage::new(&protocol::TlConnectRequest2 {
                base: protocol::TlConnectRequest {
                    service_id: request.service_id,
                    endpoint_id: request.endpoint_id,
                },
                silo_id: request.silo_id,
            })
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
}
