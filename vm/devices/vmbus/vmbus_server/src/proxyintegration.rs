// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Implements support for using kernel-mode VMBus channel provider (VSPs) via
//! the vmbusproxy driver.

#![cfg(windows)]

use super::ChannelRequest;
use super::Guid;
use super::OfferInfo;
use super::OfferRequest;
use super::ProxyHandle;
use super::TaggedStream;
use super::VmbusServerControl;
use crate::HvsockRelayChannelHalf;
use crate::event::MaybeWrappedEvent;
use crate::event::WrappedEvent;
use anyhow::Context;
use futures::FutureExt;
use futures::StreamExt;
use futures::future::OptionFuture;
use futures::stream::SelectAll;
use guestmem::GuestMemory;
use mesh::Cancel;
use mesh::CancelContext;
use mesh::rpc::Rpc;
use mesh::rpc::RpcSend;
use pal_async::driver::SpawnDriver;
use pal_async::task::Spawn;
use pal_async::windows::TpPool;
use pal_event::Event;
use parking_lot::Mutex;
use std::collections::HashMap;
use std::collections::HashSet;
use std::future::Future;
use std::future::poll_fn;
use std::io;
use std::os::windows::prelude::*;
use std::pin::pin;
use std::sync::Arc;
use std::task::Poll;
use std::time::Duration;
use vmbus_channel::bus::ChannelServerRequest;
use vmbus_channel::bus::ChannelType;
use vmbus_channel::bus::OfferParams;
use vmbus_channel::bus::OpenRequest;
use vmbus_channel::bus::OpenResult;
use vmbus_channel::gpadl::GpadlId;
use vmbus_core::HvsockConnectRequest;
use vmbus_core::HvsockConnectResult;
use vmbus_core::protocol;
use vmbus_proxy::ProxyAction;
use vmbus_proxy::VmbusProxy;
use vmbus_proxy::vmbusioctl::VMBUS_SERVER_OPEN_CHANNEL_OUTPUT_PARAMETERS;
use vmcore::interrupt::Interrupt;
use windows::Win32::Foundation::ERROR_CANCELLED;
use windows::core::HRESULT;
use zerocopy::IntoBytes;

/// Provides access to a vmbus server, and its optional hvsocket relay.
pub struct ProxyServerInfo {
    control: Arc<VmbusServerControl>,
    hvsock_relay: Option<HvsockRelayChannelHalf>,
}

impl ProxyServerInfo {
    /// Creates a new `ProxyServerInfo` instance.
    pub fn new(
        control: Arc<VmbusServerControl>,
        hvsock_relay: Option<HvsockRelayChannelHalf>,
    ) -> Self {
        Self {
            control,
            hvsock_relay,
        }
    }
}

pub struct ProxyIntegration {
    cancel: Cancel,
    handle: OwnedHandle,
    flush_send: mesh::Sender<Rpc<(), ()>>,
}

impl ProxyIntegration {
    /// Cancels the vmbus proxy.
    pub fn cancel(&mut self) {
        self.cancel.cancel();
    }

    /// Wait for all currently ready pending actions to complete. E.g., wait for
    /// all channels that have been offered to the kernel driver to have been
    /// processed.
    pub async fn flush_actions(&mut self) {
        self.flush_send.call(|v| v, ()).await.ok();
    }

    /// Returns the handle to the vmbus proxy driver.
    pub fn handle(&self) -> BorrowedHandle<'_> {
        self.handle.as_handle()
    }

    /// Starts the vmbus proxy.
    pub async fn start(
        driver: &(impl SpawnDriver + Clone),
        handle: ProxyHandle,
        server: ProxyServerInfo,
        vtl2_server: Option<ProxyServerInfo>,
        mem: Option<&GuestMemory>,
    ) -> io::Result<Self> {
        let (cancel_ctx, cancel) = CancelContext::new().with_cancel();
        let mut proxy = VmbusProxy::new(driver, handle, cancel_ctx)?;
        let handle = proxy.handle().try_clone_to_owned()?;
        if let Some(mem) = mem {
            proxy.set_memory(mem).await?;
        }

        let (flush_send, flush_recv) = mesh::channel();
        driver
            .spawn(
                "vmbus_proxy",
                proxy_thread(driver.clone(), proxy, server, vtl2_server, flush_recv),
            )
            .detach();

        Ok(Self {
            cancel,
            handle,
            flush_send,
        })
    }
}

