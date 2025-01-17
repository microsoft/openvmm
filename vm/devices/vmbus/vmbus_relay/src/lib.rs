// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

#![forbid(unsafe_code)]

mod saved_state;

use anyhow::Context;
use anyhow::Result;
use client::ModifyConnectionRequest;
use futures::future::join_all;
use futures::future::OptionFuture;
use futures::stream::FusedStream;
use futures::FutureExt;
use futures::Stream;
use futures::StreamExt;
use guid::Guid;
use inspect::Inspect;
use mesh::rpc::Rpc;
use mesh::rpc::RpcSend;
use once_cell::sync::Lazy;
use pal_async::driver::Driver;
use pal_async::driver::SpawnDriver;
use pal_async::task::Spawn;
use pal_async::task::Task;
use pal_async::wait::PolledWait;
use pal_event::Event;
use parking_lot::Mutex;
use saved_state::SavedState;
use std::collections::HashMap;
use std::fmt::Debug;
use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::AtomicU32;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::task::Poll;
use unicycle::FuturesUnordered;
use vmbus_channel::bus::ChannelRequest;
use vmbus_channel::bus::ChannelServerRequest;
use vmbus_channel::bus::GpadlRequest;
use vmbus_channel::bus::ModifyRequest;
use vmbus_channel::bus::OpenRequest;
use vmbus_channel::bus::OpenResult;
use vmbus_client as client;
use vmbus_client::VmbusClient;
use vmbus_core::protocol;
use vmbus_core::protocol::ChannelId;
use vmbus_core::protocol::FeatureFlags;
use vmbus_core::protocol::GpadlId;
use vmbus_core::HvsockConnectRequest;
use vmbus_core::HvsockConnectResult;
use vmbus_core::VersionInfo;
use vmbus_server::HvsockRelayChannelHalf;
use vmbus_server::ModifyConnectionResponse;
use vmbus_server::OfferInfo;
use vmbus_server::OfferParamsInternal;
use vmbus_server::Update;
use vmbus_server::VmbusRelayChannelHalf;
use vmbus_server::VmbusServerControl;
use vmcore::interrupt::Interrupt;

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

const VMBUS_RELAY_CLIENT_ID: Guid = Guid::from_static_str("ceb1cd55-6a3b-41c5-9473-4dd30624c3d8");

/// Represents a relay between a vmbus server on the host, and the vmbus server running in
/// Underhill, allowing offers from the host and offers from Underhill to be mixed.
///
/// The relay will connect to the host when it first receives a start request through its state
/// unit, and will remain connected until it is destroyed.
pub struct HostVmbusTransport {
    _relay_task: Task<()>,
    task_send: mesh::Sender<TaskRequest>,
    from_handle_send: Option<mesh::Sender<RequestFromHandle>>,
}

