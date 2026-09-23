// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Network backend traits and infrastructure.
//!
//! This crate defines the abstraction boundary between network
//! **frontends** (guest-facing devices) and network **backends**
//! (host-side packet I/O). The key types are:
//!
//! * [`Endpoint`] — a backend factory. One per NIC, responsible for
//!   creating [`Queue`] objects when the frontend activates the device.
//!
//! * [`Queue`] — a single TX/RX data path. Backends implement this to
//!   send and receive packets. A device may have multiple queues (RSS).
//!
//! * [`BufferAccess`] — owned by the frontend, provides access to
//!   guest memory receive buffers. Passed by `&mut` reference to every
//!   [`Queue`] method that needs it, so the frontend retains exclusive
//!   ownership and no internal locking is required.
//!
//! ## Lifecycle
//!
//! 1. The frontend creates a [`BufferAccess`] implementation and one
//!    [`QueueConfig`] per desired queue (containing just a driver).
//! 2. It calls [`Endpoint::get_queues`], which returns boxed [`Queue`]
//!    objects.
//! 3. The frontend posts initial receive buffers by calling
//!    [`Queue::rx_avail`] with its [`BufferAccess`].
//! 4. The main loop polls [`Queue::poll_ready`] for backend events,
//!    then calls [`Queue::rx_poll`] / [`Queue::tx_avail`] /
//!    [`Queue::tx_poll`] to exchange packets—always passing
//!    `&mut dyn BufferAccess`.
//! 5. On shutdown, queues are dropped and [`Endpoint::stop`] is called.

#![expect(missing_docs)]
#![forbid(unsafe_code)]

pub mod loopback;
pub mod null;
pub mod resolve;
pub mod tests;

use async_trait::async_trait;
use futures::FutureExt;
use futures::StreamExt;
use futures::TryFutureExt;
use futures::lock::Mutex;
use futures_concurrency::future::Race;
use guestmem::GuestMemory;
use guestmem::GuestMemoryError;
use inspect::InspectMut;
use inspect_counters::Counter;
use mesh::rpc::Rpc;
use mesh::rpc::RpcSend;
use null::NullEndpoint;
use pal_async::driver::Driver;
use std::future::pending;
use std::sync::Arc;
use std::task::Context;
use std::task::Poll;
use thiserror::Error;

pub use net_backend_core::*;

/// Per-queue configuration passed to [`Endpoint::get_queues`].
///
/// Contains only an async driver handle. Receive buffers are posted
/// separately via [`Queue::rx_avail`] after queue creation.
pub struct QueueConfig {
    pub driver: Box<dyn Driver>,
}

/// A network endpoint — the backend side of a NIC.
///
/// An endpoint is a factory for [`Queue`] objects. It represents a
/// connection to some packet transport (TAP device, hardware NIC,
/// user-space network stack, etc.) and can create one or more queues
/// for parallel TX/RX processing.
///
/// Frontends (e.g. `virtio_net`, `netvsp`, `gdma`) own the endpoint
/// and call [`get_queues`](Endpoint::get_queues) when the guest
/// activates the NIC.
#[async_trait]
pub trait Endpoint: Send + Sync + InspectMut {
    /// Returns an informational endpoint type.
    fn endpoint_type(&self) -> &'static str;

    /// Initializes the queues associated with the endpoint.
    async fn get_queues(
        &mut self,
        config: Vec<QueueConfig>,
        rss: Option<&RssConfig<'_>>,
        queues: &mut Vec<Box<dyn Queue>>,
    ) -> anyhow::Result<()>;

    /// Stops the endpoint.
    ///
    /// All queues returned via `get_queues` must have been dropped.
    async fn stop(&mut self);

    /// Whether the endpoint completes buffers in the order they were made
    /// available (RX buffers returned from `rx_poll`, and TX packets completed,
    /// in available-ring order).
    fn is_ordered(&self) -> bool;

    /// Specifies the supported set of transmit offloads.
    fn tx_offload_support(&self) -> TxOffloadSupport {
        TxOffloadSupport::default()
    }

