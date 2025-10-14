// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Implements a vmbus channel relay, which consumes channels from the host
//! vmbus control plane (via [`vmbus_client`]) and relays them as channels to
//! the guest OS (via [`vmbus_server`]).
//!
//! This is used to allow the paravisor to implement the vmbus control plane
//! while still passing through channels from the host, without any paravisor
//! presence in the data plane.

#![expect(missing_docs)]
#![forbid(unsafe_code)]

pub mod legacy_saved_state;
mod saved_state;

pub use saved_state::SavedState;

use anyhow::Context;
use anyhow::Result;
use client::ModifyConnectionRequest;
use futures::FutureExt;
use futures::StreamExt;
use futures::future::BoxFuture;
use futures::future::OptionFuture;
use futures::future::join_all;
use guid::Guid;
use inspect::Inspect;
use inspect::InspectMut;
use mesh::rpc::FailableRpc;
use mesh::rpc::Rpc;
use mesh::rpc::RpcSend;
use pal_async::driver::SpawnDriver;
use pal_async::task::Spawn;
use pal_async::task::Task;
use pal_event::Event;
use std::collections::HashMap;
use std::fmt::Debug;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;
use unicycle::FuturesUnordered;
use vmbus_channel::bus::ChannelRequest;
use vmbus_channel::bus::ChannelServerRequest;
use vmbus_channel::bus::GpadlRequest;
use vmbus_channel::bus::ModifyRequest;
use vmbus_channel::bus::OfferKey;
use vmbus_channel::bus::OpenRequest;
use vmbus_client as client;
use vmbus_core::HvsockConnectRequest;
use vmbus_core::HvsockConnectResult;
use vmbus_core::VersionInfo;
use vmbus_core::protocol;
use vmbus_core::protocol::ChannelId;
use vmbus_core::protocol::FeatureFlags;
use vmbus_core::protocol::GpadlId;
use vmbus_server::HvsockRelayChannelHalf;
use vmbus_server::MnfUsage;
use vmbus_server::ModifyRelayResponse;
use vmbus_server::OfferInfo;
use vmbus_server::OfferParamsInternal;
use vmbus_server::Update;
use vmbus_server::VmbusRelayChannelHalf;
use vmbus_server::VmbusServerControl;
use vmcore::interrupt::Interrupt;
use vmcore::notify::Notify;
use vmcore::notify::PolledNotify;

pub enum InterceptChannelRequest {
    Start,
    Stop(Rpc<(), ()>),
    Save(Rpc<(), vmcore::save_restore::SavedStateBlob>),
    Restore(vmcore::save_restore::SavedStateBlob),
    Offer(client::OfferInfo),
}

const REQUIRED_FEATURE_FLAGS: FeatureFlags = FeatureFlags::new()
    .with_channel_interrupt_redirection(true)
    .with_guest_specified_signal_parameters(true)
    .with_modify_connection(true);

/// Represents a relay between a vmbus server on the host, and the vmbus server running in
/// Underhill, allowing offers from the host and offers from Underhill to be mixed.
///
/// The relay will connect to the host when it first receives a start request through its state
/// unit, and will remain connected until it is destroyed.
#[derive(Inspect, Debug)]
pub struct HostVmbusTransport {
    #[inspect(skip)]
    _relay_task: Task<()>,
    #[inspect(flatten, send = "TaskRequest::Inspect")]
    task_send: mesh::Sender<TaskRequest>,
}