impl HostVmbusTransport {
    /// Create a new instance of the host vmbus relay.
    pub fn new(
        driver: impl SpawnDriver + Clone,
        control: Arc<VmbusServerControl>,
        channel: VmbusRelayChannelHalf,
        hvsock_relay: HvsockRelayChannelHalf,
        synic: Arc<dyn client::SynicClient>,
        msg_source: impl 'static + client::VmbusMessageSource,
    ) -> Result<Self> {
        let (offer_send, offer_recv) = mesh::channel();
        let vmbus_client = VmbusClient::new(synic.clone(), offer_send, msg_source, &driver);

        let mut relay_task = RelayTask::new(
            Arc::new(driver.clone()),
            vmbus_client,
            control,
            synic,
            channel.response_send,
            hvsock_relay,
        );

        let (task_send, task_recv) = mesh::channel();
        let (from_handle_send, from_handle_recv) = mesh::channel();

        let relay_task = driver.spawn("vmbus hcl relay", async move {
            relay_task
                .run(
                    channel.request_receive,
                    offer_recv,
                    task_recv,
                    from_handle_recv,
                )
                .await
                .unwrap()
        });

        Ok(Self {
            _relay_task: relay_task,
            task_send,
            from_handle_send: Some(from_handle_send),
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

    pub fn take_handle_sender(&mut self) -> mesh::Sender<RequestFromHandle> {
        self.from_handle_send.take().unwrap()
    }
}

impl Inspect for HostVmbusTransport {
    fn inspect(&self, req: inspect::Request<'_>) {
        self.task_send.send(TaskRequest::Inspect(req.defer()));
    }
}

impl Debug for HostVmbusTransport {
    fn fmt(&self, fmt: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        writeln!(fmt, "HostVmbusTransport")
    }
}

/// Tracks used flag indices for registering hcl_vmbus events.
/// FUTURE: This state is system global, hard-coded to SINT7. If the linux side
///         is ever modified to work with multiple SINTs this needs to be
///         refactored.
static REGISTERED_EVENT_USED_FLAG_INDICES: Lazy<Mutex<Vec<bool>>> = Lazy::new(|| {
    let indices = Mutex::new(Vec::with_capacity(64));
    indices.lock().resize(64, false);
    indices
});

/// Represents an eventfd that has been registered with /dev/hcl_vmbus to receive host interrupts.
#[derive(Inspect)]
pub struct RegisteredEvent {
    flag: u16,
    #[inspect(skip)]
    wait: PolledWait<Event>,
    #[inspect(skip)]
    hcl_vmbus: Arc<dyn client::SynicClient>,
}

impl RegisteredEvent {
    /// Creates a new event and registers it to receive interrupts. Only one
    /// event can be registered for each flag index, so on creation this will
    /// be assigned a unique index. This flag index will need to be registered
    /// with the host, and can be retrieved via a call to get_flag_index().
    pub fn new(
        driver: &(impl ?Sized + Driver),
        synic: Arc<dyn client::SynicClient>,
    ) -> Result<Self> {
        let flag = {
            let mut used_indices = REGISTERED_EVENT_USED_FLAG_INDICES.lock();
            if let Some(i) = used_indices.iter().position(|&used| !used) {
                used_indices[i] = true;
                i as u16
            } else {
                used_indices.push(true);
                (used_indices.len() - 1) as u16
            }
        };
        Self::new_internal(driver, synic, flag)
    }

    /// Creates a new event with a known flag. This is used to restore
    /// connections across save/restore.
    pub fn new_with_flag(
        driver: &(impl ?Sized + Driver),
        synic: Arc<dyn client::SynicClient>,
        flag: u16,
    ) -> Result<Self> {
        {
            let flag_index = flag as usize;
            let mut used_indices = REGISTERED_EVENT_USED_FLAG_INDICES.lock();
            if used_indices.len() <= flag_index {
                used_indices.resize(flag_index + 1, false);
            }
            if used_indices[flag_index] {
                tracing::warn!(flag_index, "Specified flag is already in use; overwriting")
            }
            used_indices[flag_index] = true;
        }
        Self::new_internal(driver, synic, flag)
    }

    fn new_internal(
        driver: &(impl ?Sized + Driver),
        synic: Arc<dyn client::SynicClient>,
        flag: u16,
    ) -> Result<Self> {
        let event = Event::new();
        synic.map_event(flag, &event)?;
        Ok(Self {
            flag,
            wait: PolledWait::new(driver, event)?,
            hcl_vmbus: synic,
        })
    }

    pub fn get_flag_index(&self) -> u16 {
        self.flag
    }

    pub fn event(&self) -> &Event {
        self.wait.get()
    }
}

impl Drop for RegisteredEvent {
    fn drop(&mut self) {
        self.hcl_vmbus.unmap_event(self.flag);
        let mut used_indices = REGISTERED_EVENT_USED_FLAG_INDICES.lock();
        used_indices[self.flag as usize] = false;
    }
}

impl Stream for RegisteredEvent {
    type Item = ();

    fn poll_next(
        mut self: Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> Poll<Option<Self::Item>> {
        let ready = Some(std::task::ready!(self.wait.poll_wait(cx)));
        Poll::Ready(ready.and_then(|r| r.ok()))
    }
}

impl FusedStream for RegisteredEvent {
    fn is_terminated(&self) -> bool {
        false
    }
}

/// State needed to relay host-to-guest interrupts.
struct InterruptRelay {
    /// Event signaled when the host sends an interrupt.
    event: RegisteredEvent,
    /// Interrupt used to signal the guest.
    interrupt: Interrupt,
}

enum RelayChannelRequest {
    Start,
    Stop(Rpc<(), ()>),
    Save(Rpc<(), saved_state::Channel>),
}

impl Debug for RelayChannelRequest {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            RelayChannelRequest::Start => f.pad("Start"),
            RelayChannelRequest::Stop(..) => f.pad("Stop"),
            RelayChannelRequest::Save(..) => f.pad("Save channel"),
        }
    }
}

struct RelayChannelInfo {
    relay_request_send: mesh::Sender<RelayChannelRequest>,
    server_request_send: mesh::Sender<ChannelServerRequest>,
}

enum ChannelInfo {
    Relay(RelayChannelInfo),
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
struct RelayChannel {
    /// The Channel Id given to us by the client
    channel_id: ChannelId,
    /// Receives requests from the relay.
    relay_request_recv: mesh::Receiver<RelayChannelRequest>,
    /// Receives requests from the server.
    server_request_recv: mesh::Receiver<ChannelRequest>,
    /// Receives responses to requests sent to the client
    response_recv: mesh::Receiver<client::ChannelResponse>,
    /// Sends requests to the client
    request_send: mesh::Sender<client::ChannelRequest>,
    /// Indicates whether or not interrupts should be relayed. This is shared with the relay server
    /// connection, which sets this to true only if the guest uses the channel bitmap.
    use_interrupt_relay: Arc<AtomicBool>,
    /// Synic instance used to register for relayed interrupts.
    synic: Arc<dyn client::SynicClient>,
    /// Connection ID used to forward guest-to-host interrupts. This is shared with the guest
    /// interrupt handler lambda.
    connection_id: Arc<AtomicU32>,
    /// State used to relay host-to-guest interrupts.
    interrupt_relay: Option<InterruptRelay>,
    /// RPCs for gpadls that are waiting for a torndown message.
    gpadls_tearing_down: HashMap<GpadlId, Rpc<(), ()>>,
}

struct RelayChannelTask {
    driver: Arc<dyn SpawnDriver>,
    channel: RelayChannel,
    running: bool,
}

impl RelayChannelTask {
    /// Relay open channel request from VTL0 to Host, responding with Open Result
    async fn handle_open_channel(
        &mut self,
        open_request: &OpenRequest,
    ) -> Result<Option<OpenResult>> {
        let mut open_data = open_request.open_data;

        // If the guest uses the channel bitmap, the host can't send interrupts
        // directly and they must be relayed.
        let redirect_interrupt = self.channel.use_interrupt_relay.load(Ordering::SeqCst);
        if redirect_interrupt {
            // Register for host interrupt notification in order to forward
            // them to the guest. Generate a unique event_flag in place of the
            // existing one since we need a unique value and this may not be
            // the only code requesting events (i.e. just because it is unique
            // in the caller's context does not mean it is in ours).
            let event =
                RegisteredEvent::new(self.driver.as_ref(), Arc::clone(&self.channel.synic))?;
            open_data.event_flag = event.get_flag_index();
            self.channel.interrupt_relay = Some(InterruptRelay {
                event,
                interrupt: open_request.interrupt.clone(),
            });
        }

        let flags = protocol::OpenChannelFlags::new().with_redirect_interrupt(redirect_interrupt);
        let opened = self
            .channel
            .request_send
            .call(
                client::ChannelRequest::Open,
                client::OpenRequest { open_data, flags },
            )
            .await?;

        if !opened {
            return Ok(None);
        }

        // Always relay guest-to-host interrupts. These can be generated when:
        //
        // * The guest is using the channel bitmap.
        // * The guest is using the MNF interface and this is implemented in the
        //   paravisor instead of the hypervisor.
        // * The guest is using HvSignalEvent and hypercall handling is emulated
        //   in the paravisor instead of in the hypervisor. This is the case for
        //   some confidential VM configurations.
        //
        // There is no cost to enabling this if it's not used.
        self.channel
            .connection_id
            .store(open_data.connection_id, Ordering::SeqCst);

        Ok(Some(OpenResult {
            guest_to_host_interrupt: self.guest_to_host_event(),
        }))
    }

    fn guest_to_host_event(&self) -> Interrupt {
        let synic = self.channel.synic.clone();
        let connection_id = self.channel.connection_id.clone();

        Interrupt::from_fn(move || {
            let connection_id = connection_id.load(Ordering::SeqCst);
            // If a channel is forcibly closed by the host (during a
            // revoke), the host interrupt can be disabled before the guest
            // is aware the channel is closed. In this case, relaying the
            // interrupt can fail, which is not a problem. For example, this
            // is the case for an hvsocket channel when the VM gets paused.
            //
            // In cases were the channel this happened on is open and
            // appears stuck, this could indicate a problem.
            if connection_id != 0 {
                if let Err(err) = synic.signal_event(connection_id, 0) {
                    tracelimit::info_ratelimited!(
                        error = &err as &dyn std::error::Error,
                        "interrupt relay failure, could be normal during channel close"
                    );
                }
            } else {
                // The channel close notification reached here but has not
                // yet made it to the guest. This is expected.
                tracing::debug!("interrupt relay request after close");
            }
        })
    }

    fn handle_close_channel(&mut self) {
        let _ = &self
            .channel
            .request_send
            .send(client::ChannelRequest::Close);

        self.channel.interrupt_relay = None;
        self.channel.connection_id.store(0, Ordering::SeqCst);
    }

    /// Relay gpadl request from VTL0 to the Host and respond with gpadl created.
    async fn handle_gpadl(&mut self, request: GpadlRequest) -> Result<bool> {
        let created = self
            .channel
            .request_send
            .call(client::ChannelRequest::Gpadl, request)
            .await?;

        Ok(created)
    }

    fn handle_gpadl_teardown(&mut self, rpc: Rpc<GpadlId, ()>) {
        let (gpadl_id, rpc) = rpc.split();
        tracing::trace!(gpadl_id = gpadl_id.0, "Tearing down GPADL");

        let _ = &self
            .channel
            .request_send
            .send(client::ChannelRequest::TeardownGpadl(gpadl_id));

        // We cannot wait for GpadlTorndown here, because the host may not send the GpadlTorndown
        // message immediately, for example if the channel is still open and the host device still
        // has the gpadl mapped. We should not block further requests while waiting for the
        // response.
        let old_value = self.channel.gpadls_tearing_down.insert(gpadl_id, rpc);
        assert!(old_value.is_none(), "duplicate gpadl teardown");
    }

    fn handle_gpadl_torndown(&mut self, gpadl_id: GpadlId) {
        tracing::trace!(gpadl_id = gpadl_id.0, "Torn down GPADL");
        let rpc = self
            .channel
            .gpadls_tearing_down
            .remove(&gpadl_id)
            .expect("gpadl not tearing down.");

        // Notify the vmbus server of completion.
        rpc.complete(());
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
        tracing::trace!(request = ?request, "received channel request");
        match request {
            ChannelRequest::Open(rpc) => {
                rpc.handle(|open_request| async move {
                    self.handle_open_channel(&open_request)
                        .await
                        .unwrap_or(None)
                })
                .await;
            }

            ChannelRequest::Gpadl(rpc) => {
                rpc.handle(|gpadl| async move { self.handle_gpadl(gpadl).await.is_ok() })
                    .await;
            }

            ChannelRequest::Close(rpc) => {
                rpc.handle(|()| async move { self.handle_close_channel() })
                    .await;
            }
            ChannelRequest::TeardownGpadl(rpc) => {
                self.handle_gpadl_teardown(rpc);
            }
            ChannelRequest::Modify(rpc) => {
                rpc.handle(|request| async move {
                    self.handle_modify_channel(request).await.unwrap_or(-1)
                })
                .await;
            }
        }

        Ok(())
    }

    /// Handle responses.
    fn handle_response(&mut self, response: &client::ChannelResponse) {
        match response {
            client::ChannelResponse::TeardownGpadl(gpadl_id) => {
                // GpadlTorndown messages aren't always sent immediately in response to a
                // GpadlTeardown message, so they can arrive at any time and must be handled here.
                self.handle_gpadl_torndown(*gpadl_id);
            }
        }
    }

    fn handle_relay_request(&mut self, request: RelayChannelRequest) {
        tracing::trace!(
            channel_id = self.channel.channel_id.0,
            ?request,
            "received relay request"
        );

        match request {
            RelayChannelRequest::Start => self.running = true,
            RelayChannelRequest::Stop(rpc) => rpc.handle_sync(|()| self.running = false),
            RelayChannelRequest::Save(rpc) => rpc.handle_sync(|_| self.handle_save()),
        }
    }

    /// Request dispatch loop
    async fn run(&mut self) {
        loop {
            let mut relay_event = OptionFuture::from(
                self.channel
                    .interrupt_relay
                    .as_mut()
                    .map(|e| e.event.select_next_some()),
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
                            self.handle_relay_request(request);
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
                r = self.channel.response_recv.next() => {
                    match r {
                        Some(response) => {
                            // Needed to avoid conflicting &mut self borrow.
                            drop(relay_event);

                            // Handle responses that can arrive at any time.
                            self.handle_response(&response);
                        }
                        None => {
                            break;
                        }
                    }
                }
                _r = relay_event => {
                    // Needed to avoid conflicting interrupt_relay borrow.
                    drop(relay_event);
                    self.channel.interrupt_relay.as_ref().unwrap().interrupt.deliver();
                }
            }
        }

        // The remaining teardown requests are those that never made it to the
        // client before the channel was revoked, but will have been torndown
        // anyways as part of the revoke. The RPCs might get dropped here before
        // the server is notified, so we still need to complete any outstanding
        // requests back to the server to avoid inconsistent state. The server
        // will ignore the completions if the channel is already released.
        self.channel
            .gpadls_tearing_down
            .drain()
            .for_each(|(_, rpc)| rpc.complete(()));

        tracing::debug!(channel_id = %self.channel.channel_id.0, "dropped channel");
    }
}

enum TaskRequest {
    Inspect(inspect::Deferred),
    Save(Rpc<(), SavedState>),
    Restore(Rpc<SavedState, Result<()>>),
    Start,
    Stop(Rpc<(), ()>),
}

pub enum RequestFromHandle {
    AddIntercept(Rpc<(Guid, mesh::Sender<InterceptChannelRequest>), Result<()>>),
}

#[derive(Copy, Clone, Debug, Eq, PartialEq)]
enum RelayState {
    Disconnected,
    Connected(VersionInfo),
}

impl RelayState {
    fn is_connected(&self) -> bool {
        matches!(self, RelayState::Connected(..))
    }

    fn version(&self) -> Option<VersionInfo> {
        if let RelayState::Connected(version) = self {
            Some(*version)
        } else {
            None
        }
    }
}

/// Dispatches requests between Server/Client.
struct RelayTask {
    spawner: Arc<dyn SpawnDriver>,
    vmbus_client: VmbusClient,
    vmbus_control: Arc<VmbusServerControl>,
    channels: HashMap<ChannelId, ChannelInfo>,
    channel_workers: FuturesUnordered<Task<RelayChannelTask>>,
    intercept_channels: HashMap<Guid, mesh::Sender<InterceptChannelRequest>>,
    relay_state: RelayState,
    synic: Arc<dyn client::SynicClient>,
    use_interrupt_relay: Arc<AtomicBool>,
    server_response_send: mesh::Sender<ModifyConnectionResponse>,
    hvsock_relay: HvsockRelayChannelHalf,
    hvsock_requests: FuturesUnordered<HvsockRequestFuture>,
    running: bool,
}

type HvsockRequestFuture =
    Pin<Box<dyn Future<Output = (HvsockConnectRequest, Option<client::OfferInfo>)> + Sync + Send>>;

impl RelayTask {
    fn new(
        spawner: Arc<dyn SpawnDriver>,
        vmbus_client: VmbusClient,
        vmbus_control: Arc<VmbusServerControl>,
        synic: Arc<dyn client::SynicClient>,
        server_response_send: mesh::Sender<ModifyConnectionResponse>,
        hvsock_relay: HvsockRelayChannelHalf,
    ) -> Self {
        Self {
            spawner,
            vmbus_client,
            vmbus_control,
            channels: HashMap::new(),
            channel_workers: FuturesUnordered::new(),
            intercept_channels: HashMap::new(),
            relay_state: RelayState::Disconnected,
            synic,
            use_interrupt_relay: Arc::new(AtomicBool::new(false)),
            server_response_send,
            hvsock_relay,
            running: false,
            hvsock_requests: FuturesUnordered::new(),
        }
    }

    async fn handle_add_intercept_device(
        &mut self,
        id: Guid,
        send: mesh::Sender<InterceptChannelRequest>,
    ) -> Result<()> {
        if self.intercept_channels.insert(id, send).is_some() {
            tracing::warn!(%id, "Replacing existing intercept device");
        }
        Ok(())
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

            self.vmbus_client.start();
            self.running = true;

            // If the relay isn't connected to the host yet, do so now. This connection will not be
            // torn down on stop; it stays connected until the relay is destroyed.
            if !self.relay_state.is_connected() {
                self.connect_client().await;
            }
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
            self.vmbus_client.stop().await;
            self.running = false;
        }
    }

    /// Translates an offer received from the client to a server offer.
    /// Additionally, sets up all the appropriate channels.
    async fn handle_offer(
        &mut self,
        offer: client::OfferInfo,
        restore_open: Option<(bool, Option<&saved_state::Channel>)>,
    ) -> Result<()> {
        let (restore, open, restored_channel) = restore_open
            .map_or((false, false, None), |(open, channel)| {
                (true, open, channel)
            });
        let restored_event_flag = restored_channel.map(|c| c.event_flag).unwrap_or(None);
        let channel_id = offer.offer.channel_id.0;

        if self.channels.contains_key(&ChannelId(channel_id)) {
            if restore {
                return Err(
                    client::RestoreError::DuplicateChannelId(offer.offer.channel_id.0).into(),
                );
            }

            return Ok(());
        }

        // Check if this channel is being intercepted. A previously relayed
        // channel cannot be intercepted on restore.
        if !restore || restored_channel.map(|c| c.intercepted).unwrap_or(false) {
            if let Some(intercept) = self.intercept_channels.get(&offer.offer.instance_id) {
                self.channels.insert(
                    ChannelId(channel_id),
                    ChannelInfo::Intercept(offer.offer.instance_id),
                );
                if let Some(saved_state) =
                    restored_channel.and_then(|c| c.try_get_intercept_save_state())
                {
                    intercept.send(InterceptChannelRequest::Restore(saved_state))
                }
                intercept.send(InterceptChannelRequest::Offer(offer));
                return Ok(());
            }
        }

        // Used to Recv requests from the server.
        let (request_send, request_recv) = mesh::channel();
        // Used to Send responses from the server
        let (server_request_send, server_request_recv) = mesh::channel();

        if offer.offer.is_dedicated != 1 {
            tracing::warn!(offer = ?offer.offer, "All offers should be dedicated with Win8+ host")
        }

        let use_mnf = offer.offer.monitor_allocated != 0;
        let params = OfferParamsInternal {
            interface_name: "host relay".to_owned(),
            instance_id: offer.offer.instance_id,
            interface_id: offer.offer.interface_id,
            mmio_megabytes: offer.offer.mmio_megabytes,
            mmio_megabytes_optional: offer.offer.mmio_megabytes_optional,
            subchannel_index: offer.offer.subchannel_index,
            // The vmbus server will ignore this field if MNF is being relayed to the host.
            use_mnf,
            // Preserve channel enumeration order from the host within the same
            // interface type.
            offer_order: Some(channel_id),
            // Strip the confidential flags for relay channels if the host set them.
            flags: offer
                .offer
                .flags
                .with_confidential_ring_buffer(false)
                .with_confidential_external_memory(false),
            user_defined: offer.offer.user_defined,
            monitor_id: use_mnf.then_some(offer.offer.monitor_id),
        };

        let key = params.key();
        let connection_id = Arc::new(AtomicU32::new(0));
        let new_offer = OfferInfo {
            params,
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
        let mut channel_task = RelayChannelTask {
            driver: Arc::clone(&self.spawner),
            channel: RelayChannel {
                channel_id: ChannelId(channel_id),
                relay_request_recv,
                request_send: offer.request_send,
                response_recv: offer.response_recv,
                server_request_recv: request_recv,
                connection_id,
                use_interrupt_relay: Arc::clone(&self.use_interrupt_relay),
                synic: Arc::clone(&self.synic),
                interrupt_relay: None,
                gpadls_tearing_down: HashMap::new(),
            },
            // New channels start out running.
            running: true,
        };

        if restore {
            let open_result = open.then(|| OpenResult {
                guest_to_host_interrupt: channel_task.guest_to_host_event(),
            });
            let result = server_request_send
                .call(ChannelServerRequest::Restore, open_result)
                .await
                .context("Failed to send restore request")?
                .map_err(|err| {
                    anyhow::Error::from(err).context("failed to restore vmbus relay channel")
                })?;

            if let Some(request) = result.open_request {
                let use_interrupt_relay = self.use_interrupt_relay.load(Ordering::SeqCst);
                if use_interrupt_relay {
                    channel_task.channel.interrupt_relay = Some(InterruptRelay {
                        event: RegisteredEvent::new_with_flag(
                            self.spawner.as_ref(),
                            Arc::clone(&self.synic),
                            restored_event_flag.unwrap_or(request.open_data.event_flag),
                        )?,
                        interrupt: request.interrupt,
                    });
                }

                // TODO: save/restore this connection ID instead of getting it
                // back from `vmbus_server`. This is fundamentally the
                // connection ID that was registered with `vmbus_client`--it so
                // happens that it matches the one `vmbus_server` assigns, but
                // this isn't necessarily always going to be true for redirected
                // interrupts.
                channel_task
                    .channel
                    .connection_id
                    .store(request.open_data.connection_id, Ordering::SeqCst);
            }
        }

        let task = self.spawner.spawn("vmbus hcl channel worker", async move {
            channel_task.run().await;
            channel_task
        });

        self.channels.insert(
            ChannelId(channel_id),
            ChannelInfo::Relay(RelayChannelInfo {
                relay_request_send,
                server_request_send,
            }),
        );
        self.channel_workers.push(task);

        Ok(())
    }

    async fn handle_revoked(&mut self, task: RelayChannelTask) {
        let channel_id = task.channel.channel_id;

        // The task has already completed, so just remove the channel from the list and notify the server of the revoke.
        let channel = self
            .channels
            .remove(&channel_id)
            .expect("channel should exist");

        let ChannelInfo::Relay(channel) = channel else {
            unreachable!()
        };

        // Dropping the channel would revoke it, but since that's not synchronized there's a chance
        // we reoffer the channel before the server receives the revoke. Using the request ensures
        // that won't happen.
        if let Err(err) = channel
            .server_request_send
            .call(ChannelServerRequest::Revoke, ())
            .await
        {
            tracing::warn!(
                channel_id = channel_id.0,
                ?err,
                "failed to send revoke request"
            );
        }
    }

    async fn connect_client(&mut self) {
        assert!(!self.relay_state.is_connected());
        tracing::debug!("connecting vmbus relay");

        // Always use VP0 for messages from the host, regardless of what the guest requested, since
        // the relay cannot receive messages on other VPs. This does not affect messages sent to
        // VTL0 by the vmbus server.
        let version = self
            .vmbus_client
            .connect(0, None, VMBUS_RELAY_CLIENT_ID)
            .await
            .expect("Client was in an incorrect state for initiate contact.");

        if version.feature_flags & REQUIRED_FEATURE_FLAGS != REQUIRED_FEATURE_FLAGS {
            panic!("Underhill host must support required feature flags. Required: {REQUIRED_FEATURE_FLAGS:?}, actual: {:?}.", version.feature_flags)
        }

        for offer in self.vmbus_client.request_offers().await {
            if let Err(err) = self.handle_offer(offer, None).await {
                tracing::error!(
                    error = err.as_ref() as &dyn std::error::Error,
                    "failed to offer initial channel"
                );
            }
        }

        self.relay_state = RelayState::Connected(version);
        tracing::debug!("vmbus relay connected");
    }

    async fn handle_modify(
        &mut self,
        request: vmbus_server::ModifyRelayRequest,
    ) -> ModifyConnectionResponse {
        let connected_version = self
            .relay_state
            .version()
            .expect("Can't receive a modify request while not connected.");

        // If the guest is requesting a version change, check whether that version is not newer
        // than what the host supports.
        if let Some(version) = request.version {
            if (connected_version.version as u32) < version {
                return ModifyConnectionResponse::Unsupported;
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
                    .access()
                    .modify(ModifyConnectionRequest { monitor_page: None })
                    .await
            }
            Update::Set(value) => {
                self.vmbus_client
                    .access()
                    .modify(ModifyConnectionRequest {
                        monitor_page: Some(value),
                    })
                    .await
            }
        };

        ModifyConnectionResponse::Supported(state, connected_version.feature_flags)
    }

    async fn handle_server_request(&mut self, request: vmbus_server::ModifyRelayRequest) {
        tracing::trace!(request = ?request, "received server request");
        let result = self.handle_modify(request).await;
        self.server_response_send.send(result);
    }

    fn handle_hvsock_request(&mut self, request: HvsockConnectRequest) {
        tracing::debug!(request = ?request, "received hvsock connect request");
        let fut = self.vmbus_client.access().connect_hvsock(request);
        self.hvsock_requests
            .push(Box::pin(fut.map(move |offer| (request, offer))));
    }

    async fn handle_hvsock_response(
        &mut self,
        request: HvsockConnectRequest,
        offer: Option<client::OfferInfo>,
    ) {
        let success = if let Some(offer) = offer {
            match self.handle_offer(offer, None).await {
                Ok(()) => true,
                Err(err) => {
                    tracing::error!(
                        error = err.as_ref() as &dyn std::error::Error,
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
        if let Err(err) = self.handle_offer(request, None).await {
            tracing::error!(
                error = err.as_ref() as &dyn std::error::Error,
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
        mut from_handle_recv: mesh::Receiver<RequestFromHandle>,
    ) -> Result<()> {
        let mut server_recv = server_recv.fuse();
        loop {
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
                r = offer_recv.select_next_some() => {
                    self.handle_offer_request(r).await?;
                }
                r = task_recv.recv().fuse() => {
                    match r.unwrap() {
                        TaskRequest::Inspect(req) => req.inspect(&*self),
                        TaskRequest::Save(rpc) => rpc.handle(|()| {
                             self.handle_save()
                        }).await,
                        TaskRequest::Restore(rpc) => rpc.handle(|state|  {
                            self.handle_restore(state)
                        }).await,
                        TaskRequest::Start => self.handle_start().await,
                        TaskRequest::Stop(rpc) => rpc.handle(|()| self.handle_stop()).await,
                    }
                }
                r = from_handle_recv.select_next_some() => {
                    match r {
                        RequestFromHandle::AddIntercept(rpc) => rpc.handle(|(id, send)| self.handle_add_intercept_device(id, send)).await,
                    }
                }
                r = self.channel_workers.select_next_some() => {
                    self.handle_revoked(r).await;
                }
            }
        }
    }
}

impl Inspect for RelayTask {
    fn inspect(&self, req: inspect::Request<'_>) {
        let mut resp = req.respond();
        resp.field("vmbus_client", &self.vmbus_client);
    }
}