struct Channel {
    server_request_send: Option<mesh::Sender<ChannelServerRequest>>,
    incoming_event: Event,
    worker_result: Option<mesh::OneshotReceiver<()>>,
    wrapped_event: Option<WrappedEvent>,
}

struct ProxyTask {
    channels: Arc<Mutex<HashMap<u64, Channel>>>,
    gpadls: Arc<Mutex<HashMap<u64, HashSet<GpadlId>>>>,
    proxy: Arc<VmbusProxy>,
    server: Arc<VmbusServerControl>,
    vtl2_server: Option<Arc<VmbusServerControl>>,
    hvsock_response_send: Option<mesh::Sender<HvsockConnectResult>>,
    vtl2_hvsock_response_send: Option<mesh::Sender<HvsockConnectResult>>,
}

impl ProxyTask {
    fn new(
        server: Arc<VmbusServerControl>,
        vtl2_server: Option<Arc<VmbusServerControl>>,
        hvsock_response_send: Option<mesh::Sender<HvsockConnectResult>>,
        vtl2_hvsock_response_send: Option<mesh::Sender<HvsockConnectResult>>,
        proxy: Arc<VmbusProxy>,
    ) -> Self {
        Self {
            channels: Arc::new(Mutex::new(HashMap::new())),
            gpadls: Arc::new(Mutex::new(HashMap::new())),
            proxy,
            server,
            hvsock_response_send,
            vtl2_hvsock_response_send,
            vtl2_server,
        }
    }

    async fn handle_open(
        &self,
        proxy_id: u64,
        open_request: &OpenRequest,
    ) -> anyhow::Result<Event> {
        let maybe_wrapped =
            MaybeWrappedEvent::new(&TpPool::system(), open_request.interrupt.clone())?;

        self.proxy
            .open(
                proxy_id,
                &VMBUS_SERVER_OPEN_CHANNEL_OUTPUT_PARAMETERS {
                    RingBufferGpadlHandle: open_request.open_data.ring_gpadl_id.0,
                    DownstreamRingBufferPageOffset: open_request.open_data.ring_offset,
                    NodeNumber: 0, // BUGBUG: NUMA
                },
                maybe_wrapped.event(),
            )
            .await
            .context("failed to open channel")?;

        // Start the worker thread for the channel.
        let proxy = Arc::clone(&self.proxy);
        let (send, recv) = mesh::oneshot();
        std::thread::Builder::new()
            .name(format!("vmbus proxy worker {:?}", proxy_id))
            .spawn(move || {
                if let Err(err) = proxy.run_channel(proxy_id) {
                    tracing::error!(err = &err as &dyn std::error::Error, "channel worker error");
                }
                send.send(());
            })
            .unwrap();

        let mut channels = self.channels.lock();
        let channel = channels.get_mut(&proxy_id).unwrap();
        channel.worker_result = Some(recv);
        channel.wrapped_event = maybe_wrapped.into_wrapped();
        Ok(channel.incoming_event.clone())
    }

    async fn handle_close(&self, proxy_id: u64) {
        self.proxy
            .close(proxy_id)
            .await
            .expect("channel close failed");

        // Wait for the worker task.
        let recv = self
            .channels
            .lock()
            .get_mut(&proxy_id)
            .unwrap()
            .worker_result
            .take()
            .expect("channel should be open");

        let _ = recv.await;
    }

    async fn handle_gpadl_create(
        &self,
        proxy_id: u64,
        gpadl_id: GpadlId,
        count: u16,
        buf: &[u64],
    ) -> anyhow::Result<()> {
        self.proxy
            .create_gpadl(proxy_id, gpadl_id.0, count.into(), buf.as_bytes())
            .await
            .context("failed to create gpadl")?;

        self.gpadls
            .lock()
            .entry(proxy_id)
            .or_default()
            .insert(gpadl_id);
        Ok(())
    }

    async fn handle_gpadl_teardown(&self, proxy_id: u64, gpadl_id: GpadlId) {
        assert!(
            self.gpadls
                .lock()
                .get_mut(&proxy_id)
                .unwrap()
                .remove(&gpadl_id),
            "gpadl is registered"
        );

        self.proxy
            .delete_gpadl(proxy_id, gpadl_id.0)
            .await
            .expect("delete gpadl failed");
    }

