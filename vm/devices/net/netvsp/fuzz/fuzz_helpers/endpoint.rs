// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Fuzzable network endpoint and queue implementations.
//!
//! [`FuzzEndpoint`] wraps a [`LoopbackEndpoint`] with additional
//! fuzzer-controlled capabilities: RX packet injection, endpoint action
//! injection (link status changes), and TX error injection.

use arbitrary::Arbitrary;
use async_trait::async_trait;
use inspect::InspectMut;
use net_backend::Endpoint;
use net_backend::EndpointAction;
use net_backend::L4Protocol;
use net_backend::MultiQueueSupport;
use net_backend::Queue as BackendQueue;
use net_backend::QueueConfig;
use net_backend::RssConfig;
use net_backend::RxChecksumState;
use net_backend::RxId;
use net_backend::RxMetadata;
use net_backend::TxError;
use net_backend::TxId;
use net_backend::TxOffloadSupport;
use net_backend::TxSegment;
use net_backend::TxSegmentType;
use net_backend::linearize;
use net_backend::loopback::LoopbackEndpoint;
use std::collections::VecDeque;
use std::sync::Arc;
use std::sync::atomic::AtomicU8;
use std::sync::atomic::Ordering;
use std::task::Context;
use std::task::Poll;

/// Fuzzer-controllable receive metadata. Allows the fuzzer to exercise all
/// checksum-reporting branches in `write_header()` (48 combinations of IP
/// checksum state × L4 protocol × L4 checksum state).
#[derive(Clone, Copy, Debug, Arbitrary)]
pub enum FuzzRxChecksumState {
    Unknown,
    Good,
    Bad,
    ValidatedButWrong,
}

impl From<FuzzRxChecksumState> for RxChecksumState {
    fn from(v: FuzzRxChecksumState) -> Self {
        match v {
            FuzzRxChecksumState::Unknown => RxChecksumState::Unknown,
            FuzzRxChecksumState::Good => RxChecksumState::Good,
            FuzzRxChecksumState::Bad => RxChecksumState::Bad,
            FuzzRxChecksumState::ValidatedButWrong => RxChecksumState::ValidatedButWrong,
        }
    }
}

/// Fuzzer-controllable L4 protocol selection.
#[derive(Clone, Copy, Debug, Arbitrary)]
pub enum FuzzL4Protocol {
    Unknown,
    Tcp,
    Udp,
}

impl From<FuzzL4Protocol> for L4Protocol {
    fn from(v: FuzzL4Protocol) -> Self {
        match v {
            FuzzL4Protocol::Unknown => L4Protocol::Unknown,
            FuzzL4Protocol::Tcp => L4Protocol::Tcp,
            FuzzL4Protocol::Udp => L4Protocol::Udp,
        }
    }
}

/// Fuzzer-driven RX metadata. Use this in fuzz actions that inject RX packets
/// to exercise all checksum-flag branches in the NIC's `write_header()`.
#[derive(Clone, Copy, Debug, Arbitrary)]
pub struct FuzzRxMetadata {
    pub ip_checksum: FuzzRxChecksumState,
    pub l4_checksum: FuzzRxChecksumState,
    pub l4_protocol: FuzzL4Protocol,
}

impl FuzzRxMetadata {
    /// Convert to `RxMetadata` using the given packet length.
    pub fn to_rx_metadata(self, packet_len: usize) -> RxMetadata {
        RxMetadata {
            offset: 0,
            len: packet_len,
            ip_checksum: self.ip_checksum.into(),
            l4_checksum: self.l4_checksum.into(),
            l4_protocol: self.l4_protocol.into(),
        }
    }
}

impl Default for FuzzRxMetadata {
    fn default() -> Self {
        Self {
            ip_checksum: FuzzRxChecksumState::Unknown,
            l4_checksum: FuzzRxChecksumState::Unknown,
            l4_protocol: FuzzL4Protocol::Unknown,
        }
    }
}

