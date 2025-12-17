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
use crate::channels::SavedStateData;
use crate::channels::saved_state::GpadlState;
use crate::event::MaybeWrappedEvent;
use crate::event::WrappedEvent;
use anyhow::Context;
use futures::FutureExt;
use futures::StreamExt;
use futures::future::OptionFuture;
use futures::lock::Mutex as AsyncMutex;
use futures::stream::SelectAll;
use guestmem::GuestMemory;
use mesh::Cancel;
use mesh::CancelContext;
use mesh::rpc::FailableRpc;
use mesh::rpc::RpcError;
use mesh::rpc::RpcSend;
use pal_async::driver::SpawnDriver;
use pal_async::task::Spawn;
use pal_async::task::Task;
use pal_async::windows::TpPool;
use pal_event::Event;
use parking_lot::Mutex;
use std::collections::HashMap;
use std::collections::HashSet;
use std::future::Future;
use std::future::poll_fn;
use std::io;
use std::num::NonZeroU32;
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
use vmbus_channel::gpadl::GpadlId;
use vmbus_core::HvsockConnectRequest;
use vmbus_core::HvsockConnectResult;
use vmbus_core::protocol;
use vmbus_proxy::Gpadl;
use vmbus_proxy::ProxyAction;
use vmbus_proxy::VmbusProxy;
use vmbus_proxy::vmbusioctl::VMBUS_SERVER_OPEN_CHANNEL_OUTPUT_PARAMETERS;
use vmcore::interrupt::Interrupt;
use windows::Win32::Foundation::ERROR_NOT_FOUND;
use windows::Win32::Foundation::ERROR_OPERATION_ABORTED;
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
    pub fn new(control: Arc<VmbusServerControl>) -> Self {
        Self {
            control,
            hvsock_relay: None,
            saved_state_recv: None,
        }
    }

    /// Sets the hvsocket relay for this server.
    pub fn with_hvsock_relay(mut self, relay: Option<HvsockRelayChannelHalf>) -> Self {
        self.hvsock_relay = relay;
        self
    }

    /// Sets the saved state receiver for this server.
    pub fn with_saved_state_recv(
        mut self,
        recv: Option<mesh::Receiver<SavedStateRequest>>,
    ) -> Self {
        self.saved_state_recv = recv;
        self
    }
}

/// Specifies the options for creating a `ProxyIntegration`.
pub struct ProxyIntegrationBuilder<'a, T: SpawnDriver + Clone> {
    driver: &'a T,
    handle: ProxyHandle,
    server: ProxyServerInfo,
    vtl2_server: Option<ProxyServerInfo>,
    mem: Option<&'a GuestMemory>,
    require_flush_before_start: bool,
    vp_to_physical_node_map: Vec<u16>,
}

impl<'a, T: SpawnDriver + Clone> ProxyIntegrationBuilder<'a, T> {
    /// Sets the VTL2 server info.
    pub fn vtl2_server(mut self, server: Option<ProxyServerInfo>) -> Self {
        self.vtl2_server = server;
        self
    }

    /// Sets the guest memory the proxy driver should use.
    pub fn memory(mut self, mem: Option<&'a GuestMemory>) -> Self {
        self.mem = mem;
        self
    }

    /// Requires an initial flush before processing any actions.
    pub fn require_flush_before_start(mut self, require: bool) -> Self {
        self.require_flush_before_start = require;
        self
    }

    /// Adds a NUMA node map to be passed to the proxy driver. This map is of the format
    /// VP -> Physical NUMA Node. For example, `map[0]` is the physical NUMA node for VP 0.
    pub fn vp_to_physical_node_map(mut self, map: Vec<u16>) -> Self {
        self.vp_to_physical_node_map = map;
        self
    }

    /// Builds and starts the `ProxyIntegration`.
    pub async fn build(self) -> io::Result<ProxyIntegration> {
        let (cancel_ctx, cancel) = CancelContext::new().with_cancel();
        let mut proxy = VmbusProxy::new(self.driver, self.handle, cancel_ctx)?;
        let handle = proxy.handle().try_clone_to_owned()?;
        if let Some(mem) = self.mem {
            proxy.set_memory(mem).await?;
        }

        let (flush_send, flush_recv) = mesh::channel();
        let task = self.driver.spawn(
            "vmbus_proxy",
            proxy_thread(
                self.driver.clone(),
                proxy,
                self.server,
                self.vtl2_server,
                flush_recv,
                self.require_flush_before_start,
                self.vp_to_physical_node_map,
            ),
        );

        Ok(ProxyIntegration {
            cancel,
            handle,
            flush_send,
            task: Some(task),
        })
    }
}

