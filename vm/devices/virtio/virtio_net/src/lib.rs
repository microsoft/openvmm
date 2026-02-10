// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

#![expect(missing_docs)]
#![forbid(unsafe_code)]

mod buffers;
pub mod resolver;

// use anyhow::Context;
use crate::buffers::VirtioWorkPool;
use bitfield_struct::bitfield;
use futures::FutureExt;
use futures::StreamExt;
use futures_concurrency::future::Race;
use guestmem::GuestMemory;
use inspect::Inspect;
use inspect::InspectMut;
use inspect_counters::Counter;
use inspect_counters::Histogram;
use net_backend::Endpoint;
use net_backend::EndpointAction;
use net_backend::QueueConfig;
use net_backend::RxId;
use net_backend::TxId;
use net_backend::TxMetadata;
use net_backend::TxSegment;
use net_backend::TxSegmentType;
use net_backend_resources::mac_address::MacAddress;
use pal_async::wait::PolledWait;
use std::future::pending;
use std::mem::offset_of;
use std::sync::Arc;
use std::task::Poll;
use task_control::AsyncRun;
use task_control::InspectTaskMut;
use task_control::StopTask;
use task_control::TaskControl;
use thiserror::Error;
use virtio::DeviceTraits;
use virtio::DeviceTraitsSharedMemory;
use virtio::Resources;
use virtio::VirtioDevice;
use virtio::VirtioQueue;
use virtio::VirtioQueueCallbackWork;
use virtio::spec::VirtioDeviceFeatures;
use vmcore::vm_task::VmTaskDriver;
use vmcore::vm_task::VmTaskDriverSource;
use zerocopy::FromBytes;
use zerocopy::Immutable;
use zerocopy::IntoBytes;
use zerocopy::KnownLayout;

// These correspond to VIRTIO_NET_F_ flags.
#[bitfield(u32)]
#[derive(IntoBytes, Immutable, KnownLayout, FromBytes)]
struct NetworkFeaturesBank0 {
    pub csum: bool,
    pub guest_csum: bool,
    pub ctrl_guest_offloads: bool,
    pub mtu: bool,
    _reserved: bool,
    pub mac: bool,
    _reserved2: bool,
    pub guest_tso4: bool,
    pub guest_tso6: bool,
    pub guest_ecn: bool,
    pub guest_ufo: bool,
    pub host_tso4: bool,
    pub host_tso6: bool,
    pub host_ecn: bool,
    pub host_ufo: bool,
    pub mrg_rxbuf: bool,
    pub status: bool,
    pub ctrl_vq: bool,
    pub ctrl_rx: bool,
    pub ctrl_vlan: bool,
    _reserved3: bool,
    pub guest_announce: bool,
    pub mq: bool,
    pub ctrl_mac_addr: bool,
    #[bits(8)]
    _unavailable: u8,
}
#[bitfield(u32)]
#[derive(IntoBytes, Immutable, KnownLayout, FromBytes)]
struct NetworkFeaturesBank1 {
    #[bits(18)]
    _unused: u32,
    pub device_stats: bool, // VIRTIO_NET_F_DEVICE_STATS(50)
    pub hash_tunnel: bool,
    pub vq_notf_coal: bool,
    pub notf_coal: bool,
    pub guest_uso4: bool,
    pub guest_uso6: bool,
    pub host_uso: bool,
    pub hash_report: bool,
    _reserved: bool,
    pub guest_hdrlen: bool,
    pub rss: bool,
    pub rsc_ext: bool,
    pub standby: bool,
    pub speed_duplex: bool,
}
#[bitfield(u32)]
#[derive(IntoBytes, Immutable, KnownLayout, FromBytes)]
struct NetworkFeaturesBank2 {
    pub rss_context: bool, // VIRTIO_NET_F_RSS_CONTEXT(64)
    pub guest_udp_tunnel_gso: bool,
    pub guest_udp_tunnel_gso_csum: bool,
    pub host_udp_tunnel_gso: bool,
    pub host_udp_tunnel_gso_csum: bool,
    pub out_net_header: bool,
    pub ipsec: bool,
    #[bits(25)]
    _unused: u32,
}