    async fn handle_offer(
        &self,
        id: u64,
        offer: vmbus_proxy::vmbusioctl::VMBUS_CHANNEL_OFFER,
        incoming_event: Event,
    ) -> Option<mesh::Receiver<ChannelRequest>> {
        let server = match offer.TargetVtl {
            0 => self.server.as_ref(),
            2 => {
                if let Some(server) = self.vtl2_server.as_ref() {
                    server.as_ref()
                } else {
                    tracing::error!(?offer, "VTL2 offer without VTL2 server");
                    return None;
                }
            }
            _ => {
                tracing::error!(?offer, "unsupported offer VTL");
                return None;
            }
        };

        let channel_type = if offer.ChannelFlags.tlnpi_provider() {
            let params = offer.UserDefined.as_hvsock_params();
            ChannelType::HvSocket {
                is_connect: params.is_for_guest_accept != 0,
                is_for_container: params.is_for_guest_container != 0,
                silo_id: if params.version.get() == protocol::HvsockParametersVersion::PRE_RS5 {
                    Guid::ZERO
                } else {
                    params.silo_id.get()
                },
            }
        } else if offer.ChannelFlags.enumerate_device_interface() {
            let params = offer.UserDefined.as_pipe_params();
            let message_mode = match params.pipe_type {
                protocol::PipeType::BYTE => false,
                protocol::PipeType::MESSAGE => true,
                _ => {
                    tracing::error!(?offer, "unsupported offer pipe mode");
                    return None;
                }
            };
            ChannelType::Pipe { message_mode }
        } else {
            ChannelType::Device {
                pipe_packets: offer.ChannelFlags.named_pipe_mode(),
            }
        };

        let interface_id: Guid = offer.InterfaceType.into();
        let instance_id: Guid = offer.InterfaceInstance.into();

        let offer = OfferParams {
            interface_name: "proxy".to_owned(),
            instance_id,
            interface_id,
            mmio_megabytes: offer.MmioMegabytes,
            mmio_megabytes_optional: offer.MmioMegabytesOptional,
            subchannel_index: offer.SubChannelIndex,
            channel_type,
            mnf_interrupt_latency: offer
                .ChannelFlags
                .request_monitored_notification()
                .then(|| Duration::from_nanos(offer.InterruptLatencyIn100nsUnits * 100)),
            offer_order: id.try_into().ok(),
            allow_confidential_external_memory: false,
        };
        let (request_send, request_recv) = mesh::channel();
        let (server_request_send, server_request_recv) = mesh::channel();

        let recv = server.send.call_failable(
            OfferRequest::Offer,
            OfferInfo {
                params: offer.into(),
                request_send,
                server_request_recv,
            },
        );

        let (request_recv, server_request_send) = match recv.await {
            Ok(()) => (Some(request_recv), Some(server_request_send)),
            Err(err) => {
                // Currently there is no way to propagate this failure.
                // FUTURE: consider sending a message back to the control worker to fail the VM operation.
                tracing::error!(
                    error = &err as &dyn std::error::Error,
                    interface_id = %interface_id,
                    instance_id = %instance_id,
                    "failed to offer proxy channel"
                );
                (None, None)
            }
        };

        self.channels.lock().insert(
            id,
            Channel {
                server_request_send,
                incoming_event,
                worker_result: None,
                wrapped_event: None,
            },
        );
        request_recv
    }

    async fn handle_revoke(&self, id: u64) {
        let response_send = self
            .channels
            .lock()
            .get_mut(&id)
            .unwrap()
            .server_request_send
            .take();

        if let Some(response_send) = response_send {
            drop(response_send);
        } else {
            self.proxy
                .release(id)
                .await
                .expect("vmbus proxy state failure");

            self.channels.lock().remove(&id);
        }
    }

    fn handle_tl_connect_result(&self, result: HvsockConnectResult, vtl: u8) {
        let send = match vtl {
            0 => self.hvsock_response_send.as_ref(),
            2 => self.vtl2_hvsock_response_send.as_ref(),
            _ => panic!("hvsocket response with unsupported VTL {vtl}"),
        };

        send.expect("got hvsocket response without having sent a request")
            .send(result);
    }