impl HostVmbusTransport {
    /// Create a new instance of the host vmbus relay.
    pub async fn new(
        driver: impl SpawnDriver + Clone,
        control: Arc<VmbusServerControl>,
        channel: VmbusRelayChannelHalf,
        hvsock_relay: HvsockRelayChannelHalf,
        vmbus_client: client::VmbusClientAccess,
        connection: client::ConnectResult,
        intercept_list: Vec<(Guid, mesh::Sender<InterceptChannelRequest>)>,
    ) -> Result<Self> {
        if connection.version.feature_flags & REQUIRED_FEATURE_FLAGS != REQUIRED_FEATURE_FLAGS {
            anyhow::bail!(
                "host must support required feature flags. \
                 Required: {REQUIRED_FEATURE_FLAGS:?}, actual: {:?}.",
                connection.version.feature_flags
            );
        }

        let mut relay_task = RelayTask::new(
            Arc::new(driver.clone()),
            control,
            channel.response_send,
            hvsock_relay,
            vmbus_client,
            connection.version,
        );

        relay_task.intercept_channels.extend(intercept_list);

        for offer in connection.offers {
            relay_task.handle_offer(offer).await?;
        }

        let (task_send, task_recv) = mesh::channel();

        let relay_task = driver.spawn("vmbus hcl relay", async move {
            relay_task
                .run(channel.request_receive, connection.offer_recv, task_recv)
                .await
                .unwrap()
        });

        Ok(Self {
            _relay_task: relay_task,
            task_send,
        })
    }

    pub fn start(&self) {
        self.task_send.send(TaskRequest::Start);
    }

    pub async fn stop(&self) {
        self.task_send.call(TaskRequest::Stop, ()).await.unwrap()
    }

    pub async fn save(&self) -> SavedState {
        self.task_send.call(TaskRequest::Save, ()).await.unwrap()
    }

    pub async fn restore(&self, state: SavedState) -> Result<()> {
        self.task_send
            .call(TaskRequest::Restore, state)
            .await
            .unwrap()
    }
}

/// State needed to relay host-to-guest interrupts.
struct InterruptRelay {
    /// Event signaled when the host sends an interrupt.
    notify: PolledNotify,
    /// Interrupt used to signal the guest.
    interrupt: Interrupt,
    /// Event flag used to signal the guest.
    /// FUTURE: remove once this moves into `vmbus_client` saved state.
    event_flag: u16,
}

enum RelayChannelRequest {
    Start,
    Stop(Rpc<(), ()>),
    Save(Rpc<(), saved_state::Channel>),
    Restore(FailableRpc<saved_state::Channel, ()>),
    Inspect(inspect::Deferred),
}

impl Debug for RelayChannelRequest {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            RelayChannelRequest::Start => f.pad("Start"),
            RelayChannelRequest::Stop(..) => f.pad("Stop"),
            RelayChannelRequest::Save(..) => f.pad("Save"),
            RelayChannelRequest::Restore(..) => f.pad("Restore"),
            RelayChannelRequest::Inspect(..) => f.pad("Inspect"),
        }
    }
}

#[derive(Inspect)]
struct RelayChannelInfo {
    #[inspect(flatten, send = "RelayChannelRequest::Inspect")]
    relay_request_send: mesh::Sender<RelayChannelRequest>,
}

#[derive(Inspect)]
#[inspect(external_tag)]
enum ChannelInfo {
    #[inspect(transparent)]
    Relay(RelayChannelInfo),
    #[inspect(transparent)]
    Intercept(Guid),
}

impl RelayChannelInfo {
    async fn stop(&self) {
        if let Err(err) = self
            .relay_request_send
            .call(RelayChannelRequest::Stop, ())
            .await
        {
            tracing::warn!(?err, "failed to request channel stop");
        }
    }

    fn start(&self) {
        self.relay_request_send.send(RelayChannelRequest::Start);
    }
}

/// Connects a Client channel to a Server Channel.
#[derive(Inspect)]
struct RelayChannel {
    /// The Channel Id given to us by the client
    channel_id: ChannelId,
    /// The identifying key for this channel.
    key: OfferKey,
    /// Receives requests from the relay.
    #[inspect(skip)]
    relay_request_recv: mesh::Receiver<RelayChannelRequest>,
    /// Receives requests from the server.
    #[inspect(skip)]
    server_request_recv: mesh::Receiver<ChannelRequest>,
    #[inspect(skip)]
    server_request_send: mesh::Sender<ChannelServerRequest>,
    /// Closed when the channel has been revoked.
    #[inspect(skip)]
    revoke_recv: mesh::OneshotReceiver<()>,
    /// Sends requests to the client
    #[inspect(skip)]
    request_send: mesh::Sender<client::ChannelRequest>,
    /// Indicates whether or not interrupts should be relayed. This is shared with the relay server
    /// connection, which sets this to true only if the guest uses the channel bitmap.
    use_interrupt_relay: Arc<AtomicBool>,
    /// State used to relay host-to-guest interrupts.
    #[inspect(with = "Option::is_some")]
    interrupt_relay: Option<InterruptRelay>,
    /// Futures waiting for GPADL teardown to complete before responding to
    /// `vmbus_server`.
    #[inspect(skip)]
    gpadls_tearing_down: FuturesUnordered<BoxFuture<'static, ()>>,
    is_open: bool,
}

