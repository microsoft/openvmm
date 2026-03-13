// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Virtio network device implementation.
//!
//! This crate implements a virtio-net device that connects a guest's virtual
//! NIC to a pluggable [`net_backend::Endpoint`]. It currently operates with a
//! single queue pair (one RX, one TX) and supports synchronous and asynchronous
//! TX completion modes depending on the backend.

#![expect(missing_docs)]
#![forbid(unsafe_code)]

mod buffers;
pub mod resolver;

#[cfg(test)]
mod tests;

use crate::buffers::VirtioWorkPool;
use anyhow::Context as _;
use bitfield_struct::bitfield;
use guestmem::GuestMemory;
use inspect::Inspect;
use inspect::InspectMut;
use inspect_counters::Counter;
use inspect_counters::Histogram;
use net_backend::Endpoint;
use net_backend::EndpointAction;
use net_backend::QueueConfig;
use net_backend::RxId;
use net_backend::TxFlags;
use net_backend::TxId;
use net_backend::TxMetadata;
use net_backend::TxOffloadSupport;
use net_backend::TxSegment;
use net_backend::TxSegmentType;
use net_backend_resources::mac_address::MacAddress;
use pal_async::wait::PolledWait;
use std::future::pending;
use std::mem::offset_of;
use std::sync::Arc;
use std::task::Context;
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

const fn header_size() -> usize {
    // TODO: Verify hash flags are not set, since header size would be larger in that case.
    offset_of!(VirtioNetHeader, hash_value)
}

struct Adapter {
    driver: VmTaskDriver,
    max_queues: u16,
    tx_fast_completions: bool,
    mac_address: MacAddress,
    tx_offload_support: TxOffloadSupport,
}

pub struct Device {
    registers: NetConfig,
    memory: GuestMemory,
    coordinator: TaskControl<CoordinatorState, Coordinator>,
    adapter: Arc<Adapter>,
    driver_source: VmTaskDriverSource,
}

impl VirtioDevice for Device {
    fn traits(&self) -> DeviceTraits {
        let offloads = &self.adapter.tx_offload_support;

        // VIRTIO_NET_F_CSUM: we can handle partial checksum from the guest
        let csum = offloads.tcp || offloads.udp;
        // VIRTIO_NET_F_HOST_TSO4/6: we can handle TSO from the guest
        let host_tso4 = offloads.tso && offloads.tcp;
        let host_tso6 = offloads.tso && offloads.tcp;

        let features_bank0 = NetworkFeaturesBank0::new()
            .with_mac(true)
            .with_csum(csum)
            .with_guest_csum(true)
            .with_host_tso4(host_tso4)
            .with_host_tso6(host_tso6);

        DeviceTraits {
            device_id: 1,
            device_features: VirtioDeviceFeatures::new().with_bank(0, features_bank0.into_bits()),
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
                    | ((self.registers.rss_max_indirection_table_length as u32) << 16)
            }
            20 => self.registers.supported_hash_types,
            _ => 0,
        }
    }

    fn write_registers_u32(&mut self, _offset: u16, _val: u32) {}

    fn enable(&mut self, resources: Resources) -> anyhow::Result<()> {
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
            let rx_queue_event = PolledWait::new(&self.adapter.driver, rx_resources.event)
                .context("failed creating rx queue event")?;
            let rx_queue = VirtioQueue::new(
                resources.features.clone(),
                rx_resources.params,
                self.memory.clone(),
                rx_resources.notify,
                rx_queue_event,
            )
            .context("failed creating virtio net receive queue")?;

            let tx_queue_size = tx_resources.params.size;
            let tx_queue_event = PolledWait::new(&self.adapter.driver, tx_resources.event)
                .context("failed creating tx queue event")?;
            let tx_queue = VirtioQueue::new(
                resources.features.clone(),
                tx_resources.params,
                self.memory.clone(),
                tx_resources.notify,
                tx_queue_event,
            )
            .context("failed creating virtio net transmit queue")?;

            workers.push(VirtioState {
                rx_queue,
                rx_queue_size,
                tx_queue,
                tx_queue_size,
            });
        }

        self.insert_coordinator(workers.len() as u16);
        for (i, virtio_state) in workers.into_iter().enumerate() {
            self.insert_worker(virtio_state, i);
        }
        self.coordinator.start();
        Ok(())
    }

    fn poll_disable(&mut self, cx: &mut Context<'_>) -> Poll<()> {
        // Stop the coordinator task.
        let _ = std::task::ready!(self.coordinator.poll_stop(cx));
        // Stop all workers (coordinator may not have stopped them if it was
        // cancelled before reaching its own stop_workers call).
        if let Some(coordinator) = self.coordinator.state_mut() {
            for worker in &mut coordinator.workers {
                let _ = std::task::ready!(worker.poll_stop(cx));
            }
        }
        // Remove the coordinator state so that a subsequent enable() can
        // re-insert it.
        let _ = self.coordinator.remove();
        Poll::Ready(())
    }
}

