// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! This module defines a trait and implementations thereof for network
//! backends.

#![expect(missing_docs)]
#![forbid(unsafe_code)]

pub mod loopback;
pub mod null;
pub mod resolve;
pub mod tests;

use async_trait::async_trait;
use bitfield_struct::bitfield;
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

/// Per-queue configuration.
pub struct QueueConfig<'a> {
    pub pool: Box<dyn BufferAccess>,
    pub initial_rx: &'a [RxId],
    pub driver: Box<dyn Driver>,
}

/// A network endpoint.
#[async_trait]
pub trait Endpoint: Send + Sync + InspectMut {
    /// Returns an informational endpoint type.
    fn endpoint_type(&self) -> &'static str;

    /// Initializes the queues associated with the endpoint.
    ///
    /// `initial_rx` contains the initial set of receives buffers that are
    /// available.
    async fn get_queues(
        &mut self,
        config: Vec<QueueConfig<'_>>,
        rss: Option<&RssConfig<'_>>,
        queues: &mut Vec<Box<dyn Queue>>,
    ) -> anyhow::Result<()>;

    /// Stops the endpoint.
    ///
    /// All queues returned via `get_queues` must have been dropped.
    async fn stop(&mut self);

    /// Specifies whether packets are always completed in order.
    fn is_ordered(&self) -> bool {
        false
    }

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

/// Multi-queue related support.
#[derive(Debug, Copy, Clone)]
pub struct MultiQueueSupport {
    /// The number of supported queues.
    pub max_queues: u16,
    /// The size of the RSS indirection table.
    pub indirection_table_size: u16,
}

/// The set of supported transmit offloads.
#[derive(Debug, Copy, Clone, Default)]
pub struct TxOffloadSupport {
    /// IPv4 header checksum offload.
    pub ipv4_header: bool,
    /// TCP checksum offload.
    pub tcp: bool,
    /// UDP checksum offload.
    pub udp: bool,
    /// TCP segmentation offload.
    pub tso: bool,
}

#[derive(Debug, Clone)]
pub struct RssConfig<'a> {
    pub key: &'a [u8],
    pub indirection_table: &'a [u16],
    pub flags: u32, // TODO
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
}

/// A trait for sending and receiving network packets.
#[async_trait]
pub trait Queue: Send + InspectMut {
    /// Updates the queue's target VP.
    async fn update_target_vp(&mut self, target_vp: u32) {
        let _ = target_vp;
    }

    /// Polls the queue for readiness.
    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<()>;

    /// Makes receive buffers available for use by the device.
    fn rx_avail(&mut self, done: &[RxId]);

    /// Polls the device for receives.
    fn rx_poll(&mut self, packets: &mut [RxId]) -> anyhow::Result<usize>;

    /// Posts transmits to the device.
    ///
    /// Returns `Ok(false)` if the segments will complete asynchronously.
    fn tx_avail(&mut self, segments: &[TxSegment]) -> anyhow::Result<(bool, usize)>;

    /// Polls the device for transmit completions.
    fn tx_poll(&mut self, done: &mut [TxId]) -> Result<usize, TxError>;

    /// Get the buffer access.
    fn buffer_access(&mut self) -> Option<&mut dyn BufferAccess>;

    /// Get queue statistics
    fn queue_stats(&self) -> Option<&dyn BackendQueueStats> {
        None // Default implementation - not all queues implement stats
    }
}

/// A trait for providing access to guest memory buffers.
pub trait BufferAccess: 'static + Send {
    /// The associated guest memory accessor.
    fn guest_memory(&self) -> &GuestMemory;

    /// Writes data to the specified buffer.
    fn write_data(&mut self, id: RxId, data: &[u8]);

    /// The guest addresses of the specified buffer.
    fn guest_addresses(&mut self, id: RxId) -> &[RxBufferSegment];

    /// The capacity of the specified buffer in bytes.
    fn capacity(&self, id: RxId) -> u32;

    /// Sets the packet metadata for the receive.
    fn write_header(&mut self, id: RxId, metadata: &RxMetadata);

    /// Writes the packet header and data in a single call.
    fn write_packet(&mut self, id: RxId, metadata: &RxMetadata, data: &[u8]) {
        self.write_data(id, data);
        self.write_header(id, metadata);
    }
}

/// A receive buffer ID.
#[derive(Debug, Copy, Clone)]
#[repr(transparent)]
pub struct RxId(pub u32);

/// An individual segment in guest memory of a receive buffer.
#[derive(Debug, Copy, Clone)]
pub struct RxBufferSegment {
    /// Guest physical address.
    pub gpa: u64,
    /// The number of bytes in this range.
    pub len: u32,
}

/// Receive packet metadata.
#[derive(Debug, Copy, Clone)]
pub struct RxMetadata {
    /// The offset of the packet data from the beginning of the receive buffer.
    pub offset: usize,
    /// The length of the packet in bytes.
    pub len: usize,
    /// The IP checksum validation state.
    pub ip_checksum: RxChecksumState,
    /// The L4 checksum validation state.
    pub l4_checksum: RxChecksumState,
    /// The L4 protocol.
    pub l4_protocol: L4Protocol,
}

impl Default for RxMetadata {
    fn default() -> Self {
        Self {
            offset: 0,
            len: 0,
            ip_checksum: RxChecksumState::Unknown,
            l4_checksum: RxChecksumState::Unknown,
            l4_protocol: L4Protocol::Unknown,
        }
    }
}

/// The "L3" protocol: the IP layer.
#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub enum L3Protocol {
    Unknown,
    Ipv4,
    Ipv6,
}

