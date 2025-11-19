// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

#![expect(missing_docs)]
#![forbid(unsafe_code)]

mod channel_bitmap;
pub mod channels;
pub mod event;
pub mod hvsock;
mod monitor;
mod proxyintegration;
#[cfg(test)]
mod tests;

/// The GUID type used for vmbus channel identifiers.
pub type Guid = guid::Guid;

use anyhow::Context;
use async_trait::async_trait;
use channel_bitmap::ChannelBitmap;
use channels::ConnectionTarget;
pub use channels::InitiateContactRequest;
use channels::MessageTarget;
pub use channels::MnfUsage;
use channels::ModifyConnectionRequest;
use channels::ModifyConnectionResponse;
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
use pal_async::driver::Driver;
use pal_async::driver::SpawnDriver;
use pal_async::task::Task;
use pal_async::timer::PolledTimer;
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
use std::time::Duration;
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
use vmbus_channel::bus::ParentBus;
use vmbus_channel::bus::RestoreResult;
use vmbus_channel::gpadl::GpadlMap;
use vmbus_channel::gpadl_ring::AlignedGpadlView;
use vmbus_core::HvsockConnectRequest;
use vmbus_core::HvsockConnectResult;
use vmbus_core::MaxVersionInfo;
use vmbus_core::OutgoingMessage;
use vmbus_core::TaggedStream;
use vmbus_core::VMBUS_SINT;
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
use vmcore::synic::MonitorPageGpas;
use vmcore::synic::SynicPortAccess;

pub const REDIRECT_SINT: u8 = 7;
pub const REDIRECT_VTL: Vtl = Vtl::Vtl2;
const SHARED_EVENT_CONNECTION_ID: u32 = 2;
const EVENT_PORT_ID: u32 = 2;
const VMBUS_MESSAGE_TYPE: u32 = 1;

const MAX_CONCURRENT_HVSOCK_REQUESTS: usize = 16;

#[derive(Inspect)]
pub struct VmbusServer {
    #[inspect(flatten, send = "VmbusRequest::Inspect")]
    task_send: mesh::Sender<VmbusRequest>,
    #[inspect(skip)]
    control: Arc<VmbusServerControl>,
    #[inspect(skip)]
    _message_port: Box<dyn Sync + Send>,
    #[inspect(skip)]
    _multiclient_message_port: Option<Box<dyn Sync + Send>>,
    #[inspect(skip)]
    task: Task<ServerTask>,
}

pub struct VmbusServerBuilder<T: SpawnDriver> {
    spawner: T,
    synic: Arc<dyn SynicPortAccess>,
    gm: GuestMemory,
    private_gm: Option<GuestMemory>,
    vtl: Vtl,
    hvsock_notify: Option<HvsockServerChannelHalf>,
    server_relay: Option<VmbusServerChannelHalf>,
    saved_state_notify: Option<mesh::Sender<SavedStateRequest>>,
    external_server: Option<mesh::Sender<InitiateContactRequest>>,
    external_requests: Option<mesh::Receiver<InitiateContactRequest>>,
    use_message_redirect: bool,
    channel_id_offset: u16,
    max_version: Option<MaxVersionInfo>,
    delay_max_version: bool,
    enable_mnf: bool,
    force_confidential_external_memory: bool,
    send_messages_while_stopped: bool,
    channel_unstick_delay: Option<Duration>,
    use_absolute_channel_order: bool,
}