// These correspond to VIRTIO_NET_S_ flags.
#[bitfield(u16)]
#[derive(IntoBytes, Immutable, KnownLayout, FromBytes)]
struct NetStatus {
    pub link_up: bool,
    pub announce: bool,
    #[bits(14)]
    _reserved: u16,
}

const DEFAULT_MTU: u16 = 1514;

#[expect(dead_code)]
const VIRTIO_NET_MAX_QUEUES: u16 = 0x8000;

#[repr(C)]
struct NetConfig {
    pub mac: [u8; 6],
    pub status: u16,
    pub max_virtqueue_pairs: u16,
    pub mtu: u16,
    pub speed: u32,                            // MBit/s; 0xffffffff - unknown speed
    pub duplex: u8,                            // 0 - half, 1 - full, 0xff - unknown
    pub rss_max_key_size: u8,                  // VIRTIO_NET_F_RSS or VIRTIO_NET_F_HASH_REPORT
    pub rss_max_indirection_table_length: u16, // VIRTIO_NET_F_RSS
    pub supported_hash_types: u32,             // VIRTIO_NET_F_RSS or VIRTIO_NET_F_HASH_REPORT
}

// These correspond to VIRTIO_NET_HDR_F_ flags.
#[bitfield(u8)]
#[derive(IntoBytes, Immutable, KnownLayout, FromBytes)]
struct VirtioNetHeaderFlags {
    pub needs_csum: bool,
    pub data_valid: bool,
    pub rsc_info: bool,
    #[bits(5)]
    _reserved: u8,
}

#[bitfield(u8)]
#[derive(IntoBytes, Immutable, KnownLayout, FromBytes)]
struct VirtioNetHeaderGso {
    #[bits(3)]
    pub protocol: VirtioNetHeaderGsoProtocol,
    #[bits(4)]
    _reserved: u8,
    pub ecn: bool,
}

// These correspond to VIRTIO_NET_HDR_GSO_ values.
open_enum::open_enum! {
    #[derive(IntoBytes, Immutable, KnownLayout, FromBytes)]
    enum VirtioNetHeaderGsoProtocol: u8 {
        NONE = 0,
        TCPV4 = 1,
        UDP = 3,
        TCPV6 = 4,
        UDP_L4 = 5,
    }
}

impl VirtioNetHeaderGsoProtocol {
    const fn from_bits(bits: u8) -> Self {
        Self(bits)
    }

    const fn into_bits(self) -> u8 {
        self.0
    }
}

#[derive(IntoBytes, Immutable, KnownLayout, FromBytes)]
#[repr(C)]
struct VirtioNetHeader {
    pub flags: u8,
    pub gso_type: u8,
    pub hdr_len: u16,
    pub gso_size: u16,
    pub csum_start: u16,
    pub csum_offset: u16,
    pub num_buffers: u16,
    pub hash_value: u32,       // Only if VIRTIO_NET_F_HASH_REPORT negotiated
    pub hash_report: u16,      // Only if VIRTIO_NET_F_HASH_REPORT negotiated
    pub padding_reserved: u16, // Only if VIRTIO_NET_F_HASH_REPORT negotiated
}

fn header_size() -> usize {
    // TODO: Verify hash flags are not set, since header size would be larger in that case.
    offset_of!(VirtioNetHeader, hash_value)
}

struct Adapter {
    driver: VmTaskDriver,
    max_queues: u16,
    tx_fast_completions: bool,
    mac_address: MacAddress,
}

pub struct Device {
    registers: NetConfig,
    memory: GuestMemory,
    coordinator: TaskControl<CoordinatorState, Coordinator>,
    coordinator_send: Option<mesh::Sender<CoordinatorMessage>>,
    adapter: Arc<Adapter>,
    driver_source: VmTaskDriverSource,
}

impl Drop for Device {
    fn drop(&mut self) {}
}