#[derive(InspectMut)]
struct RelayChannelTask {
    #[inspect(skip)]
    driver: Arc<dyn SpawnDriver>,
    channel: RelayChannel,
    running: bool,
}

impl RelayChannelTask {
    /// Relay open channel request from VTL0 to Host, responding with Open Result
    async fn handle_open_channel(&mut self, open_request: &OpenRequest) -> Result<()> {
        // If the guest uses the channel bitmap, the host can't send interrupts
        // directly and they must be relayed.
        let redirect_interrupt = self.channel.use_interrupt_relay.load(Ordering::SeqCst);
        let (incoming_event, notify) = if redirect_interrupt {
            let event = Event::new();
            let notify = Notify::from_event(event.clone())
                .pollable(self.driver.as_ref())
                .context("failed to create polled notify")?;
            Some((event, notify))
        } else {
            None
        }
        .unzip();

        let opened = self
            .channel
            .request_send
            .call_failable(
                client::ChannelRequest::Open,
                client::OpenRequest {
                    open_data: open_request.open_data,
                    incoming_event,
                    use_vtl2_connection_id: false,
                },
            )
            .await?;

        if let Some(notify) = notify {
            self.channel.interrupt_relay = Some(InterruptRelay {
                notify,
                interrupt: open_request.interrupt.clone(),
                event_flag: opened.redirected_event_flag.unwrap(),
            });
        }

        self.channel.is_open = true;

        Ok(())
    }

    async fn handle_close_channel(&mut self) {
        self.channel
            .request_send
            .call(client::ChannelRequest::Close, ())
            .await
            .ok();

        self.channel.interrupt_relay = None;
        self.channel.is_open = false;
    }

    /// Relay gpadl request from VTL0 to the Host and respond with gpadl created.
    async fn handle_gpadl(&mut self, request: GpadlRequest) -> Result<()> {
        self.channel
            .request_send
            .call_failable(client::ChannelRequest::Gpadl, request)
            .await?;

        Ok(())
    }

    fn handle_gpadl_teardown(&mut self, rpc: Rpc<GpadlId, ()>) {
        let (gpadl_id, rpc) = rpc.split();
        tracing::trace!(gpadl_id = gpadl_id.0, key = %self.channel.key, "Tearing down GPADL");

        let call = self
            .channel
            .request_send
            .call(client::ChannelRequest::TeardownGpadl, gpadl_id);

        // We cannot wait for GpadlTorndown here, because the host may not send the GpadlTorndown
        // message immediately, for example if the channel is still open and the host device still
        // has the gpadl mapped. We should not block further requests while waiting for the
        // response.
        let key = self.channel.key;
        self.channel.gpadls_tearing_down.push(Box::pin(async move {
            if let Err(err) = call.await {
                tracing::warn!(
                    %key,
                    error = &err as &dyn std::error::Error,
                    "failed to send gpadl teardown"
                );
            }
            rpc.complete(());
        }));
    }

    async fn handle_modify_channel(&mut self, modify_request: ModifyRequest) -> Result<i32> {
        let status = self
            .channel
            .request_send
            .call(client::ChannelRequest::Modify, modify_request)
            .await?;

        Ok(status)
    }

