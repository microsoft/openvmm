// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Trait-based VMBus channel support.

use crate::bus::ChannelRequest;
use crate::bus::ChannelServerRequest;
use crate::bus::ModifyRequest;
use crate::bus::OfferInput;
use crate::bus::OfferParams;
use crate::bus::OfferResources;
use crate::bus::OpenRequest;
use crate::bus::ParentBus;
use crate::gpadl::GpadlMap;
use crate::gpadl::GpadlMapView;
use anyhow::Context;
use async_trait::async_trait;
use futures::StreamExt;
use futures::stream::SelectAll;
use futures::stream::select;
use inspect::Inspect;
use inspect::InspectMut;
use mesh::RecvError;
use mesh::rpc::FailableRpc;
use mesh::rpc::Rpc;
use mesh::rpc::RpcSend;
use pal_async::task::Spawn;
use pal_async::task::Task;
use pal_event::Event;
use std::any::Any;
use std::collections::BTreeSet;
use std::marker::PhantomData;
use std::pin::pin;
use std::sync::Arc;
use thiserror::Error;
use tracing::instrument;
use vmbus_core::TaggedStream;
use vmbus_core::protocol::GpadlId;
use vmbus_ring::gparange::MultiPagedRangeBuf;
use vmcore::notify::Notify;
use vmcore::save_restore::RestoreError;
use vmcore::save_restore::SaveError;
use vmcore::save_restore::SavedStateBlob;
use vmcore::slim_event::SlimEvent;

/// An error when opening a channel.
pub type ChannelOpenError = anyhow::Error;

/// Trait implemented by VMBus devices.
#[async_trait]
pub trait VmbusDevice: Send + Any + InspectMut {
    /// The offer parameters.
    fn offer(&self) -> OfferParams;

    /// The maximum number of subchannels supported by this device.
    fn max_subchannels(&self) -> u16 {
        0
    }

    /// Installs resources used by the device.
    fn install(&mut self, resources: DeviceResources);

    /// Opens the channel number `channel_idx`.
    async fn open(
        &mut self,
        channel_idx: u16,
        open_request: &OpenRequest,
    ) -> Result<(), ChannelOpenError>;

    /// Closes the channel number `channel_idx`.
    async fn close(&mut self, channel_idx: u16);

    /// Notifies the device that interrupts for channel will now target `target_vp`.
    async fn retarget_vp(&mut self, channel_idx: u16, target_vp: u32);

    /// Start processing of all channels.
    fn start(&mut self);

    /// Stop processing of all channels.
    async fn stop(&mut self);

    /// Returns a trait used to save/restore the channel.
    ///
    /// Returns `None` if save/restore is not supported, in which case the
    /// channel will be revoked and reoffered on restore.
    fn supports_save_restore(&mut self) -> Option<&mut dyn SaveRestoreVmbusDevice>;
}

/// Trait for vmbus devices that implement save/restore.
#[async_trait]
pub trait SaveRestoreVmbusDevice: VmbusDevice {
    /// Save the stopped device.
    async fn save(&mut self) -> Result<SavedStateBlob, SaveError>;

    /// Restore the stopped device.
    ///
    /// `control` must be used to restore the channel state in the server and to
    /// get the GPADL and interrupt state.
    async fn restore(
        &mut self,
        control: RestoreControl<'_>,
        state: SavedStateBlob,
    ) -> Result<(), RestoreError>;
}

/// Resources used by the device to communicate with the guest.
#[derive(Debug, Default)]
pub struct DeviceResources {
    /// Memory resources for the offer.
    pub offer_resources: OfferResources,
    /// A map providing access to GPADLs.
    pub gpadl_map: GpadlMapView,
    /// The control object for enabling subchannels.
    pub channel_control: ChannelControl,
    /// The resources for each channel.
    pub channels: Vec<ChannelResources>,
}

/// Resources used by an individual channel.
#[derive(Debug)]
pub struct ChannelResources {
    /// An event signaled by the guest.
    pub event: Notify,
}

/// Control object for enabling subchannels.
#[derive(Debug, Default, Clone)]
pub struct ChannelControl {
    send: Option<mesh::Sender<u16>>,
    max: u16,
}

/// Error indicating that too many subchannels were requested.
#[derive(Debug, Error)]
#[error("too many subchannels requested")]
pub struct TooManySubchannels;