impl VirtioDevice for Device {
    fn traits(&self) -> DeviceTraits {
        // TODO: Add network features based on endpoint capabilities (NetworkFeatures::VIRTIO_NET_F_*)
        DeviceTraits {
            device_id: 1,
            device_features: VirtioDeviceFeatures::new()
                .with_bank(0, NetworkFeaturesBank0::new().with_mac(true).into_bits()),
            max_queues: 2 * self.registers.max_virtqueue_pairs,
            device_register_length: size_of::<NetConfig>() as u32,
            shared_memory: DeviceTraitsSharedMemory { id: 0, size: 0 },
        }
    }

    fn read_registers_u32(&self, offset: u16) -> u32 {
        match offset {
            0 => u32::from_le_bytes(self.registers.mac[..4].try_into().unwrap()),
            4 => {
                (u16::from_le_bytes(self.registers.mac[4..].try_into().unwrap()) as u32)
                    | ((self.registers.status as u32) << 16)
            }
            8 => (self.registers.max_virtqueue_pairs as u32) | ((self.registers.mtu as u32) << 16),
            12 => self.registers.speed,
            16 => {
                (self.registers.duplex as u32)
                    | ((self.registers.rss_max_key_size as u32) << 8)
                    | ((self.registers.rss_max_indirection_table_length as u32) << 24)
            }
            20 => self.registers.supported_hash_types,
            _ => 0,
        }
    }

    fn write_registers_u32(&mut self, _offset: u16, _val: u32) {}

    fn enable(&mut self, resources: Resources) {
        let mut queue_resources: Vec<_> = resources.queues.into_iter().collect();
        let mut workers = Vec::with_capacity(queue_resources.len() / 2);
        while queue_resources.len() > 1 {
            let mut next = queue_resources.drain(..2);
            let rx_resources = next.next().unwrap();
            let tx_resources = next.next().unwrap();
            if !rx_resources.params.enable || !tx_resources.params.enable {
                continue;
            }

            let rx_queue_size = rx_resources.params.size;
            let rx_queue_event = PolledWait::new(&self.adapter.driver, rx_resources.event);
            if let Err(err) = rx_queue_event {
                tracing::error!(
                    err = &err as &dyn std::error::Error,
                    "Failed creating queue event"
                );
                continue;
            }
            let rx_queue = VirtioQueue::new(
                resources.features.clone(),
                rx_resources.params,
                self.memory.clone(),
                rx_resources.notify,
                rx_queue_event.unwrap(),
            );
            if let Err(err) = rx_queue {
                tracing::error!(
                    err = &err as &dyn std::error::Error,
                    "Failed creating virtio net receive queue"
                );
                continue;
            }

            let tx_queue_size = tx_resources.params.size;
            let tx_queue_event = PolledWait::new(&self.adapter.driver, tx_resources.event);
            if let Err(err) = tx_queue_event {
                tracing::error!(
                    err = &err as &dyn std::error::Error,
                    "Failed creating queue event"
                );
                continue;
            }
            let tx_queue = VirtioQueue::new(
                resources.features.clone(),
                tx_resources.params,
                self.memory.clone(),
                tx_resources.notify,
                tx_queue_event.unwrap(),
            );
            if let Err(err) = tx_queue {
                tracing::error!(
                    err = &err as &dyn std::error::Error,
                    "Failed creating virtio net transmit queue"
                );
                continue;
            }
            workers.push(VirtioState {
                rx_queue: rx_queue.unwrap(),
                rx_queue_size,
                tx_queue: tx_queue.unwrap(),
                tx_queue_size,
            });
        }

        let (tx, rx) = mesh::channel();
        self.coordinator_send = Some(tx);
        self.insert_coordinator(rx, workers.len() as u16);
        for (i, virtio_state) in workers.into_iter().enumerate() {
            self.insert_worker(virtio_state, i);
        }
        self.coordinator.start();
    }

    fn disable(&mut self) {
        if let Some(send) = self.coordinator_send.take() {
            send.send(CoordinatorMessage::Disable);
        }
    }
}

struct EndpointQueueState {
    queue: Box<dyn net_backend::Queue>,
}

struct NetQueue {
    state: Option<EndpointQueueState>,
}