    /// Dispatch requests sent by VTL0
    async fn handle_server_request(&mut self, request: ChannelRequest) -> Result<()> {
        tracing::trace!(key = %self.channel.key, request = ?request, "received channel request");
        match request {
            ChannelRequest::Open(rpc) => {
                rpc.handle(async |open_request| {
                    self.handle_open_channel(&open_request)
                        .await
                        .inspect_err(|err| {
                            tracelimit::error_ratelimited!(
                                err = err.as_ref() as &dyn std::error::Error,
                                key = %self.channel.key,
                                channel_id = self.channel.channel_id.0,
                                "failed to open channel"
                            );
                        })
                        .is_ok()
                })
                .await;
            }
            ChannelRequest::Gpadl(rpc) => {
                rpc.handle(async |gpadl| {
                    let id = gpadl.id;
                    self.handle_gpadl(gpadl)
                        .await
                        .inspect_err(|err| {
                            tracelimit::error_ratelimited!(
                                err = err.as_ref() as &dyn std::error::Error,
                                key = %self.channel.key,
                                channel_id = self.channel.channel_id.0,
                                gpadl_id = id.0,
                                "failed to create gpadl"
                            );
                        })
                        .is_ok()
                })
                .await;
            }
            ChannelRequest::Close(rpc) => {
                rpc.handle(async |()| self.handle_close_channel().await)
                    .await;
            }
            ChannelRequest::TeardownGpadl(rpc) => {
                self.handle_gpadl_teardown(rpc);
            }
            ChannelRequest::Modify(rpc) => {
                rpc.handle(async |request| self.handle_modify_channel(request).await.unwrap_or(-1))
                    .await;
            }
        }

        Ok(())
    }

    async fn handle_relay_request(&mut self, request: RelayChannelRequest) {
        tracing::trace!(
            channel_id = self.channel.channel_id.0,
            key = %self.channel.key,
            ?request,
            "received relay request"
        );

        match request {
            RelayChannelRequest::Start => self.running = true,
            RelayChannelRequest::Stop(rpc) => rpc.handle_sync(|()| self.running = false),
            RelayChannelRequest::Save(rpc) => rpc.handle_sync(|_| self.handle_save()),
            RelayChannelRequest::Restore(rpc) => {
                rpc.handle_failable(async |state| self.handle_restore(state).await)
                    .await
            }
            RelayChannelRequest::Inspect(deferred) => deferred.inspect(self),
        }
    }

    /// Request dispatch loop
    async fn run(mut self) {
        loop {
            let mut relay_event = OptionFuture::from(
                self.channel
                    .interrupt_relay
                    .as_mut()
                    .map(|e| e.notify.wait().fuse()),
            );

            let mut server_request = OptionFuture::from(
                self.running
                    .then(|| self.channel.server_request_recv.next()),
            );

            futures::select! { // merge semantics
                r = self.channel.relay_request_recv.next() => {
                    match r {
                        Some(request) => {
                            // Needed to avoid conflicting &mut self borrow.
                            drop(relay_event);
                            self.handle_relay_request(request).await;
                        }
                        None => {
                            break;
                        }
                    }
                }
                r = server_request => {
                    match r.unwrap() {
                        Some(request) => {
                            // Needed to avoid conflicting &mut self borrow.
                            drop(relay_event);
                            self
                                .handle_server_request(request)
                                .await
                                .expect("failed to get server request");
                        }
                        None => {
                            break;
                        }
                    }
                }
                _r = (&mut self.channel.revoke_recv).fuse() => {
                    break;
                }
                () = self.channel.gpadls_tearing_down.select_next_some() => {}
                _r = relay_event => {
                    // Needed to avoid conflicting interrupt_relay borrow.
                    drop(relay_event);
                    self.channel.interrupt_relay.as_ref().unwrap().interrupt.deliver();
                }
            }
        }

        // Drain GPADL teardown requests cleanly; these will all complete now
        // that the channel has been revoked.
        while let Some(()) = self.channel.gpadls_tearing_down.next().await {}

        tracing::debug!(
            channel_id = %self.channel.channel_id.0,
            key = %self.channel.key,
            "dropped channel"
        );

        // Dropping the channel would revoke it, but since that's not synchronized there's a chance
        // we reoffer the channel before the server receives the revoke. Using the request ensures
        // that won't happen.
        if let Err(err) = self
            .channel
            .server_request_send
            .call(ChannelServerRequest::Revoke, ())
            .await
        {
            tracing::warn!(
                channel_id = self.channel.channel_id.0,
                key = %self.channel.key,
                err = &err as &dyn std::error::Error,
                "failed to send revoke request"
            );
        }
    }
}

