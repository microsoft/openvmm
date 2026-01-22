// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! This module contains logic used to intercept from VTL2 a vmbus device
//! provided for a VTL0 guest. This requires the vmbus relay to be active,
//! which will filter the device out from the list provided to the VTL0 guest
//! and send any vmbus notifications for that device to the
//! SimpleVmbusClientDeviceWrapper instance.

#![expect(missing_docs)]
#![forbid(unsafe_code)]

pub mod ring_buffer;

use crate::ring_buffer::MemoryBlockRingBuffer;
use anyhow::Context;
use anyhow::Result;
use futures::StreamExt;
use futures_concurrency::stream::Merge;
use guid::Guid;
use inspect::InspectMut;
use mesh::rpc::RpcSend;
use pal_async::driver::SpawnDriver;
use std::future::Future;
use std::future::pending;
use std::pin::pin;
use std::sync::Arc;
use task_control::AsyncRun;
use task_control::Cancelled;
use task_control::InspectTaskMut;
use task_control::StopTask;
use task_control::TaskControl;
use tracing::Instrument;
use user_driver::DmaClient;
use user_driver::memory::MemoryBlock;
use vmbus_channel::ChannelClosed;
use vmbus_channel::RawAsyncChannel;
use vmbus_channel::SignalVmbusChannel;
use vmbus_channel::bus::GpadlRequest;
use vmbus_channel::bus::OpenData;
use vmbus_client::ChannelRequest;
use vmbus_client::OfferInfo;
use vmbus_client::OpenOutput;
use vmbus_client::OpenRequest;
use vmbus_core::protocol::GpadlId;
use vmbus_core::protocol::UserDefinedData;
use vmbus_relay::InterceptChannelRequest;
use vmbus_ring::IncomingRing;
use vmbus_ring::OutgoingRing;
use vmbus_ring::PAGE_SIZE;
use vmcore::interrupt::Interrupt;
use vmcore::notify::Notify;
use vmcore::notify::PolledNotify;
use vmcore::save_restore::NoSavedState;
use vmcore::save_restore::SavedStateBlob;
use vmcore::save_restore::SavedStateRoot;
use zerocopy::FromZeros;

pub enum OfferResponse {
    Ignore,
    Open,
}

pub trait SimpleVmbusClientDevice {
    /// The saved state type.
    type SavedState: SavedStateRoot + Send + Sync;

    /// The type used to run an open channel.
    type Runner: 'static + Send + Sync;

    /// Inspects a channel.
    fn inspect(&mut self, req: inspect::Request<'_>, runner: Option<&mut Self::Runner>);

    /// Returns the instance ID of the matching device.
    fn instance_id(&self) -> Guid;

    /// Respond to a new channel offer for a device matching instance_id().
    fn offer(&self, offer: &vmbus_core::protocol::OfferChannel) -> OfferResponse;

    /// Open successful for the channel number `channel_idx`.
    ///
    /// When the channel is closed, the runner will be dropped.
    fn open(
        &mut self,
        channel_idx: u16,
        channel: RawAsyncChannel<MemoryBlockRingBuffer>,
    ) -> Result<Self::Runner>;

    /// Closes the channel number `channel_idx` after the runner has been
    /// dropped.
    fn close(&mut self, channel_idx: u16);

    /// Returns a trait used to save/restore the device.
    fn supports_save_restore(
        &mut self,
    ) -> Option<
        &mut dyn SaveRestoreSimpleVmbusClientDevice<
            SavedState = Self::SavedState,
            Runner = Self::Runner,
        >,
    >;
}

pub trait SimpleVmbusClientDeviceAsync: SimpleVmbusClientDevice + 'static + Send + Sync {
    /// Runs an open channel until `stop` is signaled.
    fn run(
        &mut self,
        stop: &mut StopTask<'_>,
        runner: &mut Self::Runner,
    ) -> impl Send + Future<Output = Result<(), Cancelled>>;
}

/// Trait implemented by simple vmbus client devices that support save/restore.
///
/// If you implement this, make sure to return `Some(self)` from
/// [`SimpleVmbusClientDevice::supports_save_restore`].
pub trait SaveRestoreSimpleVmbusClientDevice: SimpleVmbusClientDevice {
    /// Saves the channel.
    ///
    /// Will only be called if the channel is open.
    fn save_open(&mut self, runner: &Self::Runner) -> Self::SavedState;