impl InspectTaskMut<Worker> for NetQueue {
    fn inspect_mut(&mut self, req: inspect::Request<'_>, worker: Option<&mut Worker>) {
        if worker.is_none() && self.state.is_none() {
            req.ignore();
            return;
        }
        let mut resp = req.respond();
        if let Some(worker) = worker {
            resp.field(
                "pending_tx_packets",
                worker
                    .active_state
                    .pending_tx_packets
                    .iter()
                    .fold(0, |acc, next| acc + if next.is_some() { 1 } else { 0 }),
            )
            .field(
                "pending_rx_packets",
                worker.active_state.pending_rx_packets.ready().len(),
            )
            .field(
                "pending_tx",
                !worker.active_state.data.tx_segments.is_empty(),
            )
            .merge(&worker.active_state.stats);
        }

        if let Some(epqueue_state) = &mut self.state {
            resp.field_mut("queue", &mut epqueue_state.queue);
        }
    }
}

/// Buffers used during packet processing.
struct ProcessingData {
    tx_segments: Vec<TxSegment>,
    tx_done: Box<[TxId]>,
    rx_ready: Box<[RxId]>,
}

impl ProcessingData {
    fn new(rx_queue_size: u16, tx_queue_size: u16) -> Self {
        Self {
            tx_segments: Vec::new(),
            tx_done: vec![TxId(0); tx_queue_size as usize].into(),
            rx_ready: vec![RxId(0); rx_queue_size as usize].into(),
        }
    }
}

#[derive(Inspect, Default)]
struct QueueStats {
    tx_stalled: Counter,
    spurious_wakes: Counter,
    rx_packets: Counter,
    tx_packets: Counter,
    tx_packets_per_wake: Histogram<10>,
    rx_packets_per_wake: Histogram<10>,
}

struct ActiveState {
    pending_tx_packets: Vec<Option<PendingTxPacket>>,
    pending_rx_packets: VirtioWorkPool,
    data: ProcessingData,
    stats: QueueStats,
}

impl ActiveState {
    fn new(mem: GuestMemory, rx_queue_size: u16, tx_queue_size: u16) -> Self {
        Self {
            pending_tx_packets: (0..tx_queue_size).map(|_| None).collect(),
            pending_rx_packets: VirtioWorkPool::new(mem, rx_queue_size),
            data: ProcessingData::new(rx_queue_size, tx_queue_size),
            stats: Default::default(),
        }
    }
}

/// The state for a tx packet that's currently pending in the backend endpoint.
struct PendingTxPacket {
    work: VirtioQueueCallbackWork,
}

pub struct NicBuilder {
    max_queues: u16,
}

impl NicBuilder {
    pub fn max_queues(mut self, max_queues: u16) -> Self {
        self.max_queues = max_queues;
        self
    }

    /// Creates a new NIC.
    pub fn build(
        self,
        driver_source: &VmTaskDriverSource,
        memory: GuestMemory,
        endpoint: Box<dyn Endpoint>,
        mac_address: MacAddress,
    ) -> Device {
        // TODO: Implement VIRTIO_NET_F_MQ and VIRTIO_NET_F_RSS logic based on mulitqueue support.
        // let multiqueue = endpoint.multiqueue_support();
        // let max_queues = self.max_queues.clamp(1, multiqueue.max_queues.min(VIRTIO_NET_MAX_QUEUES));
        let max_queues = 1;

        let driver = driver_source.simple();
        let adapter = Arc::new(Adapter {
            driver,
            max_queues,
            tx_fast_completions: endpoint.tx_fast_completions(),
            mac_address,
        });

        let coordinator = TaskControl::new(CoordinatorState {
            endpoint,
            adapter: adapter.clone(),
        });

        let registers = NetConfig {
            mac: mac_address.to_bytes(),
            status: NetStatus::new().with_link_up(true).into(),
            max_virtqueue_pairs: max_queues,
            mtu: DEFAULT_MTU,
            speed: 0xffffffff,
            duplex: 0xff,
            rss_max_key_size: 0,
            rss_max_indirection_table_length: 0,
            supported_hash_types: 0,
        };

        Device {
            registers,
            memory,
            coordinator,
            coordinator_send: None,
            adapter,
            driver_source: driver_source.clone(),
        }
    }
}

impl Device {
    pub fn builder() -> NicBuilder {
        NicBuilder { max_queues: !0 }
    }
}

