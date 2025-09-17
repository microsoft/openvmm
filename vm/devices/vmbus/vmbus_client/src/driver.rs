// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Support for authoring vmbus device drivers on top of the vmbus client
//! driver.

use crate::ChannelRequest;
use crate::OfferInfo;
use crate::OpenRequest;
use anyhow::Context as _;
use futures::FutureExt;
use futures_concurrency::future::Race;
use inspect::InspectMut;
use mesh::rpc::RpcSend;
use pal_async::driver::SpawnDriver;
use std::mem::ManuallyDrop;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::AtomicU8;
use std::sync::atomic::Ordering::Relaxed;
use std::task::Poll;
use user_driver::DmaClient;
use user_driver::memory::MemoryBlock;
use vmbus_channel::ChannelClosed;
use vmbus_channel::RawAsyncChannel;
use vmbus_channel::SignalVmbusChannel;
use vmbus_channel::bus::GpadlRequest;
use vmbus_channel::bus::OpenData;
use vmbus_core::protocol::GpadlId;
use vmbus_core::protocol::UserDefinedData;
use vmbus_ring::IncomingRing;
use vmbus_ring::OutgoingRing;
use vmbus_ring::SingleMappedRingMem;
use vmcore::interrupt::Interrupt;
use vmcore::notify::Notify;
use vmcore::notify::PolledNotify;

/// Input parameters when opening a vmbus channel.
pub struct OpenParams {
    /// The number of pages to use for the ring buffer.
    pub ring_pages: u16,
    /// The offset in pages where the downstream ring starts.
    pub ring_offset_in_pages: u16,
}

/// The memory type used for the vmbus channel ring buffer.
pub type MemoryBlockRingMem = SingleMappedRingMem<MemoryBlockView>;

/// Opens a vmbus channel, returning the ring buffer parameters.
pub async fn open_channel(
    driver: impl SpawnDriver + Clone + 'static,
    offer_info: OfferInfo,
    params: OpenParams,
    dma_client: &dyn DmaClient,
) -> anyhow::Result<RawAsyncChannel<MemoryBlockRingMem>> {
    let gpadl =
        dma_client.allocate_dma_buffer(vmbus_ring::PAGE_SIZE * params.ring_pages as usize)?;

    let (resp_send, resp_recv) = mesh::oneshot();
    // Detach the task so that it doesn't get dropped (and thereby leak the allocation).
    driver
        .clone()
        .spawn("vmbus_client", async move {
            ChannelWorker::run(driver, offer_info, params, gpadl, resp_send).await
        })
        .detach();

    resp_recv.await.context("no response opening channel")?
}

#[derive(InspectMut)]
struct ChannelWorker<D> {
    #[inspect(skip)]
    driver: D,
    offer: vmbus_core::protocol::OfferChannel,
    #[inspect(skip)]
    request_send: mesh::Sender<ChannelRequest>,
    #[inspect(skip)]
    gpadl: ManuallyDrop<Arc<MemoryBlock>>,
    #[inspect(debug)]
    ring_gpadl_id: GpadlId,
    is_gpadl_created: bool,
    is_open: bool,
}

impl<D: SpawnDriver> ChannelWorker<D> {
    async fn open(
        &mut self,
        input: OpenParams,
        close_send: mesh::OneshotSender<()>,
        guest_to_host: Interrupt,
        host_to_guest: pal_event::Event,
        revoked: Arc<AtomicBool>,
    ) -> anyhow::Result<RawAsyncChannel<MemoryBlockRingMem>> {
        let gpadl_buf = [self.gpadl.len() as u64]
            .into_iter()
            .chain(self.gpadl.pfns().iter().copied())
            .collect::<Vec<_>>();

        self.request_send
            .call_failable(
                ChannelRequest::Gpadl,
                GpadlRequest {
                    id: self.ring_gpadl_id,
                    count: 1,
                    buf: gpadl_buf,
                },
            )
            .await?;

        self.is_gpadl_created = true;

        self.request_send
            .call_failable(
                ChannelRequest::Open,
                OpenRequest {
                    open_data: OpenData {
                        target_vp: 0, // TODO: improve
                        ring_offset: input.ring_offset_in_pages.into(),
                        ring_gpadl_id: self.ring_gpadl_id,
                        event_flag: !0,
                        connection_id: !0,
                        user_data: UserDefinedData::default(),
                    },
                    incoming_event: Some(host_to_guest.clone()),
                    use_vtl2_connection_id: true,
                },
            )
            .await?;

        self.is_open = true;

        let in_ring = MemoryBlockView {
            mem: Arc::clone(&self.gpadl),
            offset: input.ring_offset_in_pages as usize * vmbus_ring::PAGE_SIZE,
            len: (input.ring_pages - input.ring_offset_in_pages) as usize * vmbus_ring::PAGE_SIZE,
        };

        let out_ring = MemoryBlockView {
            mem: Arc::clone(&self.gpadl),
            offset: 0,
            len: input.ring_offset_in_pages as usize * vmbus_ring::PAGE_SIZE,
        };

        let signal = ClientSignaller {
            guest_to_host,
            host_to_guest: Notify::from_event(host_to_guest).pollable(&self.driver)?,
            revoked: revoked.clone(),
            _close: close_send,
        };

        Ok(RawAsyncChannel {
            in_ring: IncomingRing::new(SingleMappedRingMem(in_ring))?,
            out_ring: OutgoingRing::new(SingleMappedRingMem(out_ring))?,
            signal: Box::new(signal),
        })
    }

