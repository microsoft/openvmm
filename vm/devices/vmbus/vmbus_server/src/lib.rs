// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

#![expect(missing_docs)]
#![forbid(unsafe_code)]

mod channel_bitmap;
mod channels;
pub mod event;
pub mod hvsock;
mod monitor;
mod proxyintegration;

/// The GUID type used for vmbus channel identifiers.
pub type Guid = guid::Guid;

use anyhow::Context;
use async_trait::async_trait;
use channel_bitmap::ChannelBitmap;
use channels::ConnectionTarget;
pub use channels::InitiateContactRequest;
use channels::MessageTarget;
use channels::ModifyConnectionRequest;
pub use channels::ModifyConnectionResponse;
use channels::Notifier;
use channels::OfferId;
pub use channels::OfferParamsInternal;
use channels::OpenParams;
use channels::RestoreError;
pub use channels::Update;
use futures::FutureExt;
use futures::StreamExt;
use futures::channel::mpsc;
use futures::channel::mpsc::SendError;
use futures::future::OptionFuture;
use futures::future::poll_fn;
use futures::stream::SelectAll;
use guestmem::GuestMemory;
use hvdef::Vtl;
use inspect::Inspect;
use mesh::payload::Protobuf;
use mesh::rpc::FailableRpc;
use mesh::rpc::Rpc;
use mesh::rpc::RpcError;
use mesh::rpc::RpcSend;
use pal_async::task::Spawn;
use pal_async::task::Task;
use pal_event::Event;
#[cfg(windows)]
pub use proxyintegration::ProxyIntegration;
#[cfg(windows)]
pub use proxyintegration::ProxyServerInfo;
use ring::PAGE_SIZE;
use std::collections::HashMap;
use std::future;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::task::Poll;
use std::task::ready;
use unicycle::FuturesUnordered;
use vmbus_channel::bus::ChannelRequest;
use vmbus_channel::bus::ChannelServerRequest;
use vmbus_channel::bus::GpadlRequest;
use vmbus_channel::bus::ModifyRequest;
use vmbus_channel::bus::OfferInput;
use vmbus_channel::bus::OfferKey;
use vmbus_channel::bus::OfferResources;
use vmbus_channel::bus::OpenData;
use vmbus_channel::bus::OpenRequest;
use vmbus_channel::bus::OpenResult;
use vmbus_channel::bus::ParentBus;
use vmbus_channel::bus::RestoreResult;
use vmbus_channel::gpadl::GpadlMap;
use vmbus_channel::gpadl_ring::AlignedGpadlView;
use vmbus_channel::gpadl_ring::GpadlRingMem;
use vmbus_core::HvsockConnectRequest;
use vmbus_core::HvsockConnectResult;
use vmbus_core::MaxVersionInfo;
use vmbus_core::MonitorPageGpas;
use vmbus_core::OutgoingMessage;
use vmbus_core::TaggedStream;
use vmbus_core::VersionInfo;
use vmbus_core::protocol;
pub use vmbus_core::protocol::GpadlId;
#[cfg(windows)]
use vmbus_proxy::ProxyHandle;
use vmbus_ring as ring;
use vmbus_ring::gparange::MultiPagedRangeBuf;
use vmcore::interrupt::Interrupt;
use vmcore::save_restore::SavedStateRoot;
use vmcore::synic::EventPort;
use vmcore::synic::GuestEventPort;
use vmcore::synic::GuestMessagePort;
use vmcore::synic::MessagePort;
use vmcore::synic::SynicPortAccess;

const SINT: u8 = 2;
pub const REDIRECT_SINT: u8 = 7;
pub const REDIRECT_VTL: Vtl = Vtl::Vtl2;
const SHARED_EVENT_CONNECTION_ID: u32 = 2;
const VMBUS_MESSAGE_TYPE: u32 = 1;

const MAX_CONCURRENT_HVSOCK_REQUESTS: usize = 16;

pub struct VmbusServer {
    task_send: mesh::Sender<VmbusRequest>,
    control: Arc<VmbusServerControl>,
    _message_port: Box<dyn Sync + Send>,
    _multiclient_message_port: Option<Box<dyn Sync + Send>>,
    task: Task<ServerTask>,
}

pub struct VmbusServerBuilder<'a, T: Spawn> {
    spawner: &'a T,
    synic: Arc<dyn SynicPortAccess>,
    gm: GuestMemory,
    private_gm: Option<GuestMemory>,
    vtl: Vtl,
    hvsock_notify: Option<HvsockServerChannelHalf>,
    server_relay: Option<VmbusServerChannelHalf>,
    external_server: Option<mesh::Sender<InitiateContactRequest>>,
    external_requests: Option<mesh::Receiver<InitiateContactRequest>>,
    use_message_redirect: bool,
    channel_id_offset: u16,
    max_version: Option<MaxVersionInfo>,
    delay_max_version: bool,
    enable_mnf: bool,
    force_confidential_external_memory: bool,
    send_messages_while_stopped: bool,
}

/// The server side of the connection between a vmbus server and a relay.
pub struct ServerChannelHalf<Request, Response> {
    request_send: mesh::Sender<Request>,
    response_receive: mesh::Receiver<Response>,
}

/// The relay side of a connection between a vmbus server and a relay.
pub struct RelayChannelHalf<Request, Response> {
    pub request_receive: mesh::Receiver<Request>,
    pub response_send: mesh::Sender<Response>,
}

/// A connection between a vmbus server and a relay.
pub struct RelayChannel<Request, Response> {
    pub relay_half: RelayChannelHalf<Request, Response>,
    pub server_half: ServerChannelHalf<Request, Response>,
}

impl<Request: 'static + Send, Response: 'static + Send> RelayChannel<Request, Response> {
    /// Creates a new channel between the vmbus server and a relay.
    pub fn new() -> Self {
        let (request_send, request_receive) = mesh::channel();
        let (response_send, response_receive) = mesh::channel();
        Self {
            relay_half: RelayChannelHalf {
                request_receive,
                response_send,
            },
            server_half: ServerChannelHalf {
                request_send,
                response_receive,
            },
        }
    }
}

pub type VmbusServerChannelHalf = ServerChannelHalf<ModifyRelayRequest, ModifyConnectionResponse>;
pub type VmbusRelayChannelHalf = RelayChannelHalf<ModifyRelayRequest, ModifyConnectionResponse>;
pub type VmbusRelayChannel = RelayChannel<ModifyRelayRequest, ModifyConnectionResponse>;
pub type HvsockServerChannelHalf = ServerChannelHalf<HvsockConnectRequest, HvsockConnectResult>;
pub type HvsockRelayChannelHalf = RelayChannelHalf<HvsockConnectRequest, HvsockConnectResult>;
pub type HvsockRelayChannel = RelayChannel<HvsockConnectRequest, HvsockConnectResult>;

/// A request from the server to the relay to modify connection state.
///
/// The version, use_interrupt_page and target_message_vp field can only be present if this request
/// was sent for an InitiateContact message from the guest.
#[derive(Debug, Copy, Clone)]
pub struct ModifyRelayRequest {
    pub version: Option<u32>,
    pub monitor_page: Update<MonitorPageGpas>,
    pub use_interrupt_page: Option<bool>,
}

impl From<ModifyConnectionRequest> for ModifyRelayRequest {
    fn from(value: ModifyConnectionRequest) -> Self {
        Self {
            version: value.version,
            monitor_page: value.monitor_page,
            use_interrupt_page: match value.interrupt_page {
                Update::Unchanged => None,
                Update::Reset => Some(false),
                Update::Set(_) => Some(true),
            },
        }
    }
}

#[derive(Debug)]
enum VmbusRequest {
    Reset(Rpc<(), ()>),
    Inspect(inspect::Deferred),
    Save(Rpc<(), SavedState>),
    Restore(Rpc<SavedState, Result<(), RestoreError>>),
    PostRestore(Rpc<(), Result<(), RestoreError>>),
    Start,
    Stop(Rpc<(), ()>),
}

#[derive(mesh::MeshPayload, Debug)]
pub struct OfferInfo {
    pub params: OfferParamsInternal,
    pub request_send: mesh::Sender<ChannelRequest>,
    pub server_request_recv: mesh::Receiver<ChannelServerRequest>,
}

#[derive(mesh::MeshPayload)]
pub(crate) enum OfferRequest {
    Offer(FailableRpc<OfferInfo, ()>),
    ForceReset(Rpc<(), ()>),
}

impl Inspect for VmbusServer {
    fn inspect(&self, req: inspect::Request<'_>) {
        self.task_send.send(VmbusRequest::Inspect(req.defer()));
    }
}

struct ChannelEvent(Interrupt);

impl EventPort for ChannelEvent {
    fn handle_event(&self, _flag: u16) {
        self.0.deliver();
    }

    fn os_event(&self) -> Option<&Event> {
        self.0.event()
    }
}

#[derive(Debug, Protobuf, SavedStateRoot)]
#[mesh(package = "vmbus.server")]
pub struct SavedState {
    #[mesh(1)]
    server: channels::SavedState,
    // Indicates if the lost synic bug is fixed or not. By default it's false.
    // During the restore process, we check if the field is not true then
    // unstick_channels() function will be called to mitigate the issue.
    #[mesh(2)]
    lost_synic_bug_fixed: bool,
}

const MESSAGE_CONNECTION_ID: u32 = 1;
const MULTICLIENT_MESSAGE_CONNECTION_ID: u32 = 4;

impl<'a, T: Spawn> VmbusServerBuilder<'a, T> {
    /// Creates a new builder for `VmbusServer` with the default options.
    pub fn new(spawner: &'a T, synic: Arc<dyn SynicPortAccess>, gm: GuestMemory) -> Self {
        Self {
            spawner,
            synic,
            gm,
            private_gm: None,
            vtl: Vtl::Vtl0,
            hvsock_notify: None,
            server_relay: None,
            external_server: None,
            external_requests: None,
            use_message_redirect: false,
            channel_id_offset: 0,
            max_version: None,
            delay_max_version: false,
            enable_mnf: false,
            force_confidential_external_memory: false,
            send_messages_while_stopped: false,
        }
    }

    /// Sets a separate guest memory instance to use for channels that are confidential (non-relay
    /// channels in Underhill on a hardware isolated VM). This is not relevant for a non-Underhill
    /// VmBus server.
    pub fn private_gm(mut self, private_gm: Option<GuestMemory>) -> Self {
        self.private_gm = private_gm;
        self
    }

    /// Sets the VTL that this instance will serve.
    pub fn vtl(mut self, vtl: Vtl) -> Self {
        self.vtl = vtl;
        self
    }

    /// Sets a send/receive pair used to handle hvsocket requests.
    pub fn hvsock_notify(mut self, hvsock_notify: Option<HvsockServerChannelHalf>) -> Self {
        self.hvsock_notify = hvsock_notify;
        self
    }

    /// Sets a send/receive pair that will be notified of server requests. This is used by the
    /// Underhill relay.
    pub fn server_relay(mut self, server_relay: Option<VmbusServerChannelHalf>) -> Self {
        self.server_relay = server_relay;
        self
    }