/// Configuration for creating a [`FuzzEndpoint`].
#[derive(Default)]
pub struct FuzzEndpointConfig {
    /// Whether to enable RX packet injection via a channel.
    pub enable_rx_injection: bool,
    /// Whether to enable endpoint action injection (link status, restart).
    pub enable_action_injection: bool,
    /// When true, `set_data_path_to_guest_vf()` returns `Err`, exercising
    /// the `DataPathSynthetic` fallback state.  When false, it succeeds,
    /// exercising the `DataPathSwitched` state.  Default: false (succeeds).
    pub fail_vf_switch: bool,
    /// When true, creates a shared `Arc<AtomicU8>` for TX error injection.
    /// The fuzz loop can set the atomic to 1 (`TxError::TryRestart`) or
    /// 2 (`TxError::Fatal`) to exercise error handling in
    /// `process_endpoint_tx`.  Default: false.
    pub enable_tx_error_injection: bool,
    /// When true, `tx_avail` returns `(false, sent)` (asynchronous TX),
    /// and completed `TxId`s are returned via `tx_poll`. This exercises
    /// the `process_endpoint_tx` → `complete_tx_packet` path and
    /// `reset_tx_after_endpoint_stop` cleanup, which are unreachable
    /// when TX is always synchronous.  Default: false (synchronous TX).
    pub enable_async_tx: bool,
}

/// Handles returned to the fuzz loop for injecting events into the endpoint.
pub struct FuzzEndpointHandles {
    /// Send RX packets (with metadata) to be delivered by the endpoint.
    /// Only present if `enable_rx_injection` was set.
    pub rx_send: Option<mesh::Sender<(Vec<u8>, FuzzRxMetadata)>>,
    /// Send endpoint actions (link status, restart) to be delivered.
    /// Only present if `enable_action_injection` was set.
    pub action_send: Option<mesh::Sender<EndpointAction>>,
    /// Shared atomic controlling TX error injection in `FuzzableQueue::tx_poll`.
    /// Set to 0 (normal), 1 (`TxError::TryRestart`), or 2 (`TxError::Fatal`).
    /// Only present if `enable_tx_error_injection` was set.
    pub tx_error_mode: Option<Arc<AtomicU8>>,
}

/// A network endpoint wrapping [`LoopbackEndpoint`] with additional
/// fuzzer-controlled capabilities:
/// - RX packet injection via a channel
/// - EndpointAction injection (link status changes, restart signals)
#[derive(InspectMut)]
#[inspect(skip)]
pub struct FuzzEndpoint {
    inner: LoopbackEndpoint,
    rx_recv: Option<mesh::Receiver<(Vec<u8>, FuzzRxMetadata)>>,
    action_recv: Option<mesh::Receiver<EndpointAction>>,
    /// Metadata template applied to loopback (TX→RX) packets.
    /// Configurable per-endpoint so that all loopback traffic also exercises
    /// varied checksum branches in `write_header()`.
    pub loopback_metadata: FuzzRxMetadata,
    /// When true, `set_data_path_to_guest_vf()` returns an error.
    fail_vf_switch: bool,
    /// Shared atomic for TX error injection.  Cloned into each
    /// `FuzzableQueue` during `get_queues()`.
    tx_error_mode: Option<Arc<AtomicU8>>,
    /// When true, `FuzzableQueue` uses asynchronous TX completions.
    async_tx: bool,
}

#[derive(InspectMut)]
#[inspect(skip)]
struct FuzzableQueue {
    pool: Box<dyn net_backend::BufferAccess>,
    rx_avail: VecDeque<RxId>,
    rx_done: VecDeque<RxId>,
    rx_recv: Option<mesh::Receiver<(Vec<u8>, FuzzRxMetadata)>>,
    pending_injected: VecDeque<(Vec<u8>, FuzzRxMetadata)>,
    /// Metadata template for loopback (TX→RX) packets.
    loopback_metadata: FuzzRxMetadata,
    /// Shared atomic for TX error injection.  When non-zero,
    /// `tx_poll` returns the corresponding `TxError` variant.
    tx_error_mode: Option<Arc<AtomicU8>>,
    /// When false, `tx_avail` returns `(false, sent)` and completed
    /// `TxId`s are stored for retrieval via `tx_poll`.
    sync_tx: bool,
    /// Completed TX packet IDs pending retrieval via `tx_poll`.
    /// Only populated when `sync_tx` is false.
    pending_tx_completions: VecDeque<TxId>,
}