    /// Restores the channel.
    ///
    /// Will only be called if the channel was saved open.
    fn restore_open(
        &mut self,
        state: Self::SavedState,
        channel: RawAsyncChannel<MemoryBlockRingBuffer>,
    ) -> Result<Self::Runner>;
}

#[derive(InspectMut)]
pub struct SimpleVmbusClientDeviceWrapper<T: SimpleVmbusClientDeviceAsync> {
    instance_id: Guid,
    #[inspect(skip)]
    spawner: Arc<dyn SpawnDriver>,
    #[inspect(mut)]
    vmbus_listener: TaskControl<SimpleVmbusClientDeviceTask<T>, SimpleVmbusClientDeviceTaskState>,
}

impl<T: SimpleVmbusClientDeviceAsync> SimpleVmbusClientDeviceWrapper<T> {
    /// Create a new instance.
    pub fn new(
        driver: impl SpawnDriver + Clone,
        dma_alloc: Arc<dyn DmaClient>,
        device: T,
    ) -> Result<Self> {
        let spawner = Arc::new(driver.clone());
        Ok(Self {
            instance_id: device.instance_id(),
            vmbus_listener: TaskControl::new(SimpleVmbusClientDeviceTask::new(
                device,
                spawner.clone(),
                dma_alloc,
            )),
            spawner,
        })
    }

    pub fn instance_id(&self) -> Guid {
        self.instance_id
    }

    pub fn detach(
        mut self,
        driver: impl SpawnDriver,
        recv_relay: mesh::Receiver<InterceptChannelRequest>,
    ) -> Result<()> {
        let (send_disconnected, recv_disconnected) = mesh::oneshot();
        self.vmbus_listener.insert(
            &self.spawner,
            format!("{}", self.instance_id),
            SimpleVmbusClientDeviceTaskState {
                offer: None,
                recv_relay,
                send_disconnected: Some(send_disconnected),
                vtl_pages: None,
            },
        );
        driver
            .spawn(
                format!("vmbus_relay_device {}", self.instance_id),
                async move {
                    self.vmbus_listener.start();
                    let _ = recv_disconnected.await;
                    assert!(!self.vmbus_listener.stop().await);
                    if self.vmbus_listener.state().unwrap().vtl_pages.is_some() {
                        // The VTL pages were not freed. This can occur if an
                        // error is hit that drops the vmbus parent tasks. Just
                        // pend here and let the outer error cause the VM to
                        // exit.
                        pending::<()>().await;
                    }
                },
            )
            .detach();
        Ok(())
    }
}

struct RelayDeviceTask<T>(T);

impl<T: SimpleVmbusClientDeviceAsync> AsyncRun<T::Runner> for RelayDeviceTask<T> {
    async fn run(
        &mut self,
        stop: &mut StopTask<'_>,
        runner: &mut T::Runner,
    ) -> Result<(), Cancelled> {
        self.0.run(stop, runner).await
    }
}

impl<T: SimpleVmbusClientDeviceAsync> InspectTaskMut<T::Runner> for RelayDeviceTask<T> {
    fn inspect_mut(&mut self, req: inspect::Request<'_>, runner: Option<&mut T::Runner>) {
        self.0.inspect(req, runner)
    }
}

#[derive(InspectMut)]
struct SimpleVmbusClientDeviceTaskState {
    offer: Option<OfferInfo>,
    #[inspect(skip)]
    recv_relay: mesh::Receiver<InterceptChannelRequest>,
    #[inspect(skip)]
    send_disconnected: Option<mesh::OneshotSender<()>>,
    #[inspect(hex, with = "|x| x.as_ref().map(|x| inspect::iter_by_index(x.pfns()))")]
    vtl_pages: Option<MemoryBlock>,
}

struct SimpleVmbusClientDeviceTask<T: SimpleVmbusClientDeviceAsync> {
    device: TaskControl<RelayDeviceTask<T>, T::Runner>,
    saved_state: Option<T::SavedState>,
    spawner: Arc<dyn SpawnDriver>,
    dma_alloc: Arc<dyn DmaClient>,
}