impl ChannelControl {
    /// Enables the first `count` subchannels.
    ///
    /// If more than `count` subchannels are already enabled, this does nothing.
    ///
    /// Fails if `count` is bigger than the requested maximum returned by
    /// [`VmbusDevice::max_subchannels`].
    pub fn enable_subchannels(&self, count: u16) -> Result<(), TooManySubchannels> {
        if count > self.max {
            return Err(TooManySubchannels);
        }
        if let Some(send) = &self.send {
            send.send(count);
        }
        Ok(())
    }

    /// Returns the maximum number of supported subchannels.
    pub fn max_subchannels(&self) -> u16 {
        self.max
    }
}

/// A handle to an offered channel.
///
/// The channel will be revoked when this is dropped.
#[must_use]
#[derive(Inspect)]
pub(crate) struct GenericChannelHandle {
    #[inspect(flatten, send = "StateRequest::Inspect")]
    state_req: mesh::Sender<StateRequest>,
    #[inspect(skip)]
    task: Task<Box<dyn VmbusDevice>>,
}

#[derive(Debug)]
enum StateRequest {
    /// Start asynchronous operations.
    Start,
    /// Stop asynchronous operations.
    Stop(Rpc<(), ()>),

    /// Reset to initial state.
    ///
    /// Must be stopped.
    Reset(Rpc<(), ()>),

    /// Save state.
    ///
    /// Must be stopped.
    Save(FailableRpc<(), Option<SavedStateBlob>>),

    /// Restore state.
    ///
    /// Must be stopped.
    Restore(FailableRpc<SavedStateBlob, ()>),

    /// Inspect state.
    Inspect(inspect::Deferred),
}

impl std::fmt::Debug for GenericChannelHandle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.pad("ChannelHandle")
    }
}

impl GenericChannelHandle {
    /// Revokes the channel, returning it if the VMBus server is still running.
    pub async fn revoke(self) -> Option<Box<dyn VmbusDevice>> {
        drop(self.state_req);
        Some(self.task.await)
    }

    pub fn start(&self) {
        self.state_req.send(StateRequest::Start);
    }

    pub async fn stop(&self) {
        self.state_req
            .call(StateRequest::Stop, ())
            .await
            .expect("critical channel failure")
    }

    pub async fn reset(&self) {
        self.state_req
            .call(StateRequest::Reset, ())
            .await
            .expect("critical channel failure")
    }

    pub async fn save(&self) -> anyhow::Result<Option<SavedStateBlob>> {
        self.state_req
            .call(StateRequest::Save, ())
            .await
            .expect("critical channel failure")
            .map_err(|err| err.into())
    }

    pub async fn restore(&self, buffer: SavedStateBlob) -> anyhow::Result<()> {
        self.state_req
            .call(StateRequest::Restore, buffer)
            .await
            .expect("critical channel failure")
            .map_err(|err| err.into())
    }
}

/// A handle to an offered channel.
///
/// The channel will be revoked when this is dropped.
#[must_use]
#[derive(Inspect)]
#[inspect(transparent)]
pub struct ChannelHandle<T: ?Sized>(GenericChannelHandle, PhantomData<fn() -> Box<T>>);

impl<T: ?Sized> std::fmt::Debug for ChannelHandle<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.pad("ChannelHandle")
    }
}

impl<T: 'static + VmbusDevice> ChannelHandle<T> {
    /// Revokes the channel, returning it if the VMBus server is still running.
    pub async fn revoke(self) -> Option<T> {
        let device = self.0.revoke().await? as Box<dyn Any>;
        Some(
            *device
                .downcast()
                .expect("type must match the one used to create it"),
        )
    }
}

impl ChannelHandle<dyn VmbusDevice> {
    /// Revokes the channel, returning it if the VMBus server is still running.
    pub async fn revoke(self) -> Option<Box<dyn VmbusDevice>> {
        self.0.revoke().await
    }
}

impl<T: 'static + VmbusDevice + ?Sized> ChannelHandle<T> {
    /// Starts the device.
    pub fn start(&self) {
        self.0.start()
    }

    /// Stops the device.
    pub async fn stop(&self) {
        self.0.stop().await
    }

    /// Resets a stopped device.
    pub async fn reset(&self) {
        self.0.reset().await
    }

    /// Saves a stopped device.
    pub async fn save(&self) -> anyhow::Result<Option<SavedStateBlob>> {
        self.0.save().await
    }

    /// Restores a stopped device.
    pub async fn restore(&self, buffer: SavedStateBlob) -> anyhow::Result<()> {
        self.0.restore(buffer).await
    }
}