impl FuzzableQueue {
    fn ingest_injected_packets(&mut self) {
        if let Some(rx_recv) = &mut self.rx_recv {
            while let Ok(entry) = rx_recv.try_recv() {
                self.pending_injected.push_back(entry);
            }
        }
    }

    fn materialize_injected_packets(&mut self) {
        self.ingest_injected_packets();
        while !self.pending_injected.is_empty() && !self.rx_avail.is_empty() {
            let Some((packet, fuzz_meta)) = self.pending_injected.pop_front() else {
                break;
            };
            let rx_id = self
                .rx_avail
                .pop_front()
                .expect("checked non-empty rx_avail");
            // Use the fuzzer-provided metadata so that the NIC's RX path
            // exercises all checksum-flag branches in `write_header()`.
            let metadata = fuzz_meta.to_rx_metadata(packet.len());
            self.pool.write_packet(rx_id, &metadata, &packet);
            self.rx_done.push_back(rx_id);
        }
    }
}

impl BackendQueue for FuzzableQueue {
    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<()> {
        self.materialize_injected_packets();
        if !self.rx_done.is_empty() || !self.pending_tx_completions.is_empty() {
            return Poll::Ready(());
        }
        if let Some(rx_recv) = &mut self.rx_recv {
            // Poll the receiver to register the waker for new injected
            // RX messages.  When a new packet arrives, the executor
            // re-polls this queue.
            match rx_recv.poll_recv(cx) {
                Poll::Ready(Ok(entry)) => {
                    self.pending_injected.push_back(entry);
                    self.materialize_injected_packets();
                    if !self.rx_done.is_empty() {
                        return Poll::Ready(());
                    }
                }
                Poll::Ready(Err(_)) => {} // channel closed
                Poll::Pending => {}       // waker registered
            }
        }
        // When `rx_recv` is None, no external waker needed:
        // Worker drives `tx_avail` → `rx_avail` → `poll_ready`
        Poll::Pending
    }

    fn rx_avail(&mut self, done: &[RxId]) {
        self.rx_avail.extend(done.iter().copied());
        self.materialize_injected_packets();
    }

    fn rx_poll(&mut self, packets: &mut [RxId]) -> anyhow::Result<usize> {
        self.materialize_injected_packets();
        let n = packets.len().min(self.rx_done.len());
        for (d, s) in packets.iter_mut().zip(self.rx_done.drain(..n)) {
            *d = s;
        }
        Ok(n)
    }

    fn tx_avail(&mut self, mut segments: &[TxSegment]) -> anyhow::Result<(bool, usize)> {
        let mut sent = 0;
        let original_segments = segments;
        // Consume all segments to prevent the NIC worker retry the same segments.
        while !segments.is_empty() {
            let before = segments.len();
            let packet = linearize(self.pool.as_ref(), &mut segments)?;
            sent += before - segments.len();
            if let Some(rx_id) = self.rx_avail.pop_front() {
                // Use the loopback metadata template so that TX→RX loopback
                // packets exercise varied checksum branches in `write_header()`.
                let metadata = self.loopback_metadata.to_rx_metadata(packet.len());
                self.pool.write_packet(rx_id, &metadata, &packet);
                self.rx_done.push_back(rx_id);
            }
            // else: RX pool exhausted — packet is discarded.
        }

        if !self.sync_tx {
            // Extract TxIds from consumed segments for async completion.
            for seg in &original_segments[..sent] {
                if let TxSegmentType::Head(metadata) = &seg.ty {
                    self.pending_tx_completions.push_back(metadata.id);
                }
            }
        }

        self.materialize_injected_packets();
        Ok((self.sync_tx, sent))
    }

    fn tx_poll(&mut self, done: &mut [TxId]) -> Result<usize, TxError> {
        if let Some(mode) = &self.tx_error_mode {
            match mode.swap(0, Ordering::Relaxed) {
                1 => {
                    return Err(TxError::TryRestart(anyhow::anyhow!(
                        "fuzz: injected TxError::TryRestart"
                    )));
                }
                2 => {
                    return Err(TxError::Fatal(anyhow::anyhow!(
                        "fuzz: injected TxError::Fatal"
                    )));
                }
                _ => {}
            }
        }
        // Drain pending async TX completions.
        let n = done.len().min(self.pending_tx_completions.len());
        for (d, s) in done.iter_mut().zip(self.pending_tx_completions.drain(..n)) {
            *d = s;
        }
        Ok(n)
    }