    /// Sets a receiver that receives requests from another server.
    pub fn external_requests(
        mut self,
        external_requests: Option<mesh::Receiver<InitiateContactRequest>>,
    ) -> Self {
        self.external_requests = external_requests;
        self
    }

    /// Sets a sender used to forward unhandled connect requests (which used a different VTL)
    /// to another server.
    pub fn external_server(
        mut self,
        external_server: Option<mesh::Sender<InitiateContactRequest>>,
    ) -> Self {
        self.external_server = external_server;
        self
    }

    /// Sets a value which indicates whether the vmbus control plane is redirected to Underhill.
    pub fn use_message_redirect(mut self, use_message_redirect: bool) -> Self {
        self.use_message_redirect = use_message_redirect;
        self
    }

    /// Tells the server to use an offset when generating channel IDs to void collisions with
    /// another vmbus server.
    ///
    /// N.B. This should only be used by the Underhill vmbus server.
    pub fn enable_channel_id_offset(mut self, enable: bool) -> Self {
        self.channel_id_offset = if enable { 1024 } else { 0 };
        self
    }

    /// Tells the server to limit the protocol version offered to the guest.
    ///
    /// N.B. This is used for testing older protocols without requiring a specific guest OS.
    pub fn max_version(mut self, max_version: Option<MaxVersionInfo>) -> Self {
        self.max_version = max_version;
        self
    }

    /// Delay limiting the maximum version until after the first `Unload` message.
    ///
    /// N.B. This is used to enable the use of versions older than `Version::Win10` with Uefi boot,
    ///      since that's the oldest version the Uefi client supports.
    pub fn delay_max_version(mut self, delay: bool) -> Self {
        self.delay_max_version = delay;
        self
    }

    /// Enable MNF support in the server.
    ///
    /// N.B. Enabling this has no effect if the synic does not support mapping monitor pages.
    pub fn enable_mnf(mut self, enable: bool) -> Self {
        self.enable_mnf = enable;
        self
    }

    /// Force all non-relay channels to use encrypted external memory. Used for testing purposes
    /// only.
    pub fn force_confidential_external_memory(mut self, force: bool) -> Self {
        self.force_confidential_external_memory = force;
        self
    }

    /// Send messages to the partition even while stopped, which can cause
    /// corrupted synic states across VM reset.
    ///
    /// This option is used to prevent messages from getting into the queue, for
    /// saved state compatibility with release/2411. It can be removed once that
    /// release is no longer supported.
    pub fn send_messages_while_stopped(mut self, send: bool) -> Self {
        self.send_messages_while_stopped = send;
        self
    }

    /// Creates a new instance of the server.
    ///
    /// When the object is dropped, all channels will be closed and revoked
    /// automatically.
    pub fn build(self) -> anyhow::Result<VmbusServer> {
        #[expect(clippy::disallowed_methods)] // TODO
        let (message_send, message_recv) = mpsc::channel(64);
        let message_sender = Arc::new(MessageSender {
            send: message_send.clone(),
            multiclient: self.use_message_redirect,
        });

        let (redirect_vtl, redirect_sint) = if self.use_message_redirect {
            (REDIRECT_VTL, REDIRECT_SINT)
        } else {
            (self.vtl, SINT)
        };

        // If this server is not for VTL2, use a server-specific connection ID rather than the
        // standard one.
        let connection_id = if self.vtl == Vtl::Vtl0 && !self.use_message_redirect {
            MESSAGE_CONNECTION_ID
        } else {
            // TODO: This ID should be using the correct target VP, but that is not known until
            //       InitiateContact.
            VmbusServer::get_child_message_connection_id(0, redirect_sint, redirect_vtl)
        };

        let _message_port = self
            .synic
            .add_message_port(connection_id, redirect_vtl, message_sender)
            .context("failed to create vmbus synic ports")?;

        // If this server is for VTL0, it is also responsible for the multiclient message port.
        // N.B. If control plane redirection is enabled, the redirected message port is used for
        //      multiclient and no separate multiclient port is created.
        let _multiclient_message_port = if self.vtl == Vtl::Vtl0 && !self.use_message_redirect {
            let multiclient_message_sender = Arc::new(MessageSender {
                send: message_send,
                multiclient: true,
            });

            Some(
                self.synic
                    .add_message_port(
                        MULTICLIENT_MESSAGE_CONNECTION_ID,
                        self.vtl,
                        multiclient_message_sender,
                    )
                    .context("failed to create vmbus synic ports")?,
            )
        } else {
            None
        };

        let (offer_send, offer_recv) = mesh::mpsc_channel();
        let control = Arc::new(VmbusServerControl {
            mem: self.gm.clone(),
            private_mem: self.private_gm.clone(),
            send: offer_send,
            use_event: self.synic.prefer_os_events(),
            force_confidential_external_memory: self.force_confidential_external_memory,
        });

        let mut server = channels::Server::new(self.vtl, connection_id, self.channel_id_offset);

        // If requested, limit the maximum protocol version and feature flags.
        if let Some(version) = self.max_version {
            server.set_compatibility_version(version, self.delay_max_version);
        }
        let (relay_request_send, relay_response_recv) =
            if let Some(server_relay) = self.server_relay {
                let r = server_relay.response_receive.boxed().fuse();
                (server_relay.request_send, r)
            } else {
                let (req_send, req_recv) = mesh::channel();
                let resp_recv = req_recv
                    .map(|_| {
                        ModifyConnectionResponse::Supported(
                            protocol::ConnectionState::SUCCESSFUL,
                            protocol::FeatureFlags::from_bits(u32::MAX),
                        )
                    })
                    .boxed()
                    .fuse();
                (req_send, resp_recv)
            };

        // If no hvsock notifier was specified, use a default one that always sends an error response.
        let (hvsock_send, hvsock_recv) = if let Some(hvsock_notify) = self.hvsock_notify {
            let r = hvsock_notify.response_receive.boxed().fuse();
            (hvsock_notify.request_send, r)
        } else {
            let (req_send, req_recv) = mesh::channel();
            let resp_recv = req_recv
                .map(|r: HvsockConnectRequest| HvsockConnectResult::from_request(&r, false))
                .boxed()
                .fuse();
            (req_send, resp_recv)
        };

        let inner = ServerTaskInner {
            running: false,
            send_messages_while_stopped: self.send_messages_while_stopped,
            gm: self.gm,
            private_gm: self.private_gm,
            vtl: self.vtl,
            redirect_vtl,
            redirect_sint,
            message_port: self
                .synic
                .new_guest_message_port(redirect_vtl, 0, redirect_sint)?,
            synic: self.synic,
            hvsock_requests: 0,
            hvsock_send,
            channels: HashMap::new(),
            channel_responses: FuturesUnordered::new(),
            relay_send: relay_request_send,
            external_server_send: self.external_server,
            channel_bitmap: None,
            shared_event_port: None,
            reset_done: Vec::new(),
            enable_mnf: self.enable_mnf,
        };

        let (task_send, task_recv) = mesh::channel();
        let mut server_task = ServerTask {
            server,
            task_recv,
            offer_recv,
            message_recv,
            server_request_recv: SelectAll::new(),
            inner,
            external_requests: self.external_requests,
            next_seq: 0,
            unstick_on_start: false,
        };

        let task = self.spawner.spawn("vmbus server", async move {
            server_task.run(relay_response_recv, hvsock_recv).await;
            server_task
        });

        Ok(VmbusServer {
            task_send,
            control,
            _message_port,
            _multiclient_message_port,
            task,
        })
    }
}

impl VmbusServer {
    /// Creates a new builder for `VmbusServer` with the default options.
    pub fn builder<T: Spawn>(
        spawner: &T,
        synic: Arc<dyn SynicPortAccess>,
        gm: GuestMemory,
    ) -> VmbusServerBuilder<'_, T> {
        VmbusServerBuilder::new(spawner, synic, gm)
    }

    pub async fn save(&self) -> SavedState {
        self.task_send.call(VmbusRequest::Save, ()).await.unwrap()
    }

    pub async fn restore(&self, state: SavedState) -> Result<(), RestoreError> {
        self.task_send
            .call(VmbusRequest::Restore, state)
            .await
            .unwrap()
    }

    pub async fn post_restore(&self) -> Result<(), RestoreError> {
        self.task_send
            .call(VmbusRequest::PostRestore, ())
            .await
            .unwrap()
    }

    /// Stop the control plane.
    pub async fn stop(&self) {
        self.task_send.call(VmbusRequest::Stop, ()).await.unwrap()
    }

    /// Starts the control plane.
    pub fn start(&self) {
        self.task_send.send(VmbusRequest::Start);
    }

    /// Resets the vmbus channel state.
    pub async fn reset(&self) {
        tracing::debug!("resetting channel state");
        self.task_send.call(VmbusRequest::Reset, ()).await.unwrap()
    }

    /// Tears down the vmbus control plane.
    pub async fn shutdown(self) {
        drop(self.task_send);
        let _ = self.task.await;
    }

    /// Returns an object that can be used to offer channels.
    pub fn control(&self) -> Arc<VmbusServerControl> {
        self.control.clone()
    }

    /// Returns the message connection ID to use for a communication from the guest for servers
    /// that use a non-standard SINT or VTL.
    fn get_child_message_connection_id(vp_index: u32, sint_index: u8, vtl: Vtl) -> u32 {
        MULTICLIENT_MESSAGE_CONNECTION_ID
            | (vtl as u32) << 22
            | vp_index << 8
            | (sint_index as u32) << 4
    }
}

#[derive(mesh::MeshPayload)]
pub struct RestoreInfo {
    open_data: Option<OpenData>,
    gpadls: Vec<(GpadlId, u16, Vec<u64>)>,
    interrupt: Option<Interrupt>,
}

#[derive(Default)]
pub struct SynicMessage {
    data: Vec<u8>,
    multiclient: bool,
    trusted: bool,
}

struct ServerTask {
    server: channels::Server,
    task_recv: mesh::Receiver<VmbusRequest>,
    offer_recv: mesh::Receiver<OfferRequest>,
    message_recv: mpsc::Receiver<SynicMessage>,
    server_request_recv: SelectAll<TaggedStream<OfferId, mesh::Receiver<ChannelServerRequest>>>,
    inner: ServerTaskInner,
    external_requests: Option<mesh::Receiver<InitiateContactRequest>>,
    /// Next value for [`Channel::seq`].
    next_seq: u64,
    unstick_on_start: bool,
}

struct ServerTaskInner {
    running: bool,
    send_messages_while_stopped: bool,
    gm: GuestMemory,
    private_gm: Option<GuestMemory>,
    synic: Arc<dyn SynicPortAccess>,
    vtl: Vtl,
    redirect_vtl: Vtl,
    redirect_sint: u8,
    message_port: Box<dyn GuestMessagePort>,
    hvsock_requests: usize,
    hvsock_send: mesh::Sender<HvsockConnectRequest>,
    channels: HashMap<OfferId, Channel>,
    channel_responses: FuturesUnordered<
        Pin<Box<dyn Send + Future<Output = (OfferId, u64, Result<ChannelResponse, RpcError>)>>>,
    >,
    external_server_send: Option<mesh::Sender<InitiateContactRequest>>,
    relay_send: mesh::Sender<ModifyRelayRequest>,
    channel_bitmap: Option<Arc<ChannelBitmap>>,
    shared_event_port: Option<Box<dyn Send>>,
    reset_done: Vec<Rpc<(), ()>>,
    enable_mnf: bool,
}

