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
use crate::SavedStateRequest;
use crate::channels::SavedState;
use crate::event::MaybeWrappedEvent;
use crate::event::WrappedEvent;
use anyhow::Context;
use anyhow::anyhow;
use futures::FutureExt;
use futures::StreamExt;
use futures::future::OptionFuture;
use futures::lock::Mutex as AsyncMutex;
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
use std::task::ready;
use std::time::Duration;
use vmbus_channel::bus::ChannelServerRequest;
use vmbus_channel::bus::ChannelType;
use vmbus_channel::bus::OfferKey;
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
use windows::Win32::Foundation::ERROR_OPERATION_ABORTED;
use windows::core::HRESULT;
use zerocopy::IntoBytes;

/// Provides access to a vmbus server, its optional hvsocket relay, and
/// a channel to received saved state information.
pub struct ProxyServerInfo {
    control: Arc<VmbusServerControl>,
    hvsock_relay: Option<HvsockRelayChannelHalf>,
    saved_state_recv: Option<mesh::Receiver<SavedStateRequest>>,
}

impl ProxyServerInfo {
    /// Creates a new `ProxyServerInfo` instance.
    pub fn new(
        control: Arc<VmbusServerControl>,
        hvsock_relay: Option<HvsockRelayChannelHalf>,
        saved_state_recv: Option<mesh::Receiver<SavedStateRequest>>,
    ) -> Self {
        Self {
            control,
            hvsock_relay,
            saved_state_recv,
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

struct SavedStatePair {
    saved_state: Option<SavedState>,
    vtl2_saved_state: Option<SavedState>,
}

struct ProxyTask {
    channels: Arc<Mutex<HashMap<u64, Channel>>>,
    gpadls: Arc<Mutex<HashMap<u64, HashSet<GpadlId>>>>,
    proxy: Arc<VmbusProxy>,
    server: Arc<VmbusServerControl>,
    vtl2_server: Option<Arc<VmbusServerControl>>,
    hvsock_response_send: Option<mesh::Sender<HvsockConnectResult>>,
    vtl2_hvsock_response_send: Option<mesh::Sender<HvsockConnectResult>>,
    saved_states: Arc<AsyncMutex<SavedStatePair>>,
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
            saved_states: Arc::new(AsyncMutex::new(SavedStatePair {
                saved_state: None,
                vtl2_saved_state: None,
            })),
        }
    }

    fn create_worker_thread(&self, proxy_id: u64) -> mesh::OneshotReceiver<()> {
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

        recv
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

        let recv = self.create_worker_thread(proxy_id);

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

    async fn restore_open_channel_on_offer(
        &self,
        proxy_id: u64,
        offer_key: OfferKey,
        vtl: u8,
        server_request_send: Option<mesh::Sender<ChannelServerRequest>>,
        incoming_event: Event,
    ) -> Option<(Option<WrappedEvent>, mesh::OneshotReceiver<()>)> {
        let channel_saved_open = {
            let saved_states = self.saved_states.lock().await;
            match vtl {
                0 => saved_states.saved_state.as_ref(),
                2 => saved_states.vtl2_saved_state.as_ref(),
                _ => unreachable!(),
            }?
            .find_channel(offer_key)?
            .saved_open()
        };

        let send = server_request_send.as_ref()?;
        tracing::trace!(interface_id = %offer_key.interface_id,
            instance_id = %offer_key.instance_id,
            "restoring channel after offer");

        let restore_result = match send
            .call_failable(
                ChannelServerRequest::Restore,
                channel_saved_open.then(|| OpenResult {
                    guest_to_host_interrupt: Interrupt::from_event(incoming_event.clone()),
                }),
            )
            .await
        {
            Ok(result) => result,
            Err(err) => {
                tracing::warn!(
                    err = &err as &dyn std::error::Error,
                    interface_id = %offer_key.interface_id,
                    instance_id = %offer_key.instance_id,
                    "failed to restore channel"
                );
                return None;
            }
        };

        let Some(open_request) = restore_result.open_request else {
            if channel_saved_open {
                panic!("failed to restore channel {}: no OpenRequest", offer_key);
            } else {
                // The channel was not saved open. There is no more work to do.
                return None;
            }
        };

        let maybe_wrapped =
            MaybeWrappedEvent::new(&TpPool::system(), open_request.interrupt.clone()).unwrap();

        self.proxy
            .set_interrupt(proxy_id, maybe_wrapped.event())
            .await
            .unwrap_or_else(|e| {
                panic!(
                    "failed to set interrupt in proxy for channel {}: {:?}",
                    offer_key, e
                )
            });

        let recv = self.create_worker_thread(proxy_id);

        Some((maybe_wrapped.into_wrapped(), recv))
    }

    async fn handle_offer(
        &self,
        proxy_id: u64,
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

        let new_offer = OfferParams {
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
            offer_order: proxy_id.try_into().ok(),
            allow_confidential_external_memory: false,
        };
        let (request_send, request_recv) = mesh::channel();
        let (server_request_send, server_request_recv) = mesh::channel();

        let recv = server.send.call_failable(
            OfferRequest::Offer,
            OfferInfo {
                params: new_offer.into(),
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

        let restore_result = self
            .restore_open_channel_on_offer(
                proxy_id,
                OfferKey {
                    interface_id,
                    instance_id,
                    subchannel_index: offer.SubChannelIndex,
                },
                offer.TargetVtl,
                server_request_send.clone(),
                incoming_event.clone(),
            )
            .await;

        let (wrapped_event, worker_result) = match restore_result {
            Some((wrapped_event, restore_result)) => (wrapped_event, Some(restore_result)),
            None => (None, None),
        };

        self.channels.lock().insert(
            proxy_id,
            Channel {
                server_request_send,
                incoming_event,
                worker_result,
                wrapped_event,
            },
        );

        request_recv
    }

    async fn handle_revoke(&self, proxy_id: u64) {
        let response_send = self
            .channels
            .lock()
            .get_mut(&proxy_id)
            .unwrap()
            .server_request_send
            .take();

        if let Some(response_send) = response_send {
            drop(response_send);
        } else {
            if let Err(err) = self.proxy.release(proxy_id).await {
                if err.code() == HRESULT::from(ERROR_OPERATION_ABORTED) {
                    tracing::trace!(%proxy_id, "proxy release aborted during ioctl");
                } else {
                    tracing::error!(
                        error = &err as &dyn std::error::Error,
                        "proxy channel release failed"
                    );
                    panic!("vmbus proxy state failure");
                }
            }

            self.channels.lock().remove(&proxy_id);
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
                    let Some(recv) = &mut flush_recv else {
                        break Poll::Pending;
                    };
                    if let Some(rpc) = ready!(recv.poll_next_unpin(cx)) {
                        // We received a flush request from the client. Save
                        // this request and loop around to poll the action
                        // again.
                        pending_flush = Some(rpc);
                    } else {
                        // The flush channel was closed, so we can stop
                        // waiting for flushes.
                        flush_recv = None;
                    }
                }
            })
            .await;

            let action = match action {
                Ok(action) => action,
                Err(e) => {
                    if e == ERROR_OPERATION_ABORTED.into() {
                        tracing::debug!("proxy cancelled");
                    } else {
                        tracing::error!(
                            error = &e as &dyn std::error::Error,
                            "failed to get action",
                        );
                    }
                    break;
                }
            };

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
                                if e.code() == HRESULT::from(ERROR_OPERATION_ABORTED) {
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
                    if let Err(err) = self.proxy.release(proxy_id).await {
                        if err.code() == HRESULT::from(ERROR_OPERATION_ABORTED) {
                            tracing::trace!(%proxy_id, "proxy release aborted during ioctl");
                        } else {
                            tracing::error!(
                                error = &err as &dyn std::error::Error,
                                "proxy channel release failed"
                            );
                            panic!("vmbus proxy state failure");
                        }
                    }

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

    async fn handle_saved_state_request(
        &self,
        request: Result<SavedStateRequest, mesh::RecvError>,
        vtl: u8,
    ) -> bool {
        let request = match request {
            Ok(request) => request,
            Err(e) => {
                // Closed can happen normally during shutdown, so does not need to be logged.
                if !matches!(e, mesh::RecvError::Closed) {
                    tracelimit::error_ratelimited!(
                        error = ?&e as &dyn std::error::Error,
                        "saved state request receive failed"
                    );
                }
                return false;
            }
        };

        let mut saved_states = self.saved_states.lock().await;
        let saved_state_option = match vtl {
            0 => &mut saved_states.saved_state,
            2 => &mut saved_states.vtl2_saved_state,
            _ => {
                tracelimit::error_ratelimited!(
                    vtl = ?vtl,
                    "saved state request receive failed: Unsupported VTL"
                );

                return true;
            }
        };

        match request {
            SavedStateRequest::Set(rpc) => {
                // Map the vmbus server channel ID to the newly created proxy channel ID
                let mut proxy_ids: HashMap<u32, u64> = HashMap::new();
                tracing::trace!("restoring channels...");

                rpc.handle_failable(async |saved_state| {
                    // Restore channel state in the proxy for each channel in the SavedState.
                    if let Some(channels) = saved_state.channels() {
                        for channel in channels {
                            tracing::trace!(?channel, "restoring channel");
                            let key = channel.key();
                            let open_params = channel.open_request();
                            let Some(open_params) = open_params else {
                                continue;
                            };
                            let proxy_id = self
                                .proxy
                                .restore(
                                    key.interface_id.into(),
                                    key.instance_id.into(),
                                    key.subchannel_index,
                                    VMBUS_SERVER_OPEN_CHANNEL_OUTPUT_PARAMETERS {
                                        RingBufferGpadlHandle: open_params.ring_buffer_gpadl_id.0,
                                        DownstreamRingBufferPageOffset: open_params
                                            .downstream_ring_buffer_page_offset,
                                        NodeNumber: 0, // BUGBUG: NUMA
                                    },
                                    channel.saved_open(),
                                )
                                .await
                                .with_context(|| {
                                    format!(
                                        "Failed to restore channel {} in proxy",
                                        channel.channel_id()
                                    )
                                })?;

                            proxy_ids.insert(channel.channel_id(), proxy_id);
                        }
                        if let Some(gpadls) = saved_state.gpadls() {
                            for gpadl in gpadls {
                                if gpadl.is_tearing_down() {
                                    continue;
                                }
                                let Some(proxy_id) = proxy_ids.get(&gpadl.channel_id) else {
                                    continue;
                                };
                                tracing::trace!(
                                    id = %gpadl.id,
                                    channel_id = %gpadl.channel_id,
                                    proxy_id = %proxy_id,
                                    "restoring gpadl in proxy"
                                );
                                self.handle_gpadl_create(
                                    *proxy_id,
                                    GpadlId(gpadl.id),
                                    gpadl.count,
                                    gpadl.buf.as_slice(),
                                )
                                .await
                                .with_context(|| {
                                    format!(
                                        "failed to restore GPADLs ID {} for channel {} in proxy",
                                        gpadl.channel_id, gpadl.id
                                    )
                                })?;
                            }
                        }
                    } else {
                        return Err(anyhow!("No channels exist in the saved state"));
                    }

                    *saved_state_option = Some(*saved_state);
                    Ok(())
                })
                .await;
            }
            SavedStateRequest::Clear(rpc) => {
                rpc.handle(async |()| {
                    if saved_state_option.is_some() {
                        // The VM has started. Tell the proxy to revoke all unclaimed channels.
                        if let Err(err) = self.proxy.revoke_unclaimed_channels().await {
                            tracing::error!(
                                error = &err as &dyn std::error::Error,
                                "revoke unclaimed channels ioctl failed"
                            );
                        }
                    }
                })
                .await;

                *saved_state_option = None;
            }
        }

        true
    }

    async fn run_server_requests(
        self: &Arc<Self>,
        spawner: impl Spawn,
        mut recv: mesh::Receiver<TaggedStream<u64, mesh::Receiver<ChannelRequest>>>,
        mut hvsock_request_recv: Option<mesh::Receiver<HvsockConnectRequest>>,
        mut vtl2_hvsock_request_recv: Option<mesh::Receiver<HvsockConnectRequest>>,
        mut saved_state_recv: Option<mesh::Receiver<SavedStateRequest>>,
        mut vtl2_saved_state_recv: Option<mesh::Receiver<SavedStateRequest>>,
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

                let mut saved_state_requests = OptionFuture::from(
                    saved_state_recv
                        .as_mut()
                        .map(|recv| Box::pin(recv.recv()).fuse()),
                );

                let mut vtl2_saved_state_requests = OptionFuture::from(
                    vtl2_saved_state_recv
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
                    r = saved_state_requests => {
                        if !self.handle_saved_state_request(r.unwrap(), 0).await {
                            saved_state_recv = None;
                        }
                    }
                    r = vtl2_saved_state_requests => {
                        if !self.handle_saved_state_request(r.unwrap(), 2).await {
                            vtl2_saved_state_recv = None;
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
    let (vtl2_control, vtl2_hvsock_request_recv, vtl2_hvsock_response_send, vtl2_saved_state_recv) =
        if let Some(server) = vtl2_server {
            let (vtl2_hvsock_request_recv, vtl2_hvsock_response_send) = server
                .hvsock_relay
                .map(|relay| (relay.request_receive, relay.response_send))
                .unzip();
            let vtl2_saved_state_recv = server.saved_state_recv;
            (
                Some(server.control),
                vtl2_hvsock_request_recv,
                vtl2_hvsock_response_send,
                vtl2_saved_state_recv,
            )
        } else {
            (None, None, None, None)
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
    let requests = task.run_server_requests(
        spawner,
        recv,
        hvsock_request_recv,
        vtl2_hvsock_request_recv,
        server.saved_state_recv,
        vtl2_saved_state_recv,
    );

    futures::future::join(offers, requests).await;
    tracing::debug!("proxy thread finished");
    // BUGBUG: cancel all IO if something goes wrong?
}