impl<T: SimpleVmbusClientDeviceAsync> AsyncRun<SimpleVmbusClientDeviceTaskState>
    for SimpleVmbusClientDeviceTask<T>
{
    async fn run(
        &mut self,
        stop: &mut StopTask<'_>,
        state: &mut SimpleVmbusClientDeviceTaskState,
    ) -> Result<(), Cancelled> {
        stop.until_stopped(self.process_messages(state)).await?;
        state
            .send_disconnected
            .take()
            .expect("task should not be restarted")
            .send(());
        Ok(())
    }
}

impl<T: SimpleVmbusClientDeviceAsync> InspectTaskMut<SimpleVmbusClientDeviceTaskState>
    for SimpleVmbusClientDeviceTask<T>
{
    fn inspect_mut(
        &mut self,
        req: inspect::Request<'_>,
        state: Option<&mut SimpleVmbusClientDeviceTaskState>,
    ) {
        req.respond()
            .merge(state)
            .field_mut("device", &mut self.device)
            .field("dma_alloc", &self.dma_alloc);
    }
}

impl<T: SimpleVmbusClientDeviceAsync> SimpleVmbusClientDeviceTask<T> {
    pub fn new(device: T, spawner: Arc<dyn SpawnDriver>, dma_alloc: Arc<dyn DmaClient>) -> Self {
        Self {
            device: TaskControl::new(RelayDeviceTask(device)),
            saved_state: None,
            spawner,
            dma_alloc,
        }
    }

    fn insert_runner(&mut self, state: &SimpleVmbusClientDeviceTaskState, runner: T::Runner) {
        let offer = state.offer.as_ref().unwrap().offer;
        self.device.insert(
            &self.spawner,
            format!("{}-{}", offer.interface_id, offer.instance_id),
            runner,
        );
    }

    /// Configures channel.
    async fn handle_offer(
        &mut self,
        offer: OfferInfo,
        state: &mut SimpleVmbusClientDeviceTaskState,
    ) -> Result<()> {
        tracing::info!(?offer, "matching channel offered");

        if offer.offer.is_dedicated != 1 {
            tracing::warn!(offer = ?offer.offer, "All offers should be dedicated with Win8+ host")
        }

        if matches!(
            self.device.task_mut().0.offer(&offer.offer),
            OfferResponse::Ignore
        ) {
            return Ok(());
        }

        let interrupt_event = pal_event::Event::new();
        let (memory, ring_gpadl_id) = self
            .reserve_memory(state, &offer.request_send, 4)
            .await
            .context("reserve memory")?;
        let guest_to_host_interrupt = offer.guest_to_host_interrupt.clone();
        state.offer = Some(offer);
        let offer = state.offer.as_ref().unwrap();
        self.open_channel(&offer.request_send, ring_gpadl_id, &interrupt_event)
            .await
            .context("open channel")?;
        let channel = self
            .create_vmbus_channel(&memory, &interrupt_event, guest_to_host_interrupt)
            .context("create vmbus queue")?;

        let save_restore = self.device.task_mut().0.supports_save_restore();
        let saved_state = self.saved_state.take();
        let device_runner = if let Some(save_restore) = save_restore
            && let Some(saved_state) = saved_state
        {
            save_restore
                .restore_open(saved_state, channel)
                .context("device restore_open callback")?
        } else {
            self.device
                .task_mut()
                .0
                .open(offer.offer.subchannel_index, channel)
                .context("device open callback")?
        };
        self.insert_runner(state, device_runner);
        self.device.start();
        Ok(())
    }

    /// Start channel after it has been stopped.
    async fn handle_start(&mut self, state: &mut SimpleVmbusClientDeviceTaskState) {
        if self.device.is_running() {
            return;
        }

        let offer = state.offer.take();
        if offer.is_none() {
            return;
        }

        // If there is a previous valid offer, open the channel again.
        if let Err(err) = self.handle_offer(offer.unwrap(), state).await {
            tracing::error!(
                err = err.as_ref() as &dyn std::error::Error,
                "Failed to reconnect vmbus channel"
            );
        }
    }