    /// Specifies parameters related to supporting multiple queues.
    fn multiqueue_support(&self) -> MultiQueueSupport {
        MultiQueueSupport {
            max_queues: 1,
            indirection_table_size: 0,
        }
    }

    /// If true, transmits are guaranteed to complete quickly. This is used to
    /// allow eliding tx notifications from the guest when there are already
    /// some tx packets in flight.
    fn tx_fast_completions(&self) -> bool {
        false
    }

    /// Sets the current data path for packet flow (e.g. via vmbus synthnic or through virtual function).
    /// This is only supported for endpoints that pair with an accelerated device.
    async fn set_data_path_to_guest_vf(&self, _use_vf: bool) -> anyhow::Result<()> {
        Err(anyhow::Error::msg("Unsupported in current endpoint"))
    }

    async fn get_data_path_to_guest_vf(&self) -> anyhow::Result<bool> {
        Err(anyhow::Error::msg("Unsupported in current endpoint"))
    }

    /// On completion, the return value indicates the specific endpoint action to take.
    async fn wait_for_endpoint_action(&mut self) -> EndpointAction {
        pending().await
    }

    /// Link speed in bps.
    fn link_speed(&self) -> u64 {
        // Reporting a reasonable default value (10Gbps) here that the individual endpoints
        // can overwrite.
        10 * 1000 * 1000 * 1000
    }
}