    async fn run_proxy_actions(
        &self,
        send: mesh::Sender<TaggedStream<u64, mesh::Receiver<ChannelRequest>>>,
        flush_recv: mesh::Receiver<Rpc<(), ()>>,
    ) {
        let mut pending_flush = None::<Rpc<(), ()>>;
        let mut flush_recv = Some(flush_recv);
        loop {
            let mut action_fut = pin!(self.proxy.next_action());
            let action = poll_fn(|cx| {
                loop {
                    if let r @ Poll::Ready(_) = action_fut.as_mut().poll(cx) {
                        break r;
                    }
                    if let Some(pending_flush) = pending_flush.take() {
                        // The next action future was polled after this flush
                        // was received, and it has returned pending. This
                        // definitively means there are currently no more
                        // actions pending, because the action future will check
                        // if the pending IOCTL completed (via its IO status
                        // block) when the future is polled.
                        pending_flush.complete(());
                    }
                    if let Some(recv) = &mut flush_recv {
                        match recv.poll_next_unpin(cx) {
                            Poll::Ready(Some(rpc)) => {
                                // We received a flush request from the client. Save
                                // this request and loop around to poll the action
                                // again.
                                pending_flush = Some(rpc);
                            }
                            Poll::Ready(None) => {
                                // The flush channel was closed, so we can stop
                                // waiting for flushes.
                                flush_recv = None;
                            }
                            Poll::Pending => {}
                        }
                    } else {
                        break Poll::Pending;
                    }
                }
            })
            .await;

            let Ok(action) = action else { break };

            tracing::debug!(action = ?action, "action");
            match action {
                ProxyAction::Offer {
                    id,
                    offer,
                    incoming_event,
                    outgoing_event: _,
                } => {
                    if let Some(recv) = self.handle_offer(id, offer, incoming_event).await {
                        send.send(TaggedStream::new(id, recv));
                    }
                }
                ProxyAction::Revoke { id } => {
                    self.handle_revoke(id).await;
                }
                ProxyAction::InterruptPolicy {} => {}
                ProxyAction::TlConnectResult { result, vtl } => {
                    self.handle_tl_connect_result(result, vtl);
                }
            }
        }

        tracing::debug!("proxy offers finished");
    }

    async fn handle_request(&self, proxy_id: u64, request: Option<ChannelRequest>) {
        match request {
            Some(request) => {
                match request {
                    ChannelRequest::Open(rpc) => {
                        rpc.handle(async |open_request| {
                            let result = self.handle_open(proxy_id, &open_request).await;
                            match result {
                                Ok(event) => Some(OpenResult {
                                    guest_to_host_interrupt: Interrupt::from_event(event),
                                }),
                                Err(err) => {
                                    tracing::error!(
                                        error = err.as_ref() as &dyn std::error::Error,
                                        "failed to open proxy channel"
                                    );
                                    None
                                }
                            }
                        })
                        .await
                    }
                    ChannelRequest::Close(rpc) => {
                        rpc.handle(async |()| {
                            self.handle_close(proxy_id).await;
                        })
                        .await
                    }
                    ChannelRequest::Gpadl(rpc) => {
                        rpc.handle(async |gpadl| {
                            let result = self
                                .handle_gpadl_create(proxy_id, gpadl.id, gpadl.count, &gpadl.buf)
                                .await;
                            result.is_ok()
                        })
                        .await
                    }
                    ChannelRequest::TeardownGpadl(rpc) => {
                        rpc.handle(async |id| {
                            self.handle_gpadl_teardown(proxy_id, id).await;
                        })
                        .await
                    }
                    // Modifying the target VP is handle by the server, there is nothing the proxy
                    // driver needs to do.
                    ChannelRequest::Modify(rpc) => rpc.complete(0),
                }
            }
            None => {
                // Due to a bug in some versions of vmbusproxy, this causes bugchecks if there are
                // any GPADLs still registered. This seems to happen during teardown.
                let _ = self.proxy.close(proxy_id).await;
                let gpadls = self.gpadls.lock().remove(&proxy_id);
                if let Some(gpadls) = gpadls {
                    if !gpadls.is_empty() {
                        tracing::info!(proxy_id, "closed while some gpadls are still registered");
                        for gpadl_id in gpadls {
                            if let Err(e) = self.proxy.delete_gpadl(proxy_id, gpadl_id.0).await {
                                if e.code() == HRESULT::from(ERROR_CANCELLED) {
                                    // No further IOs will succeed if one was cancelled. This can
                                    // happen here if we're in the process of shutting down.
                                    tracing::debug!("gpadl delete cancelled");
                                    break;
                                }

                                tracing::error!(error = ?e, "failed to delete gpadl");
                            }
                        }
                    }
                }

                // Only release the channel with the driver if the driver has already revoked it.
                if self
                    .channels
                    .lock()
                    .get(&proxy_id)
                    .unwrap()
                    .server_request_send
                    .is_none()
                {
                    self.proxy
                        .release(proxy_id)
                        .await
                        .expect("vmbus proxy state failure");

                    self.channels.lock().remove(&proxy_id);
                }
            }
        }
    }