#[derive(Debug)]
enum ChannelResponse {
    Open(Option<OpenResult>),
    Close,
    Gpadl(GpadlId, bool),
    TeardownGpadl(GpadlId),
    Modify(i32),
}

struct Channel {
    key: OfferKey,
    send: mesh::Sender<ChannelRequest>,
    seq: u64,
    state: ChannelState,
    gpadls: Arc<GpadlMap>,
    guest_event_port: Box<dyn GuestEventPort>,
    flags: protocol::OfferFlags,
    // A channel can be reserved no matter what state it is in. This allows the message port for a
    // reserved channel to remain available even if the channel is closed, so the guest can read the
    // close reserved channel response. The reserved state is cleared when the channel is revoked,
    // reopened, or the guest sends an unload message.
    reserved_state: ReservedState,
}

struct ReservedState {
    message_port: Option<Box<dyn GuestMessagePort>>,
    target: ConnectionTarget,
}

enum ChannelState {
    Closed,
    Opening {
        open_params: OpenParams,
        monitor: Option<Box<dyn Send>>,
        host_to_guest_interrupt: Interrupt,
    },
    Open {
        open_params: OpenParams,
        _event_port: Box<dyn Send>,
        monitor: Option<Box<dyn Send>>,
        host_to_guest_interrupt: Interrupt,
        guest_to_host_event: Arc<ChannelEvent>,
    },
    FailedOpen,
}

impl ServerTask {
    fn handle_offer(&mut self, mut info: OfferInfo) -> anyhow::Result<()> {
        let key = info.params.key();
        let flags = info.params.flags;

        // Disable mnf if the synic doesn't support it or it's not enabled in this server.
        if info.params.use_mnf
            && (!self.inner.enable_mnf || self.inner.synic.monitor_support().is_none())
        {
            info.params.use_mnf = false;
        }

        let offer_id = self
            .server
            .with_notifier(&mut self.inner)
            .offer_channel(info.params)
            .context("channel offer failed")?;

        let guest_event_port = self.inner.synic.new_guest_event_port()?;

        tracing::debug!(?offer_id, %key, "offered channel");

        let id = self.next_seq;
        self.next_seq += 1;
        self.inner.channels.insert(
            offer_id,
            Channel {
                key,
                send: info.request_send,
                state: ChannelState::Closed,
                gpadls: GpadlMap::new(),
                guest_event_port,
                seq: id,
                flags,
                reserved_state: ReservedState {
                    message_port: None,
                    target: ConnectionTarget { vp: 0, sint: 0 },
                },
            },
        );

        self.server_request_recv
            .push(TaggedStream::new(offer_id, info.server_request_recv));

        Ok(())
    }

    fn handle_revoke(&mut self, offer_id: OfferId) {
        // The channel may or may not exist in the map depending on whether it's been explicitly
        // revoked before being dropped.
        if self.inner.channels.remove(&offer_id).is_some() {
            tracing::info!(?offer_id, "revoking channel");
            self.server
                .with_notifier(&mut self.inner)
                .revoke_channel(offer_id);
        }
    }

    fn handle_response(
        &mut self,
        offer_id: OfferId,
        seq: u64,
        response: Result<ChannelResponse, RpcError>,
    ) {
        // Validate the sequence to ensure the response is not for a revoked channel.
        let channel = self
            .inner
            .channels
            .get(&offer_id)
            .filter(|channel| channel.seq == seq);

        if let Some(channel) = channel {
            match response {
                Ok(response) => match response {
                    ChannelResponse::Open(result) => self.handle_open(offer_id, result),
                    ChannelResponse::Close => self.handle_close(offer_id),
                    ChannelResponse::Gpadl(gpadl_id, ok) => {
                        self.handle_gpadl_create(offer_id, gpadl_id, ok)
                    }
                    ChannelResponse::TeardownGpadl(gpadl_id) => {
                        self.handle_gpadl_teardown(offer_id, gpadl_id)
                    }
                    ChannelResponse::Modify(status) => self.handle_modify_channel(offer_id, status),
                },
                Err(err) => {
                    tracing::error!(
                        key = %channel.key,
                        error = &err as &dyn std::error::Error,
                        "channel response failure, channel is in inconsistent state until revoked"
                    );
                }
            }
        } else {
            tracing::debug!(offer_id = ?offer_id, seq, ?response, "received response after revoke");
        }
    }

    fn handle_open(&mut self, offer_id: OfferId, result: Option<OpenResult>) {
        let status = if result.is_some() {
            0
        } else {
            protocol::STATUS_UNSUCCESSFUL
        };
        if let Err(err) = self.inner.complete_open(offer_id, result) {
            tracelimit::error_ratelimited!(
                error = err.as_ref() as &dyn std::error::Error,
                "failed to complete open"
            );
            // If complete_open failed, the channel is now in FailedOpen state and the device needs
            // to notified to close it. Calling open_complete is postponed until the device responds
            // to the close request.
            self.inner.notify(offer_id, channels::Action::Close);
        } else {
            self.server
                .with_notifier(&mut self.inner)
                .open_complete(offer_id, status);
        }
    }

    fn handle_close(&mut self, offer_id: OfferId) {
        let channel = self
            .inner
            .channels
            .get_mut(&offer_id)
            .expect("channel still exists");

        match &mut channel.state {
            ChannelState::Open { .. } => {
                channel.state = ChannelState::Closed;
                self.server
                    .with_notifier(&mut self.inner)
                    .close_complete(offer_id);
            }
            ChannelState::FailedOpen => {
                // Now that the device has processed the close request after open failed, we can
                // finish handling the failed open and send an open result to the guest.
                channel.state = ChannelState::Closed;
                self.server
                    .with_notifier(&mut self.inner)
                    .open_complete(offer_id, protocol::STATUS_UNSUCCESSFUL);
            }
            _ => {
                tracing::error!(?offer_id, "invalid close channel response");
            }
        };
    }

    fn handle_gpadl_create(&mut self, offer_id: OfferId, gpadl_id: GpadlId, ok: bool) {
        let status = if ok { 0 } else { protocol::STATUS_UNSUCCESSFUL };
        self.server
            .with_notifier(&mut self.inner)
            .gpadl_create_complete(offer_id, gpadl_id, status);
    }

    fn handle_gpadl_teardown(&mut self, offer_id: OfferId, gpadl_id: GpadlId) {
        self.server
            .with_notifier(&mut self.inner)
            .gpadl_teardown_complete(offer_id, gpadl_id);
    }

    fn handle_modify_channel(&mut self, offer_id: OfferId, status: i32) {
        self.server
            .with_notifier(&mut self.inner)
            .modify_channel_complete(offer_id, status);
    }

    fn handle_restore_channel(
        &mut self,
        offer_id: OfferId,
        open: Option<OpenResult>,
    ) -> anyhow::Result<RestoreResult> {
        let gpadls = self.server.channel_gpadls(offer_id);

        // If the channel is opened, handle that before calling into channels so that failure can
        // be handled before the channel is marked restored.
        let open_request = open
            .map(|result| -> anyhow::Result<_> {
                let params = self.server.get_restore_open_params(offer_id)?;
                let (_, interrupt) = self.inner.open_channel(offer_id, &params)?;
                let channel = self.inner.complete_open(offer_id, Some(result))?;
                Ok(OpenRequest::new(
                    params.open_data,
                    interrupt,
                    self.server
                        .get_version()
                        .expect("must be connected")
                        .feature_flags,
                    channel.flags,
                ))
            })
            .transpose()?;

        self.server
            .with_notifier(&mut self.inner)
            .restore_channel(offer_id, open_request.is_some())?;

        let channel = self.inner.channels.get_mut(&offer_id).unwrap();
        for gpadl in &gpadls {
            if let Ok(buf) =
                MultiPagedRangeBuf::new(gpadl.request.count.into(), gpadl.request.buf.clone())
            {
                channel.gpadls.add(gpadl.request.id, buf);
            }
        }

        let result = RestoreResult {
            open_request,
            gpadls,
        };
        Ok(result)
    }

    fn handle_request(&mut self, request: VmbusRequest) {
        tracing::debug!(?request, "handle_request");
        match request {
            VmbusRequest::Reset(rpc) => self.handle_reset(rpc),
            VmbusRequest::Inspect(deferred) => {
                deferred.respond(|resp| {
                    resp.field("message_port", &self.inner.message_port)
                        .field("running", self.inner.running)
                        .field("hvsock_requests", self.inner.hvsock_requests)
                        .field_mut_with("unstick_channels", |v| {
                            let v: inspect::Value = if let Some(v) = v {
                                if v == "force" {
                                    self.unstick_channels(true);
                                    v.into()
                                } else {
                                    let v =
                                        v.parse().ok().context("expected false, true, or force")?;
                                    if v {
                                        self.unstick_channels(false);
                                    }
                                    v.into()
                                }
                            } else {
                                false.into()
                            };
                            anyhow::Ok(v)
                        })
                        .merge(&self.server.with_notifier(&mut self.inner));
                });
            }
            VmbusRequest::Save(rpc) => rpc.handle_sync(|()| SavedState {
                server: self.server.save(),
                lost_synic_bug_fixed: true,
            }),
            VmbusRequest::Restore(rpc) => rpc.handle_sync(|state| {
                self.unstick_on_start = !state.lost_synic_bug_fixed;
                self.server.restore(state.server)
            }),
            VmbusRequest::PostRestore(rpc) => {
                rpc.handle_sync(|()| self.server.with_notifier(&mut self.inner).post_restore())
            }
            VmbusRequest::Stop(rpc) => rpc.handle_sync(|()| {
                if self.inner.running {
                    self.inner.running = false;
                }
            }),
            VmbusRequest::Start => {
                if !self.inner.running {
                    self.inner.running = true;
                    if self.unstick_on_start {
                        tracing::info!(
                            "lost synic bug fix is not in yet, call unstick_channels to mitigate the issue."
                        );
                        self.unstick_channels(false);
                        self.unstick_on_start = false;
                    }
                }
            }
        }
    }

    fn handle_reset(&mut self, rpc: Rpc<(), ()>) {
        let needs_reset = self.inner.reset_done.is_empty();
        self.inner.reset_done.push(rpc);
        if needs_reset {
            self.server.with_notifier(&mut self.inner).reset();
        }
    }

    fn handle_relay_response(&mut self, response: ModifyConnectionResponse) {
        self.server
            .with_notifier(&mut self.inner)
            .complete_modify_connection(response);
    }

    fn handle_tl_connect_result(&mut self, result: HvsockConnectResult) {
        assert_ne!(self.inner.hvsock_requests, 0);
        self.inner.hvsock_requests -= 1;

        self.server
            .with_notifier(&mut self.inner)
            .send_tl_connect_result(result);
    }