#[derive(Error, Debug)]
pub enum TxError {
    #[error("error requiring queue restart. {0}")]
    TryRestart(#[source] anyhow::Error),
    #[error("unrecoverable error. {0}")]
    Fatal(#[source] anyhow::Error),
}
pub trait BackendQueueStats {
    fn rx_errors(&self) -> Counter;
    fn tx_errors(&self) -> Counter;
    fn rx_packets(&self) -> Counter;
    fn tx_packets(&self) -> Counter;
    fn tx_vlan_packets(&self) -> Counter {
        Counter::new()
    }
    fn rx_vlan_packets(&self) -> Counter {
        Counter::new()
    }
}

/// A single TX/RX data path for sending and receiving network packets.
///
/// Created by [`Endpoint::get_queues`] and driven by the frontend in
/// a poll loop. Every method that touches receive buffers takes
/// `pool: &mut dyn BufferAccess` so the frontend retains ownership
/// of guest memory state.
///
/// Typical poll loop:
/// ```text
/// loop {
///     poll_ready(cx, pool)  // wait for backend events
///     rx_poll(pool, ..)     // drain completed receives
///     tx_avail(pool, ..)    // post guest TX packets
///     tx_poll(pool, ..)     // drain TX completions
/// }
/// ```
#[async_trait]
pub trait Queue: Send + InspectMut {
    /// Updates the queue's target VP.
    async fn update_target_vp(&mut self, target_vp: u32) {
        let _ = target_vp;
    }

    /// Polls the queue for readiness.
    fn poll_ready(&mut self, cx: &mut Context<'_>, pool: &mut dyn BufferAccess) -> Poll<()>;

    /// Makes receive buffers available for use by the device.
    fn rx_avail(&mut self, pool: &mut dyn BufferAccess, done: &[RxId]);

    /// Polls the device for receives.
    fn rx_poll(
        &mut self,
        pool: &mut dyn BufferAccess,
        packets: &mut [RxId],
    ) -> anyhow::Result<usize>;

    /// Posts transmits to the device.
    ///
    /// Returns `Ok(false)` if the segments will complete asynchronously.
    fn tx_avail(
        &mut self,
        pool: &mut dyn BufferAccess,
        segments: &[TxSegment],
    ) -> anyhow::Result<(bool, usize)>;

    /// Polls the device for transmit completions.
    fn tx_poll(&mut self, pool: &mut dyn BufferAccess, done: &mut [TxId])
    -> Result<usize, TxError>;

    /// Get queue statistics
    fn queue_stats(&self) -> Option<&dyn BackendQueueStats> {
        None // Default implementation - not all queues implement stats
    }
}

/// Frontend-owned access to guest receive buffers.
///
/// Each frontend implements this trait to map [`RxId`] values to
/// guest memory regions. The backend writes received packet data
/// and metadata through these methods.
///
/// The frontend owns the `BufferAccess` and passes `&mut` references
/// to [`Queue`] methods. This means no `Arc`/`Mutex` is needed
/// between the frontend and backend for buffer access—the borrow
/// checker enforces exclusive access statically.
pub trait BufferAccess {
    /// The associated guest memory accessor.
    fn guest_memory(&self) -> &GuestMemory;

    /// Writes data to the specified buffer.
    fn write_data(&mut self, id: RxId, data: &[u8]);

    /// Appends the guest address segments for the specified buffer to `buf`.
    ///
    /// Callers must clear `buf` before calling if they do not want segments
    /// from a previous call to be retained.
    fn push_guest_addresses(&self, id: RxId, buf: &mut Vec<RxBufferSegment>);

    /// The capacity of the specified buffer in bytes.
    fn capacity(&self, id: RxId) -> u32;

    /// Sets the packet metadata for the receive.
    fn write_header(&mut self, id: RxId, metadata: &RxMetadata);

    /// Writes the packet header and data in a single call.
    fn write_packet(&mut self, id: RxId, metadata: &RxMetadata, data: &[u8]) {
        self.write_data(id, data);
        self.write_header(id, metadata);
    }

    /// Writes the packet header and a payload composed of multiple
    /// discontiguous segments, in order, as a single logical packet.
    ///
    /// This allows callers to hand off a frame whose bytes are not contiguous
    /// in memory (for example, an Ethernet/IP/TCP header followed by payload
    /// that wraps a ring buffer) without first linearizing it into a scratch
    /// buffer.
    ///
    /// The default implementation copies the segments into a temporary
    /// contiguous buffer and forwards to [`BufferAccess::write_packet`].
    /// Backends that write directly into guest memory should override this to
    /// write each segment at its running offset and avoid the copy.
    fn write_packet_segments(&mut self, id: RxId, metadata: &RxMetadata, segments: &[&[u8]]) {
        if let [segment] = segments {
            self.write_packet(id, metadata, segment);
            return;
        }
        let total = segments.iter().map(|s| s.len()).sum();
        let mut data = Vec::with_capacity(total);
        for segment in segments {
            data.extend_from_slice(segment);
        }
        self.write_packet(id, metadata, &data);
    }
}

/// Linearizes the next packet in a list of segments, returning the buffer data
/// and advancing the segment list.
pub fn linearize(
    pool: &dyn BufferAccess,
    segments: &mut &[TxSegment],
) -> Result<Vec<u8>, GuestMemoryError> {
    let (head, this, rest) = next_packet(segments);
    let mut v = vec![0; head.len as usize];
    let mut offset = 0;
    let mem = pool.guest_memory();
    for segment in this {
        let dest = &mut v[offset..offset + segment.len as usize];
        mem.read_at(segment.gpa, dest)?;
        offset += segment.len as usize;
    }
    assert_eq!(v.len(), offset);
    *segments = rest;
    Ok(v)
}

enum DisconnectableEndpointUpdate {
    EndpointConnected(Box<dyn Endpoint>),
    EndpointDisconnected(Rpc<(), Option<Box<dyn Endpoint>>>),
}

pub struct DisconnectableEndpointControl {
    send_update: mesh::Sender<DisconnectableEndpointUpdate>,
    is_ordered: Option<bool>,
}

impl DisconnectableEndpointControl {
    pub fn connect(&mut self, endpoint: Box<dyn Endpoint>) -> anyhow::Result<()> {
        let new_is_ordered = endpoint.is_ordered();
        if let Some(is_ordered) = self.is_ordered {
            anyhow::ensure!(
                !is_ordered || new_is_ordered,
                "network endpoint cannot be reattached as unordered after being ordered"
            );
        } else {
            self.is_ordered = Some(new_is_ordered);
        }
        self.send_update
            .send(DisconnectableEndpointUpdate::EndpointConnected(endpoint));
        Ok(())
    }

    pub async fn disconnect(&mut self) -> anyhow::Result<Option<Box<dyn Endpoint>>> {
        self.send_update
            .call(DisconnectableEndpointUpdate::EndpointDisconnected, ())
            .map_err(anyhow::Error::from)
            .await
    }
}

pub struct DisconnectableEndpointCachedState {
    is_ordered: bool,
    tx_offload_support: TxOffloadSupport,
    multiqueue_support: MultiQueueSupport,
    tx_fast_completions: bool,
    link_speed: u64,
}

pub struct DisconnectableEndpoint {
    endpoint: Option<Box<dyn Endpoint>>,
    null_endpoint: Box<dyn Endpoint>,
    cached_state: Option<DisconnectableEndpointCachedState>,
    receive_update: Arc<Mutex<mesh::Receiver<DisconnectableEndpointUpdate>>>,
    notify_disconnect_complete: Option<(
        Rpc<(), Option<Box<dyn Endpoint>>>,
        Option<Box<dyn Endpoint>>,
    )>,
}

impl InspectMut for DisconnectableEndpoint {
    fn inspect_mut(&mut self, req: inspect::Request<'_>) {
        self.current_mut().inspect_mut(req)
    }
}

impl DisconnectableEndpoint {
    pub fn new() -> (Self, DisconnectableEndpointControl) {
        let (endpoint_tx, endpoint_rx) = mesh::channel();
        let control = DisconnectableEndpointControl {
            send_update: endpoint_tx,
            is_ordered: None,
        };
        (
            Self {
                endpoint: None,
                null_endpoint: Box::new(NullEndpoint::new()),
                cached_state: None,
                receive_update: Arc::new(Mutex::new(endpoint_rx)),
                notify_disconnect_complete: None,
            },
            control,
        )
    }

    fn current(&self) -> &dyn Endpoint {
        self.endpoint
            .as_ref()
            .unwrap_or(&self.null_endpoint)
            .as_ref()
    }

    fn current_mut(&mut self) -> &mut dyn Endpoint {
        self.endpoint
            .as_mut()
            .unwrap_or(&mut self.null_endpoint)
            .as_mut()
    }
}

#[async_trait]
impl Endpoint for DisconnectableEndpoint {
    fn endpoint_type(&self) -> &'static str {
        self.current().endpoint_type()
    }

    async fn get_queues(
        &mut self,
        config: Vec<QueueConfig>,
        rss: Option<&RssConfig<'_>>,
        queues: &mut Vec<Box<dyn Queue>>,
    ) -> anyhow::Result<()> {
        self.current_mut().get_queues(config, rss, queues).await
    }

    async fn stop(&mut self) {
        self.current_mut().stop().await
    }

    fn is_ordered(&self) -> bool {
        self.cached_state
            .as_ref()
            .expect("Endpoint needs connected at least once before use")
            .is_ordered
    }

    fn tx_offload_support(&self) -> TxOffloadSupport {
        self.cached_state
            .as_ref()
            .expect("Endpoint needs connected at least once before use")
            .tx_offload_support
    }

    fn multiqueue_support(&self) -> MultiQueueSupport {
        self.cached_state
            .as_ref()
            .expect("Endpoint needs connected at least once before use")
            .multiqueue_support
    }

    fn tx_fast_completions(&self) -> bool {
        self.cached_state
            .as_ref()
            .expect("Endpoint needs connected at least once before use")
            .tx_fast_completions
    }

    async fn set_data_path_to_guest_vf(&self, use_vf: bool) -> anyhow::Result<()> {
        self.current().set_data_path_to_guest_vf(use_vf).await
    }

    async fn get_data_path_to_guest_vf(&self) -> anyhow::Result<bool> {
        self.current().get_data_path_to_guest_vf().await
    }

    async fn wait_for_endpoint_action(&mut self) -> EndpointAction {
        // If the previous message disconnected the endpoint, notify the caller
        // that the operation has completed, returning the old endpoint.
        if let Some((rpc, old_endpoint)) = self.notify_disconnect_complete.take() {
            rpc.handle(async |_| old_endpoint).await;
        }

        enum Message {
            DisconnectableEndpointUpdate(DisconnectableEndpointUpdate),
            UpdateFromEndpoint(EndpointAction),
        }
        let receiver = self.receive_update.clone();
        let mut receive_update = receiver.lock().await;
        let update = async {
            match receive_update.next().await {
                Some(m) => Message::DisconnectableEndpointUpdate(m),
                None => {
                    pending::<()>().await;
                    unreachable!()
                }
            }
        };
        let ep_update = self
            .current_mut()
            .wait_for_endpoint_action()
            .map(Message::UpdateFromEndpoint);
        let m = (update, ep_update).race().await;
        match m {
            Message::DisconnectableEndpointUpdate(
                DisconnectableEndpointUpdate::EndpointConnected(endpoint),
            ) => {
                let old_endpoint = self.endpoint.take();
                assert!(old_endpoint.is_none());
                self.endpoint = Some(endpoint);
                let new_is_ordered = self.current().is_ordered();
                let is_ordered = if let Some(prev) = &self.cached_state {
                    assert!(
                        !prev.is_ordered || new_is_ordered,
                        "network endpoint reattached as unordered after being ordered"
                    );
                    prev.is_ordered
                } else {
                    new_is_ordered
                };
                self.cached_state = Some(DisconnectableEndpointCachedState {
                    is_ordered,
                    tx_offload_support: self.current().tx_offload_support(),
                    multiqueue_support: self.current().multiqueue_support(),
                    tx_fast_completions: self.current().tx_fast_completions(),
                    link_speed: self.current().link_speed(),
                });
                EndpointAction::RestartRequired
            }
            Message::DisconnectableEndpointUpdate(
                DisconnectableEndpointUpdate::EndpointDisconnected(rpc),
            ) => {
                let old_endpoint = self.endpoint.take();
                // Wait until the next call into this function to notify the
                // caller that the operation has completed. This makes it more
                // likely that the endpoint is no longer referenced (old queues
                // have been disposed, etc.).
                self.notify_disconnect_complete = Some((rpc, old_endpoint));
                EndpointAction::RestartRequired
            }
            Message::UpdateFromEndpoint(update) => update,
        }
    }

    fn link_speed(&self) -> u64 {
        self.cached_state
            .as_ref()
            .expect("Endpoint needs connected at least once before use")
            .link_speed
    }
}

#[cfg(test)]
mod disconnectable_endpoint_tests {
    use super::*;
    use test_with_tracing::test;

    #[derive(InspectMut)]
    struct TestEndpoint {
        is_ordered: bool,
    }

    #[async_trait]
    impl Endpoint for TestEndpoint {
        fn endpoint_type(&self) -> &'static str {
            "test"
        }

        async fn get_queues(
            &mut self,
            _config: Vec<QueueConfig>,
            _rss: Option<&RssConfig<'_>>,
            _queues: &mut Vec<Box<dyn Queue>>,
        ) -> anyhow::Result<()> {
            unreachable!()
        }

        async fn stop(&mut self) {
            unreachable!()
        }

        fn is_ordered(&self) -> bool {
            self.is_ordered
        }
    }

    #[test]
    fn connect_pins_endpoint_ordering() {
        let (_endpoint, mut control) = DisconnectableEndpoint::new();
        control
            .connect(Box::new(TestEndpoint { is_ordered: true }))
            .unwrap();

        let err = control
            .connect(Box::new(TestEndpoint { is_ordered: false }))
            .unwrap_err();
        assert_eq!(
            err.to_string(),
            "network endpoint cannot be reattached as unordered after being ordered"
        );

        let (_endpoint, mut control) = DisconnectableEndpoint::new();
        control
            .connect(Box::new(TestEndpoint { is_ordered: false }))
            .unwrap();
        control
            .connect(Box::new(TestEndpoint { is_ordered: true }))
            .unwrap();
    }
}