    async fn cleanup_device_resources(&mut self, state: &mut SimpleVmbusClientDeviceTaskState) {
        let Some(offer) = state.offer.as_mut() else {
            return;
        };

        if state.vtl_pages.is_some() {
            match offer
                .request_send
                .call(
                    ChannelRequest::TeardownGpadl,
                    GpadlId(state.vtl_pages.as_ref().unwrap().pfns()[1] as u32),
                )
                .await
            {
                Ok(()) => {
                    state.vtl_pages = None;
                }
                Err(err) => {
                    // If the ring buffer pages are still in use by the host, which
                    // has to be assumed, the memory pages cannot be used again as
                    // they have been marked as visible to VTL0.
                    tracing::error!(
                        error = &err as &dyn std::error::Error,
                        "Failed to teardown gpadl -- leaking memory."
                    );
                }
            }
        }
    }

    /// Stop channel
    async fn handle_stop(&mut self, state: &mut SimpleVmbusClientDeviceTaskState) {
        if !self.device.stop().await {
            return;
        }

        // Close the channel on every stop. Overlay devices cannot be saved /
        // restored because the physical pages used for the ring buffer, et al.
        // would need to be reserved at boot, otherwise the host may end up
        // scribbling on random memory as it continues updating a ring buffer it
        // assumes it has ownership of.
        //
        // TODO: We could support save restore, if we had a pool of memory that
        // supports that. This should be possible once the page_pool_alloc is
        // available everywhere.
        {
            let offer = state.offer.as_ref().expect("device opened");
            offer
                .request_send
                .call(ChannelRequest::Close, ())
                .await
                .ok();
        }
        // N.B. This will wait for a TeardownGpadl response which can be used
        // as a signal that the channel is closed and the ring buffers are no
        // longer in use.
        self.cleanup_device_resources(state).await;
        let runner = self.device.remove();
        let device = self.device.task_mut();
        if let Some(save_restore) = device.0.supports_save_restore() {
            self.saved_state = Some(save_restore.save_open(&runner));
        }
        drop(runner);
        let offer = state.offer.as_ref().expect("device opened");
        device.0.close(offer.offer.subchannel_index);
    }

    /// Allocates memory to be shared with the host and registers it with a
    /// GPADL ID.
    async fn reserve_memory(
        &mut self,
        state: &mut SimpleVmbusClientDeviceTaskState,
        request_send: &mesh::Sender<ChannelRequest>,
        page_count: usize,
    ) -> Result<(MemoryBlock, GpadlId)> {
        // Incoming and outgoing rings require a minimum of two pages apiece:
        // one for the control bytes and at least one for the ring.
        assert!(page_count >= 4);

        let mem = self
            .dma_alloc
            .allocate_dma_buffer(page_count * PAGE_SIZE)
            .context("allocating memory for vmbus rings")?;
        state.vtl_pages = Some(mem.clone());
        let buf: Vec<_> = [mem.len() as u64]
            .iter()
            .chain(mem.pfns())
            .copied()
            .collect();

        let gpadl_id = GpadlId(state.vtl_pages.as_ref().unwrap().pfns()[1] as u32);
        request_send
            .call_failable(
                ChannelRequest::Gpadl,
                GpadlRequest {
                    id: gpadl_id,
                    count: 1,
                    buf,
                },
            )
            .await
            .context("registering gpadl")?;
        Ok((mem, gpadl_id))
    }

    /// Open the channel offered by the host.
    async fn open_channel(
        &self,
        request_send: &mesh::Sender<ChannelRequest>,
        ring_gpadl_id: GpadlId,
        event: &pal_event::Event,
    ) -> Result<OpenOutput> {
        let open_request = OpenRequest {
            open_data: OpenData {
                target_vp: 0,
                ring_offset: 2,
                ring_gpadl_id,
                event_flag: !0,
                connection_id: !0,
                user_data: UserDefinedData::new_zeroed(),
            },
            incoming_event: Some(event.clone()),
            use_vtl2_connection_id: true,
        };

        request_send
            .call_failable(ChannelRequest::Open, open_request)
            .instrument(tracing::info_span!(
                "opening vmbus channel for intercepted device"
            ))
            .await
            .context("open vmbus channel")
    }