    fn handle_synic_message(&mut self, message: SynicMessage) {
        match self
            .server
            .with_notifier(&mut self.inner)
            .handle_synic_message(message)
        {
            Ok(()) => {}
            Err(err) => {
                tracing::warn!(
                    error = &err as &dyn std::error::Error,
                    "synic message error"
                );
            }
        }
    }

    /// Handles a request forwarded by a different vmbus server. This is used to forward requests
    /// for different VTLs to different servers.
    ///
    /// N.B. This uses the same mechanism as the HCL server relay, so all requests, even the ones
    ///      meant for the primary server, are forwarded. In that case the primary server depends
    ///      on this server to send back a response so it can continue handling it.
    fn handle_external_request(&mut self, request: InitiateContactRequest) {
        self.server
            .with_notifier(&mut self.inner)
            .initiate_contact(request);
    }

    async fn run(
        &mut self,
        mut relay_response_recv: impl futures::stream::FusedStream<Item = ModifyConnectionResponse>
        + Unpin,
        mut hvsock_recv: impl futures::stream::FusedStream<Item = HvsockConnectResult> + Unpin,
    ) {
        loop {
            // Create an OptionFuture for each event that should only be handled
            // while the VM is running. In other cases, leave the events in
            // their respective queues.

            let running_not_resetting = self.inner.running && self.inner.reset_done.is_empty();
            let mut external_requests = OptionFuture::from(
                running_not_resetting
                    .then(|| {
                        self.external_requests
                            .as_mut()
                            .map(|r| r.select_next_some())
                    })
                    .flatten(),
            );

            // Try to send any pending messages while the VM is running.
            let has_pending_messages = self.server.has_pending_messages();
            let message_port = self.inner.message_port.as_mut();
            let mut flush_pending_messages =
                OptionFuture::from((running_not_resetting && has_pending_messages).then(|| {
                    poll_fn(|cx| {
                        self.server.poll_flush_pending_messages(|msg| {
                            message_port.poll_post_message(cx, VMBUS_MESSAGE_TYPE, msg.data())
                        })
                    })
                    .fuse()
                }));

            // Only handle new incoming messages if there are no outgoing messages pending, and not
            // too many hvsock requests outstanding. This puts a bound on the resources used by the
            // guest.
            let mut message_recv = OptionFuture::from(
                (running_not_resetting
                    && !has_pending_messages
                    && self.inner.hvsock_requests < MAX_CONCURRENT_HVSOCK_REQUESTS)
                    .then(|| self.message_recv.select_next_some()),
            );

            // Accept channel responses until stopped or when resetting.
            let mut channel_response = OptionFuture::from(
                (self.inner.running || !self.inner.reset_done.is_empty())
                    .then(|| self.inner.channel_responses.select_next_some()),
            );

            // Accept hvsock connect responses while the VM is running.
            let mut hvsock_response =
                OptionFuture::from(running_not_resetting.then(|| hvsock_recv.select_next_some()));

            futures::select! { // merge semantics
                r = self.task_recv.recv().fuse() => {
                    if let Ok(request) = r {
                        self.handle_request(request);
                    } else {
                        break;
                    }
                }
                r = self.offer_recv.select_next_some() => {
                    match r {
                        OfferRequest::Offer(rpc) => {
                            rpc.handle_failable_sync(|request| { self.handle_offer(request) })
                        },
                        OfferRequest::ForceReset(rpc) => {
                            self.handle_reset(rpc);
                        }
                    }
                }
                r = self.server_request_recv.select_next_some() => {
                    match r {
                        (id, Some(request)) => match request {
                            ChannelServerRequest::Restore(rpc) => rpc.handle_failable_sync(|open| {
                                self.handle_restore_channel(id, open)
                            }),
                            ChannelServerRequest::Revoke(rpc) => rpc.handle_sync(|_| {
                                self.handle_revoke(id);
                            })
                        },
                        (id, None) => self.handle_revoke(id),
                    }
                }
                r = channel_response => {
                    let (id, seq, response) = r.unwrap();
                    self.handle_response(id, seq, response);
                }
                r = relay_response_recv.select_next_some() => {
                    self.handle_relay_response(r);
                },
                r = hvsock_response => {
                    self.handle_tl_connect_result(r.unwrap());
                }
                data = message_recv => {
                    let data = data.unwrap();
                    self.handle_synic_message(data);
                }
                r = external_requests => {
                    let r = r.unwrap();
                    self.handle_external_request(r);
                }
                _r = flush_pending_messages => {}
                complete => break,
            }
        }
    }

    /// Wakes the host and guest for every open channel. If `force`, always
    /// wakes both the host and guest. If `!force`, only wake for rings that are
    /// in the state where a notification is expected.
    fn unstick_channels(&self, force: bool) {
        for channel in self.inner.channels.values() {
            if let Err(err) = self.unstick_channel(channel, force) {
                tracing::warn!(
                    channel = %channel.key,
                    error = err.as_ref() as &dyn std::error::Error,
                    "could not unstick channel"
                );
            }
        }
    }

    fn unstick_channel(&self, channel: &Channel, force: bool) -> anyhow::Result<()> {
        if let ChannelState::Open {
            open_params,
            host_to_guest_interrupt,
            guest_to_host_event,
            ..
        } = &channel.state
        {
            if force {
                tracing::info!(channel = %channel.key, "waking host and guest");
                guest_to_host_event.0.deliver();
                host_to_guest_interrupt.deliver();
                return Ok(());
            }

            let gpadl = channel
                .gpadls
                .clone()
                .view()
                .map(open_params.open_data.ring_gpadl_id)
                .context("couldn't find ring gpadl")?;

            let aligned = AlignedGpadlView::new(gpadl)
                .ok()
                .context("ring not aligned")?;
            let (in_gpadl, out_gpadl) = aligned
                .split(open_params.open_data.ring_offset)
                .ok()
                .context("couldn't split ring")?;

            if let Err(err) = self.unstick_incoming_ring(
                channel,
                in_gpadl,
                guest_to_host_event,
                host_to_guest_interrupt,
            ) {
                tracing::warn!(
                    channel = %channel.key,
                    error = err.as_ref() as &dyn std::error::Error,
                    "could not unstick incoming ring"
                );
            }
            if let Err(err) = self.unstick_outgoing_ring(
                channel,
                out_gpadl,
                guest_to_host_event,
                host_to_guest_interrupt,
            ) {
                tracing::warn!(
                    channel = %channel.key,
                    error = err.as_ref() as &dyn std::error::Error,
                    "could not unstick outgoing ring"
                );
            }
        }
        Ok(())
    }

    fn unstick_incoming_ring(
        &self,
        channel: &Channel,
        in_gpadl: AlignedGpadlView,
        guest_to_host_event: &ChannelEvent,
        host_to_guest_interrupt: &Interrupt,
    ) -> Result<(), anyhow::Error> {
        let incoming_mem = GpadlRingMem::new(in_gpadl, &self.inner.gm)?;
        if ring::reader_needs_signal(&incoming_mem) {
            tracing::info!(channel = %channel.key, "waking host for incoming ring");
            guest_to_host_event.0.deliver();
        }
        if ring::writer_needs_signal(&incoming_mem) {
            tracing::info!(channel = %channel.key, "waking guest for incoming ring");
            host_to_guest_interrupt.deliver();
        }
        Ok(())
    }

    fn unstick_outgoing_ring(
        &self,
        channel: &Channel,
        out_gpadl: AlignedGpadlView,
        guest_to_host_event: &ChannelEvent,
        host_to_guest_interrupt: &Interrupt,
    ) -> Result<(), anyhow::Error> {
        let outgoing_mem = GpadlRingMem::new(out_gpadl, &self.inner.gm)?;
        if ring::reader_needs_signal(&outgoing_mem) {
            tracing::info!(channel = %channel.key, "waking guest for outgoing ring");
            host_to_guest_interrupt.deliver();
        }
        if ring::writer_needs_signal(&outgoing_mem) {
            tracing::info!(channel = %channel.key, "waking host for outgoing ring");
            guest_to_host_event.0.deliver();
        }
        Ok(())
    }
}

impl Notifier for ServerTaskInner {
    fn notify(&mut self, offer_id: OfferId, action: channels::Action) {
        let channel = self
            .channels
            .get_mut(&offer_id)
            .expect("channel does not exist");

        fn handle<I: 'static + Send, R: 'static + Send>(
            offer_id: OfferId,
            channel: &Channel,
            req: impl FnOnce(Rpc<I, R>) -> ChannelRequest,
            input: I,
            f: impl 'static + Send + FnOnce(R) -> ChannelResponse,
        ) -> Pin<Box<dyn Send + Future<Output = (OfferId, u64, Result<ChannelResponse, RpcError>)>>>
        {
            let recv = channel.send.call(req, input);
            let seq = channel.seq;
            Box::pin(async move {
                let r = recv.await.map(f);
                (offer_id, seq, r)
            })
        }

        let response = match action {
            channels::Action::Open(open_params, version) => {
                let seq = channel.seq;
                match self.open_channel(offer_id, &open_params) {
                    Ok((channel, interrupt)) => handle(
                        offer_id,
                        channel,
                        ChannelRequest::Open,
                        OpenRequest::new(
                            open_params.open_data,
                            interrupt,
                            version.feature_flags,
                            channel.flags,
                        ),
                        ChannelResponse::Open,
                    ),
                    Err(err) => {
                        tracelimit::error_ratelimited!(
                            err = err.as_ref() as &dyn std::error::Error,
                            ?offer_id,
                            "could not open channel",
                        );

                        // Return an error response to the channels module if the open_channel call
                        // failed.
                        Box::pin(future::ready((
                            offer_id,
                            seq,
                            Ok(ChannelResponse::Open(None)),
                        )))
                    }
                }
            }
            channels::Action::Close => {
                if let Some(channel_bitmap) = self.channel_bitmap.as_ref() {
                    if let ChannelState::Open { open_params, .. } = channel.state {
                        channel_bitmap.unregister_channel(open_params.event_flag);
                    }
                }

                channel.guest_event_port.clear();
                handle(offer_id, channel, ChannelRequest::Close, (), |()| {
                    ChannelResponse::Close
                })
            }
            channels::Action::Gpadl(gpadl_id, count, buf) => {
                channel.gpadls.add(
                    gpadl_id,
                    MultiPagedRangeBuf::new(count.into(), buf.clone()).unwrap(),
                );
                handle(
                    offer_id,
                    channel,
                    ChannelRequest::Gpadl,
                    GpadlRequest {
                        id: gpadl_id,
                        count,
                        buf,
                    },
                    move |r| ChannelResponse::Gpadl(gpadl_id, r),
                )
            }
            channels::Action::TeardownGpadl {
                gpadl_id,
                post_restore,
            } => {
                if !post_restore {
                    channel.gpadls.remove(gpadl_id, Box::new(|| ()));
                }

                handle(
                    offer_id,
                    channel,
                    ChannelRequest::TeardownGpadl,
                    gpadl_id,
                    move |()| ChannelResponse::TeardownGpadl(gpadl_id),
                )
            }
            channels::Action::Modify { target_vp } => {
                if let ChannelState::Open { open_params, .. } = channel.state {
                    let (target_vtl, target_sint) = if open_params.flags.redirect_interrupt() {
                        (self.redirect_vtl, self.redirect_sint)
                    } else {
                        (self.vtl, SINT)
                    };

                    if let Err(err) = channel.guest_event_port.set(
                        target_vtl,
                        target_vp,
                        target_sint,
                        open_params.event_flag,
                    ) {
                        tracelimit::error_ratelimited!(
                            error = &err as &dyn std::error::Error,
                            channel = %channel.key,
                            "could not modify channel",
                        );
                        let seq = channel.seq;
                        Box::pin(async move {
                            (
                                offer_id,
                                seq,
                                Ok(ChannelResponse::Modify(protocol::STATUS_UNSUCCESSFUL)),
                            )
                        })
                    } else {
                        handle(
                            offer_id,
                            channel,
                            ChannelRequest::Modify,
                            ModifyRequest::TargetVp { target_vp },
                            ChannelResponse::Modify,
                        )
                    }
                } else {
                    unreachable!();
                }
            }
        };
        self.channel_responses.push(response);
    }