impl InspectMut for Device {
    fn inspect_mut(&mut self, req: inspect::Request<'_>) {
        self.coordinator.inspect_mut(req);
    }
}

impl Device {
    fn insert_coordinator(&mut self, recv: mesh::Receiver<CoordinatorMessage>, num_queues: u16) {
        self.coordinator.insert(
            &self.adapter.driver,
            "virtio-net-coordinator".to_string(),
            Coordinator {
                recv,
                workers: (0..self.adapter.max_queues)
                    .map(|_| TaskControl::new(NetQueue { state: None }))
                    .collect(),
                num_queues,
                restart: true,
            },
        );
    }

    /// Allocates and inserts a worker.
    ///
    /// The coordinator must be stopped.
    fn insert_worker(&mut self, virtio_state: VirtioState, idx: usize) {
        let mut builder = self.driver_source.builder();
        // TODO: set this correctly
        builder.target_vp(0);
        // If tx completions arrive quickly, then just do tx processing
        // on whatever processor the guest happens to signal from.
        // Subsequent transmits will be pulled from the completion
        // processor.
        builder.run_on_target(!self.adapter.tx_fast_completions);
        let driver = builder.build("virtio-net");

        let active_state = ActiveState::new(
            self.memory.clone(),
            virtio_state.rx_queue_size,
            virtio_state.tx_queue_size,
        );
        let worker = Worker {
            virtio_state,
            active_state,
        };
        let coordinator = self.coordinator.state_mut().unwrap();
        let worker_task = &mut coordinator.workers[idx];
        worker_task.insert(&driver, "virtio-net".to_string(), worker);
        worker_task.start();
    }
}

#[derive(PartialEq)]
enum CoordinatorMessage {
    Disable,
}

struct Coordinator {
    recv: mesh::Receiver<CoordinatorMessage>,
    workers: Vec<TaskControl<NetQueue, Worker>>,
    num_queues: u16,
    restart: bool,
}

struct CoordinatorState {
    endpoint: Box<dyn Endpoint>,
    adapter: Arc<Adapter>,
}

impl InspectTaskMut<Coordinator> for CoordinatorState {
    fn inspect_mut(&mut self, req: inspect::Request<'_>, coordinator: Option<&mut Coordinator>) {
        let mut resp = req.respond();

        let adapter = self.adapter.as_ref();
        resp.field("mac_address", adapter.mac_address)
            .field("max_queues", adapter.max_queues);

        resp.field("endpoint_type", self.endpoint.endpoint_type())
            .field(
                "endpoint_max_queues",
                self.endpoint.multiqueue_support().max_queues,
            )
            .field_mut("endpoint", self.endpoint.as_mut());

        if let Some(coordinator) = coordinator {
            resp.fields_mut(
                "queues",
                coordinator.workers[..coordinator.num_queues as usize]
                    .iter_mut()
                    .enumerate(),
            );
        }
    }
}

impl AsyncRun<Coordinator> for CoordinatorState {
    async fn run(
        &mut self,
        stop: &mut StopTask<'_>,
        coordinator: &mut Coordinator,
    ) -> Result<(), task_control::Cancelled> {
        coordinator.process(stop, self).await
    }
}

impl Coordinator {
    async fn process(
        &mut self,
        stop: &mut StopTask<'_>,
        state: &mut CoordinatorState,
    ) -> Result<(), task_control::Cancelled> {
        loop {
            if self.restart {
                stop.until_stopped(self.stop_workers()).await?;
                // The queue restart operation is not restartable, so do not
                // poll on `stop` here.
                if let Err(err) = self.restart_queues(state).await {
                    tracing::error!(
                        error = &err as &dyn std::error::Error,
                        "failed to restart queues"
                    );
                }
                self.restart = false;
            }
            self.start_workers();
            enum Message {
                Internal(CoordinatorMessage),
                ChannelDisconnected,
                UpdateFromEndpoint(EndpointAction),
            }
            let message = {
                let wait_for_message = async {
                    let internal_msg = self
                        .recv
                        .next()
                        .map(|x| x.map_or(Message::ChannelDisconnected, Message::Internal));
                    let endpoint_restart = state
                        .endpoint
                        .wait_for_endpoint_action()
                        .map(Message::UpdateFromEndpoint);
                    (internal_msg, endpoint_restart).race().await
                };
                stop.until_stopped(wait_for_message).await?
            };
            match message {
                Message::UpdateFromEndpoint(EndpointAction::RestartRequired) => self.restart = true,
                Message::UpdateFromEndpoint(EndpointAction::LinkStatusNotify(_)) => {
                    tracing::error!("unexpected link status notification")
                }
                Message::Internal(CoordinatorMessage::Disable) | Message::ChannelDisconnected => {
                    stop.until_stopped(self.stop_workers()).await?;
                    break;
                }
            };
        }
        Ok(())
    }