    /// Create a raw vmbus channel.
    fn create_vmbus_channel(
        &self,
        mem: &MemoryBlock,
        host_to_guest_event: &pal_event::Event,
        guest_to_host_interrupt: Interrupt,
    ) -> Result<RawAsyncChannel<MemoryBlockRingBuffer>> {
        let (out_ring_mem, in_ring_mem) = (
            mem.subblock(0, 2 * PAGE_SIZE),
            mem.subblock(2 * PAGE_SIZE, 2 * PAGE_SIZE),
        );
        let (in_ring, out_ring) = (
            IncomingRing::new(in_ring_mem.into()).unwrap(),
            OutgoingRing::new(out_ring_mem.into()).unwrap(),
        );

        let signal = MemoryBlockChannelSignal {
            event: Notify::from_event(host_to_guest_event.clone())
                .pollable(self.spawner.as_ref())
                .unwrap(),
            interrupt: guest_to_host_interrupt,
        };
        Ok(RawAsyncChannel {
            in_ring,
            out_ring,
            signal: Box::new(signal),
        })
    }

    /// Responds to the channel being revoked by the host.
    async fn handle_revoke(&mut self, state: &mut SimpleVmbusClientDeviceTaskState) {
        let Some(offer) = state.offer.as_ref() else {
            return;
        };
        tracing::info!("device revoked");
        if self.device.stop().await {
            drop(self.device.remove());
            self.device.task_mut().0.close(offer.offer.subchannel_index);
        }
        self.cleanup_device_resources(state).await;
        drop(state.offer.take());
    }

    fn handle_save(&mut self) -> SavedStateBlob {
        let saved_state = self.saved_state.take();
        if let Some(saved_state) = saved_state {
            let blob = SavedStateBlob::new(saved_state);
            self.handle_restore(&blob);
            blob
        } else {
            SavedStateBlob::new(NoSavedState)
        }
    }

    fn handle_restore(&mut self, saved_state_blob: &SavedStateBlob) {
        self.saved_state = match saved_state_blob.parse() {
            Ok(saved_state) => Some(saved_state),
            Err(err) => {
                tracing::error!(
                    err = &err as &dyn std::error::Error,
                    "Protobuf conversion error saving state"
                );
                None
            }
        };
    }

    /// Handle vmbus messages from the host and control messages from the
    /// device wrapper.
    pub async fn process_messages(&mut self, state: &mut SimpleVmbusClientDeviceTaskState) {
        loop {
            #[expect(clippy::large_enum_variant)]
            enum Event {
                Request(InterceptChannelRequest),
                Revoke,
            }
            let r = if let Some(offer) = &mut state.offer {
                (
                    (&mut state.recv_relay).map(Event::Request),
                    futures::stream::once(&mut offer.revoke_recv).map(|_| Event::Revoke),
                )
                    .merge()
                    .next()
                    .await
            } else {
                let mut recv_relay = pin!(&mut state.recv_relay);
                recv_relay.next().await.map(Event::Request)
            };
            let Some(r) = r else {
                break;
            };
            match r {
                Event::Revoke => {
                    self.handle_revoke(state).await;
                }
                Event::Request(InterceptChannelRequest::Offer(offer)) => {
                    // Any extraneous offer notifications (e.g. from a request offers
                    // query) are ignored.
                    if !self.device.is_running() {
                        if let Err(err) = self.handle_offer(offer, state).await {
                            tracing::error!(
                                error = err.as_ref() as &dyn std::error::Error,
                                "failed offer handling"
                            );
                        }
                    }
                }
                Event::Request(InterceptChannelRequest::Start) => {
                    self.handle_start(state).await;
                }
                Event::Request(InterceptChannelRequest::Stop(rpc)) => {
                    rpc.handle(async |()| self.handle_stop(state).await).await;
                }
                Event::Request(InterceptChannelRequest::Save(rpc)) => {
                    rpc.handle_sync(|()| self.handle_save());
                }
                Event::Request(InterceptChannelRequest::Restore(saved_state)) => {
                    self.handle_restore(&saved_state);
                }
            }
        }
    }
}

struct MemoryBlockChannelSignal {
    event: PolledNotify,
    interrupt: Interrupt,
}

impl SignalVmbusChannel for MemoryBlockChannelSignal {
    fn signal_remote(&self) {
        self.interrupt.deliver();
    }

    fn poll_for_signal(
        &self,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Result<(), ChannelClosed>> {
        self.event.poll_wait(cx).map(Ok)
    }
}