    fn modify_connection(&mut self, mut request: ModifyConnectionRequest) -> anyhow::Result<()> {
        self.map_interrupt_page(request.interrupt_page)
            .context("Failed to map interrupt page.")?;

        self.set_monitor_page(request.monitor_page, request.force)
            .context("Failed to map monitor page.")?;

        if let Some(vp) = request.target_message_vp {
            self.message_port.set_target_vp(vp)?;
        }

        if request.notify_relay {
            // If this server is handling MNF, the monitor pages should not be relayed.
            // N.B. Since the relay is being asked not to update the monitor pages, rather than
            //      reset them, this is only safe because the value of enable_mnf won't change after
            //      the server has been created.
            if self.enable_mnf {
                request.monitor_page = Update::Unchanged;
            }

            self.relay_send.send(request.into());
        }

        Ok(())
    }

    fn forward_unhandled(&mut self, request: InitiateContactRequest) {
        if let Some(external_server) = &self.external_server_send {
            external_server.send(request);
        } else {
            tracing::warn!(?request, "nowhere to forward unhandled request")
        }
    }

    fn inspect(&self, version: Option<VersionInfo>, offer_id: OfferId, req: inspect::Request<'_>) {
        let channel = self.channels.get(&offer_id).expect("should exist");
        let mut resp = req.respond();
        if let ChannelState::Open { open_params, .. } = &channel.state {
            let mem = if self.private_gm.is_some()
                && channel.flags.confidential_ring_buffer()
                && version
                    .expect("must be connected")
                    .feature_flags
                    .confidential_channels()
            {
                self.private_gm.as_ref().unwrap()
            } else {
                &self.gm
            };

            inspect_rings(
                &mut resp,
                mem,
                channel.gpadls.clone(),
                &open_params.open_data,
            );
        }
    }

    fn send_message(&mut self, message: &OutgoingMessage, target: MessageTarget) -> bool {
        // If the server is paused, queue all messages, to avoid affecting synic
        // state during/after it has been saved or reset.
        //
        // Note that messages to reserved channels or custom targets will be
        // dropped. However, such messages should only be sent in response to
        // guest requests, which should not be processed while the server is
        // paused.
        //
        // FUTURE: it would be better to ensure that no messages are generated
        // by operations that run while the server is paused. E.g., defer
        // sending offer or revoke messages for new or revoked offers. This
        // would prevent the queue from growing without bound.
        if !self.running && !self.send_messages_while_stopped {
            if !matches!(target, MessageTarget::Default) {
                tracelimit::error_ratelimited!(?target, "dropping message while paused");
            }
            return false;
        }

        let mut port_storage;
        let port = match target {
            MessageTarget::Default => self.message_port.as_mut(),
            MessageTarget::ReservedChannel(offer_id, target) => {
                if let Some(port) = self.get_reserved_channel_message_port(offer_id, target) {
                    port.as_mut()
                } else {
                    // Updating the port failed, so there is no way to send the message.
                    return true;
                }
            }
            MessageTarget::Custom(target) => {
                port_storage = match self.synic.new_guest_message_port(
                    self.redirect_vtl,
                    target.vp,
                    target.sint,
                ) {
                    Ok(port) => port,
                    Err(err) => {
                        tracing::error!(
                            ?err,
                            ?self.redirect_vtl,
                            ?target,
                            "could not create message port"
                        );

                        // There is no way to send the message.
                        return true;
                    }
                };
                port_storage.as_mut()
            }
        };

        // If this returns Pending, the channels module will queue the message and the ServerTask
        // main loop will try to send it again later.
        matches!(
            port.poll_post_message(
                &mut std::task::Context::from_waker(std::task::Waker::noop()),
                VMBUS_MESSAGE_TYPE,
                message.data()
            ),
            Poll::Ready(())
        )
    }

    fn notify_hvsock(&mut self, request: &HvsockConnectRequest) {
        self.hvsock_requests += 1;
        self.hvsock_send.send(*request);
    }

    fn reset_complete(&mut self) {
        if let Some(monitor) = self.synic.monitor_support() {
            if let Err(err) = monitor.set_monitor_page(None) {
                tracing::warn!(?err, "resetting monitor page failed")
            }
        }

        self.unreserve_channels();
        for done in self.reset_done.drain(..) {
            done.complete(());
        }
    }

    fn unload_complete(&mut self) {
        self.unreserve_channels();
    }
}

impl ServerTaskInner {
    fn open_channel(
        &mut self,
        offer_id: OfferId,
        open_params: &OpenParams,
    ) -> anyhow::Result<(&mut Channel, Interrupt)> {
        let channel = self
            .channels
            .get_mut(&offer_id)
            .expect("channel does not exist");

        // For pre-Win8 guests, the host-to-guest event always targets vp 0 and the channel
        // bitmap is used instead of the event flag.
        let (target_vp, event_flag) = if self.channel_bitmap.is_some() {
            (0, 0)
        } else {
            (open_params.open_data.target_vp, open_params.event_flag)
        };
        let (target_vtl, target_sint) = if open_params.flags.redirect_interrupt() {
            (self.redirect_vtl, self.redirect_sint)
        } else {
            (self.vtl, SINT)
        };
        channel
            .guest_event_port
            .set(target_vtl, target_vp, target_sint, event_flag)?;

        let interrupt = ChannelBitmap::create_interrupt(
            &self.channel_bitmap,
            channel.guest_event_port.interrupt(),
            open_params.event_flag,
        );

        let monitor = open_params.monitor_id.and_then(|monitor_id| {
            self.synic
                .monitor_support()
                .map(|monitor| monitor.register_monitor(monitor_id, open_params.connection_id))
        });

        // Delete any previously reserved state.
        channel.reserved_state.message_port = None;

        // If the channel is reserved, create a message port for it.
        if let Some(target) = open_params.reserved_target {
            channel.reserved_state.message_port = Some(self.synic.new_guest_message_port(
                self.redirect_vtl,
                target.vp,
                target.sint,
            )?);

            channel.reserved_state.target = target;
        }

        channel.state = ChannelState::Opening {
            open_params: *open_params,
            monitor,
            host_to_guest_interrupt: interrupt.clone(),
        };
        Ok((channel, interrupt))
    }

    fn complete_open(
        &mut self,
        offer_id: OfferId,
        result: Option<OpenResult>,
    ) -> anyhow::Result<&mut Channel> {
        let channel = self
            .channels
            .get_mut(&offer_id)
            .expect("channel does not exist");

        channel.state = if let Some(result) = result {
            // The channel will be left in the FailedOpen state only if an error occurs in the match
            // arm.
            match std::mem::replace(&mut channel.state, ChannelState::FailedOpen) {
                ChannelState::Opening {
                    open_params,
                    monitor,
                    host_to_guest_interrupt,
                } => {
                    let guest_to_host_event =
                        Arc::new(ChannelEvent(result.guest_to_host_interrupt));
                    // Always register with the channel bitmap; if Win7, this may be unnecessary.
                    if let Some(channel_bitmap) = self.channel_bitmap.as_ref() {
                        channel_bitmap.register_channel(
                            open_params.event_flag,
                            guest_to_host_event.0.clone(),
                        );
                    }
                    // Always set up an event port; if V1, this will be unused.
                    let event_port = self
                        .synic
                        .add_event_port(
                            open_params.connection_id,
                            self.vtl,
                            guest_to_host_event.clone(),
                        )
                        .with_context(|| {
                            format!(
                                "failed to create event port for VTL {:?}, connection ID {:#x}",
                                self.vtl, open_params.connection_id
                            )
                        })?;

                    ChannelState::Open {
                        open_params,
                        _event_port: event_port,
                        monitor,
                        host_to_guest_interrupt,
                        guest_to_host_event,
                    }
                }
                s => {
                    tracing::error!("attempting to complete open of open or closed channel");
                    // Restore the original state
                    s
                }
            }
        } else {
            ChannelState::Closed
        };
        Ok(channel)
    }

    /// If the client specified an interrupt page, map it into host memory and
    /// set up the shared event port.
    fn map_interrupt_page(&mut self, interrupt_page: Update<u64>) -> anyhow::Result<()> {
        let interrupt_page = match interrupt_page {
            Update::Unchanged => return Ok(()),
            Update::Reset => {
                self.channel_bitmap = None;
                self.shared_event_port = None;
                return Ok(());
            }
            Update::Set(interrupt_page) => interrupt_page,
        };

        assert_ne!(interrupt_page, 0);

        if interrupt_page % PAGE_SIZE as u64 != 0 {
            anyhow::bail!("interrupt page {:#x} is not page aligned", interrupt_page);
        }

        // Use a subrange to access the interrupt page to give GuestMemory's without a full mapping
        // a chance to create one.
        let interrupt_page = self
            .gm
            .lockable_subrange(interrupt_page, PAGE_SIZE as u64)?
            .lock_gpns(false, &[0])?;

        let channel_bitmap = Arc::new(ChannelBitmap::new(interrupt_page));
        self.channel_bitmap = Some(channel_bitmap.clone());

        // Create the shared event port for pre-Win8 guests.
        let interrupt = Interrupt::from_fn(move || {
            channel_bitmap.handle_shared_interrupt();
        });

        self.shared_event_port = Some(self.synic.add_event_port(
            SHARED_EVENT_CONNECTION_ID,
            self.vtl,
            Arc::new(ChannelEvent(interrupt)),
        )?);

        Ok(())
    }

    fn set_monitor_page(
        &mut self,
        monitor_page: Update<MonitorPageGpas>,
        force: bool,
    ) -> anyhow::Result<()> {
        let monitor_page = match monitor_page {
            Update::Unchanged => return Ok(()),
            Update::Reset => None,
            Update::Set(value) => Some(value),
        };

        // Force is used by restore because there may be restored channels in the open state.
        // TODO: can this check be moved into channels.rs?
        if !force
            && self.channels.iter().any(|(_, c)| {
                matches!(
                    &c.state,
                    ChannelState::Open {
                        monitor: Some(_),
                        ..
                    } | ChannelState::Opening {
                        monitor: Some(_),
                        ..
                    }
                )
            })
        {
            anyhow::bail!("attempt to change monitor page while open channels using mnf");
        }

        if self.enable_mnf {
            if let Some(monitor) = self.synic.monitor_support() {
                if let Err(err) =
                    monitor.set_monitor_page(monitor_page.map(|mp| mp.child_to_parent))
                {
                    anyhow::bail!(
                        "setting monitor page failed, err = {err:?}, monitor_page = {monitor_page:?}"
                    );
                }
            }
        }

        Ok(())
    }