/// The "L4" protocol: the TCP/UDP layer.
#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub enum L4Protocol {
    Unknown,
    Tcp,
    Udp,
}

/// The receive checksum state for a packet.
#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub enum RxChecksumState {
    /// The checksum was not evaluated.
    Unknown,
    /// The checksum value is correct.
    Good,
    /// The checksum value is incorrect.
    Bad,
    /// The checksum has been validated, but the value in the header is wrong.
    ///
    /// This occurs when LRO/RSC offload has been performed--multiple packet
    /// payloads are glommed together without updating the checksum in the first
    /// packet's header.
    ValidatedButWrong,
}

impl RxChecksumState {
    /// Returns true if the checksum has been validated.
    pub fn is_valid(self) -> bool {
        self == Self::Good || self == Self::ValidatedButWrong
    }
}

/// A transmit ID. This may be used by multiple segments at the same time.
#[derive(Debug, Copy, Clone)]
#[repr(transparent)]
pub struct TxId(pub u32);

#[derive(Debug, Clone)]
/// The segment type.
pub enum TxSegmentType {
    /// The start of a packet.
    Head(TxMetadata),
    /// A packet continuation.
    Tail,
}

#[derive(Debug, Clone)]
/// Transmit packet metadata.
pub struct TxMetadata {
    /// The transmit ID.
    pub id: TxId,
    /// The number of segments, including this one.
    pub segment_count: u8,
    /// Flags.
    pub flags: TxFlags,
    /// The total length of the packet in bytes.
    pub len: u32,
    /// The length of the Ethernet frame header. Only guaranteed to be set if
    /// various offload flags are set.
    pub l2_len: u8,
    /// The length of the IP header. Only guaranteed to be set if various
    /// offload flags are set.
    pub l3_len: u16,
    /// The length of the TCP header. Only guaranteed to be set if various
    /// offload flags are set.
    pub l4_len: u8,
    /// The maximum TCP segment size, used for segmentation. Only guaranteed to
    /// be set if [`TxFlags::offload_tcp_segmentation`] is set.
    pub max_tcp_segment_size: u16,
}

/// Flags affecting transmit behavior.
#[bitfield(u8)]
pub struct TxFlags {
    /// Offload IPv4 header checksum calculation.
    ///
    /// `l3_protocol`, `l2_len`, and `l3_len` must be set.
    pub offload_ip_header_checksum: bool,
    /// Offload the TCP checksum calculation.
    ///
    /// `l3_protocol`, `l2_len`, and `l3_len` must be set.
    pub offload_tcp_checksum: bool,
    /// Offload the UDP checksum calculation.
    ///
    /// `l3_protocol`, `l2_len`, and `l3_len` must be set.
    pub offload_udp_checksum: bool,
    /// Offload the TCP segmentation, allowing packets to be larger than the
    /// MTU.
    ///
    /// `l3_protocol`, `l2_len`, `l3_len`, `l4_len`, and `tcp_segment_size` must
    /// be set.
    pub offload_tcp_segmentation: bool,
    /// If true, the packet is IPv4.
    pub is_ipv4: bool,
    /// If true, the packet is IPv6. Mutually exclusive with `is_ipv4`.
    pub is_ipv6: bool,
    #[bits(2)]
    _reserved: u8,
}

impl Default for TxMetadata {
    fn default() -> Self {
        Self {
            id: TxId(0),
            segment_count: 0,
            len: 0,
            flags: TxFlags::new(),
            l2_len: 0,
            l3_len: 0,
            l4_len: 0,
            max_tcp_segment_size: 0,
        }
    }
}

#[derive(Debug, Clone)]
/// A transmit packet segment.
pub struct TxSegment {
    /// The segment type (head or tail).
    pub ty: TxSegmentType,
    /// The guest address of this segment.
    pub gpa: u64,
    /// The length of this segment.
    pub len: u32,
}

/// Computes the number of packets in `segments`.
pub fn packet_count(mut segments: &[TxSegment]) -> usize {
    let mut packet_count = 0;
    while let Some(head) = segments.first() {
        let TxSegmentType::Head(metadata) = &head.ty else {
            unreachable!()
        };
        segments = &segments[metadata.segment_count as usize..];
        packet_count += 1;
    }
    packet_count
}

/// Gets the next packet from a list of segments, returning the packet metadata,
/// the segments in the packet, and the remaining segments.
pub fn next_packet(segments: &[TxSegment]) -> (&TxMetadata, &[TxSegment], &[TxSegment]) {
    let metadata = if let TxSegmentType::Head(metadata) = &segments[0].ty {
        metadata
    } else {
        unreachable!();
    };
    let (this, rest) = segments.split_at(metadata.segment_count.into());
    (metadata, this, rest)
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

#[derive(PartialEq, Debug)]
pub enum EndpointAction {
    RestartRequired,
    LinkStatusNotify(bool),
}

enum DisconnectableEndpointUpdate {
    EndpointConnected(Box<dyn Endpoint>),
    EndpointDisconnected(Rpc<(), Option<Box<dyn Endpoint>>>),
}

pub struct DisconnectableEndpointControl {
    send_update: mesh::Sender<DisconnectableEndpointUpdate>,
}

impl DisconnectableEndpointControl {
    pub fn connect(&mut self, endpoint: Box<dyn Endpoint>) -> anyhow::Result<()> {
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
        config: Vec<QueueConfig<'_>>,
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
                self.cached_state = Some(DisconnectableEndpointCachedState {
                    is_ordered: self.current().is_ordered(),
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