async fn offer_generic(
    driver: &impl Spawn,
    bus: &(impl ParentBus + ?Sized),
    mut channel: Box<dyn VmbusDevice>,
) -> anyhow::Result<GenericChannelHandle> {
    let offer = channel.offer();
    let max_subchannels = channel.max_subchannels();
    let instance_id = offer.instance_id;
    let (request_send, request_recv) = mesh::channel();
    let (server_request_send, server_request_recv) = mesh::channel();
    let (state_req_send, state_req_recv) = mesh::channel();

    let use_event = bus.use_event();

    let events: Vec<_> = (0..max_subchannels + 1)
        .map(|_| {
            if use_event {
                Notify::from_event(Event::new())
            } else {
                Notify::from_slim_event(Arc::new(SlimEvent::new()))
            }
        })
        .collect();

    let request = OfferInput {
        params: offer,
        event: events[0].clone().interrupt(),
        request_send,
        server_request_recv,
    };

    let gpadl_map = GpadlMap::new();

    let offer_result = bus.add_child(request).await?;

    let resources = events
        .iter()
        .map(|event| ChannelResources {
            event: event.clone(),
        })
        .collect();

    let (subchannel_enable_send, subchannel_enable_recv) = mesh::channel();
    channel.install(DeviceResources {
        offer_resources: offer_result,
        gpadl_map: gpadl_map.clone().view(),
        channels: resources,
        channel_control: ChannelControl {
            send: Some(subchannel_enable_send),
            max: max_subchannels,
        },
    });

    let bus = bus.clone_bus();
    let task = driver.spawn(format!("vmbus offer {}", instance_id), async move {
        let device = Device::new(
            request_recv,
            server_request_send,
            events,
            gpadl_map,
            subchannel_enable_recv,
        );
        device
            .run_channel(bus.as_ref(), channel.as_mut(), state_req_recv)
            .await;
        channel
    });

    Ok(GenericChannelHandle {
        state_req: state_req_send,
        task,
    })
}

/// A control interface for use to restore channels during the lifetime of the
/// [`SaveRestoreVmbusDevice::restore`] method.
pub struct RestoreControl<'a> {
    device: &'a mut Device,
    bus: &'a dyn ParentBus,
    offer: OfferParams,
}

impl RestoreControl<'_> {
    /// Restore the channel and subchannels.
    ///
    /// If this is never called, then the channel is revoked and reoffered
    /// instead of restored.
    ///
    /// `states` contains a boolean for the channel and each offered subchannel.
    /// If true, restore the channel into an open state. If false, restore it
    /// into a closed state.
    pub async fn restore(
        &mut self,
        states: &[bool],
    ) -> Result<Vec<Option<OpenRequest>>, ChannelRestoreError> {
        self.device.restore(self.bus, &self.offer, states).await
    }
}