    fn get_reserved_channel_message_port(
        &mut self,
        offer_id: OfferId,
        new_target: ConnectionTarget,
    ) -> Option<&mut Box<dyn GuestMessagePort>> {
        let channel = self
            .channels
            .get_mut(&offer_id)
            .expect("channel does not exist");

        assert!(
            channel.reserved_state.message_port.is_some(),
            "channel is not reserved"
        );

        // On close, the guest may have changed the message target it wants to use for the close
        // response. If so, update the message port.
        if channel.reserved_state.target.sint != new_target.sint {
            // Destroy the old port before creating the new one.
            channel.reserved_state.message_port = None;
            let message_port = self
                .synic
                .new_guest_message_port(self.redirect_vtl, new_target.vp, new_target.sint)
                .inspect_err(|err| {
                    tracing::error!(
                        ?err,
                        ?self.redirect_vtl,
                        ?new_target,
                        "could not create reserved channel message port"
                    )
                })
                .ok()?;

            channel.reserved_state.message_port = Some(message_port);
            channel.reserved_state.target = new_target;
        } else if channel.reserved_state.target.vp != new_target.vp {
            let message_port = channel.reserved_state.message_port.as_mut().unwrap();

            // The vp has changed, but the SINT is the same. Just update the vp. If this fails,
            // ignore it and just send to the old vp.
            if let Err(err) = message_port.set_target_vp(new_target.vp) {
                tracing::error!(
                    ?err,
                    ?self.redirect_vtl,
                    ?new_target,
                    "could not update reserved channel message port"
                );
            }

            channel.reserved_state.target = new_target;
            return Some(message_port);
        }

        Some(channel.reserved_state.message_port.as_mut().unwrap())
    }

    fn unreserve_channels(&mut self) {
        // Unreserve all closed channels.
        for channel in self.channels.values_mut() {
            if let ChannelState::Closed = channel.state {
                channel.reserved_state.message_port = None;
            }
        }
    }
}

/// Control point for [`VmbusServer`], allowing callers to offer channels.
#[derive(Clone)]
pub struct VmbusServerControl {
    mem: GuestMemory,
    private_mem: Option<GuestMemory>,
    send: mesh::Sender<OfferRequest>,
    use_event: bool,
    force_confidential_external_memory: bool,
}

impl VmbusServerControl {
    /// Offers a channel to the vmbus server, where the flags and user_defined data are already set.
    /// This is used by the relay to forward the host's parameters.
    pub async fn offer_core(&self, offer_info: OfferInfo) -> anyhow::Result<OfferResources> {
        let flags = offer_info.params.flags;
        self.send
            .call_failable(OfferRequest::Offer, offer_info)
            .await?;
        Ok(OfferResources::new(
            self.mem.clone(),
            if flags.confidential_ring_buffer() || flags.confidential_external_memory() {
                self.private_mem.clone()
            } else {
                None
            },
        ))
    }

    /// Force reset all channels and protocol state, without requiring the
    /// server to be paused.
    pub async fn force_reset(&self) -> anyhow::Result<()> {
        self.send
            .call(OfferRequest::ForceReset, ())
            .await
            .context("vmbus server is gone")
    }

    async fn offer(&self, request: OfferInput) -> anyhow::Result<OfferResources> {
        let mut offer_info = OfferInfo {
            params: request.params.into(),
            request_send: request.request_send,
            server_request_recv: request.server_request_recv,
        };

        if self.force_confidential_external_memory {
            tracing::warn!(
                key = %offer_info.params.key(),
                "forcing confidential external memory for channel"
            );

            offer_info
                .params
                .flags
                .set_confidential_external_memory(true);
        }

        self.offer_core(offer_info).await
    }
}

/// Inspects the specified ring buffer state by directly accessing guest memory.
fn inspect_rings(
    resp: &mut inspect::Response<'_>,
    gm: &GuestMemory,
    gpadl_map: Arc<GpadlMap>,
    open_data: &OpenData,
) -> Option<()> {
    let gpadl = gpadl_map
        .view()
        .map(GpadlId(open_data.ring_gpadl_id.0))
        .ok()?;
    let aligned = AlignedGpadlView::new(gpadl).ok()?;
    let (in_gpadl, out_gpadl) = aligned.split(open_data.ring_offset).ok()?;
    if let Ok(incoming_mem) = GpadlRingMem::new(in_gpadl, gm) {
        resp.child("incoming_ring", |req| ring::inspect_ring(incoming_mem, req));
    }
    if let Ok(outgoing_mem) = GpadlRingMem::new(out_gpadl, gm) {
        resp.child("outgoing_ring", |req| ring::inspect_ring(outgoing_mem, req));
    }
    Some(())
}

pub(crate) struct MessageSender {
    send: mpsc::Sender<SynicMessage>,
    multiclient: bool,
}

impl MessageSender {
    fn poll_handle_message(
        &self,
        cx: &mut std::task::Context<'_>,
        msg: &[u8],
        trusted: bool,
    ) -> Poll<Result<(), SendError>> {
        let mut send = self.send.clone();
        ready!(send.poll_ready(cx))?;
        send.start_send(SynicMessage {
            data: msg.to_vec(),
            multiclient: self.multiclient,
            trusted,
        })?;

        Poll::Ready(Ok(()))
    }
}

impl MessagePort for MessageSender {
    fn poll_handle_message(
        &self,
        cx: &mut std::task::Context<'_>,
        msg: &[u8],
        trusted: bool,
    ) -> Poll<()> {
        if let Err(err) = ready!(self.poll_handle_message(cx, msg, trusted)) {
            tracelimit::error_ratelimited!(
                error = &err as &dyn std::error::Error,
                "failed to send message"
            );
        }

        Poll::Ready(())
    }
}

#[async_trait]
impl ParentBus for VmbusServerControl {
    async fn add_child(&self, request: OfferInput) -> anyhow::Result<OfferResources> {
        self.offer(request).await
    }

    fn clone_bus(&self) -> Box<dyn ParentBus> {
        Box::new(self.clone())
    }