    async fn stop_workers(&mut self) {
        for worker in &mut self.workers {
            worker.stop().await;
        }
    }

    async fn restart_queues(&mut self, c_state: &mut CoordinatorState) -> Result<(), WorkerError> {
        // Drop all of the current queues.
        for worker in &mut self.workers {
            worker.task_mut().state = None;
        }

        let (rx_pools, ready_packets): (Vec<_>, Vec<_>) = self
            .workers
            .iter()
            .map(|worker| {
                let pool = worker
                    .state()
                    .unwrap()
                    .active_state
                    .pending_rx_packets
                    .clone();
                let ready = pool.ready();
                (pool, ready)
            })
            .collect::<Vec<_>>()
            .into_iter()
            .unzip();
        let mut queue_config = Vec::with_capacity(rx_pools.len());
        for (i, pool) in rx_pools.into_iter().enumerate() {
            queue_config.push(QueueConfig {
                pool: Box::new(pool),
                initial_rx: ready_packets[i].as_slice(),
                driver: Box::new(c_state.adapter.driver.clone()),
            });
        }

        let mut queues = Vec::new();
        c_state
            .endpoint
            .get_queues(queue_config, None, &mut queues)
            .await
            .map_err(WorkerError::Endpoint)?;

        assert_eq!(queues.len(), self.workers.len());

        for (worker, queue) in self.workers.iter_mut().zip(queues) {
            worker.task_mut().state = Some(EndpointQueueState { queue });
        }

        Ok(())
    }

    fn start_workers(&mut self) {
        for worker in &mut self.workers {
            worker.start();
        }
    }
}

impl AsyncRun<Worker> for NetQueue {
    async fn run(
        &mut self,
        stop: &mut StopTask<'_>,
        worker: &mut Worker,
    ) -> Result<(), task_control::Cancelled> {
        match worker.process(stop, self).await {
            Ok(()) => {}
            Err(WorkerError::Cancelled(cancelled)) => return Err(cancelled),
            Err(err) => {
                tracing::error!(err = &err as &dyn std::error::Error, "virtio net error");
            }
        }
        Ok(())
    }
}

struct VirtioState {
    rx_queue: VirtioQueue,
    rx_queue_size: u16,
    tx_queue: VirtioQueue,
    tx_queue_size: u16,
}