/// An error returned by [`RestoreControl::restore`].
#[derive(Debug, Error)]
pub enum ChannelRestoreError {
    /// Failed to enable subchannels.
    #[error("failed to enable subchannels")]
    EnablingSubchannels(#[source] anyhow::Error),
    /// Failed to restore vmbus channel.
    #[error("failed to restore vmbus channel")]
    RestoreError(#[source] anyhow::Error),
    /// Failed to restore gpadl.
    #[error("failed to restore gpadl")]
    GpadlError(#[source] vmbus_ring::gparange::Error),
}

impl From<ChannelRestoreError> for RestoreError {
    fn from(err: ChannelRestoreError) -> Self {
        RestoreError::Other(err.into())
    }
}

enum DeviceState {
    Running,
    // Track updates while the channel is stopped. If it is restarted, need to
    // process outstanding requests. If the channel goes through save/restore,
    // vmbus_server will resend the requests.
    Stopped(Vec<(usize, ChannelRequest)>),
}

struct Device {
    state: DeviceState,
    server_requests: Vec<mesh::Sender<ChannelServerRequest>>,
    open: Vec<bool>,
    subchannel_gpadls: Vec<BTreeSet<GpadlId>>,
    requests: SelectAll<TaggedStream<usize, mesh::Receiver<ChannelRequest>>>,
    events: Vec<Notify>,
    gpadl_map: Arc<GpadlMap>,
    subchannel_enable_recv: mesh::Receiver<u16>,
}

impl Device {
    fn new(
        request_recv: mesh::Receiver<ChannelRequest>,
        server_request_send: mesh::Sender<ChannelServerRequest>,
        events: Vec<Notify>,
        gpadl_map: Arc<GpadlMap>,
        subchannel_enable_recv: mesh::Receiver<u16>,
    ) -> Self {
        let open: Vec<bool> = vec![false];
        let subchannel_gpadls: Vec<BTreeSet<GpadlId>> = vec![];
        let mut requests: SelectAll<TaggedStream<usize, mesh::Receiver<ChannelRequest>>> =
            SelectAll::new();
        requests.push(TaggedStream::new(0, request_recv));
        Self {
            state: DeviceState::Running,
            server_requests: vec![server_request_send],
            open,
            subchannel_gpadls,
            requests,
            events,
            gpadl_map,
            subchannel_enable_recv,
        }
    }

    /// Runs a VMBus channel, taking requests from `open_recv`.
    async fn run_channel(
        mut self,
        bus: &dyn ParentBus,
        channel: &mut dyn VmbusDevice,
        state_req_recv: mesh::Receiver<StateRequest>,
    ) {
        enum Event {
            Request(usize, Option<ChannelRequest>),
            EnableSubchannels(u16),
            StateRequest(Result<StateRequest, RecvError>),
        }

        let mut state_req_recv = pin!(futures::stream::unfold(state_req_recv, async |mut recv| {
            Some((recv.recv().await, recv))
        }));

        let map_request = |(idx, req)| Event::Request(idx, req);
        loop {
            let mut s = select(
                (&mut self.requests).map(map_request),
                select(
                    (&mut self.subchannel_enable_recv).map(Event::EnableSubchannels),
                    (&mut state_req_recv).map(Event::StateRequest),
                ),
            );
            if let Some(event) = s.next().await {
                match event {
                    Event::Request(idx, Some(request)) => {
                        self.handle_channel_request(idx, request, channel).await;
                    }
                    Event::Request(_idx, None) => continue,
                    Event::EnableSubchannels(count) => {
                        let offer = channel.offer();
                        let _ = self.enable_channels(bus, &offer, count as usize + 1).await;
                    }
                    Event::StateRequest(Ok(request)) => {
                        self.handle_state_request(request, channel, bus).await;
                    }
                    Event::StateRequest(Err(_)) => {
                        // Revoke.
                        break;
                    }
                }
            }
        }
        // Revoke the channel.
        drop(self.server_requests);
        // Wait for the revokes to finish.
        // When vmbus (sub)channels are closed, `self.requests` ends up with stale
        // channels i.e. (self.requests.value.is_none()) that are not getting cleaned
        // up. Waiting on those channels never completes here. Workaround the issue by
        // only waiting on `valid` channels.
        // TODO: The original issue should be fixed and the code here should be reverted
        //       to wait for all (i.e. while self.requests.next().await.is_some() {})
        for recv in self.requests.iter_mut() {
            if recv.value().is_some() {
                while recv.next().await.is_some() {}
            }
        }

        for subchannel_idx in (0..self.open.len()).rev() {
            if self.open[subchannel_idx] {
                channel.close(subchannel_idx as u16).await;
            }
        }
    }

    #[instrument(level = "debug", skip_all, fields(channel_idx, ?request))]
    async fn handle_channel_request(
        &mut self,
        channel_idx: usize,
        request: ChannelRequest,
        channel: &mut dyn VmbusDevice,
    ) {
        // When the device is stopped, the wrapped channel should not receive
        // any new vmbus requests. The 'close' callback is special-cased to
        // handle vmbus_server reset, and the GPADL requests are handled without a
        // callback. This leaves 'open' and 'modify' which will be pended until
        // restart.
        if matches!(request, ChannelRequest::Open(_) | ChannelRequest::Modify(_)) {
            if let DeviceState::Stopped(pending_messages) = &mut self.state {
                pending_messages.push((channel_idx, request));
                return;
            }
        }

        match request {
            ChannelRequest::Open(rpc) => {
                rpc.handle(async |open_request| {
                    self.handle_open(channel, channel_idx, open_request).await
                })
                .await
            }
            ChannelRequest::Close(rpc) => {
                rpc.handle(async |()| {
                    self.handle_close(channel_idx, channel).await;
                })
                .await
            }
            ChannelRequest::Gpadl(rpc) => rpc.handle_sync(|gpadl| {
                self.handle_gpadl(gpadl.id, gpadl.count, gpadl.buf, channel_idx);
                true
            }),
            ChannelRequest::TeardownGpadl(rpc) => {
                self.handle_teardown_gpadl(rpc, channel_idx);
            }
            ChannelRequest::Modify(rpc) => {
                rpc.handle(async |req| {
                    self.handle_modify(channel, channel_idx, req).await;
                    0
                })
                .await
            }
        }
    }

    async fn handle_open(
        &mut self,
        channel: &mut dyn VmbusDevice,
        channel_idx: usize,
        open_request: OpenRequest,
    ) -> bool {
        assert!(!self.open[channel_idx]);
        // N.B. Any asynchronous GPADL requests will block while in
        //      open(). This should be fine for all known devices.
        let opened = channel
            .open(channel_idx as u16, &open_request)
            .await
            .inspect_err(|error| {
                tracelimit::error_ratelimited!(
                    error = error.as_ref() as &dyn std::error::Error,
                    "failed to open channel"
                );
            })
            .is_ok();
        self.open[channel_idx] = opened;
        opened
    }

    async fn handle_close(&mut self, channel_idx: usize, channel: &mut dyn VmbusDevice) {
        assert!(self.open[channel_idx]);
        if channel_idx == 0 {
            // Revoke all subchannels.
            self.server_requests.truncate(1);
            for recv in self.requests.iter_mut() {
                if let Some(&idx) = recv.value() {
                    if idx > 0 {
                        while recv.next().await.is_some() {}
                    }
                }
            }
            for subchannel_idx in 1..self.open.len() {
                if self.open[subchannel_idx] {
                    channel.close(subchannel_idx as u16).await;
                }
                for &gpadl_id in &self.subchannel_gpadls[subchannel_idx - 1] {
                    self.gpadl_map.remove(gpadl_id, Box::new(|| ()));
                }
            }
            self.open.truncate(1);
            self.subchannel_gpadls.clear();
        }
        channel.close(channel_idx as u16).await;
        self.open[channel_idx] = false;
        if channel_idx == 0 {
            // Drain any stale enable subchannel requests.
            while self.subchannel_enable_recv.try_recv().is_ok() {}
        }
    }

    fn handle_gpadl(&mut self, id: GpadlId, count: u16, buf: Vec<u64>, channel_idx: usize) {
        self.gpadl_map.add(
            id,
            MultiPagedRangeBuf::from_range_buffer(count.into(), buf).unwrap(),
        );
        if channel_idx > 0 {
            self.subchannel_gpadls[channel_idx - 1].insert(id);
        }
    }

    fn handle_teardown_gpadl(&mut self, rpc: Rpc<GpadlId, ()>, channel_idx: usize) {
        let id = *rpc.input();
        if let Some(f) = self.gpadl_map.remove(
            id,
            Box::new(move || {
                rpc.complete(());
            }),
        ) {
            f()
        }
        if channel_idx > 0 {
            assert!(self.subchannel_gpadls[channel_idx - 1].remove(&id));
        }
    }

    async fn handle_modify(
        &mut self,
        channel: &mut dyn VmbusDevice,
        channel_idx: usize,
        req: ModifyRequest,
    ) {
        match req {
            ModifyRequest::TargetVp { target_vp } => {
                channel.retarget_vp(channel_idx as u16, target_vp).await
            }
        }
    }

    #[instrument(level = "debug", skip_all, fields(?request))]
    async fn handle_state_request(
        &mut self,
        request: StateRequest,
        channel: &mut dyn VmbusDevice,
        bus: &dyn ParentBus,
    ) {
        match request {
            StateRequest::Start => {
                channel.start();
                if let DeviceState::Stopped(pending_messages) =
                    std::mem::replace(&mut self.state, DeviceState::Running)
                {
                    for (channel_idx, request) in pending_messages.into_iter() {
                        self.handle_channel_request(channel_idx, request, channel)
                            .await;
                    }
                }
            }
            StateRequest::Stop(rpc) => {
                if matches!(self.state, DeviceState::Running) {
                    self.state = DeviceState::Stopped(Vec::new());
                    rpc.handle(async |()| {
                        channel.stop().await;
                    })
                    .await;
                } else {
                    rpc.complete(());
                }
            }
            StateRequest::Reset(rpc) => {
                if let DeviceState::Stopped(pending_messages) = &mut self.state {
                    pending_messages.clear();
                }
                rpc.complete(());
            }
            StateRequest::Save(rpc) => {
                rpc.handle_failable(async |()| {
                    if let Some(channel) = channel.supports_save_restore() {
                        channel.save().await.map(Some)
                    } else {
                        Ok(None)
                    }
                })
                .await;
            }
            StateRequest::Restore(rpc) => {
                rpc.handle_failable(async |buffer| {
                    let channel = channel
                        .supports_save_restore()
                        .context("saved state not supported")?;
                    let control = RestoreControl {
                        device: &mut *self,
                        offer: channel.offer(),
                        bus,
                    };
                    channel
                        .restore(control, buffer)
                        .await
                        .map_err(anyhow::Error::from)?;
                    anyhow::Ok(())
                })
                .await;
            }
            StateRequest::Inspect(deferred) => {
                deferred.inspect(&mut *channel);
            }
        }
    }

    async fn enable_channels(
        &mut self,
        bus: &dyn ParentBus,
        offer: &OfferParams,
        count: usize,
    ) -> anyhow::Result<()> {
        // Offer new subchannels.
        let mut r = Ok(());
        for subchannel_idx in self.server_requests.len()..count {
            let (request_send, request_recv) = mesh::channel();
            let (server_request_send, server_request_recv) = mesh::channel();
            let request = OfferInput {
                params: OfferParams {
                    subchannel_index: subchannel_idx as u16,
                    ..offer.clone()
                },
                event: self.events[subchannel_idx].clone().interrupt(),
                request_send,
                server_request_recv,
            };
            match bus.add_child(request).await {
                Ok(_) => {
                    self.requests
                        .push(TaggedStream::new(subchannel_idx, request_recv));
                    self.server_requests.push(server_request_send);
                    self.subchannel_gpadls.push(BTreeSet::new());
                    self.open.push(false);
                }
                Err(err) => {
                    tracing::error!(
                        error = err.as_ref() as &dyn std::error::Error,
                        "could not offer subchannel"
                    );
                    if r.is_ok() {
                        r = Err(err);
                    }
                }
            }
        }
        r
    }

    pub async fn restore(
        &mut self,
        bus: &dyn ParentBus,
        offer: &OfferParams,
        states: &[bool],
    ) -> Result<Vec<Option<OpenRequest>>, ChannelRestoreError> {
        self.enable_channels(bus, offer, states.len())
            .await
            .map_err(ChannelRestoreError::EnablingSubchannels)?;

        let mut results = Vec::with_capacity(states.len());
        for (channel_idx, open) in states.iter().copied().enumerate() {
            let result = self.server_requests[channel_idx]
                .call_failable(ChannelServerRequest::Restore, open)
                .await
                .map_err(|err| ChannelRestoreError::RestoreError(err.into()))?;

            assert!(open == result.open_request.is_some());

            for gpadl in result.gpadls {
                let buf = match MultiPagedRangeBuf::from_range_buffer(
                    gpadl.request.count.into(),
                    gpadl.request.buf,
                ) {
                    Ok(buf) => buf,
                    Err(err) => {
                        if gpadl.accepted {
                            return Err(ChannelRestoreError::GpadlError(err));
                        } else {
                            // The GPADL will be reoffered later and we can fail
                            // it then.
                            continue;
                        }
                    }
                };
                self.gpadl_map.add(gpadl.request.id, buf);
                if channel_idx > 0 {
                    self.subchannel_gpadls[channel_idx - 1].insert(gpadl.request.id);
                }
            }

            results.push(result.open_request);
        }
        self.open.copy_from_slice(states);
        Ok(results)
    }
}

/// Offers a new channel, returning a typed handle to get back the original
/// channel when it's revoked.
pub async fn offer_channel<T: 'static + VmbusDevice>(
    driver: &impl Spawn,
    bus: &(impl ParentBus + ?Sized),
    channel: T,
) -> anyhow::Result<ChannelHandle<T>> {
    let handle = offer_generic(driver, bus, Box::new(channel)).await?;
    Ok(ChannelHandle(handle, PhantomData))
}

/// Offers a new channel with the type erased.
pub async fn offer_generic_channel(
    driver: &impl Spawn,
    bus: &(impl ParentBus + ?Sized),
    channel: Box<dyn VmbusDevice>,
) -> anyhow::Result<ChannelHandle<dyn VmbusDevice>> {
    let handle = offer_generic(driver, bus, channel).await?;
    Ok(ChannelHandle(handle, PhantomData))
}