enum TaskRequest {
    Inspect(inspect::Deferred),
    Save(Rpc<(), SavedState>),
    Restore(Rpc<SavedState, Result<()>>),
    Start,
    Stop(Rpc<(), ()>),
}

/// Dispatches requests between Server/Client.
#[derive(InspectMut)]
struct RelayTask {
    #[inspect(skip)]
    spawner: Arc<dyn SpawnDriver>,
    #[inspect(skip)]
    vmbus_client: client::VmbusClientAccess,
    version: VersionInfo,
    #[inspect(skip)]
    vmbus_control: Arc<VmbusServerControl>,
    #[inspect(with = "|x| inspect::iter_by_key(x).map_key(|x| x.0)")]
    channels: HashMap<ChannelId, ChannelInfo>,
    #[inspect(skip)]
    channel_workers: FuturesUnordered<Task<ChannelId>>,
    #[inspect(with = "|x| inspect::iter_by_key(x).map_value(|_| ())")]
    intercept_channels: HashMap<Guid, mesh::Sender<InterceptChannelRequest>>,
    use_interrupt_relay: Arc<AtomicBool>,
    #[inspect(skip)]
    server_response_send: mesh::Sender<ModifyRelayResponse>,
    #[inspect(skip)]
    hvsock_relay: HvsockRelayChannelHalf,
    #[inspect(skip)]
    hvsock_requests: FuturesUnordered<HvsockRequestFuture>,
    running: bool,
}

type HvsockRequestFuture =
    Pin<Box<dyn Future<Output = (HvsockConnectRequest, Option<client::OfferInfo>)> + Sync + Send>>;

impl RelayTask {
    fn new(
        spawner: Arc<dyn SpawnDriver>,
        vmbus_control: Arc<VmbusServerControl>,
        server_response_send: mesh::Sender<ModifyRelayResponse>,
        hvsock_relay: HvsockRelayChannelHalf,
        vmbus_client: client::VmbusClientAccess,
        version: VersionInfo,
    ) -> Self {
        Self {
            spawner,
            vmbus_client,
            version,
            vmbus_control,
            channels: HashMap::new(),
            channel_workers: FuturesUnordered::new(),
            intercept_channels: HashMap::new(),
            use_interrupt_relay: Arc::new(AtomicBool::new(false)),
            server_response_send,
            hvsock_relay,
            running: false,
            hvsock_requests: FuturesUnordered::new(),
        }
    }

    async fn handle_start(&mut self) {
        if !self.running {
            // Resume all channels.
            for c in self.channels.values() {
                match c {
                    ChannelInfo::Relay(relay) => relay.start(),
                    ChannelInfo::Intercept(id) => {
                        let Some(intercept_channel) = self.intercept_channels.get(id) else {
                            tracing::error!(%id, "Intercept device missing from list");
                            continue;
                        };
                        intercept_channel.send(InterceptChannelRequest::Start);
                    }
                }
            }

            self.running = true;
        }
    }

    async fn handle_stop(&mut self) {
        if self.running {
            // Stop all the channels before the relay itself can stop.
            join_all(self.channels.values().map(|c| match c {
                ChannelInfo::Relay(relay) => futures::future::Either::Left(relay.stop()),
                ChannelInfo::Intercept(id) => futures::future::Either::Right(async {
                    let id = *id;
                    if let Some(intercept_channel) = self.intercept_channels.get(&id) {
                        if let Err(err) = intercept_channel
                            .call(InterceptChannelRequest::Stop, ())
                            .await
                        {
                            tracing::error!(
                                err = &err as &dyn std::error::Error,
                                %id,
                                "Failed to stop intercepted device"
                            );
                        }
                    }
                }),
            }))
            .await;

            // Because requests are handled "synchronously" (async is used but everything is awaited
            // before another request is handled), there is no need for rundown and the relay can
            // stop immediately.
            self.running = false;
        }
    }