    fn buffer_access(&mut self) -> Option<&mut dyn net_backend::BufferAccess> {
        Some(self.pool.as_mut())
    }
}

impl FuzzEndpoint {
    /// Create a new fuzzable endpoint and its control handles.
    pub fn new(config: FuzzEndpointConfig) -> (Self, FuzzEndpointHandles) {
        let (rx_send, rx_recv) = if config.enable_rx_injection {
            let (s, r) = mesh::channel();
            (Some(s), Some(r))
        } else {
            (None, None)
        };

        let (action_send, action_recv) = if config.enable_action_injection {
            let (s, r) = mesh::channel();
            (Some(s), Some(r))
        } else {
            (None, None)
        };

        let tx_error_mode = if config.enable_tx_error_injection {
            Some(Arc::new(AtomicU8::new(0)))
        } else {
            None
        };

        (
            Self {
                inner: LoopbackEndpoint::new(),
                rx_recv,
                action_recv,
                loopback_metadata: FuzzRxMetadata::default(),
                fail_vf_switch: config.fail_vf_switch,
                tx_error_mode: tx_error_mode.clone(),
                async_tx: config.enable_async_tx,
            },
            FuzzEndpointHandles {
                rx_send,
                action_send,
                tx_error_mode,
            },
        )
    }
}

#[async_trait]
impl Endpoint for FuzzEndpoint {
    fn endpoint_type(&self) -> &'static str {
        "fuzzable"
    }

    fn tx_offload_support(&self) -> TxOffloadSupport {
        TxOffloadSupport {
            ipv4_header: true,
            tcp: true,
            udp: true,
            tso: true,
        }
    }

    async fn get_queues(
        &mut self,
        config: Vec<QueueConfig<'_>>,
        rss: Option<&RssConfig<'_>>,
        queues: &mut Vec<Box<dyn BackendQueue>>,
    ) -> anyhow::Result<()> {
        if self.rx_recv.is_none() && self.tx_error_mode.is_none() && !self.async_tx {
            return self.inner.get_queues(config, rss, queues).await;
        }

        // RSS is intentionally not applied to FuzzableQueue.
        let _ = rss;
        let loopback_metadata = self.loopback_metadata;
        let tx_error_mode = self.tx_error_mode.clone();
        let sync_tx = !self.async_tx;
        queues.extend(config.into_iter().enumerate().map(|(idx, config)| {
            let rx_recv = if idx == 0 { self.rx_recv.take() } else { None };
            Box::new(FuzzableQueue {
                pool: config.pool,
                rx_avail: config.initial_rx.to_vec().into(),
                rx_done: VecDeque::new(),
                rx_recv,
                pending_injected: VecDeque::new(),
                loopback_metadata,
                tx_error_mode: tx_error_mode.clone(),
                sync_tx,
                pending_tx_completions: VecDeque::new(),
            }) as Box<dyn BackendQueue>
        }));
        Ok(())
    }

    async fn stop(&mut self) {
        self.inner.stop().await
    }

    fn is_ordered(&self) -> bool {
        self.inner.is_ordered()
    }

    fn multiqueue_support(&self) -> MultiQueueSupport {
        self.inner.multiqueue_support()
    }

    async fn set_data_path_to_guest_vf(&self, _use_vf: bool) -> anyhow::Result<()> {
        if self.fail_vf_switch {
            Err(anyhow::anyhow!("fuzz: simulated VF switch failure"))
        } else {
            Ok(())
        }
    }

    async fn wait_for_endpoint_action(&mut self) -> EndpointAction {
        if let Some(recv) = &mut self.action_recv {
            match recv.recv().await {
                Ok(action) => action,
                Err(_) => std::future::pending().await,
            }
        } else {
            std::future::pending().await
        }
    }
}