    fn use_event(&self) -> bool {
        self.use_event
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use inspect::InspectMut;
    use mesh::CancelReason;
    use pal_async::DefaultDriver;
    use pal_async::async_test;
    use pal_async::driver::SpawnDriver;
    use pal_async::timer::Instant;
    use pal_async::timer::PolledTimer;
    use parking_lot::Mutex;
    use protocol::UserDefinedData;
    use std::time::Duration;
    use test_with_tracing::test;
    use vmbus_channel::bus::OfferParams;
    use vmbus_channel::channel::ChannelOpenError;
    use vmbus_channel::channel::DeviceResources;
    use vmbus_channel::channel::SaveRestoreVmbusDevice;
    use vmbus_channel::channel::VmbusDevice;
    use vmbus_channel::channel::offer_channel;
    use vmbus_core::protocol::ChannelId;
    use vmbus_core::protocol::VmbusMessage;
    use vmcore::synic::SynicPortAccess;
    use zerocopy::FromBytes;
    use zerocopy::Immutable;
    use zerocopy::IntoBytes;
    use zerocopy::KnownLayout;

    struct MockSynicInner {
        message_port: Option<Arc<dyn MessagePort>>,
    }

    struct MockSynic {
        inner: Mutex<MockSynicInner>,
        message_send: mesh::Sender<Vec<u8>>,
        spawner: Arc<dyn SpawnDriver>,
    }

    impl MockSynic {
        fn new(message_send: mesh::Sender<Vec<u8>>, spawner: Arc<dyn SpawnDriver>) -> Self {
            Self {
                inner: Mutex::new(MockSynicInner { message_port: None }),
                message_send,
                spawner,
            }
        }

        fn send_message(&self, msg: impl VmbusMessage + IntoBytes + Immutable + KnownLayout) {
            self.send_message_core(OutgoingMessage::new(&msg), false);
        }

        fn send_message_trusted(
            &self,
            msg: impl VmbusMessage + IntoBytes + Immutable + KnownLayout,
        ) {
            self.send_message_core(OutgoingMessage::new(&msg), true);
        }

        fn send_message_core(&self, msg: OutgoingMessage, trusted: bool) {
            assert_eq!(
                self.inner
                    .lock()
                    .message_port
                    .as_ref()
                    .unwrap()
                    .poll_handle_message(
                        &mut std::task::Context::from_waker(std::task::Waker::noop()),
                        msg.data(),
                        trusted,
                    ),
                Poll::Ready(())
            );
        }
    }

    #[derive(Debug)]
    struct MockGuestPort {}

    impl GuestEventPort for MockGuestPort {
        fn interrupt(&self) -> Interrupt {
            Interrupt::null()
        }

        fn clear(&mut self) {}

        fn set(
            &mut self,
            _vtl: Vtl,
            _vp: u32,
            _sint: u8,
            _flag: u16,
        ) -> Result<(), vmcore::synic::HypervisorError> {
            Ok(())
        }
    }

    struct MockGuestMessagePort {
        send: mesh::Sender<Vec<u8>>,
        spawner: Arc<dyn SpawnDriver>,
        timer: Option<(PolledTimer, Instant)>,
    }

    impl GuestMessagePort for MockGuestMessagePort {
        fn poll_post_message(
            &mut self,
            cx: &mut std::task::Context<'_>,
            _typ: u32,
            payload: &[u8],
        ) -> Poll<()> {
            if let Some((timer, deadline)) = self.timer.as_mut() {
                ready!(timer.sleep_until(*deadline).poll_unpin(cx));
                self.timer = None;
            }

            // Return pending 25% of the time.
            let mut pending_chance = [0; 1];
            getrandom::fill(&mut pending_chance).unwrap();
            if pending_chance[0] % 4 == 0 {
                let mut timer = PolledTimer::new(self.spawner.as_ref());
                let deadline = Instant::now() + Duration::from_millis(10);
                match timer.sleep_until(deadline).poll_unpin(cx) {
                    Poll::Ready(_) => {}
                    Poll::Pending => {
                        self.timer = Some((timer, deadline));
                        return Poll::Pending;
                    }
                }
            }

            self.send.send(payload.into());
            Poll::Ready(())
        }

        fn set_target_vp(&mut self, _vp: u32) -> Result<(), vmcore::synic::HypervisorError> {
            Ok(())
        }
    }

    impl Inspect for MockGuestMessagePort {
        fn inspect(&self, _req: inspect::Request<'_>) {}
    }

    impl SynicPortAccess for MockSynic {
        fn add_message_port(
            &self,
            connection_id: u32,
            _minimum_vtl: Vtl,
            port: Arc<dyn MessagePort>,
        ) -> Result<Box<dyn Sync + Send>, vmcore::synic::Error> {
            self.inner.lock().message_port = Some(port);
            Ok(Box::new(connection_id))
        }

        fn add_event_port(
            &self,
            connection_id: u32,
            _minimum_vtl: Vtl,
            _port: Arc<dyn EventPort>,
        ) -> Result<Box<dyn Sync + Send>, vmcore::synic::Error> {
            Ok(Box::new(connection_id))
        }

        fn new_guest_message_port(
            &self,
            _vtl: Vtl,
            _vp: u32,
            _sint: u8,
        ) -> Result<Box<(dyn GuestMessagePort)>, vmcore::synic::HypervisorError> {
            Ok(Box::new(MockGuestMessagePort {
                send: self.message_send.clone(),
                spawner: Arc::clone(&self.spawner),
                timer: None,
            }))
        }

        fn new_guest_event_port(
            &self,
        ) -> Result<Box<(dyn GuestEventPort)>, vmcore::synic::HypervisorError> {
            Ok(Box::new(MockGuestPort {}))
        }

        fn prefer_os_events(&self) -> bool {
            false
        }
    }

    struct TestChannel {
        request_recv: mesh::Receiver<ChannelRequest>,
        server_request_send: mesh::Sender<ChannelServerRequest>,
        _resources: OfferResources,
    }

    impl TestChannel {
        async fn next_request(&mut self) -> ChannelRequest {
            self.request_recv.next().await.unwrap()
        }

        async fn handle_gpadl(&mut self) {
            let ChannelRequest::Gpadl(rpc) = self.next_request().await else {
                panic!("Wrong request");
            };

            rpc.complete(true);
        }

        async fn handle_open(&mut self, f: fn(&OpenRequest)) {
            let ChannelRequest::Open(rpc) = self.next_request().await else {
                panic!("Wrong request");
            };

            f(rpc.input());
            rpc.complete(Some(OpenResult {
                guest_to_host_interrupt: Interrupt::null(),
            }));
        }

        async fn handle_gpadl_teardown(&mut self) {
            let rpc = self.get_gpadl_teardown().await;
            rpc.complete(());
        }

        async fn get_gpadl_teardown(&mut self) -> Rpc<GpadlId, ()> {
            let ChannelRequest::TeardownGpadl(rpc) = self.next_request().await else {
                panic!("Wrong request");
            };

            rpc
        }

        async fn restore(&self) {
            self.server_request_send
                .call(ChannelServerRequest::Restore, None)
                .await
                .unwrap()
                .unwrap();
        }
    }

    struct TestEnv {
        vmbus: VmbusServer,
        synic: Arc<MockSynic>,
        message_recv: mesh::Receiver<Vec<u8>>,
        trusted: bool,
    }

    impl TestEnv {
        fn new(spawner: DefaultDriver) -> Self {
            let spawner: Arc<dyn SpawnDriver> = Arc::new(spawner);
            let (message_send, message_recv) = mesh::channel();
            let synic = Arc::new(MockSynic::new(message_send, Arc::clone(&spawner)));
            let gm = GuestMemory::empty();
            let vmbus = VmbusServerBuilder::new(&spawner, synic.clone(), gm)
                .build()
                .unwrap();

            Self {
                vmbus,
                synic,
                message_recv,
                trusted: false,
            }
        }

        async fn offer(&self, id: u32, allow_confidential_external_memory: bool) -> TestChannel {
            let guid = Guid {
                data1: id,
                ..Guid::ZERO
            };
            let (request_send, request_recv) = mesh::channel();
            let (server_request_send, server_request_recv) = mesh::channel();
            let offer = OfferInput {
                request_send,
                server_request_recv,
                params: OfferParams {
                    interface_name: "test".into(),
                    instance_id: guid,
                    interface_id: guid,
                    mmio_megabytes: 0,
                    mmio_megabytes_optional: 0,
                    channel_type: vmbus_channel::bus::ChannelType::Device {
                        pipe_packets: false,
                    },
                    subchannel_index: 0,
                    use_mnf: false,
                    offer_order: None,
                    allow_confidential_external_memory,
                },
            };

            let control = self.vmbus.control();
            let _resources = control.add_child(offer).await.unwrap();

            TestChannel {
                request_recv,
                server_request_send,
                _resources,
            }
        }

        async fn gpadl(&mut self, channel_id: u32, gpadl_id: u32, channel: &mut TestChannel) {
            self.synic.send_message_core(
                OutgoingMessage::with_data(
                    &protocol::GpadlHeader {
                        channel_id: ChannelId(channel_id),
                        gpadl_id: GpadlId(gpadl_id),
                        count: 1,
                        len: 16,
                    },
                    [1u64, 0u64].as_bytes(),
                ),
                self.trusted,
            );

            channel.handle_gpadl().await;
            self.expect_response(protocol::MessageType::GPADL_CREATED)
                .await;
        }

        async fn open_channel(
            &mut self,
            channel_id: u32,
            ring_gpadl_id: u32,
            channel: &mut TestChannel,
            f: fn(&OpenRequest),
        ) {
            self.gpadl(channel_id, ring_gpadl_id, channel).await;
            self.synic.send_message_core(
                OutgoingMessage::new(&protocol::OpenChannel {
                    channel_id: ChannelId(channel_id),
                    open_id: 0,
                    ring_buffer_gpadl_id: GpadlId(ring_gpadl_id),
                    target_vp: 0,
                    downstream_ring_buffer_page_offset: 0,
                    user_data: UserDefinedData::default(),
                }),
                self.trusted,
            );

            channel.handle_open(f).await;
            self.expect_response(protocol::MessageType::OPEN_CHANNEL_RESULT)
                .await;
        }

        async fn expect_response(&mut self, expected: protocol::MessageType) {
            let data = self.message_recv.next().await.unwrap();
            let header = protocol::MessageHeader::read_from_prefix(&data).unwrap().0; // TODO: zerocopy: use-rest-of-range (https://github.com/microsoft/openvmm/issues/759)
            assert_eq!(expected, header.message_type())
        }

        async fn get_response<T: VmbusMessage + FromBytes + Immutable + KnownLayout>(
            &mut self,
        ) -> T {
            let data = self.message_recv.next().await.unwrap();
            let (header, message) = protocol::MessageHeader::read_from_prefix(&data).unwrap(); // TODO: zerocopy: unwrap (https://github.com/microsoft/openvmm/issues/759)
            assert_eq!(T::MESSAGE_TYPE, header.message_type());
            T::read_from_prefix(message).unwrap().0 // TODO: zerocopy: use-rest-of-range (https://github.com/microsoft/openvmm/issues/759)
        }

        fn initiate_contact(
            &mut self,
            version: protocol::Version,
            feature_flags: protocol::FeatureFlags,
            trusted: bool,
        ) {
            self.synic.send_message_core(
                OutgoingMessage::new(&protocol::InitiateContact {
                    version_requested: version as u32,
                    target_message_vp: 0,
                    child_to_parent_monitor_page_gpa: 0,
                    parent_to_child_monitor_page_gpa: 0,
                    interrupt_page_or_target_info: protocol::TargetInfo::new()
                        .with_sint(2)
                        .with_vtl(0)
                        .with_feature_flags(feature_flags.into())
                        .into(),
                }),
                trusted,
            );

            self.trusted = trusted;
        }

        async fn connect(
            &mut self,
            offer_count: u32,
            feature_flags: protocol::FeatureFlags,
            trusted: bool,
        ) {
            self.initiate_contact(protocol::Version::Copper, feature_flags, trusted);

            self.expect_response(protocol::MessageType::VERSION_RESPONSE)
                .await;

            self.synic
                .send_message_core(OutgoingMessage::new(&protocol::RequestOffers {}), trusted);

            for _ in 0..offer_count {
                self.expect_response(protocol::MessageType::OFFER_CHANNEL)
                    .await;
            }

            self.expect_response(protocol::MessageType::ALL_OFFERS_DELIVERED)
                .await;
        }
    }

    #[async_test]
    async fn test_save_restore(spawner: DefaultDriver) {
        // Most save/restore state is tested in mod channels::tests; this test specifically checks
        // that ServerTaskInner correctly handles some aspects of the save/restore.
        //
        // If this test fails, it is more likely to hang than panic.
        let mut env = TestEnv::new(spawner);
        let mut channel = env.offer(1, false).await;
        env.vmbus.start();
        env.connect(1, protocol::FeatureFlags::new(), false).await;

        // Create a GPADL for the channel.
        env.gpadl(1, 10, &mut channel).await;

        // Start tearing it down.
        env.synic.send_message(protocol::GpadlTeardown {
            channel_id: ChannelId(1),
            gpadl_id: GpadlId(10),
        });

        // Wait for the teardown request here to make sure the server has processed the teardown
        // message, but do not complete it before saving.
        let rpc = channel.get_gpadl_teardown().await;
        env.vmbus.stop().await;
        let saved_state = env.vmbus.save().await;
        env.vmbus.start();

        // Finish tearing down the gpadl and release the channel so the server can reset.
        rpc.complete(());
        env.expect_response(protocol::MessageType::GPADL_TORNDOWN)
            .await;

        env.synic.send_message(protocol::RelIdReleased {
            channel_id: ChannelId(1),
        });

        env.vmbus.reset().await;
        env.vmbus.stop().await;

        // When restoring with a gpadl in the TearingDown state, the teardown request for the device
        // will be repeated. This must not panic.
        env.vmbus.restore(saved_state).await.unwrap();
        channel.restore().await;
        env.vmbus.post_restore().await.unwrap();
        env.vmbus.start();

        // Handle the teardown after restore.
        channel.handle_gpadl_teardown().await;
        env.expect_response(protocol::MessageType::GPADL_TORNDOWN)
            .await;

        env.synic.send_message(protocol::RelIdReleased {
            channel_id: ChannelId(1),
        });
    }

    struct TestDeviceState {
        id: u32,
        started: bool,
        resources: Option<DeviceResources>,
        open_requests: HashMap<u16, OpenRequest>,
        target_vps: HashMap<u16, u32>,
    }

    impl TestDeviceState {
        pub fn id(this: &Arc<Mutex<Self>>) -> u32 {
            this.lock().id
        }

        pub fn started(this: &Arc<Mutex<Self>>) -> bool {
            this.lock().started
        }
        pub fn set_started(this: &Arc<Mutex<Self>>, started: bool) {
            this.lock().started = started;
        }

        pub fn open_request(this: &Arc<Mutex<Self>>, channel_idx: u16) -> Option<OpenRequest> {
            this.lock().open_requests.get(&channel_idx).cloned()
        }
        pub fn set_open_request(
            this: &Arc<Mutex<Self>>,
            channel_idx: u16,
            open_request: OpenRequest,
        ) {
            assert!(
                this.lock()
                    .open_requests
                    .insert(channel_idx, open_request)
                    .is_none()
            );
        }
        pub fn remove_open_request(
            this: &Arc<Mutex<Self>>,
            channel_idx: u16,
        ) -> Option<OpenRequest> {
            this.lock().open_requests.remove(&channel_idx)
        }

        pub fn target_vp(this: &Arc<Mutex<Self>>, channel_idx: u16) -> Option<u32> {
            this.lock().target_vps.get(&channel_idx).copied()
        }
        pub fn set_target_vp(this: &Arc<Mutex<Self>>, channel_idx: u16, target_vp: u32) {
            let _ = this.lock().target_vps.insert(channel_idx, target_vp);
        }
    }

    #[derive(InspectMut)]
    struct TestDevice {
        #[inspect(skip)]
        pub state: Arc<Mutex<TestDeviceState>>,
    }

    impl TestDevice {
        pub fn new_and_state(id: u32) -> (Self, Arc<Mutex<TestDeviceState>>) {
            let state = TestDeviceState {
                id,
                resources: None,
                open_requests: HashMap::new(),
                target_vps: HashMap::new(),
                started: false,
            };
            let state = Arc::new(Mutex::new(state));
            let this = Self {
                state: state.clone(),
            };
            (this, state)
        }
    }

    #[async_trait]
    impl VmbusDevice for TestDevice {
        fn offer(&self) -> OfferParams {
            let guid = Guid {
                data1: TestDeviceState::id(&self.state),
                ..Guid::ZERO
            };

            OfferParams {
                interface_name: "test".into(),
                instance_id: guid,
                interface_id: guid,
                mmio_megabytes: 0,
                mmio_megabytes_optional: 0,
                channel_type: vmbus_channel::bus::ChannelType::Device {
                    pipe_packets: false,
                },
                subchannel_index: 0,
                use_mnf: false,
                offer_order: None,
                allow_confidential_external_memory: false,
            }
        }

        fn max_subchannels(&self) -> u16 {
            0
        }

        fn install(&mut self, resources: DeviceResources) {
            self.state.lock().resources = Some(resources);
        }

        async fn open(
            &mut self,
            channel_idx: u16,
            open_request: &OpenRequest,
        ) -> Result<(), ChannelOpenError> {
            tracing::info!("OPEN");
            TestDeviceState::set_open_request(&self.state, channel_idx, open_request.clone());
            Ok(())
        }

        async fn close(&mut self, channel_idx: u16) {
            tracing::info!("CLOSE");
            assert!(TestDeviceState::remove_open_request(&self.state, channel_idx).is_some());
        }

        async fn retarget_vp(&mut self, channel_idx: u16, target_vp: u32) {
            TestDeviceState::set_target_vp(&self.state, channel_idx, target_vp);
        }

        fn start(&mut self) {
            tracing::info!("START");
            TestDeviceState::set_started(&self.state, true);
        }

        async fn stop(&mut self) {
            tracing::info!("STOP");
            TestDeviceState::set_started(&self.state, false);
        }

        fn supports_save_restore(&mut self) -> Option<&mut dyn SaveRestoreVmbusDevice> {
            None
        }
    }

    #[async_test]
    async fn test_stopped_child(spawner: DefaultDriver) {
        // This is mostly testing vmbus_channel behavior when a channel is
        // stopped but vbmus_server is not and continues to receive
        // messages.
        let mut env = TestEnv::new(spawner.clone());
        let (test_device, test_device_state) = TestDevice::new_and_state(1);
        let control = env.vmbus.control();
        let channel = offer_channel(&spawner, control.as_ref(), test_device)
            .await
            .expect("test device failed to offer");

        env.vmbus.start();
        env.connect(1, protocol::FeatureFlags::new(), false).await;

        // Stop the channel.
        channel.stop().await;

        assert_eq!(TestDeviceState::started(&test_device_state), false);

        // GPADL processing is currently allowed while the channel is stopped,
        // so this should complete.
        env.synic.send_message_core(
            OutgoingMessage::with_data(
                &protocol::GpadlHeader {
                    channel_id: ChannelId(1),
                    gpadl_id: GpadlId(1),
                    count: 1,
                    len: 16,
                },
                [1u64, 0u64].as_bytes(),
            ),
            false,
        );
        env.expect_response(protocol::MessageType::GPADL_CREATED)
            .await;

        // Open will pend while the channel is stopped.
        env.synic.send_message_core(
            OutgoingMessage::new(&protocol::OpenChannel {
                channel_id: ChannelId(1),
                open_id: 0,
                ring_buffer_gpadl_id: GpadlId(1),
                target_vp: 0,
                downstream_ring_buffer_page_offset: 0,
                user_data: UserDefinedData::default(),
            }),
            false,
        );
        let wait_for_response = mesh::CancelContext::new()
            .with_timeout(Duration::from_millis(150))
            .until_cancelled(env.expect_response(protocol::MessageType::OPEN_CHANNEL_RESULT))
            .await;
        assert!(matches!(
            wait_for_response,
            Err(CancelReason::DeadlineExceeded)
        ));
        assert!(TestDeviceState::open_request(&test_device_state, 0).is_none());

        // Restart the channel and confirm that open completes.
        channel.start();
        env.expect_response(protocol::MessageType::OPEN_CHANNEL_RESULT)
            .await;
        assert!(TestDeviceState::open_request(&test_device_state, 0).is_some());

        // Stop the channel and send a modify request.
        assert!(TestDeviceState::target_vp(&test_device_state, 0).is_none());
        channel.stop().await;
        env.synic.send_message_core(
            OutgoingMessage::new(&protocol::ModifyChannel {
                channel_id: ChannelId(1),
                target_vp: 2,
            }),
            false,
        );
        let wait_for_response = mesh::CancelContext::new()
            .with_timeout(Duration::from_millis(150))
            .until_cancelled(env.expect_response(protocol::MessageType::MODIFY_CHANNEL_RESPONSE))
            .await;
        assert!(matches!(
            wait_for_response,
            Err(CancelReason::DeadlineExceeded)
        ));

        // Restart the channel and verify the modify request completes.
        channel.start();
        env.expect_response(protocol::MessageType::MODIFY_CHANNEL_RESPONSE)
            .await;
        assert_eq!(
            TestDeviceState::target_vp(&test_device_state, 0)
                .expect("Modify channel request received"),
            2
        );

        // Stop the channel and send a close request.
        channel.stop().await;
        env.synic.send_message_core(
            OutgoingMessage::new(&protocol::CloseChannel {
                channel_id: ChannelId(1),
            }),
            false,
        );
        PolledTimer::new(&spawner)
            .sleep(Duration::from_millis(150))
            .await;
        assert!(TestDeviceState::open_request(&test_device_state, 0).is_some());

        // Restart the channel and verify the close was relayed to the inner device.
        channel.start();
        PolledTimer::new(&spawner)
            .sleep(Duration::from_millis(150))
            .await;
        assert!(TestDeviceState::open_request(&test_device_state, 0).is_none());

        env.vmbus.reset().await;
        env.vmbus.stop().await;
    }

    #[async_test]
    async fn test_confidential_connection(spawner: DefaultDriver) {
        let mut env = TestEnv::new(spawner);
        // Add regular bus child channels, one of which supports confidential external memory.
        let mut channel = env.offer(1, false).await;
        let mut channel2 = env.offer(2, true).await;

        // Add a channel directly, like the relay would do.
        let (request_send, request_recv) = mesh::channel();
        let (server_request_send, server_request_recv) = mesh::channel();
        let id = Guid {
            data1: 3,
            ..Guid::ZERO
        };
        let control = env.vmbus.control();
        let relay_resources = control
            .offer_core(OfferInfo {
                params: OfferParamsInternal {
                    interface_name: "test".into(),
                    instance_id: id,
                    interface_id: id,
                    mmio_megabytes: 0,
                    mmio_megabytes_optional: 0,
                    subchannel_index: 0,
                    use_mnf: false,
                    offer_order: None,
                    flags: protocol::OfferFlags::new().with_enumerate_device_interface(true),
                    ..Default::default()
                },
                request_send,
                server_request_recv,
            })
            .await
            .unwrap();

        let mut relay_channel = TestChannel {
            request_recv,
            server_request_send,
            _resources: relay_resources,
        };

        env.vmbus.start();
        env.initiate_contact(
            protocol::Version::Copper,
            protocol::FeatureFlags::new().with_confidential_channels(true),
            true,
        );

        env.expect_response(protocol::MessageType::VERSION_RESPONSE)
            .await;

        env.synic.send_message_trusted(protocol::RequestOffers {});

        // All offers added with add_child have confidential ring support.
        let offer = env.get_response::<protocol::OfferChannel>().await;
        assert!(offer.flags.confidential_ring_buffer());
        assert!(!offer.flags.confidential_external_memory());
        let offer = env.get_response::<protocol::OfferChannel>().await;
        assert!(offer.flags.confidential_ring_buffer());
        assert!(offer.flags.confidential_external_memory());

        // The "relay" channel will not have its flags modified.
        let offer = env.get_response::<protocol::OfferChannel>().await;
        assert!(!offer.flags.confidential_ring_buffer());
        assert!(!offer.flags.confidential_external_memory());

        env.expect_response(protocol::MessageType::ALL_OFFERS_DELIVERED)
            .await;

        // Make sure that the correct confidential flags are set in the open request when opening
        // the channels.
        env.open_channel(1, 1, &mut channel, |request| {
            assert!(request.use_confidential_ring);
            assert!(!request.use_confidential_external_memory);
        })
        .await;

        env.open_channel(2, 2, &mut channel2, |request| {
            assert!(request.use_confidential_ring);
            assert!(request.use_confidential_external_memory);
        })
        .await;

        env.open_channel(3, 3, &mut relay_channel, |request| {
            assert!(!request.use_confidential_ring);
            assert!(!request.use_confidential_external_memory);
        })
        .await;
    }

    #[async_test]
    async fn test_confidential_channels_unsupported(spawner: DefaultDriver) {
        let mut env = TestEnv::new(spawner);
        let mut channel = env.offer(1, false).await;
        let mut channel2 = env.offer(2, true).await;

        env.vmbus.start();
        env.connect(2, protocol::FeatureFlags::new(), true).await;

        // Make sure that the correct confidential flags are always false when the client doesn't
        // support confidential channels.
        env.open_channel(1, 1, &mut channel, |request| {
            assert!(!request.use_confidential_ring);
            assert!(!request.use_confidential_external_memory);
        })
        .await;

        env.open_channel(2, 2, &mut channel2, |request| {
            assert!(!request.use_confidential_ring);
            assert!(!request.use_confidential_external_memory);
        })
        .await;
    }

    #[async_test]
    async fn test_confidential_channels_untrusted(spawner: DefaultDriver) {
        let mut env = TestEnv::new(spawner);
        let mut channel = env.offer(1, false).await;
        let mut channel2 = env.offer(2, true).await;

        env.vmbus.start();
        // Client claims to support confidential channels, but they can't be used because the
        // connection is untrusted.
        env.connect(
            2,
            protocol::FeatureFlags::new().with_confidential_channels(true),
            false,
        )
        .await;

        // Make sure that the correct confidential flags are always false when the client doesn't
        // support confidential channels.
        env.open_channel(1, 1, &mut channel, |request| {
            assert!(!request.use_confidential_ring);
            assert!(!request.use_confidential_external_memory);
        })
        .await;

        env.open_channel(2, 2, &mut channel2, |request| {
            assert!(!request.use_confidential_ring);
            assert!(!request.use_confidential_external_memory);
        })
        .await;
    }
}