    /// Translates an offer received from the client to a server offer.
    /// Additionally, sets up all the appropriate channels.
    async fn handle_offer(&mut self, offer: client::OfferInfo) -> Result<()> {
        let channel_id = offer.offer.channel_id.0;

        if let Some(intercept) = self.intercept_channels.get(&offer.offer.instance_id) {
            self.channels.insert(
                ChannelId(channel_id),
                ChannelInfo::Intercept(offer.offer.instance_id),
            );
            intercept.send(InterceptChannelRequest::Offer(offer));
            return Ok(());
        }

        if self.channels.contains_key(&ChannelId(channel_id)) {
            anyhow::bail!("channel {channel_id} already exists");
        }

        // Used to Recv requests from the server.
        let (request_send, request_recv) = mesh::channel();
        // Used to Send responses from the server
        let (server_request_send, server_request_recv) = mesh::channel();

        if offer.offer.is_dedicated != 1 {
            tracing::warn!(offer = ?offer.offer, "All offers should be dedicated with Win8+ host")
        }

        // If the vmbus server is handling MnF, instead of relaying it, it will ignore this monitor
        // ID and allocate its own.
        let use_mnf = if offer.offer.monitor_allocated != 0 {
            MnfUsage::Relayed {
                monitor_id: offer.offer.monitor_id,
            }
        } else {
            MnfUsage::Disabled
        };

        let params = OfferParamsInternal {
            interface_name: "host relay".to_owned(),
            instance_id: offer.offer.instance_id,
            interface_id: offer.offer.interface_id,
            mmio_megabytes: offer.offer.mmio_megabytes,
            mmio_megabytes_optional: offer.offer.mmio_megabytes_optional,
            subchannel_index: offer.offer.subchannel_index,
            use_mnf,
            // Preserve channel enumeration order from the host within the same
            // interface type.
            offer_order: Some(channel_id.into()),
            // Strip the confidential flags for relay channels if the host set them.
            flags: offer
                .offer
                .flags
                .with_confidential_ring_buffer(false)
                .with_confidential_external_memory(false),
            user_defined: offer.offer.user_defined,
        };

        let key = params.key();
        let new_offer = OfferInfo {
            params,
            event: offer.guest_to_host_interrupt,
            request_send,
            server_request_recv,
        };

        // Don't send the client's channel and connection ID to the server. Instead, the server will
        // decide its own IDs which are communicated back to the host as part of the open message
        // using guest-specified signal parameters, which the host must support.
        //
        // The vmbus server will ignore the monitor ID and allocate its own if MNF is handled by it
        // and not the host.
        self.vmbus_control
            .offer_core(new_offer)
            .await
            .with_context(|| format!("failed to offer relay channel {key}"))?;

        let (relay_request_send, relay_request_recv) = mesh::channel();
        let channel_task = RelayChannelTask {
            driver: Arc::clone(&self.spawner),
            channel: RelayChannel {
                channel_id: ChannelId(channel_id),
                key,
                relay_request_recv,
                request_send: offer.request_send,
                revoke_recv: offer.revoke_recv,
                server_request_send,
                server_request_recv: request_recv,
                use_interrupt_relay: Arc::clone(&self.use_interrupt_relay),
                interrupt_relay: None,
                gpadls_tearing_down: FuturesUnordered::new(),
                is_open: false,
            },
            running: self.running,
        };

        let task = self.spawner.spawn("vmbus hcl channel worker", async move {
            channel_task.run().await;
            ChannelId(channel_id)
        });

        self.channels.insert(
            ChannelId(channel_id),
            ChannelInfo::Relay(RelayChannelInfo { relay_request_send }),
        );
        self.channel_workers.push(task);

        Ok(())
    }

    async fn handle_revoked(&mut self, channel_id: ChannelId) {
        // The task has already completed, so just remove the channel from the list.
        self.channels
            .remove(&channel_id)
            .expect("channel should exist");
    }