#[derive(Debug, Error)]
enum WorkerError {
    #[error("packet error")]
    Packet(#[from] PacketError),
    #[error("virtio queue processing error")]
    VirtioQueue(#[source] std::io::Error),
    #[error("endpoint")]
    Endpoint(#[source] anyhow::Error),
    #[error("cancelled")]
    Cancelled(task_control::Cancelled),
}

impl From<task_control::Cancelled> for WorkerError {
    fn from(value: task_control::Cancelled) -> Self {
        Self::Cancelled(value)
    }
}

#[derive(Debug, Error)]
enum PacketError {
    #[error("Empty packet")]
    Empty,
}

struct Worker {
    virtio_state: VirtioState,
    active_state: ActiveState,
}

impl Worker {
    async fn process(
        &mut self,
        stop: &mut StopTask<'_>,
        queue: &mut NetQueue,
    ) -> Result<(), WorkerError> {
        // Be careful not to wait on actions with unbounded blocking time (e.g.
        // guest actions, or waiting for network packets to arrive) without
        // wrapping the wait on `stop.until_stopped`.
        if queue.state.is_none() {
            // wait for an active queue
            stop.until_stopped(pending()).await?
        }

        self.main_loop(stop, queue).await?;
        Ok(())
    }

    async fn main_loop(
        &mut self,
        stop: &mut StopTask<'_>,
        queue: &mut NetQueue,
    ) -> Result<(), WorkerError> {
        let epqueue_state = queue.state.as_mut().unwrap();

        loop {
            let did_some_work = self.process_endpoint_rx(epqueue_state.queue.as_mut())?
                | self.process_virtio_rx(epqueue_state.queue.as_mut())?
                | self.process_endpoint_tx(epqueue_state.queue.as_mut())?;

            if !did_some_work {
                self.active_state.stats.spurious_wakes.increment();
            }

            // This should be the only await point waiting on network traffic or
            // guest actions. Wrap it in `stop.until_stopped` to allow
            // cancellation.
            stop.until_stopped(async {
                enum WakeReason {
                    PacketFromClient(Result<VirtioQueueCallbackWork, std::io::Error>),
                    PacketToClient(Result<VirtioQueueCallbackWork, std::io::Error>),
                    NetworkBackend,
                }
                loop {
                    let net_queue = std::future::poll_fn(|cx| -> Poll<()> {
                        // Check the network endpoint for tx completion or rx.
                        epqueue_state.queue.poll_ready(cx)
                    })
                    .map(|_| WakeReason::NetworkBackend);
                    let to_client = self.virtio_state.rx_queue.next().map(|work| {
                        WakeReason::PacketToClient(work.expect("queue never completes"))
                    });
                    let wake_reason = if self.active_state.data.tx_segments.is_empty() {
                        let from_client = self.virtio_state.tx_queue.next().map(|work| {
                            WakeReason::PacketFromClient(work.expect("queue never completes"))
                        });
                        (net_queue, from_client, to_client).race().await
                    } else {
                        (net_queue, to_client).race().await
                    };
                    match wake_reason {
                        WakeReason::NetworkBackend => {
                            tracing::trace!("endpoint ready");
                            return Ok::<(), WorkerError>(());
                        }
                        WakeReason::PacketFromClient(work) => {
                            tracing::trace!("tx packet");
                            let work = work.map_err(WorkerError::VirtioQueue)?;
                            self.queue_tx_packet(work)?;
                            self.process_virtio_rx(epqueue_state.queue.as_mut())?;
                            if !self.transmit_pending_segments(epqueue_state)? {
                                self.active_state.stats.tx_stalled.increment();
                            }
                        }
                        WakeReason::PacketToClient(work) => {
                            tracing::trace!("rx packet");
                            let work = work.map_err(WorkerError::VirtioQueue)?;
                            epqueue_state
                                .queue
                                .rx_avail(&[self.active_state.pending_rx_packets.queue_work(work)]);
                        }
                    }
                }
            })
            .await??;
        }
    }

    fn queue_tx_packet(&mut self, mut work: VirtioQueueCallbackWork) -> Result<(), WorkerError> {
        let mut header_bytes_remaining = header_size() as u32;
        let mut segments = work
            .payload
            .iter()
            .filter_map(|p| {
                if p.writeable {
                    None
                } else if header_bytes_remaining >= p.length {
                    header_bytes_remaining -= p.length;
                    None
                } else if header_bytes_remaining > 0 {
                    let segment = TxSegment {
                        ty: TxSegmentType::Tail,
                        gpa: p.address + header_bytes_remaining as u64,
                        len: p.length - header_bytes_remaining,
                    };
                    header_bytes_remaining = 0;
                    Some(segment)
                } else {
                    Some(TxSegment {
                        ty: TxSegmentType::Tail,
                        gpa: p.address,
                        len: p.length,
                    })
                }
            })
            .collect::<Vec<_>>();
        if segments.is_empty() {
            work.complete(0);
            return Err(WorkerError::Packet(PacketError::Empty));
        }
        let idx = work.descriptor_index();
        segments[0].ty = TxSegmentType::Head(TxMetadata {
            id: TxId(idx.into()),
            segment_count: segments.len().try_into().unwrap(),
            len: (work.get_payload_length(false) as usize - header_size())
                .try_into()
                .unwrap(),
            ..Default::default()
        });
        let state = &mut self.active_state;
        state.data.tx_segments.append(&mut segments);
        assert!(state.pending_tx_packets[idx as usize].is_none());
        state.pending_tx_packets[idx as usize] = Some(PendingTxPacket { work });
        Ok(())
    }

    fn process_virtio_rx(
        &mut self,
        epqueue: &mut dyn net_backend::Queue,
    ) -> Result<bool, WorkerError> {
        // Fill the receive queue with any available buffers.
        let mut rx_ids = Vec::new();
        while let Some(Some(work)) = self.virtio_state.rx_queue.next().now_or_never() {
            tracing::trace!("rx packet");
            let work = work.map_err(WorkerError::VirtioQueue)?;
            rx_ids.push(self.active_state.pending_rx_packets.queue_work(work));
        }
        if !rx_ids.is_empty() {
            epqueue.rx_avail(rx_ids.as_slice());
            Ok(true)
        } else {
            Ok(false)
        }
    }

    fn process_endpoint_rx(
        &mut self,
        epqueue: &mut dyn net_backend::Queue,
    ) -> Result<bool, WorkerError> {
        let state = &mut self.active_state;
        let n = epqueue
            .rx_poll(&mut state.data.rx_ready)
            .map_err(WorkerError::Endpoint)?;
        if n == 0 {
            return Ok(false);
        }

        for ready_id in state.data.rx_ready[..n].iter() {
            state.stats.rx_packets.increment();
            state.pending_rx_packets.complete_packet(*ready_id);
        }

        state.stats.rx_packets_per_wake.add_sample(n as u64);
        Ok(true)
    }

    fn process_endpoint_tx(
        &mut self,
        epqueue: &mut dyn net_backend::Queue,
    ) -> Result<bool, WorkerError> {
        // Drain completed transmits.
        let n = epqueue
            .tx_poll(&mut self.active_state.data.tx_done)
            .map_err(|tx_error| WorkerError::Endpoint(tx_error.into()))?;
        if n == 0 {
            return Ok(false);
        }

        let pending_segment_id = if !self.active_state.data.tx_segments.is_empty() {
            let TxSegmentType::Head(metadata) = &self.active_state.data.tx_segments[0].ty else {
                unreachable!()
            };
            Some(metadata.id)
        } else {
            None
        };
        for i in 0..n {
            let id = self.active_state.data.tx_done[i];
            self.complete_tx_packet(id)?;
            if let Some(pending_segment_id) = pending_segment_id {
                if pending_segment_id.0 == id.0 {
                    self.active_state.data.tx_segments.clear();
                }
            }
        }
        self.active_state
            .stats
            .tx_packets_per_wake
            .add_sample(n as u64);

        Ok(true)
    }

    fn transmit_pending_segments(
        &mut self,
        queue_state: &mut EndpointQueueState,
    ) -> Result<bool, WorkerError> {
        if self.active_state.data.tx_segments.is_empty() {
            return Ok(false);
        }
        let TxSegmentType::Head(metadata) = &self.active_state.data.tx_segments[0].ty else {
            unreachable!()
        };
        let id = metadata.id;
        self.transmit_segments(queue_state, id)?;
        Ok(true)
    }

    fn transmit_segments(
        &mut self,
        queue_state: &mut EndpointQueueState,
        id: TxId,
    ) -> Result<(), WorkerError> {
        let (sync, segments_sent) = queue_state
            .queue
            .tx_avail(&self.active_state.data.tx_segments)
            .map_err(WorkerError::Endpoint)?;

        assert!(segments_sent <= self.active_state.data.tx_segments.len());

        if sync && segments_sent == self.active_state.data.tx_segments.len() {
            self.active_state.data.tx_segments.clear();
            self.complete_tx_packet(id)?;
        }
        Ok(())
    }

    fn complete_tx_packet(&mut self, id: TxId) -> Result<(), WorkerError> {
        let state = &mut self.active_state;
        let mut tx_packet = state.pending_tx_packets[id.0 as usize].take().unwrap();
        tx_packet.work.complete(0);
        self.active_state.stats.tx_packets.increment();
        Ok(())
    }
}