pub struct ProxyIntegration {
    cancel: Cancel,
    handle: OwnedHandle,
    flush_send: mesh::Sender<FailableRpc<(), ()>>,
    task: Option<Task<()>>,
}

impl ProxyIntegration {
    /// Creates a new `ProxyIntegrationBuilder`.
    pub fn builder<T: SpawnDriver + Clone>(
        driver: &T,
        handle: ProxyHandle,
        server: ProxyServerInfo,
    ) -> ProxyIntegrationBuilder<'_, T> {
        ProxyIntegrationBuilder {
            driver,
            handle,
            server,
            vtl2_server: None,
            mem: None,
            require_flush_before_start: false,
            vp_to_physical_node_map: vec![],
        }
    }

    /// Cancels the vmbus proxy, and waits for it to finish.
    pub async fn cancel(&mut self) {
        self.cancel.cancel();
        if let Some(task) = self.task.take() {
            task.await
        }
    }

    /// Wait for all currently ready pending actions to complete. E.g., wait for
    /// all channels that have been offered to the kernel driver to have been
    /// processed.
    pub async fn flush_actions(&mut self) -> Result<(), RpcError<mesh::error::RemoteError>> {
        self.flush_send.call_failable(|v| v, ()).await
    }

    /// Returns the handle to the vmbus proxy driver.
    pub fn handle(&self) -> BorrowedHandle<'_> {
        self.handle.as_handle()
    }
}

struct ChannelOpenState {
    worker_result: mesh::OneshotReceiver<()>,
    _wrapped_event: Option<WrappedEvent>,
}

#[derive(Default)]
struct Channel {
    server_request_send: Option<mesh::Sender<ChannelServerRequest>>,
    open_state: Option<ChannelOpenState>,
}

struct SavedStatePair {
    saved_state: Option<SavedStateData>,
    vtl2_saved_state: Option<SavedStateData>,
}

impl SavedStatePair {
    fn for_vtl(&self, vtl: u8) -> Option<&SavedStateData> {
        match vtl {
            0 => self.saved_state.as_ref(),
            2 => self.vtl2_saved_state.as_ref(),
            _ => None,
        }
    }

    fn for_vtl_mut(&mut self, vtl: u8) -> &mut Option<SavedStateData> {
        match vtl {
            0 => &mut self.saved_state,
            2 => &mut self.vtl2_saved_state,
            _ => unreachable!("unsupported VTL {vtl}"),
        }
    }
}

struct VpToPhysicalNodeMap(Vec<u16>);

impl VpToPhysicalNodeMap {
    fn get_numa_node(&self, vp_index: u32) -> u16 {
        self.0.get(vp_index as usize).copied().unwrap_or(0)
    }
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
    vp_to_physical_node_map: VpToPhysicalNodeMap,
}