    async fn handle_modify(
        &mut self,
        request: vmbus_server::ModifyRelayRequest,
    ) -> ModifyRelayResponse {
        // If the guest is requesting a version change, check whether that version is not newer
        // than what the host supports.
        if let Some(version) = request.version {
            if (self.version.version as u32) < version {
                return ModifyRelayResponse::Unsupported;
            }
        }

        if let Some(use_interrupt_page) = request.use_interrupt_page {
            // If the guest is using the channel bitmap, the host can't send interrupts directly and
            // must relay them through Underhill.
            self.use_interrupt_relay
                .store(use_interrupt_page, Ordering::SeqCst);
        }

        // If the monitor page is not changing, there is no need to send any request to the host.
        let state = match request.monitor_page {
            Update::Unchanged => protocol::ConnectionState::SUCCESSFUL,
            Update::Reset => {
                self.vmbus_client
                    .modify(ModifyConnectionRequest { monitor_page: None })
                    .await
            }
            Update::Set(value) => {
                self.vmbus_client
                    .modify(ModifyConnectionRequest {
                        monitor_page: Some(value),
                    })
                    .await
            }
        };

        // Use Supported only for new connections (which have a version).
        if request.version.is_some() {
            ModifyRelayResponse::Supported(state, self.version.feature_flags)
        } else {
            ModifyRelayResponse::Modified(state)
        }
    }

    async fn handle_server_request(&mut self, request: vmbus_server::ModifyRelayRequest) {
        tracing::trace!(request = ?request, "received server request");
        let result = self.handle_modify(request).await;
        self.server_response_send.send(result);
    }

    fn handle_hvsock_request(&mut self, request: HvsockConnectRequest) {
        tracing::debug!(request = ?request, "received hvsock connect request");
        let fut = self.vmbus_client.connect_hvsock(request);
        self.hvsock_requests
            .push(Box::pin(fut.map(move |offer| (request, offer))));
    }

    async fn handle_hvsock_response(
        &mut self,
        request: HvsockConnectRequest,
        offer: Option<client::OfferInfo>,
    ) {
        let success = if let Some(offer) = offer {
            match self.handle_offer(offer).await {
                Ok(()) => true,
                Err(err) => {
                    tracing::error!(
                        error = err.as_ref() as &dyn std::error::Error,
                        ?request,
                        "failed add hvsock offer"
                    );
                    false
                }
            }
        } else {
            false
        };
        self.hvsock_relay
            .response_send
            .send(HvsockConnectResult::from_request(&request, success));
    }

    async fn handle_offer_request(&mut self, request: client::OfferInfo) -> Result<()> {
        let offer = request.offer;
        if let Err(err) = self.handle_offer(request).await {
            tracing::error!(
                error = err.as_ref() as &dyn std::error::Error,
                ?offer,
                "failed to hot add offer"
            );
        }

        Ok(())
    }

    async fn run(
        &mut self,
        server_recv: mesh::Receiver<vmbus_server::ModifyRelayRequest>,
        mut offer_recv: mesh::Receiver<client::OfferInfo>,
        mut task_recv: mesh::Receiver<TaskRequest>,
    ) -> Result<()> {
        let mut server_recv = server_recv.fuse();
        loop {
            let mut offer_recv =
                OptionFuture::from(self.running.then(|| offer_recv.select_next_some()));

            futures::select! { // merge semantics
                r = server_recv.select_next_some() => {
                    self.handle_server_request(r).await;
                }
                r = self.hvsock_relay.request_receive.select_next_some() => {
                    self.handle_hvsock_request(r);
                }
                r = self.hvsock_requests.select_next_some() => {
                    self.handle_hvsock_response(r.0, r.1).await;
                }
                r = offer_recv => {
                    self.handle_offer_request(r.unwrap()).await?;
                }
                r = task_recv.recv().fuse() => {
                    match r.unwrap() {
                        TaskRequest::Inspect(req) => req.inspect(&mut *self),
                        TaskRequest::Save(rpc) => rpc.handle(async |()| {
                             self.handle_save().await
                        }).await,
                        TaskRequest::Restore(rpc) => rpc.handle(async |state|  {
                            self.handle_restore(state).await
                        }).await,
                        TaskRequest::Start => self.handle_start().await,
                        TaskRequest::Stop(rpc) => rpc.handle(async |()| self.handle_stop().await).await,
                    }
                }
                r = self.channel_workers.select_next_some() => {
                    self.handle_revoked(r).await;
                }
            }
        }
    }
}