#[derive(InspectMut)]
struct EndpointQueueState {
    #[inspect(mut)]
    queue: Box<dyn net_backend::Queue>,
}

#[derive(InspectMut)]
struct NetQueue {
    #[inspect(flatten, mut)]
    state: Option<EndpointQueueState>,
}

impl InspectTaskMut<Worker> for NetQueue {
    fn inspect_mut(&mut self, req: inspect::Request<'_>, worker: Option<&mut Worker>) {
        req.respond().merge(self).merge(worker);
    }
}

/// Buffers used during packet processing.
#[derive(Inspect)]
struct ProcessingData {
    #[inspect(with = "Vec::len")]
    tx_segments: Vec<TxSegment>,
    #[inspect(skip)]
    tx_done: Box<[TxId]>,
    #[inspect(skip)]
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

#[derive(Inspect)]
struct ActiveState {
    mem: GuestMemory,
    #[inspect(with = "|x| x.iter().flatten().count()")]
    pending_tx_packets: Vec<Option<PendingTxPacket>>,
    pending_rx_packets: VirtioWorkPool,
    data: ProcessingData,
    stats: QueueStats,
}

impl ActiveState {
    fn new(mem: GuestMemory, rx_queue_size: u16, tx_queue_size: u16) -> Self {
        Self {
            pending_tx_packets: (0..tx_queue_size).map(|_| None).collect(),
            pending_rx_packets: VirtioWorkPool::new(mem.clone(), rx_queue_size),
            data: ProcessingData::new(rx_queue_size, tx_queue_size),
            stats: Default::default(),
            mem,
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
        let tx_offload_support = endpoint.tx_offload_support();
        let adapter = Arc::new(Adapter {
            driver,
            max_queues,
            tx_fast_completions: endpoint.tx_fast_completions(),
            mac_address,
            tx_offload_support,
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
    fn insert_coordinator(&mut self, num_queues: u16) {
        self.coordinator.insert(
            &self.adapter.driver,
            "virtio-net-coordinator".to_string(),
            Coordinator {
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

struct Coordinator {
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
            match stop
                .until_stopped(state.endpoint.wait_for_endpoint_action())
                .await?
            {
                EndpointAction::RestartRequired => self.restart = true,
                EndpointAction::LinkStatusNotify(_) => {
                    tracing::error!("unexpected link status notification")
                }
            }
        }
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

#[derive(Inspect)]
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

#[derive(InspectMut)]
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
                | self.process_virtio_tx(epqueue_state)?
                | self.process_endpoint_tx(epqueue_state.queue.as_mut())?;

            if !did_some_work {
                self.active_state.stats.spurious_wakes.increment();
            }

            // This should be the only await point waiting on network traffic or
            // guest actions. Wrap it in `stop.until_stopped` to allow
            // cancellation.
            stop.until_stopped(std::future::poll_fn(|cx| {
                if let Poll::Ready(()) = epqueue_state.queue.poll_ready(cx) {
                    return Poll::Ready(());
                }

                if self.active_state.data.tx_segments.is_empty()
                    && let Poll::Ready(()) = self.virtio_state.tx_queue.poll_kick(cx)
                {
                    return Poll::Ready(());
                }

                if let Poll::Ready(()) = self.virtio_state.rx_queue.poll_kick(cx) {
                    return Poll::Ready(());
                }

                Poll::Pending
            }))
            .await?;
        }
    }

    fn process_virtio_tx(
        &mut self,
        queue_state: &mut EndpointQueueState,
    ) -> Result<bool, WorkerError> {
        let mut did_work = false;
        loop {
            did_work |= self.transmit_pending_segments(queue_state)?;
            if !self.active_state.data.tx_segments.is_empty() {
                break;
            }
            // Only batch up to 8 packets at a time.
            for _ in 0..8 {
                let Some(work) = self
                    .virtio_state
                    .tx_queue
                    .try_next()
                    .map_err(WorkerError::VirtioQueue)?
                else {
                    break;
                };
                self.queue_tx_packet(work)?;
                did_work = true;
            }
            if self.active_state.data.tx_segments.is_empty() {
                break;
            }
        }
        Ok(did_work)
    }

    fn queue_tx_packet(&mut self, mut work: VirtioQueueCallbackWork) -> Result<(), WorkerError> {
        // Read the virtio-net header + enough of the Ethernet frame to parse
        // the EtherType (and a potential VLAN tag).
        const ETH_PEEK: usize = 18; // 14 standard + 4 for VLAN tag
        let mut peek_buf = [0u8; size_of::<VirtioNetHeader>() + ETH_PEEK];
        let bytes_read = work
            .read(
                &self.active_state.mem,
                &mut peek_buf[..header_size() + ETH_PEEK],
            )
            .unwrap_or(0);
        let header = VirtioNetHeader::read_from_prefix(&peek_buf)
            .map(|(h, _)| h)
            .ok();
        let packet_prefix = if bytes_read > header_size() {
            &peek_buf[header_size()..bytes_read]
        } else {
            &[]
        };

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
        let packet_len: u32 = (work.get_payload_length(false) as usize - header_size())
            .try_into()
            .unwrap();

        // Map virtio-net header fields to TxMetadata offload flags.
        let tx_metadata = Self::parse_tx_offloads(header.as_ref(), packet_prefix, packet_len);

        segments[0].ty = TxSegmentType::Head(TxMetadata {
            id: TxId(idx.into()),
            segment_count: segments.len().try_into().unwrap(),
            len: packet_len,
            ..tx_metadata
        });
        let state = &mut self.active_state;
        state.data.tx_segments.append(&mut segments);
        assert!(state.pending_tx_packets[idx as usize].is_none());
        state.pending_tx_packets[idx as usize] = Some(PendingTxPacket { work });
        Ok(())
    }

    /// Parse virtio-net header offload fields into a `TxMetadata` template.
    ///
    /// `packet_prefix` should contain at least the first 18 bytes of the
    /// Ethernet frame (enough to read the EtherType and a potential VLAN tag).
    ///
    /// The returned `TxMetadata` has `id`, `segment_count`, and `len` set to
    /// defaults — the caller must overwrite those.
    fn parse_tx_offloads(
        header: Option<&VirtioNetHeader>,
        packet_prefix: &[u8],
        packet_len: u32,
    ) -> TxMetadata {
        let Some(header) = header else {
            return TxMetadata {
                len: packet_len,
                ..Default::default()
            };
        };

        let flags_byte = VirtioNetHeaderFlags::from(header.flags);
        let gso = VirtioNetHeaderGso::from(header.gso_type);
        let gso_protocol = gso.protocol();

        let mut flags = TxFlags::new();
        let mut l2_len: u8 = 0;
        let mut l3_len: u16 = 0;
        let mut l4_len: u8 = 0;
        let mut max_tcp_segment_size: u16 = 0;

        // Determine IP version from GSO type when available.
        let is_ipv4_from_gso = gso_protocol == VirtioNetHeaderGsoProtocol::TCPV4;
        let is_ipv6_from_gso = gso_protocol == VirtioNetHeaderGsoProtocol::TCPV6;

        // Parse the Ethernet header to determine IP version and L2 length.
        // EtherType is at offset 12. If it's 0x8100 (VLAN), the real
        // EtherType is at offset 16 and L2 is 18 bytes.
        let (parsed_l2_len, is_ipv4_from_eth, is_ipv6_from_eth) =
            Self::parse_ethertype(packet_prefix);

        if flags_byte.needs_csum() {
            // The guest requests partial checksum offload.
            // csum_start is the byte offset (from packet start) of the L4
            // header. csum_offset is the byte offset within the L4 header
            // of the checksum field.
            l2_len = parsed_l2_len;
            if header.csum_start >= l2_len as u16 {
                l3_len = header.csum_start - l2_len as u16;
            }

            // Determine TCP vs UDP from csum_offset:
            //   TCP checksum is at offset 16 within the TCP header.
            //   UDP checksum is at offset 6 within the UDP header.
            let is_tcp = header.csum_offset == 16;
            let is_udp = header.csum_offset == 6;

            if is_tcp {
                flags.set_offload_tcp_checksum(true);
            } else if is_udp {
                flags.set_offload_udp_checksum(true);
            }

            // Prefer GSO-derived IP version, then EtherType-derived.
            let is_ipv4 = is_ipv4_from_gso || (!is_ipv6_from_gso && is_ipv4_from_eth);
            let is_ipv6 = is_ipv6_from_gso || (!is_ipv4_from_gso && is_ipv6_from_eth);
            flags.set_is_ipv4(is_ipv4);
            flags.set_is_ipv6(is_ipv6);
            // Don't set offload_ip_header_checksum here: virtio guests
            // always compute the IPv4 header checksum themselves (the
            // virtio CSUM feature only covers L4 checksums). The GSO
            // path below sets it because hardware backends (e.g. MANA)
            // need it to know they must compute per-segment checksums.
        }

        // GSO (segmentation offload)
        if gso_protocol == VirtioNetHeaderGsoProtocol::TCPV4
            || gso_protocol == VirtioNetHeaderGsoProtocol::TCPV6
        {
            flags.set_offload_tcp_segmentation(true);
            flags.set_offload_tcp_checksum(true);
            max_tcp_segment_size = header.gso_size;

            if l2_len == 0 {
                l2_len = parsed_l2_len;
            }

            flags.set_is_ipv4(is_ipv4_from_gso);
            flags.set_is_ipv6(is_ipv6_from_gso);
            if is_ipv4_from_gso {
                flags.set_offload_ip_header_checksum(true);
            }

            // Derive l3_len from csum_start if we haven't already.
            if l3_len == 0 && header.csum_start >= l2_len as u16 {
                l3_len = header.csum_start - l2_len as u16;
            }

            // Derive l4_len from hdr_len if available:
            //   hdr_len = l2_len + l3_len + l4_len (total header length)
            let total_hdr = header.hdr_len as u32;
            let l2_l3 = l2_len as u32 + l3_len as u32;
            if total_hdr > l2_l3 {
                l4_len = (total_hdr - l2_l3) as u8;
            }
        }

        TxMetadata {
            flags,
            l2_len,
            l3_len,
            l4_len,
            max_tcp_segment_size,
            ..Default::default()
        }
    }

    /// Parse the EtherType from the start of an Ethernet frame.
    ///
    /// Returns `(l2_len, is_ipv4, is_ipv6)`. Handles 802.1Q VLAN tags.
    fn parse_ethertype(packet: &[u8]) -> (u8, bool, bool) {
        const ETHERTYPE_IPV4: u16 = 0x0800;
        const ETHERTYPE_IPV6: u16 = 0x86DD;
        const ETHERTYPE_VLAN: u16 = 0x8100;

        if packet.len() < 14 {
            return (14, false, false);
        }

        let ethertype = u16::from_be_bytes([packet[12], packet[13]]);
        if ethertype == ETHERTYPE_VLAN {
            // VLAN-tagged: real EtherType is 4 bytes further.
            if packet.len() < 18 {
                return (18, false, false);
            }
            let inner = u16::from_be_bytes([packet[16], packet[17]]);
            (18, inner == ETHERTYPE_IPV4, inner == ETHERTYPE_IPV6)
        } else {
            (14, ethertype == ETHERTYPE_IPV4, ethertype == ETHERTYPE_IPV6)
        }
    }

    fn process_virtio_rx(
        &mut self,
        epqueue: &mut dyn net_backend::Queue,
    ) -> Result<bool, WorkerError> {
        // Fill the receive queue with any available buffers.
        let mut rx_ids = Vec::new();
        while let Some(work) = self
            .virtio_state
            .rx_queue
            .try_next()
            .map_err(WorkerError::VirtioQueue)?
        {
            tracing::trace!("rx packet");
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

        for i in 0..n {
            let id = self.active_state.data.tx_done[i];
            self.complete_tx_packet(id)?;
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
        let (sync, segments_sent) = queue_state
            .queue
            .tx_avail(&self.active_state.data.tx_segments)
            .map_err(WorkerError::Endpoint)?;

        if sync {
            // Complete the packets now.
            let mut i = 0;
            loop {
                let segments = &self.active_state.data.tx_segments[..segments_sent][i..];
                let Some(head) = segments.first() else {
                    break;
                };
                let TxSegmentType::Head(metadata) = &head.ty else {
                    unreachable!()
                };
                let id = metadata.id;
                i += metadata.segment_count as usize;
                self.complete_tx_packet(id)?;
            }
        }

        self.active_state.data.tx_segments.drain(..segments_sent);
        Ok(segments_sent != 0)
    }

    fn complete_tx_packet(&mut self, id: TxId) -> Result<(), WorkerError> {
        let state = &mut self.active_state;
        let mut tx_packet = state.pending_tx_packets[id.0 as usize].take().unwrap();
        tx_packet.work.complete(0);
        self.active_state.stats.tx_packets.increment();
        Ok(())
    }
}