    async fn shutdown(self) {
        if self.is_open {
            self.request_send.call(ChannelRequest::Close, ()).await.ok();
        }

        if self.is_gpadl_created {
            self.request_send
                .call(ChannelRequest::TeardownGpadl, self.ring_gpadl_id)
                .await
                .ok();
        }

        // Now it is safe to deallocate the gpadl memory.
        ManuallyDrop::into_inner(self.gpadl);
    }

    async fn run(
        driver: D,
        offer_info: OfferInfo,
        input: OpenParams,
        gpadl_mem: MemoryBlock,
        resp: mesh::OneshotSender<anyhow::Result<RawAsyncChannel<MemoryBlockRingMem>>>,
    ) {
        let instance_id = offer_info.offer.instance_id;
        let ring_gpadl_id = GpadlId((1 << 31) | offer_info.offer.channel_id.0);

        let mut worker = ChannelWorker {
            driver,
            offer: offer_info.offer,
            request_send: offer_info.request_send,
            gpadl: ManuallyDrop::new(Arc::new(gpadl_mem)),
            ring_gpadl_id,
            is_gpadl_created: false,
            is_open: false,
        };

        let revoked = Arc::new(AtomicBool::new(false));
        let (close_send, close_recv) = mesh::oneshot();
        let host_to_guest = pal_event::Event::new();
        match worker
            .open(
                input,
                close_send,
                offer_info.guest_to_host_interrupt,
                host_to_guest.clone(),
                revoked.clone(),
            )
            .await
        {
            Ok(channel) => {
                resp.send(Ok(channel));
            }
            Err(e) => {
                resp.send(Err(e));
                worker.shutdown().await;
                return;
            }
        }

        enum Event<T, U> {
            Close(T),
            Revoke(U),
        }

        let revoke = offer_info.revoke_recv.map(Event::Revoke);
        let close = close_recv.map(Event::Close);
        let event = (revoke, close).race().await;
        match event {
            Event::Close(_) => {
                tracing::debug!(%instance_id, "channel close requested");
            }
            Event::Revoke(_) => {
                tracing::debug!(%instance_id, "channel revoked");
                revoked.store(true, Relaxed);
                host_to_guest.signal();
                worker.is_open = false;
            }
        }

        worker.shutdown().await;
    }
}

/// A view into a [`MemoryBlock`].
pub struct MemoryBlockView {
    mem: Arc<MemoryBlock>,
    offset: usize,
    len: usize,
}

impl AsRef<[AtomicU8]> for MemoryBlockView {
    fn as_ref(&self) -> &[AtomicU8] {
        &self.mem.as_slice()[self.offset..][..self.len]
    }
}

struct ClientSignaller {
    guest_to_host: Interrupt,
    host_to_guest: PolledNotify,
    revoked: Arc<AtomicBool>,
    // This closes the channel on drop.
    _close: mesh::OneshotSender<()>,
}

impl SignalVmbusChannel for ClientSignaller {
    fn signal_remote(&self) {
        self.guest_to_host.deliver();
    }

    fn poll_for_signal(&self, cx: &mut std::task::Context<'_>) -> Poll<Result<(), ChannelClosed>> {
        if self.revoked.load(Relaxed) {
            return Poll::Ready(Err(ChannelClosed));
        }
        self.host_to_guest.poll_wait(cx).map(Ok)
    }
}
