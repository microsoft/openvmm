// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Vmbus channel offer support.

use crate::ChannelClosed;
use crate::RawAsyncChannel;
use crate::SignalVmbusChannel;
use crate::bus::ChannelRequest;
use crate::bus::ChannelServerRequest;
use crate::bus::OfferInput;
use crate::bus::OfferParams;
use crate::bus::OfferResources;
use crate::bus::OpenRequest;
use crate::bus::ParentBus;
use crate::gpadl::GpadlMap;
use crate::gpadl::GpadlMapView;
use crate::gpadl_ring;
use crate::gpadl_ring::GpadlRingMem;
use crate::gpadl_ring::make_rings;
use futures::StreamExt;
use mesh::rpc::Rpc;
use pal_async::driver::Driver;
use pal_async::task::Spawn;
use pal_async::task::Task;
use pal_event::Event;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;
use vmbus_ring::gparange::MultiPagedRangeBuf;
use vmcore::interrupt::Interrupt;
use vmcore::notify::Notify;
use vmcore::notify::PolledNotify;

/// A channel accept error.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// channel revoked
    #[error("the channel has been revoked")]
    Revoked,
    /// GPADL ring buffer error
    #[error(transparent)]
    GpadlRing(#[from] gpadl_ring::Error),
    /// Driver error
    #[error("io driver error")]
    Driver(#[source] std::io::Error),
}

/// A channel offer.
pub struct Offer {
    task: Task<()>,
    open_recv: mesh::Receiver<OpenMessage>,
    gpadl_map: GpadlMapView,
    event: Notify,
    offer_resources: OfferResources,
    _server_request_send: mesh::Sender<ChannelServerRequest>,
}

impl Offer {
    /// Offers a new channel.
    pub async fn new(
        driver: impl Spawn,
        bus: &dyn ParentBus,
        offer_params: OfferParams,
    ) -> anyhow::Result<Self> {
        let instance_id = offer_params.instance_id;
        let event = Event::new();
        let (request_send, request_recv) = mesh::channel();
        let (server_request_send, server_request_recv) = mesh::channel();
        let result = bus
            .add_child(OfferInput {
                params: offer_params,
                event: Interrupt::from_event(event.clone()),
                request_send,
                server_request_recv,
            })
            .await?;

        let gpadls = GpadlMap::new();
        let gpadl_map = gpadls.clone().view();
        let (open_send, open_recv) = mesh::channel();
        let task = driver.spawn(format!("vmbus-offer-{}", instance_id), {
            let event = event.clone();
            async move { Self::task(event, gpadls, request_recv, open_send).await }
        });

        let offer = Self {
            offer_resources: result,
            task,
            open_recv,
            gpadl_map,
            event: Notify::from_event(event),
            _server_request_send: server_request_send,
        };
        Ok(offer)
    }

    async fn task(
        event: Event,
        gpadls: Arc<GpadlMap>,
        mut request_recv: mesh::Receiver<ChannelRequest>,
        send: mesh::Sender<OpenMessage>,
    ) {
        let mut open_done = None;
        while let Ok(request) = request_recv.recv().await {
            match request {
                ChannelRequest::Open(rpc) => {
                    let (open_request, response_send) = rpc.split();
                    let done = Arc::new(AtomicBool::new(false));
                    send.send(OpenMessage {
                        open_request,
                        done: done.clone(),
                        response: OpenResponse(Some(response_send)),
                    });
                    open_done = Some(done);
                }
                ChannelRequest::Close(rpc) => {
                    let _response_send = rpc; // TODO: figure out if we should really just drop this here.
                    open_done
                        .take()
                        .expect("channel must be open")
                        .store(true, Ordering::Relaxed);
                    event.signal();
                }
                ChannelRequest::Gpadl(rpc) => rpc.handle_sync(|gpadl| {
                    match MultiPagedRangeBuf::from_range_buffer(gpadl.count.into(), gpadl.buf) {
                        Ok(buf) => {
                            gpadls.add(gpadl.id, buf);
                            true
                        }
                        Err(err) => {
                            tracelimit::error_ratelimited!(
                                error = &err as &dyn std::error::Error,
                                "failed to parse gpadl"
                            );
                            false
                        }
                    }
                }),
                ChannelRequest::TeardownGpadl(rpc) => {
                    let (id, response_send) = rpc.split();
                    if let Some(f) = gpadls.remove(
                        id,
                        Box::new(move || {
                            response_send.complete(());
                        }),
                    ) {
                        f();
                    }
                }
                ChannelRequest::Modify(rpc) => rpc.handle_sync(|_| 0),
            }
        }
    }

    /// Accepts a channel open request from the guest.
    pub async fn wait_for_open(
        &mut self,
        driver: &(impl Driver + ?Sized),
    ) -> Result<OpeningChannel, Error> {
        let message = self.open_recv.next().await.ok_or(Error::Revoked)?;

        let (in_ring, out_ring) = make_rings(
            self.offer_resources.ring_memory(&message.open_request),
            &self.gpadl_map,
            &message.open_request.open_data,
        )?;
        let event = OfferChannelSignal {
            event: self.event.clone().pollable(driver).map_err(Error::Driver)?,
            interrupt: message.open_request.interrupt.clone(),
            done: message.done,
        };
        let channel = RawAsyncChannel {
            in_ring,
            out_ring,
            signal: Box::new(event),
        };
        let resources = OpenChannelResources {
            channel,
            gpadl_map: self.gpadl_map.clone(),
        };
        Ok(OpeningChannel {
            resources,
            response: message.response,
        })
    }

    /// Revokes the channel.
    pub async fn revoke(self) {
        drop(self.open_recv);
        self.task.await;
    }
}

/// An in-progress channel opening, returned by [`Offer::wait_for_open`].
pub struct OpeningChannel {
    resources: OpenChannelResources,
    response: OpenResponse,
}

impl OpeningChannel {
    /// Accepts the channel open request.
    pub fn accept(self) -> OpenChannelResources {
        self.response.respond(true);
        self.resources
    }

    /// Rejects the channel open request.
    pub fn reject(self) {
        self.response.respond(false);
    }
}

struct OfferChannelSignal {
    event: PolledNotify,
    interrupt: Interrupt,
    done: Arc<AtomicBool>,
}

impl SignalVmbusChannel for OfferChannelSignal {
    fn signal_remote(&self) {
        self.interrupt.deliver();
    }

    fn poll_for_signal(
        &self,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Result<(), ChannelClosed>> {
        if self.done.load(Ordering::Relaxed) {
            return Err(ChannelClosed).into();
        }
        self.event.poll_wait(cx).map(Ok)
    }
}

struct OpenMessage {
    open_request: OpenRequest,
    done: Arc<AtomicBool>,
    response: OpenResponse,
}

struct OpenResponse(Option<Rpc<(), bool>>);

impl OpenResponse {
    fn respond(mut self, open: bool) {
        self.0.take().unwrap().complete(open)
    }
}

impl Drop for OpenResponse {
    fn drop(&mut self) {
        if let Some(rpc) = self.0.take() {
            rpc.complete(false);
        }
    }
}

/// Channel resources for an open channel.
pub struct OpenChannelResources {
    /// The channel ring buffer and interrupt state.
    pub channel: RawAsyncChannel<GpadlRingMem>,
    /// The channel's GPADL map.
    pub gpadl_map: GpadlMapView,
}