impl ProxyTask {
    fn new(
        server: Arc<VmbusServerControl>,
        vtl2_server: Option<Arc<VmbusServerControl>>,
        hvsock_response_send: Option<mesh::Sender<HvsockConnectResult>>,
        vtl2_hvsock_response_send: Option<mesh::Sender<HvsockConnectResult>>,
        proxy: Arc<VmbusProxy>,
        vp_to_physical_node_map: VpToPhysicalNodeMap,
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
            vp_to_physical_node_map,
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

    /// Determines if the ioctl was successful or an expected error.
    ///
    /// The only error this function will actually return is ERROR_NOT_FOUND, which callers can
    /// ignore if they do not need to behave differently.
    ///
    /// It panics on all other errors.
    fn check_ioctl_result(
        result: windows::core::Result<()>,
        op: &str,
        proxy_id: u64,
    ) -> windows::core::Result<()> {
        result.inspect_err(|err| {
            // Due to various operations racing with revoke, calls into the proxy driver may
            // fail with ERROR_NOT_FOUND if the channel no longer exists. Other error codes indicate
            // the driver and server are out of sync and can likely not be recovered from.
            assert!(err.code() == ERROR_NOT_FOUND.into());
            tracing::info!(
                error = err as &dyn std::error::Error,
                op,
                proxy_id,
                "channel not found during ioctl (possibly revoked)"
            );
        })
    }

    async fn handle_open(&self, proxy_id: u64, open_request: &OpenRequest) -> anyhow::Result<()> {
        let maybe_wrapped =
            MaybeWrappedEvent::new(&TpPool::system(), open_request.interrupt.clone())?;

        self.proxy
            .open(
                proxy_id,
                &VMBUS_SERVER_OPEN_CHANNEL_OUTPUT_PARAMETERS {
                    RingBufferGpadlHandle: open_request.open_data.ring_gpadl_id.0,
                    DownstreamRingBufferPageOffset: open_request.open_data.ring_offset,
                    NodeNumber: self
                        .vp_to_physical_node_map
                        .get_numa_node(open_request.open_data.target_vp),
                    Padding: 0,
                },
                maybe_wrapped.event(),
            )
            .await
            .context("failed to open channel")?;

        let mut channels = self.channels.lock();
        let channel = channels
            .get_mut(&proxy_id)
            .ok_or_else(|| anyhow::anyhow!("channel revoked during open"))?;

        let recv = self.create_worker_thread(proxy_id);
        channel.open_state = Some(ChannelOpenState {
            worker_result: recv,
            _wrapped_event: maybe_wrapped.into_wrapped(),
        });

        Ok(())
    }

    async fn handle_close(&self, proxy_id: u64) {
        let _ = Self::check_ioctl_result(self.proxy.close(proxy_id).await, "close", proxy_id);

        // Wait for the worker task.
        // N.B. The channel may have been revoked.
        let open_state = self
            .channels
            .lock()
            .get_mut(&proxy_id)
            .and_then(|channel| channel.open_state.take());

        if let Some(open_state) = open_state {
            let _ = open_state.worker_result.await;
        }
    }

    async fn handle_gpadl_create(
        &self,
        proxy_id: u64,
        gpadl_id: GpadlId,
        count: u16,
        buf: &[u64],
    ) -> anyhow::Result<()> {
        Self::check_ioctl_result(
            self.proxy
                .create_gpadl(proxy_id, gpadl_id.0, count.into(), buf.as_bytes())
                .await,
            "create_gpadl",
            proxy_id,
        )
        .context("failed to create gpadl")?;

        self.gpadls
            .lock()
            .entry(proxy_id)
            .or_default()
            .insert(gpadl_id);
        Ok(())
    }

    async fn handle_gpadl_teardown(&self, proxy_id: u64, gpadl_id: GpadlId) {
        if let Some(gpadls) = self.gpadls.lock().get_mut(&proxy_id) {
            assert!(gpadls.remove(&gpadl_id), "gpadl is registered");
        } else {
            return;
        }

        let _ = Self::check_ioctl_result(
            self.proxy.delete_gpadl(proxy_id, gpadl_id.0).await,
            "delete_gpadl",
            proxy_id,
        );
    }

    async fn check_channel_saved_open(&self, vtl: u8, offer_key: OfferKey) -> Option<bool> {
        let channel_saved_open = self
            .saved_states
            .lock()
            .await
            .for_vtl(vtl)?
            .find_channel(offer_key)?
            .open_request()
            .is_some();

        Some(channel_saved_open)
    }

    async fn restore_channel_on_offer(
        &self,
        proxy_id: u64,
        offer_key: OfferKey,
        vtl: u8,
        server_request_send: mesh::Sender<ChannelServerRequest>,
    ) -> anyhow::Result<Option<ChannelOpenState>> {
        // A channel is considered saved in the "open" state if it is in any state that has an open
        // request. This is because the server will not notify the channel for the open in any of
        // those states after restore. In the Closing and ClosingReopen state it will notify the
        // close, so there too we need to restore as open.
        // N.B. If there is no saved state, or the channel is not in the saved state, there is
        //      nothing to be done here.
        let Some(channel_saved_open) = self.check_channel_saved_open(vtl, offer_key).await else {
            return Ok(None);
        };

        tracing::trace!(interface_id = %offer_key.interface_id,
            instance_id = %offer_key.instance_id,
            "restoring channel after offer");

        let restore_result = server_request_send
            .call_failable(ChannelServerRequest::Restore, channel_saved_open)
            .await
            .context("failed to restore channel")?;

        let Some(open_request) = restore_result.open_request else {
            if channel_saved_open {
                anyhow::bail!("failed to restore channel {offer_key}: no OpenRequest");
            }

            // The channel was not saved open. There is no more work to do.
            return Ok(None);
        };

        let maybe_wrapped =
            MaybeWrappedEvent::new(&TpPool::system(), open_request.interrupt.clone()).unwrap();

        self.proxy
            .set_interrupt(proxy_id, maybe_wrapped.event())
            .await
            .unwrap_or_else(|e| {
                panic!("failed to set interrupt in proxy for channel {offer_key}: {e:?}")
            });

        let recv = self.create_worker_thread(proxy_id);

        Ok(Some(ChannelOpenState {
            worker_result: recv,
            _wrapped_event: maybe_wrapped.into_wrapped(),
        }))
    }

    async fn handle_offer(
        &self,
        proxy_id: u64,
        offer: vmbus_proxy::vmbusioctl::VMBUS_CHANNEL_OFFER,
        incoming_event: Event,
        device_order: Option<NonZeroU32>,
    ) -> anyhow::Result<mesh::Receiver<ChannelRequest>> {
        self.handle_offer_core(proxy_id, offer, incoming_event, device_order)
            .await
            .inspect_err(|err| {
                tracing::error!(
                    error = err.as_ref() as &dyn std::error::Error,
                    proxy_id,
                    ?offer,
                    "failed to offer vmbusproxy channel"
                );

                // A channel cannot be released unless it was revoked, so we need to keep track of
                // the proxy ID so the driver can revoke it later.
                // N.B. There is currently no way to propagate the failure to the device that
                //      offered the channel.
                assert!(
                    self.channels
                        .lock()
                        .insert(proxy_id, Channel::default(),)
                        .is_none(),
                    "proxy driver used duplicate proxy id {proxy_id}"
                );
            })
    }

    async fn handle_offer_core(
        &self,
        proxy_id: u64,
        offer: vmbus_proxy::vmbusioctl::VMBUS_CHANNEL_OFFER,
        incoming_event: Event,
        device_order: Option<NonZeroU32>,
    ) -> anyhow::Result<mesh::Receiver<ChannelRequest>> {
        tracing::debug!(proxy_id, ?offer, ?device_order, "received vmbusproxy offer");
        let server = match offer.TargetVtl {
            0 => self.server.as_ref(),
            2 => {
                if let Some(server) = self.vtl2_server.as_ref() {
                    server.as_ref()
                } else {
                    anyhow::bail!("VTL2 offer without VTL2 server");
                }
            }
            _ => {
                anyhow::bail!("unsupported offer VTL");
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
            if offer.ChannelFlags.named_pipe_mode() {
                let params = offer.UserDefined.as_pipe_params();
                let message_mode = match params.pipe_type {
                    protocol::PipeType::BYTE => false,
                    protocol::PipeType::MESSAGE => true,
                    _ => {
                        anyhow::bail!("unsupported offer pipe mode");
                    }
                };
                ChannelType::Pipe { message_mode }
            } else {
                ChannelType::Interface {
                    user_defined: offer.UserDefined,
                }
            }
        } else {
            ChannelType::Device {
                pipe_packets: offer.ChannelFlags.named_pipe_mode(),
            }
        };

        let interface_id: Guid = offer.InterfaceType.into();
        let instance_id: Guid = offer.InterfaceInstance.into();

        // Create an offer order by combining the device order from the proxy driver and the
        // proxy_id. This has the effect that channels offered by the same device are ordered
        // together, even if the use_absolute_channel_order option is enabled.
        //
        // Offers without a device order are ordered after all offers with a device order.
        //
        // N.B. No order is set if the proxy_id does not fit in a u32, which should not typically
        //      happen.
        let offer_order = proxy_id.try_into().ok().map(|proxy_id: u32| {
            ((device_order.unwrap_or(NonZeroU32::MAX).get() as u64) << 32) | proxy_id as u64
        });

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
            offer_order,
            allow_confidential_external_memory: false,
        };
        let (request_send, request_recv) = mesh::channel();
        let (server_request_send, server_request_recv) = mesh::channel();

        server
            .send
            .call_failable(
                OfferRequest::Offer,
                OfferInfo {
                    params: new_offer.into(),
                    event: Interrupt::from_event(incoming_event),
                    request_send,
                    server_request_recv,
                },
            )
            .await
            .context("failed to offer proxy channel")?;

        // Do not call restore if the device is requesting that the channel be reoffered.
        let restored_open_state = if !offer.ChannelFlags.force_new_channel() {
            self.restore_channel_on_offer(
                proxy_id,
                OfferKey {
                    interface_id,
                    instance_id,
                    subchannel_index: offer.SubChannelIndex,
                },
                offer.TargetVtl,
                server_request_send.clone(),
            )
            .await?
        } else {
            None
        };

        assert!(
            self.channels
                .lock()
                .insert(
                    proxy_id,
                    Channel {
                        server_request_send: Some(server_request_send),
                        open_state: restored_open_state,
                    },
                )
                .is_none(),
            "proxy driver used duplicate proxy id {proxy_id}"
        );

        Ok(request_recv)
    }

    async fn handle_revoke(&self, proxy_id: u64) {
        let server_request_send = self
            .channels
            .lock()
            .get_mut(&proxy_id)
            .unwrap()
            .server_request_send
            .take();

        // This can be None if offering the channel to the server failed.
        if let Some(server_request_send) = server_request_send {
            // Perform an explicit revoke so we can release the channel immediately after.
            if let Err(err) = server_request_send
                .call(ChannelServerRequest::Revoke, ())
                .await
            {
                // If the mesh channel is closed, the revoke happened after the server is shutting
                // down, which is benign as long as we still release the channel below.
                if !matches!(err, RpcError::Channel(mesh::RecvError::Closed)) {
                    tracing::error!(
                        error = &err as &dyn std::error::Error,
                        id = %proxy_id,
                        "failed to revoke channel"
                    );
                }
            }

            // Close the channel if it was opened in the proxy; if not this is a no-op.
            let _ = self.proxy.close(proxy_id).await;
            let gpadls = self.gpadls.lock().remove(&proxy_id);

            // Delete all GPADLs for this channel that weren't removed by the guest.
            // N.B. Due to a bug in some versions of vmbusproxy, not doing this causes bugchecks if
            //      there are any GPADLs still registered during teardown.
            if let Some(gpadls) = gpadls {
                if !gpadls.is_empty() {
                    tracing::info!(proxy_id, "closed while some gpadls are still registered");
                    for gpadl_id in gpadls {
                        if let Err(e) = self.proxy.delete_gpadl(proxy_id, gpadl_id.0).await {
                            tracing::error!(error = ?e, "failed to delete gpadl");
                        }
                    }
                }
            }
        };

        self.proxy
            .release(proxy_id)
            .await
            .expect("vmbus proxy state failure");

        // At this point, no more driver or server requests can come in for this channel.
        self.channels.lock().remove(&proxy_id);
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
        mut flush_recv: mesh::Receiver<FailableRpc<(), ()>>,
        await_flush: bool,
    ) {
        // If requested, wait for an initial flush before processing any actions. This allows the
        // caller to ensure that the initial set of offers are handled at a predictable time.
        let mut pending_flush = None;
        if await_flush {
            tracing::trace!("waiting for initial flush");
            let flush = match flush_recv.recv().await {
                Ok(flush) => flush,
                Err(err) => {
                    tracing::error!(
                        error = &err as &dyn std::error::Error,
                        "flush channel closed before initial flush"
                    );
                    return;
                }
            };

            tracing::debug!("initial flush received");
            pending_flush = Some(flush);
        }

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
                        pending_flush.complete(Ok(()));
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
                    device_order,
                } => {
                    match self
                        .handle_offer(id, offer, incoming_event, device_order)
                        .await
                    {
                        Ok(recv) => send.send(TaggedStream::new(id, recv)),
                        Err(err) => {
                            if let Some(pending_flush) = pending_flush.take() {
                                pending_flush.fail(err);
                            }
                        }
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

    async fn handle_request(&self, proxy_id: u64, request: ChannelRequest) {
        match request {
            ChannelRequest::Open(rpc) => {
                rpc.handle(async |open_request| {
                    self.handle_open(proxy_id, &open_request)
                        .await
                        .inspect_err(|err| {
                            tracing::error!(
                                error = err.as_ref() as &dyn std::error::Error,
                                "failed to open channel"
                            );
                        })
                        .is_ok()
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

        match request {
            SavedStateRequest::Set(rpc) => {
                rpc.handle_failable(async |saved_state| {
                    self.handle_saved_state_set(saved_state, vtl).await
                })
                .await;
            }
            SavedStateRequest::Clear(rpc) => {
                rpc.handle(async |()| self.handle_saved_state_clear(vtl).await)
                    .await;
            }
        }

        true
    }

    /// Restores proxy state from a VMBus saved state, and stores the state for use during offers.
    async fn handle_saved_state_set(
        &self,
        saved_state: Box<SavedState>,
        vtl: u8,
    ) -> anyhow::Result<()> {
        tracing::trace!("restoring channels...");
        let mut saved_states = self.saved_states.lock().await;
        let saved_state_option = saved_states.for_vtl_mut(vtl);
        let saved_state = SavedStateData::from(*saved_state);
        let (channels, gpadls) = saved_state.channels_and_gpadls();
        assert!(
            saved_state_option.is_none(),
            "saved state for VTL {vtl} already set"
        );

        for channel in channels {
            tracing::trace!(?channel, "restoring channel");
            let key = channel.key();
            let channel_gpadls = gpadls.iter().filter_map(|g| {
                (g.channel_id == channel.channel_id() && matches!(g.state, GpadlState::Accepted))
                    .then_some(Gpadl {
                        gpadl_id: g.id,
                        range_count: g.count.into(),
                        range_buffer: &g.buf,
                    })
            });

            let open_params =
                channel
                    .open_request()
                    .map(|request| VMBUS_SERVER_OPEN_CHANNEL_OUTPUT_PARAMETERS {
                        RingBufferGpadlHandle: request.ring_buffer_gpadl_id.0,
                        DownstreamRingBufferPageOffset: request.downstream_ring_buffer_page_offset,
                        NodeNumber: self
                            .vp_to_physical_node_map
                            .get_numa_node(request.target_vp),
                        Padding: 0,
                    });

            let proxy_id = self
                .proxy
                .restore(
                    key.interface_id,
                    key.instance_id,
                    key.subchannel_index,
                    vtl,
                    open_params,
                    channel_gpadls.clone(),
                )
                .await
                .with_context(|| {
                    format!(
                        "Failed to restore channel {} in proxy",
                        channel.channel_id()
                    )
                })?;

            // Register the restored GPADLs.
            assert!(
                self.gpadls
                    .lock()
                    .insert(
                        proxy_id,
                        channel_gpadls.map(|g| GpadlId(g.gpadl_id)).collect()
                    )
                    .is_none(),
                "proxy driver used duplicate proxy id {proxy_id} during restore",
            )
        }

        *saved_state_option = Some(saved_state);
        Ok(())
    }

    /// Clears the saved state for the given VTL, and notifies the proxy the restore operation is
    /// complete.
    async fn handle_saved_state_clear(&self, vtl: u8) {
        let mut saved_states = self.saved_states.lock().await;
        let saved_state_option = saved_states.for_vtl_mut(vtl);
        if saved_state_option.take().is_some() {
            // The VM has started. Tell the proxy to revoke all unclaimed channels.
            // N.B. For a VM with VTL2 we will end up doing this twice, but that is benign.
            if let Err(err) = self.proxy.revoke_unclaimed_channels().await {
                tracing::error!(
                    error = &err as &dyn std::error::Error,
                    "revoke unclaimed channels ioctl failed"
                );
            }
        }
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

            if let Some(request) = request {
                let this = self.clone();
                spawner
                    .spawn("vmbus-proxy-req", async move {
                        this.handle_request(proxy_id, request).await
                    })
                    .detach();
            }
        }

        tracing::debug!("proxy channel requests finished");
    }
}

async fn proxy_thread(
    spawner: impl Spawn,
    proxy: VmbusProxy,
    server: ProxyServerInfo,
    vtl2_server: Option<ProxyServerInfo>,
    flush_recv: mesh::Receiver<FailableRpc<(), ()>>,
    await_flush: bool,
    vp_to_physical_node_map: Vec<u16>,
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
        VpToPhysicalNodeMap(vp_to_physical_node_map),
    ));
    let offers = task.run_proxy_actions(send, flush_recv, await_flush);
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