    /// Returns true if the request was handled successfully, and false if a receive error happened
    /// so the hvsocket relay should not be used again.
    fn handle_hvsock_request(
        &self,
        spawner: &impl Spawn,
        request: Result<HvsockConnectRequest, mesh::RecvError>,
        vtl: u8,
    ) -> bool {
        let request = match request {
            Ok(request) => request,
            Err(e) => {
                // Closed can happen normally during shutdown, so does not need to be logged.
                if !matches!(e, mesh::RecvError::Closed) {
                    tracelimit::error_ratelimited!(
                        error = ?&e as &dyn std::error::Error,
                        "hvsock request receive failed"
                    );
                }

                return false;
            }
        };

        let proxy = self.proxy.clone();
        spawner
            .spawn("vmbus-proxy-hvsock-req", async move {
                proxy.tl_connect_request(&request, vtl).await
            })
            .detach();

        true
    }

    async fn run_server_requests(
        self: &Arc<Self>,
        spawner: impl Spawn,
        mut recv: mesh::Receiver<TaggedStream<u64, mesh::Receiver<ChannelRequest>>>,
        mut hvsock_request_recv: Option<mesh::Receiver<HvsockConnectRequest>>,
        mut vtl2_hvsock_request_recv: Option<mesh::Receiver<HvsockConnectRequest>>,
    ) {
        let mut channel_requests = SelectAll::new();

        'outer: loop {
            let (proxy_id, request) = loop {
                let mut hvsock_requests = OptionFuture::from(
                    hvsock_request_recv
                        .as_mut()
                        .map(|recv| Box::pin(recv.recv()).fuse()),
                );

                let mut vtl2_hvsock_requests = OptionFuture::from(
                    vtl2_hvsock_request_recv
                        .as_mut()
                        .map(|recv| Box::pin(recv.recv()).fuse()),
                );

                futures::select! { // merge semantics
                    r = recv.select_next_some() => {
                        channel_requests.push(r);
                    }
                    r = channel_requests.select_next_some() => break r,
                    r = hvsock_requests => {
                        if !self.handle_hvsock_request(&spawner, r.unwrap(), 0) {
                            hvsock_request_recv = None;
                        }
                    }
                    r = vtl2_hvsock_requests => {
                        if !self.handle_hvsock_request(&spawner, r.unwrap(), 2) {
                            vtl2_hvsock_request_recv = None;
                        }
                    }
                    complete => break 'outer,
                }
            };

            let this = self.clone();
            spawner
                .spawn("vmbus-proxy-req", async move {
                    this.handle_request(proxy_id, request).await
                })
                .detach();
        }

        tracing::debug!("proxy channel requests finished");
    }
}

async fn proxy_thread(
    spawner: impl Spawn,
    proxy: VmbusProxy,
    server: ProxyServerInfo,
    vtl2_server: Option<ProxyServerInfo>,
    flush_recv: mesh::Receiver<Rpc<(), ()>>,
) {
    // Separate the hvsocket relay channels.
    let (hvsock_request_recv, hvsock_response_send) = server
        .hvsock_relay
        .map(|relay| (relay.request_receive, relay.response_send))
        .unzip();

    // Separate the hvsocket relay channels and the server for VTL2.
    let (vtl2_control, vtl2_hvsock_request_recv, vtl2_hvsock_response_send) =
        if let Some(server) = vtl2_server {
            let (vtl2_hvsock_request_recv, vtl2_hvsock_response_send) = server
                .hvsock_relay
                .map(|relay| (relay.request_receive, relay.response_send))
                .unzip();
            (
                Some(server.control),
                vtl2_hvsock_request_recv,
                vtl2_hvsock_response_send,
            )
        } else {
            (None, None, None)
        };

    let (send, recv) = mesh::channel();
    let proxy = Arc::new(proxy);
    let task = Arc::new(ProxyTask::new(
        server.control,
        vtl2_control,
        hvsock_response_send,
        vtl2_hvsock_response_send,
        Arc::clone(&proxy),
    ));
    let offers = task.run_proxy_actions(send, flush_recv);
    let requests =
        task.run_server_requests(spawner, recv, hvsock_request_recv, vtl2_hvsock_request_recv);

    futures::future::join(offers, requests).await;
    tracing::debug!("proxy thread finished");
    // BUGBUG: cancel all IO if something goes wrong?
}