#[derive(mesh::MeshPayload)]
/// The request to send to the proxy to set or clear its saved state cache.
pub enum SavedStateRequest {
    Set(FailableRpc<Box<channels::SavedState>, ()>),
    Clear(Rpc<(), ()>),
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

pub type VmbusServerChannelHalf = ServerChannelHalf<ModifyRelayRequest, ModifyRelayResponse>;
pub type VmbusRelayChannelHalf = RelayChannelHalf<ModifyRelayRequest, ModifyRelayResponse>;
pub type VmbusRelayChannel = RelayChannel<ModifyRelayRequest, ModifyRelayResponse>;
pub type HvsockServerChannelHalf = ServerChannelHalf<HvsockConnectRequest, HvsockConnectResult>;
pub type HvsockRelayChannelHalf = RelayChannelHalf<HvsockConnectRequest, HvsockConnectResult>;
pub type HvsockRelayChannel = RelayChannel<HvsockConnectRequest, HvsockConnectResult>;

/// A request from the server to the relay to modify connection state.
///
/// The version and use_interrupt_page fields can only be present if this request was sent for an
/// InitiateContact message from the guest.
///
/// If `version` is `Some`, the relay must respond with either `ModifyRelayResponse::Supported` or
/// `ModifyRelayResponse::Unsupported`. If `version` is `None`, the relay must respond with
/// `ModifyRelayResponse::Modified`.
#[derive(Debug, Copy, Clone)]
pub struct ModifyRelayRequest {
    pub version: Option<u32>,
    pub monitor_page: Update<MonitorPageGpas>,
    pub use_interrupt_page: Option<bool>,
}

/// A response from the relay to a ModifyRelayRequest from the server.
#[derive(Debug, Copy, Clone)]
pub enum ModifyRelayResponse {
    /// The requested version change is supported, and the relay completed the connection
    /// modification with the specified status. All of the feature flags supported by the relay host
    /// are included, regardless of what features were requested.
    Supported(protocol::ConnectionState, protocol::FeatureFlags),
    /// A version change was requested but the relay host doesn't support that version. This
    /// response cannot be returned for a request with no version change set.
    Unsupported,
    /// The connection modification completed with the specified status. This response must be sent
    /// if no version change was requested.
    Modified(protocol::ConnectionState),
}

impl From<ModifyConnectionRequest> for ModifyRelayRequest {
    fn from(value: ModifyConnectionRequest) -> Self {
        Self {
            version: value.version.map(|v| v.version as u32),
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
    Restore(Rpc<Box<SavedState>, Result<(), RestoreError>>),
    Start,
    Stop(Rpc<(), ()>),
}

#[derive(mesh::MeshPayload, Debug)]
pub struct OfferInfo {
    pub params: OfferParamsInternal,
    pub event: Interrupt,
    pub request_send: mesh::Sender<ChannelRequest>,
    pub server_request_recv: mesh::Receiver<ChannelServerRequest>,
}

#[expect(clippy::large_enum_variant)]
#[derive(mesh::MeshPayload)]
pub(crate) enum OfferRequest {
    Offer(FailableRpc<OfferInfo, ()>),
    ForceReset(Rpc<(), ()>),
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
    pub server: channels::SavedState,
    // Indicates if the lost synic bug is fixed or not. By default it's false.
    // During the restore process, we check if the field is not true then
    // unstick_channels() function will be called to mitigate the issue.
    #[mesh(2)]
    pub lost_synic_bug_fixed: bool,
}

const MESSAGE_CONNECTION_ID: u32 = 1;
const MULTICLIENT_MESSAGE_CONNECTION_ID: u32 = 4;

impl<T: SpawnDriver + Clone> VmbusServerBuilder<T> {
    /// Creates a new builder for `VmbusServer` with the default options.
    pub fn new(spawner: T, synic: Arc<dyn SynicPortAccess>, gm: GuestMemory) -> Self {
        Self {
            spawner,
            synic,
            gm,
            private_gm: None,
            vtl: Vtl::Vtl0,
            hvsock_notify: None,
            server_relay: None,
            saved_state_notify: None,
            external_server: None,
            external_requests: None,
            use_message_redirect: false,
            channel_id_offset: 0,
            max_version: None,
            delay_max_version: false,
            enable_mnf: false,
            force_confidential_external_memory: false,
            send_messages_while_stopped: false,
            channel_unstick_delay: Some(Duration::from_millis(100)),
            use_absolute_channel_order: false,
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

    /// Sets a send channel used to enlighten ProxyIntegration about saved channels.
    pub fn saved_state_notify(
        mut self,
        saved_state_notify: Option<mesh::Sender<SavedStateRequest>>,
    ) -> Self {
        self.saved_state_notify = saved_state_notify;
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

    /// Sets the delay before unsticking a vmbus channel after it has been opened.
    ///
    /// This option provides a work around for guests that ignore interrupts before they receive the
    /// OpenResult message, by triggering an interrupt after the channel has been opened.
    ///
    /// If not set, the default is 100ms. If set to `None`, no interrupt will be triggered.
    pub fn channel_unstick_delay(mut self, delay: Option<Duration>) -> Self {
        self.channel_unstick_delay = delay;
        self
    }

    /// Sets whether the channel order value provided in an offer is the primary way of ordering
    /// channels when assigning channel IDs, rather than the default behavior of ordering by
    /// interface ID first.
    pub fn use_absolute_channel_order(mut self, assign: bool) -> Self {
        self.use_absolute_channel_order = assign;
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
            (self.vtl, VMBUS_SINT)
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

        let mut server = channels::Server::new(
            self.vtl,
            connection_id,
            self.channel_id_offset,
            self.use_absolute_channel_order,
        );

        // If MNF is handled by this server and this is a paravisor for an isolated VM, the monitor
        // pages must be allocated by the server, not the guest, since the guest will provide shared
        // pages which can't be used in this case. If the guest doesn't support server-specified
        // monitor pages, MNF will be disabled for all channels for that connection.
        server.set_require_server_allocated_mnf(self.enable_mnf && self.private_gm.is_some());

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
                    .map(|req: ModifyRelayRequest| {
                        // Map to the correct response type for the request.
                        if req.version.is_some() {
                            ModifyRelayResponse::Supported(
                                protocol::ConnectionState::SUCCESSFUL,
                                protocol::FeatureFlags::from_bits(u32::MAX),
                            )
                        } else {
                            ModifyRelayResponse::Modified(protocol::ConnectionState::SUCCESSFUL)
                        }
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
            saved_state_notify: self.saved_state_notify,
            channels: HashMap::new(),
            channel_responses: FuturesUnordered::new(),
            relay_send: relay_request_send,
            external_server_send: self.external_server,
            channel_bitmap: None,
            shared_event_port: None,
            reset_done: Vec::new(),
            mnf_support: self.enable_mnf.then(MnfSupport::default),
        };

        let (task_send, task_recv) = mesh::channel();
        let mut server_task = ServerTask {
            driver: Box::new(self.spawner.clone()),
            server,
            task_recv,
            offer_recv,
            message_recv,
            server_request_recv: SelectAll::new(),
            inner,
            external_requests: self.external_requests,
            next_seq: 0,
            unstick_on_start: false,
            channel_unstickers: FuturesUnordered::new(),
            channel_unstick_delay: self.channel_unstick_delay,
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
    pub fn builder<T: SpawnDriver + Clone>(
        spawner: T,
        synic: Arc<dyn SynicPortAccess>,
        gm: GuestMemory,
    ) -> VmbusServerBuilder<T> {
        VmbusServerBuilder::new(spawner, synic, gm)
    }

    pub async fn save(&self) -> SavedState {
        self.task_send.call(VmbusRequest::Save, ()).await.unwrap()
    }

    pub async fn restore(&self, state: SavedState) -> Result<(), RestoreError> {
        self.task_send
            .call(VmbusRequest::Restore, Box::new(state))
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

    fn get_child_event_port_id(channel_id: protocol::ChannelId, sint_index: u8, vtl: Vtl) -> u32 {
        EVENT_PORT_ID | (vtl as u32) << 22 | channel_id.0 << 8 | (sint_index as u32) << 4
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

/// Information used by a server that supports MNF.
#[derive(Default)]
struct MnfSupport {
    allocated_monitor_page: Option<MonitorPageGpas>,
}

/// Disambiguates offer instances that may have reused the same offer ID.
#[derive(Debug, Clone, Copy)]
struct OfferInstanceId {
    offer_id: OfferId,
    seq: u64,
}

struct ServerTask {
    driver: Box<dyn Driver>,
    server: channels::Server,
    task_recv: mesh::Receiver<VmbusRequest>,
    offer_recv: mesh::Receiver<OfferRequest>,
    message_recv: mpsc::Receiver<SynicMessage>,
    server_request_recv:
        SelectAll<TaggedStream<OfferInstanceId, mesh::Receiver<ChannelServerRequest>>>,
    inner: ServerTaskInner,
    external_requests: Option<mesh::Receiver<InitiateContactRequest>>,
    /// Next value for [`Channel::seq`].
    next_seq: u64,
    unstick_on_start: bool,
    channel_unstickers: FuturesUnordered<Pin<Box<dyn Send + Future<Output = OfferInstanceId>>>>,
    channel_unstick_delay: Option<Duration>,
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
    saved_state_notify: Option<mesh::Sender<SavedStateRequest>>,
    channels: HashMap<OfferId, Channel>,
    channel_responses: FuturesUnordered<
        Pin<Box<dyn Send + Future<Output = (OfferId, u64, Result<ChannelResponse, RpcError>)>>>,
    >,
    external_server_send: Option<mesh::Sender<InitiateContactRequest>>,
    relay_send: mesh::Sender<ModifyRelayRequest>,
    channel_bitmap: Option<Arc<ChannelBitmap>>,
    shared_event_port: Option<Box<dyn Send>>,
    reset_done: Vec<Rpc<(), ()>>,
    /// Stores information needed to support MNF. If `None`, this server doesn't support MNF (in
    /// the case of OpenHCL, that means it will be handled by the relay host).
    mnf_support: Option<MnfSupport>,
}

#[derive(Debug)]
enum ChannelResponse {
    Open(bool),
    Close,
    Gpadl(GpadlId, bool),
    TeardownGpadl(GpadlId),
    Modify(i32),
}

#[derive(Debug, Copy, Clone, PartialEq, Eq)]
enum ChannelUnstickState {
    None,
    Queued,
    NeedsRequeue,
}

struct Channel {
    key: OfferKey,
    send: mesh::Sender<ChannelRequest>,
    seq: u64,
    state: ChannelState,
    gpadls: Arc<GpadlMap>,
    guest_to_host_event: Arc<ChannelEvent>,
    flags: protocol::OfferFlags,
    // A channel can be reserved no matter what state it is in. This allows the message port for a
    // reserved channel to remain available even if the channel is closed, so the guest can read the
    // close reserved channel response. The reserved state is cleared when the channel is revoked,
    // reopened, or the guest sends an unload message.
    reserved_state: ReservedState,
    unstick_state: ChannelUnstickState,
}

struct ReservedState {
    message_port: Option<Box<dyn GuestMessagePort>>,
    target: ConnectionTarget,
}

struct ChannelOpenState {
    open_params: OpenParams,
    _event_port: Box<dyn Send>,
    guest_event_port: Box<dyn GuestEventPort>,
    host_to_guest_interrupt: Interrupt,
}

enum ChannelState {
    Closed,
    Open(Box<ChannelOpenState>),
    Closing,
}

impl ServerTask {
    fn handle_offer(&mut self, mut info: OfferInfo) -> anyhow::Result<()> {
        let key = info.params.key();
        let flags = info.params.flags;

        if self.inner.mnf_support.is_some() && self.inner.synic.monitor_support().is_some() {
            // If this server is handling MnF, ignore any relayed monitor IDs but still enable MnF
            // for those channels.
            // N.B. Since this can only happen in OpenHCL, which emulates MnF, the latency is
            //      ignored.
            if info.params.use_mnf.is_relayed() {
                info.params.use_mnf = MnfUsage::Enabled {
                    latency: Duration::ZERO,
                }
            }
        } else if info.params.use_mnf.is_enabled() {
            // If the server is not handling MnF, disable it for the channel. This does not affect
            // channels with a relayed monitor ID.
            info.params.use_mnf = MnfUsage::Disabled;
        }

        let offer_id = self
            .server
            .with_notifier(&mut self.inner)
            .offer_channel(info.params)
            .context("channel offer failed")?;

        tracing::debug!(?offer_id, %key, "offered channel");

        let seq = self.next_seq;
        self.next_seq += 1;
        self.inner.channels.insert(
            offer_id,
            Channel {
                key,
                send: info.request_send,
                state: ChannelState::Closed,
                gpadls: GpadlMap::new(),
                guest_to_host_event: Arc::new(ChannelEvent(info.event)),
                seq,
                flags,
                reserved_state: ReservedState {
                    message_port: None,
                    target: ConnectionTarget { vp: 0, sint: 0 },
                },
                unstick_state: ChannelUnstickState::None,
            },
        );

        self.server_request_recv.push(TaggedStream::new(
            OfferInstanceId { offer_id, seq },
            info.server_request_recv,
        ));

        Ok(())
    }

    fn handle_revoke(&mut self, id: OfferInstanceId) {
        // The channel may or may not exist in the map depending on whether it's been explicitly
        // revoked before being dropped.
        if let Some(channel) = self.inner.channels.get(&id.offer_id) {
            if channel.seq == id.seq {
                tracing::info!(?id.offer_id, key = %channel.key, "revoking channel");
                self.inner.channels.remove(&id.offer_id);
                self.server
                    .with_notifier(&mut self.inner)
                    .revoke_channel(id.offer_id);
            }
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

    fn handle_open(&mut self, offer_id: OfferId, success: bool) {
        let status = if success {
            let channel = self
                .inner
                .channels
                .get_mut(&offer_id)
                .expect("channel exists");

            // Some guests ignore interrupts before they receive the OpenResult message. To avoid
            // a potential hang, signal the channel after a delay if needed.
            if let Some(delay) = self.channel_unstick_delay {
                if channel.unstick_state == ChannelUnstickState::None {
                    channel.unstick_state = ChannelUnstickState::Queued;
                    let seq = channel.seq;
                    let mut timer = PolledTimer::new(&self.driver);
                    self.channel_unstickers.push(Box::pin(async move {
                        timer.sleep(delay).await;
                        OfferInstanceId { offer_id, seq }
                    }));
                } else {
                    channel.unstick_state = ChannelUnstickState::NeedsRequeue;
                }
            }

            0
        } else {
            protocol::STATUS_UNSUCCESSFUL
        };

        self.server
            .with_notifier(&mut self.inner)
            .open_complete(offer_id, status);
    }

    fn handle_close(&mut self, offer_id: OfferId) {
        let channel = self
            .inner
            .channels
            .get_mut(&offer_id)
            .expect("channel still exists");

        match &mut channel.state {
            ChannelState::Closing => {
                tracing::debug!(?offer_id, key = %channel.key, "closing channel");
                channel.state = ChannelState::Closed;
                self.server
                    .with_notifier(&mut self.inner)
                    .close_complete(offer_id);
            }
            _ => {
                tracing::error!(?offer_id, key = %channel.key, "invalid close channel response");
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
        open: bool,
    ) -> anyhow::Result<RestoreResult> {
        let gpadls = self.server.channel_gpadls(offer_id);

        // If the channel is opened, handle that before calling into channels so that failure can
        // be handled before the channel is marked restored.
        let open_request = open
            .then(|| -> anyhow::Result<_> {
                let params = self.server.get_restore_open_params(offer_id)?;
                let (channel, interrupt) = self.inner.open_channel(offer_id, &params)?;
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
            if let Ok(buf) = MultiPagedRangeBuf::from_range_buffer(
                gpadl.request.count.into(),
                gpadl.request.buf.clone(),
            ) {
                channel.gpadls.add(gpadl.request.id, buf);
            }
        }

        let result = RestoreResult {
            open_request,
            gpadls,
        };
        Ok(result)
    }

    async fn handle_request(&mut self, request: VmbusRequest) {
        tracing::debug!(?request, "handle_request");
        match request {
            VmbusRequest::Reset(rpc) => self.handle_reset(rpc),
            VmbusRequest::Inspect(deferred) => {
                deferred.respond(|resp| {
                    resp.field("message_port", &self.inner.message_port)
                        .field("running", self.inner.running)
                        .field("hvsock_requests", self.inner.hvsock_requests)
                        .field("channel_unstick_delay", self.channel_unstick_delay)
                        .field_mut_with("unstick_channels", |v| {
                            let v: inspect::ValueKind = if let Some(v) = v {
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
            VmbusRequest::Restore(rpc) => {
                rpc.handle(async |state| {
                    self.unstick_on_start = !state.lost_synic_bug_fixed;
                    if let Some(sender) = &self.inner.saved_state_notify {
                        tracing::trace!("sending saved state to proxy");
                        if let Err(err) = sender
                            .call_failable(SavedStateRequest::Set, Box::new(state.server.clone()))
                            .await
                        {
                            tracing::error!(
                                err = &err as &dyn std::error::Error,
                                "failed to restore proxy saved state"
                            );
                            return Err(RestoreError::ServerError(err.into()));
                        }
                    }

                    self.server
                        .with_notifier(&mut self.inner)
                        .restore(state.server)
                })
                .await
            }
            VmbusRequest::Stop(rpc) => rpc.handle_sync(|()| {
                if self.inner.running {
                    self.inner.running = false;
                }
            }),
            VmbusRequest::Start => {
                if !self.inner.running {
                    self.inner.running = true;
                    if let Some(sender) = self.inner.saved_state_notify.as_ref() {
                        // Indicate to the proxy that the server is starting and that it should
                        // clear its saved state cache.
                        tracing::trace!("sending clear saved state message to proxy");
                        sender
                            .call(SavedStateRequest::Clear, ())
                            .await
                            .expect("failed to clear proxy saved state");
                    }

                    self.server
                        .with_notifier(&mut self.inner)
                        .revoke_unclaimed_channels();
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

    fn handle_relay_response(&mut self, response: ModifyRelayResponse) {
        // Convert to a matching ModifyConnectionResponse.
        let response = match response {
            ModifyRelayResponse::Supported(state, features) => {
                // Provide the server-allocated monitor page to the server only if they're actually being
                // used (if not, they may still be allocated from a previous connection).
                let allocated_monitor_gpas = self
                    .inner
                    .mnf_support
                    .as_ref()
                    .and_then(|mnf| mnf.allocated_monitor_page);

                ModifyConnectionResponse::Supported(state, features, allocated_monitor_gpas)
            }
            ModifyRelayResponse::Unsupported => ModifyConnectionResponse::Unsupported,
            ModifyRelayResponse::Modified(state) => ModifyConnectionResponse::Modified(state),
        };

        self.server
            .with_notifier(&mut self.inner)
            .complete_modify_connection(response);
    }

    fn handle_tl_connect_result(&mut self, result: HvsockConnectResult) {
        tracing::debug!(?result, "hvsock connect result");
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
        mut relay_response_recv: impl futures::stream::FusedStream<Item = ModifyRelayResponse> + Unpin,
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

            let mut channel_unstickers = OptionFuture::from(
                running_not_resetting.then(|| self.channel_unstickers.select_next_some()),
            );

            futures::select! { // merge semantics
                r = self.task_recv.recv().fuse() => {
                    if let Ok(request) = r {
                        self.handle_request(request).await;
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
                                self.handle_restore_channel(id.offer_id, open)
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
                r = channel_unstickers => {
                    self.unstick_channel_by_id(r.unwrap());
                }
                _r = flush_pending_messages => {}
                complete => break,
            }
        }
    }

    /// Wakes the guest and optionally the host for every open channel. If `force`, always wakes
    /// them. If `!force`, only wake for rings that are in the state where a notification is
    /// expected.
    fn unstick_channels(&self, force: bool) {
        let Some(version) = self.server.get_version() else {
            tracing::warn!("cannot unstick when not connected");
            return;
        };

        for channel in self.inner.channels.values() {
            let gm = self.inner.get_gm_for_channel(version, channel);
            if let Err(err) = Self::unstick_channel(gm, channel, force, true) {
                tracing::warn!(
                    channel = %channel.key,
                    error = err.as_ref() as &dyn std::error::Error,
                    "could not unstick channel"
                );
            }
        }
    }

    /// Wakes the guest for the specified channel if it's open and the rings are in a state where
    /// notification is expected.
    fn unstick_channel_by_id(&mut self, id: OfferInstanceId) {
        let Some(version) = self.server.get_version() else {
            tracelimit::warn_ratelimited!("cannot unstick when not connected");
            return;
        };

        if let Some(channel) = self.inner.channels.get_mut(&id.offer_id) {
            if channel.seq != id.seq {
                // The channel was revoked.
                return;
            }

            // The channel was closed and reopened before the delay expired, so wait again to ensure
            // we don't signal too early.
            if channel.unstick_state == ChannelUnstickState::NeedsRequeue {
                channel.unstick_state = ChannelUnstickState::Queued;
                let mut timer = PolledTimer::new(&self.driver);
                let delay = self.channel_unstick_delay.unwrap();
                self.channel_unstickers.push(Box::pin(async move {
                    timer.sleep(delay).await;
                    id
                }));

                return;
            }

            channel.unstick_state = ChannelUnstickState::None;
            let gm = select_gm_for_channel(
                &self.inner.gm,
                self.inner.private_gm.as_ref(),
                version,
                channel,
            );
            if let Err(err) = Self::unstick_channel(gm, channel, false, false) {
                tracelimit::warn_ratelimited!(
                    channel = %channel.key,
                    error = err.as_ref() as &dyn std::error::Error,
                    "could not unstick channel"
                );
            }
        }
    }

    fn unstick_channel(
        gm: &GuestMemory,
        channel: &Channel,
        force: bool,
        unstick_host: bool,
    ) -> anyhow::Result<()> {
        if let ChannelState::Open(state) = &channel.state {
            if force {
                tracing::info!(channel = %channel.key, "waking host and guest");
                if unstick_host {
                    channel.guest_to_host_event.0.deliver();
                }
                state.host_to_guest_interrupt.deliver();
                return Ok(());
            }

            let gpadl = channel
                .gpadls
                .clone()
                .view()
                .map(state.open_params.open_data.ring_gpadl_id)
                .context("couldn't find ring gpadl")?;

            let aligned = AlignedGpadlView::new(gpadl)
                .ok()
                .context("ring not aligned")?;
            let (in_gpadl, out_gpadl) = aligned
                .split(state.open_params.open_data.ring_offset)
                .ok()
                .context("couldn't split ring")?;

            if let Err(err) = Self::unstick_incoming_ring(
                gm,
                channel,
                in_gpadl,
                unstick_host.then_some(channel.guest_to_host_event.as_ref()),
                &state.host_to_guest_interrupt,
            ) {
                tracelimit::warn_ratelimited!(
                    channel = %channel.key,
                    error = err.as_ref() as &dyn std::error::Error,
                    "could not unstick incoming ring"
                );
            }
            if let Err(err) = Self::unstick_outgoing_ring(
                gm,
                channel,
                out_gpadl,
                unstick_host.then_some(channel.guest_to_host_event.as_ref()),
                &state.host_to_guest_interrupt,
            ) {
                tracelimit::warn_ratelimited!(
                    channel = %channel.key,
                    error = err.as_ref() as &dyn std::error::Error,
                    "could not unstick outgoing ring"
                );
            }
        }
        Ok(())
    }

    fn unstick_incoming_ring(
        gm: &GuestMemory,
        channel: &Channel,
        in_gpadl: AlignedGpadlView,
        guest_to_host_event: Option<&ChannelEvent>,
        host_to_guest_interrupt: &Interrupt,
    ) -> anyhow::Result<()> {
        let control_page = lock_gpn_with_subrange(gm, in_gpadl.gpns()[0])?;
        if let Some(guest_to_host_event) = guest_to_host_event {
            if ring::reader_needs_signal(control_page.pages()[0]) {
                tracelimit::info_ratelimited!(channel = %channel.key, "waking host for incoming ring");
                guest_to_host_event.0.deliver();
            }
        }

        let ring_size = gpadl_ring_size(&in_gpadl).try_into()?;
        if ring::writer_needs_signal(control_page.pages()[0], ring_size) {
            tracelimit::info_ratelimited!(channel = %channel.key, "waking guest for incoming ring");
            host_to_guest_interrupt.deliver();
        }
        Ok(())
    }

    fn unstick_outgoing_ring(
        gm: &GuestMemory,
        channel: &Channel,
        out_gpadl: AlignedGpadlView,
        guest_to_host_event: Option<&ChannelEvent>,
        host_to_guest_interrupt: &Interrupt,
    ) -> anyhow::Result<()> {
        let control_page = lock_gpn_with_subrange(gm, out_gpadl.gpns()[0])?;
        if ring::reader_needs_signal(control_page.pages()[0]) {
            tracelimit::info_ratelimited!(channel = %channel.key, "waking guest for outgoing ring");
            host_to_guest_interrupt.deliver();
        }

        if let Some(guest_to_host_event) = guest_to_host_event {
            let ring_size = gpadl_ring_size(&out_gpadl).try_into()?;
            if ring::writer_needs_signal(control_page.pages()[0], ring_size) {
                tracelimit::info_ratelimited!(channel = %channel.key, "waking host for outgoing ring");
                guest_to_host_event.0.deliver();
            }
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
                let key = channel.key;
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
                            %key,
                            "could not open channel",
                        );

                        // Return an error response to the channels module if the open_channel call
                        // failed.
                        Box::pin(future::ready((
                            offer_id,
                            seq,
                            Ok(ChannelResponse::Open(false)),
                        )))
                    }
                }
            }
            channels::Action::Close => {
                if let Some(channel_bitmap) = self.channel_bitmap.as_ref() {
                    if let ChannelState::Open(ref state) = channel.state {
                        channel_bitmap.unregister_channel(state.open_params.event_flag);
                    }
                }

                channel.state = ChannelState::Closing;
                handle(offer_id, channel, ChannelRequest::Close, (), |()| {
                    ChannelResponse::Close
                })
            }
            channels::Action::Gpadl(gpadl_id, count, buf) => {
                channel.gpadls.add(
                    gpadl_id,
                    MultiPagedRangeBuf::from_range_buffer(count.into(), buf.clone()).unwrap(),
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
                if let ChannelState::Open(state) = &mut channel.state {
                    if let Err(err) = state.guest_event_port.set_target_vp(target_vp) {
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

        self.set_monitor_page(&mut request)
            .context("Failed to map monitor page.")?;

        if let Some(vp) = request.target_message_vp {
            self.message_port.set_target_vp(vp)?;
        }

        if request.notify_relay {
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
        if let ChannelState::Open(state) = &channel.state {
            let mem = self.get_gm_for_channel(version.expect("must be connected"), channel);
            inspect_rings(
                &mut resp,
                mem,
                channel.gpadls.clone(),
                &state.open_params.open_data,
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
        tracing::debug!(?request, "received hvsock connect request");
        self.hvsock_requests += 1;
        self.hvsock_send.send(*request);
    }

    fn reset_complete(&mut self) {
        if let Some(monitor) = self.synic.monitor_support() {
            if let Err(err) = monitor.set_monitor_page(self.vtl, None) {
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

        // Always register with the channel bitmap; if Win7, this may be unnecessary.
        if let Some(channel_bitmap) = self.channel_bitmap.as_ref() {
            channel_bitmap.register_channel(
                open_params.event_flag,
                channel.guest_to_host_event.0.clone(),
            );
        }
        // Always set up an event port; if V1, this will be unused.
        // N.B. The event port must be created before the device is notified of the open by the
        //      caller. The device may begin communicating with the guest immediately when it is
        //      notified, so the event port must exist so that the guest can send interrupts.
        let event_port = self
            .synic
            .add_event_port(
                open_params.connection_id,
                self.vtl,
                channel.guest_to_host_event.clone(),
                open_params.monitor_info,
            )
            .context("failed to create guest-to-host event port")?;

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
            (self.vtl, VMBUS_SINT)
        };

        let guest_event_port = self.synic.new_guest_event_port(
            VmbusServer::get_child_event_port_id(open_params.channel_id, VMBUS_SINT, self.vtl),
            target_vtl,
            target_vp,
            target_sint,
            event_flag,
            open_params.monitor_info,
        )?;

        let interrupt = ChannelBitmap::create_interrupt(
            &self.channel_bitmap,
            guest_event_port.interrupt(),
            open_params.event_flag,
        );

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

        channel.state = ChannelState::Open(Box::new(ChannelOpenState {
            open_params: *open_params,
            _event_port: event_port,
            guest_event_port,
            host_to_guest_interrupt: interrupt.clone(),
        }));
        Ok((channel, interrupt))
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
        let interrupt_page = lock_page_with_subrange(&self.gm, interrupt_page)?;
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
            None,
        )?);

        Ok(())
    }

    fn set_monitor_page(&mut self, request: &mut ModifyConnectionRequest) -> anyhow::Result<()> {
        let monitor_page = match request.monitor_page {
            Update::Unchanged => return Ok(()),
            Update::Reset => None,
            Update::Set(value) => Some(value),
        };

        // TODO: can this check be moved into channels.rs?
        if self.channels.iter().any(|(_, c)| {
            matches!(
                &c.state,
                ChannelState::Open(state) if state.open_params.monitor_info.is_some()
            )
        }) {
            anyhow::bail!("attempt to change monitor page while open channels using mnf");
        }

        // Check if the server is handling MNF.
        // N.B. If the server is not handling MNF, there is currently no way to request
        //      server-allocated monitor pages from the relay host.
        if let Some(mnf_support) = self.mnf_support.as_mut() {
            if let Some(monitor) = self.synic.monitor_support() {
                mnf_support.allocated_monitor_page = None;

                if let Some(version) = request.version {
                    if version.feature_flags.server_specified_monitor_pages() {
                        if let Some(monitor_page) = monitor.allocate_monitor_page(self.vtl)? {
                            tracelimit::info_ratelimited!(
                                ?monitor_page,
                                "using server-allocated monitor pages"
                            );
                            mnf_support.allocated_monitor_page = Some(monitor_page);
                        }
                    }
                }

                // If no monitor page was allocated above, use the one provided by the client.
                if mnf_support.allocated_monitor_page.is_none() {
                    if let Err(err) = monitor.set_monitor_page(self.vtl, monitor_page) {
                        anyhow::bail!(
                            "setting monitor page failed, err = {err:?}, monitor_page = {monitor_page:?}"
                        );
                    }
                }
            }

            // If MNF is configured to be handled by this server (even if it's not actually
            // supported by the synic), don't forward the pages to the relay.
            request.monitor_page = Update::Unchanged;
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
                        key = %channel.key,
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
                    key = %channel.key,
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

    fn get_gm_for_channel(&self, version: VersionInfo, channel: &Channel) -> &GuestMemory {
        select_gm_for_channel(&self.gm, self.private_gm.as_ref(), version, channel)
    }
}

fn select_gm_for_channel<'a>(
    gm: &'a GuestMemory,
    private_gm: Option<&'a GuestMemory>,
    version: VersionInfo,
    channel: &Channel,
) -> &'a GuestMemory {
    if channel.flags.confidential_ring_buffer() && version.feature_flags.confidential_channels() {
        if let Some(private_gm) = private_gm {
            return private_gm;
        }
    }

    gm
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
            event: request.event,
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
    resp.child("incoming_ring", |req| inspect_ring(req, &in_gpadl, gm));
    resp.child("outgoing_ring", |req| inspect_ring(req, &out_gpadl, gm));
    Some(())
}

/// Inspects the incoming or outgoing ring buffer by directly accessing guest memory.
fn inspect_ring(req: inspect::Request<'_>, gpadl: &AlignedGpadlView, gm: &GuestMemory) {
    let mut resp = req.respond();

    resp.hex("ring_size", gpadl_ring_size(gpadl));

    // Lock just the control page. Use a subrange to allow a GuestMemory without a full mapping to
    // create one.
    if let Ok(pages) = lock_gpn_with_subrange(gm, gpadl.gpns()[0]) {
        ring::inspect_ring(pages.pages()[0], &mut resp);
    }
}

fn gpadl_ring_size(gpadl: &AlignedGpadlView) -> usize {
    // Data size excluding the control page.
    (gpadl.gpns().len() - 1) * PAGE_SIZE
}

/// Helper to create a subrange before locking a single page.
///
/// This allows us to lock a page in a `GuestMemory` that doesn't have a full mapping, but can
/// create one for a subrange.
fn lock_page_with_subrange(gm: &GuestMemory, offset: u64) -> anyhow::Result<guestmem::LockedPages> {
    Ok(gm
        .lockable_subrange(offset, PAGE_SIZE as u64)?
        .lock_gpns(false, &[0])?)
}

/// Helper to create a subrange before locking a single page from a gpn.
///
/// This allows us to lock a page in a `GuestMemory` that doesn't have a full mapping, but can
/// create one for a subrange.
fn lock_gpn_with_subrange(gm: &GuestMemory, gpn: u64) -> anyhow::Result<guestmem::LockedPages> {
    lock_page_with_subrange(gm, gpn * PAGE_SIZE as u64)
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
