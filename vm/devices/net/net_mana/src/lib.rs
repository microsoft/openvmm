// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

#![forbid(unsafe_code)]
#![expect(missing_docs)]

mod test;

use anyhow::Context as _;
use async_trait::async_trait;
use futures::FutureExt;
use futures::StreamExt;
use gdma_defs::Cqe;
use gdma_defs::CqeParams;
use gdma_defs::GDMA_EQE_COMPLETION;
use gdma_defs::Sge;
use gdma_defs::WqeHeader;
use gdma_defs::bnic::CQE_RX_OKAY;
use gdma_defs::bnic::CQE_TX_GDMA_ERR;
use gdma_defs::bnic::CQE_TX_INVALID_OOB;
use gdma_defs::bnic::CQE_TX_OKAY;
use gdma_defs::bnic::MANA_LONG_PKT_FMT;
use gdma_defs::bnic::MANA_SHORT_PKT_FMT;
use gdma_defs::bnic::ManaQueryStatisticsResponse;
use gdma_defs::bnic::ManaRxcompOob;
use gdma_defs::bnic::ManaTxCompOob;
use gdma_defs::bnic::ManaTxOob;
use gdma_defs::bnic::ManaTxShortOob;
use guestmem::GuestMemory;
use inspect::Inspect;
use inspect::InspectMut;
use inspect::SensitivityLevel;
use inspect_counters::Counter;
use mana_driver::mana::BnicEq;
use mana_driver::mana::BnicWq;
use mana_driver::mana::ResourceArena;
use mana_driver::mana::RxConfig;
use mana_driver::mana::TxConfig;
use mana_driver::mana::Vport;
use mana_driver::queues::Cq;
use mana_driver::queues::Eq;
use mana_driver::queues::Wq;
use net_backend::BackendQueueStats;
use net_backend::BufferAccess;
use net_backend::Endpoint;
use net_backend::EndpointAction;
use net_backend::L4Protocol;
use net_backend::MultiQueueSupport;
use net_backend::Queue;
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
use pal_async::task::Spawn;
use safeatomic::AtomicSliceOps;
use std::collections::VecDeque;
use std::sync::Arc;
use std::sync::Weak;
use std::sync::atomic::AtomicU8;
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering;
use std::task::Context;
use std::task::Poll;
use thiserror::Error;
use user_driver::DeviceBacking;
use user_driver::DmaClient;
use user_driver::interrupt::DeviceInterrupt;
use user_driver::memory::MemoryBlock;
use user_driver::memory::PAGE_SIZE32;
use user_driver::memory::PAGE_SIZE64;
use vmcore::slim_event::SlimEvent;
use zerocopy::FromBytes;
use zerocopy::FromZeros;

/// Per queue limit, in number of pages.
/// Used to handle bounce buffering non-contiguous network packet headers.
const SPLIT_HEADER_BOUNCE_PAGE_LIMIT: u32 = 4;

/// Per queue limit for bounce buffering, in number of pages.
/// This is only used when bounce buffering is enabled for the device.
const RX_BOUNCE_BUFFER_PAGE_LIMIT: u32 = 64;
const TX_BOUNCE_BUFFER_PAGE_LIMIT: u32 = 64;

#[cfg(test)]
#[derive(Debug, Default, Clone, Copy)]
pub struct ManaTestConfiguration {
    pub allow_lso_pkt_with_one_sge: bool,
}

pub struct ManaEndpoint<T: DeviceBacking> {
    spawner: Box<dyn Spawn>,
    vport: Arc<Vport<T>>,
    queues: Vec<QueueResources>,
    arena: ResourceArena,
    receive_update: mesh::Receiver<bool>,
    queue_tracker: Arc<(AtomicUsize, SlimEvent)>,
    bounce_buffer: bool,
    #[cfg(test)]
    test_configuration: ManaTestConfiguration,
}

struct QueueResources {
    _eq: BnicEq,
    rxq: BnicWq,
    _txq: BnicWq,
}

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum GuestDmaMode {
    DirectDma,
    BounceBuffer,
}

impl<T: DeviceBacking> ManaEndpoint<T> {
    pub async fn new(
        spawner: impl 'static + Spawn,
        vport: Vport<T>,
        dma_mode: GuestDmaMode,
    ) -> Self {
        let (endpoint_tx, endpoint_rx) = mesh::channel();
        vport.register_link_status_notifier(endpoint_tx).await;
        Self {
            spawner: Box::new(spawner),
            vport: Arc::new(vport),
            queues: Vec::new(),
            arena: ResourceArena::new(),
            receive_update: endpoint_rx,
            queue_tracker: Arc::new((AtomicUsize::new(0), SlimEvent::new())),
            bounce_buffer: match dma_mode {
                GuestDmaMode::DirectDma => false,
                GuestDmaMode::BounceBuffer => true,
            },
            #[cfg(test)]
            test_configuration: ManaTestConfiguration::default(),
        }
    }

    #[cfg(test)]
    fn set_test_configuration(&mut self, config: ManaTestConfiguration) {
        self.test_configuration = config;
    }
}

fn inspect_mana_stats(stats: &ManaQueryStatisticsResponse, req: inspect::Request<'_>) {
    req.respond()
        .sensitivity_counter(
            "in_discards_no_wqe",
            SensitivityLevel::Safe,
            stats.in_discards_no_wqe,
        )
        .sensitivity_counter(
            "in_errors_rx_vport_disabled",
            SensitivityLevel::Safe,
            stats.in_errors_rx_vport_disabled,
        )
        .sensitivity_counter("hc_in_octets", SensitivityLevel::Safe, stats.hc_in_octets)
        .sensitivity_counter(
            "hc_in_ucast_pkts",
            SensitivityLevel::Safe,
            stats.hc_in_ucast_pkts,
        )
        .sensitivity_counter(
            "hc_in_ucast_octets",
            SensitivityLevel::Safe,
            stats.hc_in_ucast_octets,
        )
        .sensitivity_counter(
            "hc_in_multicast_pkts",
            SensitivityLevel::Safe,
            stats.hc_in_multicast_pkts,
        )
        .sensitivity_counter(
            "hc_in_multicast_octets",
            SensitivityLevel::Safe,
            stats.hc_in_multicast_octets,
        )
        .sensitivity_counter(
            "hc_in_broadcast_pkts",
            SensitivityLevel::Safe,
            stats.hc_in_broadcast_pkts,
        )
        .sensitivity_counter(
            "hc_in_broadcast_octets",
            SensitivityLevel::Safe,
            stats.hc_in_broadcast_octets,
        )
        .sensitivity_counter(
            "out_errors_gf_disabled",
            SensitivityLevel::Safe,
            stats.out_errors_gf_disabled,
        )
        .sensitivity_counter(
            "out_errors_vport_disabled",
            SensitivityLevel::Safe,
            stats.out_errors_vport_disabled,
        )
        .sensitivity_counter(
            "out_errors_invalid_vport_offset_packets",
            SensitivityLevel::Safe,
            stats.out_errors_invalid_vport_offset_packets,
        )
        .sensitivity_counter(
            "out_errors_vlan_enforcement",
            SensitivityLevel::Safe,
            stats.out_errors_vlan_enforcement,
        )
        .sensitivity_counter(
            "out_errors_eth_type_enforcement",
            SensitivityLevel::Safe,
            stats.out_errors_eth_type_enforcement,
        )
        .sensitivity_counter(
            "out_errors_sa_enforcement",
            SensitivityLevel::Safe,
            stats.out_errors_sa_enforcement,
        )
        .sensitivity_counter(
            "out_errors_sqpdid_enforcement",
            SensitivityLevel::Safe,
            stats.out_errors_sqpdid_enforcement,
        )
        .sensitivity_counter(
            "out_errors_cqpdid_enforcement",
            SensitivityLevel::Safe,
            stats.out_errors_cqpdid_enforcement,
        )
        .sensitivity_counter(
            "out_errors_mtu_violation",
            SensitivityLevel::Safe,
            stats.out_errors_mtu_violation,
        )
        .sensitivity_counter(
            "out_errors_invalid_oob",
            SensitivityLevel::Safe,
            stats.out_errors_invalid_oob,
        )
        .sensitivity_counter("hc_out_octets", SensitivityLevel::Safe, stats.hc_out_octets)
        .sensitivity_counter(
            "hc_out_ucast_pkts",
            SensitivityLevel::Safe,
            stats.hc_out_ucast_pkts,
        )
        .sensitivity_counter(
            "hc_out_ucast_octets",
            SensitivityLevel::Safe,
            stats.hc_out_ucast_octets,
        )
        .sensitivity_counter(
            "hc_out_multicast_pkts",
            SensitivityLevel::Safe,
            stats.hc_out_multicast_pkts,
        )
        .sensitivity_counter(
            "hc_out_multicast_octets",
            SensitivityLevel::Safe,
            stats.hc_out_multicast_octets,
        )
        .sensitivity_counter(
            "hc_out_broadcast_pkts",
            SensitivityLevel::Safe,
            stats.hc_out_broadcast_pkts,
        )
        .sensitivity_counter(
            "hc_out_broadcast_octets",
            SensitivityLevel::Safe,
            stats.hc_out_broadcast_octets,
        );
}

impl<T: DeviceBacking> InspectMut for ManaEndpoint<T> {
    fn inspect_mut(&mut self, req: inspect::Request<'_>) {
        req.respond()
            .sensitivity_child("stats", SensitivityLevel::Safe, |req| {
                let vport = self.vport.clone();
                let deferred = req.defer();
                self.spawner
                    .spawn("mana-stats", async move {
                        let stats = if let Ok(stats) = vport.query_stats().await {
                            stats
                        } else {
                            ManaQueryStatisticsResponse::new_zeroed()
                        };
                        deferred.inspect(inspect::adhoc(|req| inspect_mana_stats(&stats, req)));
                    })
                    .detach();
            });
    }
}

impl<T: DeviceBacking> ManaEndpoint<T> {
    async fn new_queue(
        &mut self,
        tx_config: &TxConfig,
        pool: Box<dyn BufferAccess>,
        initial_rx: &[RxId],
        arena: &mut ResourceArena,
        cpu: u32,
    ) -> anyhow::Result<(ManaQueue<T>, QueueResources)> {
        let eq_size = 0x1000;
        let tx_wq_size = 0x4000;
        let tx_cq_size = 0x4000;
        let rx_wq_size = 0x8000;
        let rx_cq_size = 0x4000;

        let eq = (self.vport.new_eq(arena, eq_size, cpu))
            .await
            .context("failed to create eq")?;
        let txq = (self
            .vport
            .new_wq(arena, true, tx_wq_size, tx_cq_size, eq.id()))
        .await
        .context("failed to create tx queue")?;
        let rxq = (self
            .vport
            .new_wq(arena, false, rx_wq_size, rx_cq_size, eq.id()))
        .await
        .context("failed to create rx queue")?;

        let interrupt = eq.interrupt();

        // The effective rx max may be smaller depending on the number of SGE
        // entries used in the work queue (which depends on the NIC's configured
        // MTU).
        let rx_max = (rx_cq_size / size_of::<Cqe>() as u32).min(512);

        let tx_max = tx_cq_size / size_of::<Cqe>() as u32;

        let tx_bounce_buffer = ContiguousBufferManager::new(
            self.vport.dma_client().await,
            if self.bounce_buffer {
                TX_BOUNCE_BUFFER_PAGE_LIMIT
            } else {
                SPLIT_HEADER_BOUNCE_PAGE_LIMIT
            },
        )
        .context("failed to allocate tx bounce buffer")?;

        let rx_bounce_buffer = if self.bounce_buffer {
            Some(
                ContiguousBufferManager::new(
                    self.vport.dma_client().await,
                    RX_BOUNCE_BUFFER_PAGE_LIMIT,
                )
                .context("failed to allocate rx bounce buffer")?,
            )
        } else {
            None
        };

        let mut queue = ManaQueue {
            guest_memory: pool.guest_memory().clone(),
            pool,
            rx_bounce_buffer,
            tx_bounce_buffer,
            vport: Arc::downgrade(&self.vport),
            queue_tracker: self.queue_tracker.clone(),
            eq: eq.queue(),
            eq_armed: true,
            interrupt,
            tx_cq_armed: true,
            rx_cq_armed: true,
            vp_offset: tx_config.tx_vport_offset,
            mem_key: self.vport.gpa_mkey(),
            tx_wq: txq.wq(),
            tx_cq: txq.cq(),
            rx_wq: rxq.wq(),
            rx_cq: rxq.cq(),
            avail_rx: VecDeque::new(),
            posted_rx: VecDeque::new(),
            rx_max: rx_max as usize,
            posted_tx: VecDeque::new(),
            dropped_tx: VecDeque::new(),
            tx_max: tx_max as usize,
            force_tx_header_bounce: false,
            stats: QueueStats::default(),
            #[cfg(test)]
            test_configuration: self.test_configuration,
        };
        self.queue_tracker.0.fetch_add(1, Ordering::AcqRel);
        queue.rx_avail(initial_rx);
        queue.rx_wq.commit();

        let resources = QueueResources {
            _eq: eq,
            rxq,
            _txq: txq,
        };
        Ok((queue, resources))
    }

    async fn get_queues_inner(
        &mut self,
        arena: &mut ResourceArena,
        config: Vec<QueueConfig<'_>>,
        rss: Option<&RssConfig<'_>>,
        queues: &mut Vec<Box<dyn Queue>>,
    ) -> anyhow::Result<()> {
        assert!(self.queues.is_empty());

        let tx_config = self
            .vport
            .config_tx()
            .await
            .context("failed to configure transmit")?;

        let mut queue_resources = Vec::new();

        for config in config {
            // Start the queue interrupt on CPU 0, which is already used by the
            // HWC so this is cheap. The actual interrupt will be allocated
            // later when `update_target_vp` is first called.
            let (queue, resources) = self
                .new_queue(&tx_config, config.pool, config.initial_rx, arena, 0)
                .await?;

            queues.push(Box::new(queue));
            queue_resources.push(resources);
        }

        let indirection_table;
        let rx_config = if let Some(rss) = rss {
            indirection_table = rss
                .indirection_table
                .iter()
                .map(|&queue_id| {
                    queue_resources
                        .get(queue_id as usize)
                        .unwrap_or_else(|| &queue_resources[0])
                        .rxq
                        .wq_obj()
                })
                .collect::<Vec<_>>();

            RxConfig {
                rx_enable: Some(true),
                rss_enable: Some(true),
                hash_key: Some(rss.key.try_into().ok().context("wrong hash key size")?),
                default_rxobj: Some(queue_resources[0].rxq.wq_obj()),
                indirection_table: Some(&indirection_table),
            }
        } else {
            RxConfig {
                rx_enable: Some(true),
                rss_enable: Some(false),
                hash_key: None,
                default_rxobj: Some(queue_resources[0].rxq.wq_obj()),
                indirection_table: None,
            }
        };

        self.vport.config_rx(&rx_config).await?;
        self.queues = queue_resources;
        Ok(())
    }
}

#[async_trait]
impl<T: DeviceBacking> Endpoint for ManaEndpoint<T> {
    fn endpoint_type(&self) -> &'static str {
        "mana"
    }

    async fn get_queues(
        &mut self,
        config: Vec<QueueConfig<'_>>,
        rss: Option<&RssConfig<'_>>,
        queues: &mut Vec<Box<dyn Queue>>,
    ) -> anyhow::Result<()> {
        assert!(self.arena.is_empty());
        let mut arena = ResourceArena::new();
        match self.get_queues_inner(&mut arena, config, rss, queues).await {
            Ok(()) => {
                self.arena = arena;
                Ok(())
            }
            Err(err) => {
                self.vport.destroy(arena).await;
                Err(err)
            }
        }
    }

    async fn stop(&mut self) {
        if let Err(err) = self
            .vport
            .config_rx(&RxConfig {
                rx_enable: Some(false),
                rss_enable: None,
                hash_key: None,
                default_rxobj: None,
                indirection_table: None,
            })
            .await
        {
            tracing::warn!(
                error = err.as_ref() as &dyn std::error::Error,
                "failed to stop rx"
            );
        }

        self.queues.clear();
        self.vport.destroy(std::mem::take(&mut self.arena)).await;
        // Wait for all outstanding queues. There can be a delay switching out
        // the queues when an endpoint is removed, and the queue has access to
        // the vport which is being stopped here.
        if self.queue_tracker.0.load(Ordering::Acquire) > 0 {
            self.queue_tracker.1.wait().await;
        }
    }

    fn is_ordered(&self) -> bool {
        true
    }

    fn tx_offload_support(&self) -> TxOffloadSupport {
        TxOffloadSupport {
            ipv4_header: true,
            tcp: true,
            udp: true,
            // Tbe bounce buffer path does not support TSO.
            tso: !self.bounce_buffer,
        }
    }

    fn multiqueue_support(&self) -> MultiQueueSupport {
        MultiQueueSupport {
            max_queues: self
                .vport
                .max_rx_queues()
                .min(self.vport.max_tx_queues())
                .min(u16::MAX.into()) as u16,
            indirection_table_size: self.vport.num_indirection_ent().min(u16::MAX.into()) as u16,
        }
    }

    fn tx_fast_completions(&self) -> bool {
        // The mana NIC completes packets quickly and in order.
        true
    }

    async fn set_data_path_to_guest_vf(&self, use_vf: bool) -> anyhow::Result<()> {
        self.vport.move_filter(if use_vf { 1 } else { 0 }).await?;
        Ok(())
    }

    async fn get_data_path_to_guest_vf(&self) -> anyhow::Result<bool> {
        match self.vport.get_direction_to_vtl0().await {
            Some(to_vtl0) => Ok(to_vtl0),
            None => Err(anyhow::anyhow!("Device does not support data path query")),
        }
    }

    async fn wait_for_endpoint_action(&mut self) -> EndpointAction {
        self.receive_update
            .select_next_some()
            .map(EndpointAction::LinkStatusNotify)
            .await
    }

    fn link_speed(&self) -> u64 {
        // Hard code to 200Gbps until MANA supports querying this.
        200 * 1000 * 1000 * 1000
    }
}

pub struct ManaQueue<T: DeviceBacking> {
    pool: Box<dyn BufferAccess>,
    guest_memory: GuestMemory,
    rx_bounce_buffer: Option<ContiguousBufferManager>,
    tx_bounce_buffer: ContiguousBufferManager,

    vport: Weak<Vport<T>>,
    queue_tracker: Arc<(AtomicUsize, SlimEvent)>,

    eq: Eq,
    eq_armed: bool,
    interrupt: DeviceInterrupt,
    tx_cq_armed: bool,
    rx_cq_armed: bool,

    vp_offset: u16,
    mem_key: u32,

    tx_wq: Wq,
    tx_cq: Cq,

    rx_wq: Wq,
    rx_cq: Cq,

    avail_rx: VecDeque<RxId>,
    posted_rx: VecDeque<PostedRx>,
    rx_max: usize,

    posted_tx: VecDeque<PostedTx>,
    dropped_tx: VecDeque<TxId>,
    tx_max: usize,

    force_tx_header_bounce: bool,

    stats: QueueStats,

    #[cfg(test)]
    test_configuration: ManaTestConfiguration,
}

impl<T: DeviceBacking> Drop for ManaQueue<T> {
    fn drop(&mut self) {
        // Signal the endpoint when no more queues are active.
        if self.queue_tracker.0.fetch_sub(1, Ordering::AcqRel) == 1 {
            self.queue_tracker.1.signal();
        }
    }
}

struct PostedRx {
    id: RxId,
    wqe_len: u32,
    bounced_len_with_padding: u32,
    bounce_offset: u32,
}

struct PostedTx {
    id: TxId,
    wqe_len: u32,
    bounced_len_with_padding: u32,
}

#[derive(Default, Inspect)]
struct QueueStats {
    tx_events: Counter,
    tx_packets: Counter,
    tx_errors: Counter,
    tx_dropped: Counter,
    tx_stuck: Counter,

    rx_events: Counter,
    rx_packets: Counter,
    rx_errors: Counter,

    interrupts: Counter,

    tx_packets_coalesced: Counter,
}

impl<T: DeviceBacking> InspectMut for ManaQueue<T> {
    // N.B. Inspect fields need to be kept in sync with
    // Microsoft internal diagnostics testing.
    // Search for EXPECTED_QUEUE_FIELDS_V1.
    fn inspect_mut(&mut self, req: inspect::Request<'_>) {
        req.respond()
            .merge(&self.tx_bounce_buffer)
            .field("rx_bounce_buffer", &self.rx_bounce_buffer)
            .merge(&self.stats)
            .field("eq", &self.eq)
            .field("eq/armed", self.eq_armed)
            .field_mut("force_tx_header_bounce", &mut self.force_tx_header_bounce)
            .field("rx_wq", &self.rx_wq)
            .field("rx_cq", &self.rx_cq)
            .field("rx_cq/armed", self.rx_cq_armed)
            .field("tx_wq", &self.tx_wq)
            .field("tx_cq", &self.tx_cq)
            .field("tx_cq/armed", self.tx_cq_armed)
            .field("rx_queued", self.posted_rx.len())
            .field("rx_avail", self.avail_rx.len())
            .field("tx_queued", self.posted_tx.len());
    }
}

/// RWQEs cannot be larger than 256 bytes.
pub const MAX_RWQE_SIZE: u32 = 256;

/// SWQEs cannot be larger than 512 bytes.
pub const MAX_SWQE_SIZE: u32 = 512;

impl<T: DeviceBacking> ManaQueue<T> {
    fn push_rqe(&mut self) -> bool {
        // Make sure there is enough room for an entry of the maximum size. This
        // is conservative, but it simplifies the logic.
        if self.rx_wq.available() < MAX_RWQE_SIZE {
            return false;
        }
        if let Some(id) = self.avail_rx.pop_front() {
            let rx = if let Some(pool) = &mut self.rx_bounce_buffer {
                let size = self.pool.capacity(id);
                let mut pool_tx = pool.start_allocation();
                let Ok(buffer) = pool_tx.allocate(size) else {
                    self.avail_rx.push_front(id);
                    return false;
                };
                let buffer = buffer.reserve();
                let sqe = Sge {
                    address: buffer.gpa,
                    mem_key: self.mem_key,
                    size,
                };
                let wqe_len = self.rx_wq.push((), [sqe]).expect("rq should not be full");

                PostedRx {
                    id,
                    wqe_len,
                    bounce_offset: buffer.offset,
                    bounced_len_with_padding: pool_tx.commit(),
                }
            } else {
                let sgl = self.pool.guest_addresses(id).iter().map(|seg| Sge {
                    address: self.guest_memory.iova(seg.gpa).unwrap(),
                    mem_key: self.mem_key,
                    size: seg.len,
                });

                let wqe_len = self.rx_wq.push((), sgl).expect("rq should not be full");

                assert!(wqe_len <= MAX_RWQE_SIZE, "too many scatter/gather entries");
                PostedRx {
                    id,
                    wqe_len,
                    bounce_offset: 0,
                    bounced_len_with_padding: 0,
                }
            };

            self.posted_rx.push_back(rx);
            true
        } else {
            false
        }
    }

    fn trace_tx(
        &mut self,
        tracing_level: tracing::Level,
        cqe_params: CqeParams,
        tx_oob: ManaTxCompOob,
        done_length: usize,
    ) {
        tracelimit::event_ratelimited!(
            tracing_level,
            cqe_type = tx_oob.cqe_hdr.cqe_type(),
            vendor_err = tx_oob.cqe_hdr.vendor_err(),
            wq_number = cqe_params.wq_number(),
            tx_data_offset = tx_oob.tx_data_offset,
            tx_sgl_offset = tx_oob.offsets.tx_sgl_offset(),
            tx_wqe_offset = tx_oob.offsets.tx_wqe_offset(),
            done_length,
            posted_tx_len = self.posted_tx.len(),
            "tx completion error"
        );

        let wqe_offset = tx_oob.offsets.tx_wqe_offset();
        self.trace_tx_wqe_from_offset(tracing_level, wqe_offset);

        if let Some(packet) = self.posted_tx.front() {
            tracelimit::event_ratelimited!(
                tracing_level,
                id = packet.id.0,
                wqe_len = packet.wqe_len,
                bounced_len_with_padding = packet.bounced_len_with_padding,
                "posted tx"
            );
        }
    }

    fn trace_tx_wqe_from_offset(&mut self, tracing_level: tracing::Level, wqe_offset: u32) {
        let header_size = size_of::<WqeHeader>(); // 8 bytes
        let s_oob_size = size_of::<ManaTxShortOob>(); // 8 bytes
        let size = header_size + s_oob_size;
        let bytes = self.tx_wq.read(wqe_offset, size);
        let wqe_header = WqeHeader::read_from_prefix(&bytes);
        let wqe_header = match wqe_header {
            Ok((wqe_header, _)) => wqe_header,
            Err(_) => {
                tracelimit::error_ratelimited!(size, wqe_offset, "failed to read tx WQE header");
                return;
            }
        };

        tracelimit::event_ratelimited!(
            tracing_level,
            last_vbytes = wqe_header.last_vbytes,
            num_sgl_entries = wqe_header.params.num_sgl_entries(),
            inline_client_oob_size = wqe_header.params.inline_client_oob_size(),
            client_oob_in_sgl = wqe_header.params.client_oob_in_sgl(),
            reserved = wqe_header.params.reserved(),
            gd_client_unit_data = wqe_header.params.gd_client_unit_data(),
            reserved2 = wqe_header.params.reserved2(),
            sgl_direct = wqe_header.params.sgl_direct(),
            "tx wqe header"
        );

        let bytes = &bytes[header_size..];
        let tx_s_oob = ManaTxShortOob::read_from_prefix(bytes);
        match tx_s_oob {
            Ok((tx_s_oob, _)) => {
                tracelimit::event_ratelimited!(
                    tracing_level,
                    pkt_fmt = tx_s_oob.pkt_fmt(),
                    is_outer_ipv4 = tx_s_oob.is_outer_ipv4(),
                    is_outer_ipv6 = tx_s_oob.is_outer_ipv6(),
                    comp_iphdr_csum = tx_s_oob.comp_iphdr_csum(),
                    comp_tcp_csum = tx_s_oob.comp_tcp_csum(),
                    comp_udp_csum = tx_s_oob.comp_udp_csum(),
                    suppress_txcqe_gen = tx_s_oob.suppress_txcqe_gen(),
                    vcq_num = tx_s_oob.vcq_num(),
                    trans_off = tx_s_oob.trans_off(),
                    vsq_frame = tx_s_oob.vsq_frame(),
                    short_vp_offset = tx_s_oob.short_vp_offset(),
                    "tx s_oob"
                );
            }
            Err(_) => {
                tracelimit::error_ratelimited!("failed to read tx s_oob");
            }
        }
    }

    fn trace_rx_wqe_from_offset(&mut self, wqe_offset: u32) {
        let size = size_of::<WqeHeader>();
        let bytes = self.rx_wq.read(wqe_offset, size);
        let wqe_header = WqeHeader::read_from_prefix(&bytes);
        let wqe_header = match wqe_header {
            Ok((wqe_header, _)) => wqe_header,
            Err(_) => {
                tracelimit::error_ratelimited!(size, wqe_offset, "failed to parse rx WQE header");
                return;
            }
        };

        tracelimit::error_ratelimited!(
            num_sgl_entries = wqe_header.params.num_sgl_entries(),
            inline_client_oob_size = wqe_header.params.inline_client_oob_size(),
            reserved = wqe_header.params.reserved(),
            gd_client_unit_data = wqe_header.params.gd_client_unit_data(),
            reserved2 = wqe_header.params.reserved2(),
            sgl_offset = wqe_header.sgl_offset(),
            sgl_len = wqe_header.sgl_len(),
            "rx wqe header"
        );
    }
}

#[async_trait]
impl<T: DeviceBacking + Send> Queue for ManaQueue<T> {
    async fn update_target_vp(&mut self, target_vp: u32) {
        if let Some(vport) = self.vport.upgrade() {
            let result = vport.retarget_interrupt(self.eq.id(), target_vp).await;
            match result {
                Err(err) => {
                    tracing::warn!(
                        error = err.as_ref() as &dyn std::error::Error,
                        "failed to retarget interrupt to cpu"
                    );
                }
                Ok(None) => {}
                Ok(Some(event)) => self.interrupt = event,
            }
        }
    }

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<()> {
        if !self.tx_cq_armed || !self.rx_cq_armed {
            return Poll::Ready(());
        }

        loop {
            while let Some(eqe) = self.eq.pop() {
                self.eq_armed = false;
                match eqe.params.event_type() {
                    GDMA_EQE_COMPLETION => {
                        let cq_id =
                            u32::from_le_bytes(eqe.data[..4].try_into().unwrap()) & 0xffffff;
                        if cq_id == self.tx_cq.id() {
                            self.stats.tx_events.increment();
                            self.tx_cq_armed = false;
                        } else if cq_id == self.rx_cq.id() {
                            self.stats.rx_events.increment();
                            self.rx_cq_armed = false;
                        } else {
                            tracing::error!(cq_id, "unknown cq id");
                        }
                    }
                    ty => {
                        tracing::error!(ty, "unknown completion type")
                    }
                }
            }

            if !self.tx_cq_armed || !self.rx_cq_armed {
                // When the vp count exceeds the number of queues, the event queue can easily
                // overflow when not ACK'ed prior to arming the CQ
                self.eq.ack();
                return Poll::Ready(());
            }

            if !self.eq_armed {
                self.eq.arm();
                self.eq_armed = true;
            }
            std::task::ready!(self.interrupt.poll(cx));

            self.stats.interrupts.increment();
        }
    }

    fn rx_avail(&mut self, done: &[RxId]) {
        self.avail_rx.extend(done);
        let mut commit = false;
        while self.posted_rx.len() < self.rx_max && self.push_rqe() {
            commit = true;
        }
        if commit {
            self.rx_wq.commit();
        }
    }

    fn rx_poll(&mut self, packets: &mut [RxId]) -> anyhow::Result<usize> {
        let mut i = 0;
        let mut commit = false;
        while i < packets.len() {
            if let Some(cqe) = self.rx_cq.pop() {
                let rx = self.posted_rx.pop_front().unwrap();
                let rx_oob = ManaRxcompOob::read_from_prefix(&cqe.data[..]).unwrap().0; // TODO: zerocopy: use-rest-of-range (https://github.com/microsoft/openvmm/issues/759)
                match rx_oob.cqe_hdr.cqe_type() {
                    CQE_RX_OKAY => {
                        let ip_checksum = if rx_oob.flags.rx_iphdr_csum_succeed() {
                            RxChecksumState::Good
                        } else if rx_oob.flags.rx_iphdr_csum_fail() {
                            RxChecksumState::Bad
                        } else {
                            RxChecksumState::Unknown
                        };
                        let (l4_protocol, l4_checksum) = if rx_oob.flags.rx_tcp_csum_succeed() {
                            (L4Protocol::Tcp, RxChecksumState::Good)
                        } else if rx_oob.flags.rx_tcp_csum_fail() {
                            (L4Protocol::Tcp, RxChecksumState::Bad)
                        } else if rx_oob.flags.rx_udp_csum_succeed() {
                            (L4Protocol::Udp, RxChecksumState::Good)
                        } else if rx_oob.flags.rx_udp_csum_fail() {
                            (L4Protocol::Udp, RxChecksumState::Bad)
                        } else {
                            (L4Protocol::Unknown, RxChecksumState::Unknown)
                        };
                        let len = rx_oob.ppi[0].pkt_len.into();
                        self.pool.write_header(
                            rx.id,
                            &RxMetadata {
                                offset: 0,
                                len,
                                ip_checksum,
                                l4_checksum,
                                l4_protocol,
                            },
                        );
                        if rx.bounced_len_with_padding > 0 {
                            // TODO: avoid this allocation by updating
                            // write_data to take a slice of shared memory.
                            let mut data = vec![0; len];
                            self.rx_bounce_buffer.as_mut().unwrap().as_slice()
                                [rx.bounce_offset as usize..][..len]
                                .atomic_read(&mut data);
                            self.pool.write_data(rx.id, &data);
                        }
                        self.stats.rx_packets.increment();
                        packets[i] = rx.id;
                        i += 1;
                    }
                    ty => {
                        tracelimit::error_ratelimited!(
                            ty,
                            vendor_err = rx_oob.cqe_hdr.vendor_err(),
                            rx_cq_id = self.rx_cq.id(),
                            rx_wq_id = self.rx_wq.id(),
                            "invalid rx cqe type"
                        );
                        self.trace_rx_wqe_from_offset(rx_oob.rx_wqe_offset);
                        self.stats.rx_errors.increment();
                        self.avail_rx.push_back(rx.id);
                    }
                }
                self.rx_wq.advance_head(rx.wqe_len);
                if rx.bounced_len_with_padding > 0 {
                    self.rx_bounce_buffer
                        .as_mut()
                        .unwrap()
                        .free(rx.bounced_len_with_padding);
                }
                // Replenish the rq, if possible.
                commit |= self.push_rqe();
            } else {
                if !self.rx_cq_armed {
                    self.rx_cq.arm();
                    self.rx_cq_armed = true;
                }
                break;
            }
        }
        if commit {
            self.rx_wq.commit();
        }
        Ok(i)
    }

    fn tx_avail(&mut self, segments: &[TxSegment]) -> anyhow::Result<(bool, usize)> {
        let mut i = 0;
        let mut commit = false;
        while i < segments.len()
            && self.posted_tx.len() < self.tx_max
            && self.tx_wq.available() >= MAX_SWQE_SIZE
        {
            let head = &segments[i];
            let TxSegmentType::Head(meta) = &head.ty else {
                unreachable!()
            };

            if let Some(tx) = self.handle_tx(&segments[i..i + meta.segment_count as usize])? {
                commit = true;
                self.posted_tx.push_back(tx);
            } else {
                self.dropped_tx.push_back(meta.id);
            }
            i += meta.segment_count as usize;
        }

        if commit {
            self.tx_wq.commit();
        }
        Ok((false, i))
    }

    fn tx_poll(&mut self, done: &mut [TxId]) -> Result<usize, TxError> {
        let mut i = 0;
        while i < done.len() {
            let id = if let Some(cqe) = self.tx_cq.pop() {
                let tx_oob = ManaTxCompOob::read_from_prefix(&cqe.data[..]).unwrap().0; // TODO: zerocopy: use-rest-of-range (https://github.com/microsoft/openvmm/issues/759)
                match tx_oob.cqe_hdr.cqe_type() {
                    CQE_TX_OKAY => {
                        self.stats.tx_packets.increment();
                    }
                    CQE_TX_GDMA_ERR => {
                        // Hardware hit an error with the packet coming from the Guest.
                        // CQE_TX_GDMA_ERR is how the Hardware indicates that it has disabled the queue.
                        self.stats.tx_errors.increment();
                        self.stats.tx_stuck.increment();
                        self.trace_tx(tracing::Level::ERROR, cqe.params, tx_oob, done.len());
                        // Return a TryRestart error to indicate that the queue needs to be restarted.
                        return Err(TxError::TryRestart(anyhow::anyhow!("TX GDMA error")));
                    }
                    CQE_TX_INVALID_OOB => {
                        // Invalid OOB means the metadata didn't match how the Hardware parsed the packet.
                        // This is somewhat common, usually due to Encapsulation, and only the affects the specific packet.
                        self.stats.tx_errors.increment();
                        self.trace_tx(tracing::Level::WARN, cqe.params, tx_oob, done.len());
                    }
                    ty => {
                        tracelimit::error_ratelimited!(
                            ty,
                            vendor_error = tx_oob.cqe_hdr.vendor_err(),
                            "tx completion error"
                        );
                        self.stats.tx_errors.increment();
                    }
                }
                let packet = self.posted_tx.pop_front().unwrap();
                self.tx_wq.advance_head(packet.wqe_len);
                if packet.bounced_len_with_padding > 0 {
                    self.tx_bounce_buffer.free(packet.bounced_len_with_padding);
                }
                packet.id
            } else if let Some(id) = self.dropped_tx.pop_front() {
                self.stats.tx_dropped.increment();
                id
            } else {
                if !self.tx_cq_armed {
                    self.tx_cq.arm();
                    self.tx_cq_armed = true;
                }
                break;
            };

            done[i] = id;
            i += 1;
        }
        Ok(i)
    }

    fn buffer_access(&mut self) -> Option<&mut dyn BufferAccess> {
        Some(self.pool.as_mut())
    }

    fn queue_stats(&self) -> Option<&dyn BackendQueueStats> {
        Some(&self.stats)
    }
}

impl BackendQueueStats for QueueStats {
    fn rx_errors(&self) -> Counter {
        self.rx_errors.clone()
    }
    fn tx_errors(&self) -> Counter {
        self.tx_errors.clone()
    }
    fn rx_packets(&self) -> Counter {
        self.rx_packets.clone()
    }
    fn tx_packets(&self) -> Counter {
        self.tx_packets.clone()
    }
}

impl<T: DeviceBacking> ManaQueue<T> {
    fn handle_tx(&mut self, segments: &[TxSegment]) -> anyhow::Result<Option<PostedTx>> {
        let head = &segments[0];
        let TxSegmentType::Head(meta) = &head.ty else {
            unreachable!()
        };

        let mut oob = ManaTxOob::new_zeroed();
        oob.s_oob.set_vcq_num(self.tx_cq.id());
        oob.s_oob
            .set_vsq_frame((self.tx_wq.id() >> 10) as u16 & 0x3fff);

        oob.s_oob.set_is_outer_ipv4(meta.flags.is_ipv4());
        oob.s_oob.set_is_outer_ipv6(meta.flags.is_ipv6());
        oob.s_oob
            .set_comp_iphdr_csum(meta.flags.offload_ip_header_checksum());
        oob.s_oob
            .set_comp_tcp_csum(meta.flags.offload_tcp_checksum());
        oob.s_oob
            .set_comp_udp_csum(meta.flags.offload_udp_checksum());
        if meta.flags.offload_tcp_checksum() {
            oob.s_oob.set_trans_off(meta.l2_len as u16 + meta.l3_len);
        }
        let short_format = self.vp_offset <= 0xff;
        if short_format {
            oob.s_oob.set_pkt_fmt(MANA_SHORT_PKT_FMT);
            oob.s_oob.set_short_vp_offset(self.vp_offset as u8);
        } else {
            oob.s_oob.set_pkt_fmt(MANA_LONG_PKT_FMT);
            oob.l_oob.set_long_vp_offset(self.vp_offset);
        }
        let mut builder = if short_format {
            self.tx_wq.wqe_builder(oob.s_oob)
        } else {
            self.tx_wq.wqe_builder(oob)
        };

        let mut bounce_buffer = self.tx_bounce_buffer.start_allocation();
        if self.rx_bounce_buffer.is_some() {
            assert!(!meta.flags.offload_tcp_segmentation());
            let mut buf = match bounce_buffer.allocate(meta.len) {
                Ok(buf) => buf,
                Err(err) => {
                    tracelimit::error_ratelimited!(
                        err = &err as &dyn std::error::Error,
                        meta.len,
                        "failed to bounce buffer"
                    );
                    // Drop the packet
                    return Ok(None);
                }
            };
            let mut next = buf.as_slice();
            for seg in segments {
                let len = seg.len as usize;
                self.guest_memory.read_to_atomic(seg.gpa, &next[..len])?;
                next = &next[len..];
            }
            let buf = buf.reserve();
            let sge = Sge {
                address: buf.gpa,
                mem_key: self.mem_key,
                size: meta.len,
            };
            builder.push_sge(sge);
        } else {
            let (segments, segment_offset) = if meta.flags.offload_tcp_segmentation() {
                // For LSO, GDMA requires that SGE0 should only contain the header.
                let header_len = (meta.l2_len as u16 + meta.l3_len + meta.l4_len as u16) as u32;
                if header_len > PAGE_SIZE32 {
                    tracelimit::error_ratelimited!(
                        header_len,
                        "Header larger than PAGE_SIZE unsupported"
                    );
                    // Drop the packet
                    return Ok(None);
                }
                builder.set_client_oob_in_sgl(header_len as u8);
                builder.set_gd_client_unit_data(meta.max_tcp_segment_size);

                let (head_iova, used_segments, used_segments_len) =
                    if header_len > head.len || self.force_tx_header_bounce {
                        let mut copy = match bounce_buffer.allocate(header_len) {
                            Ok(buf) => buf,
                            Err(err) => {
                                tracelimit::error_ratelimited!(
                                    err = &err as &dyn std::error::Error,
                                    header_len,
                                    "Failed to bounce buffer split header"
                                );
                                // Drop the packet
                                return Ok(None);
                            }
                        };

                        let mut data = copy.as_slice();
                        let mut used_segments = 0;
                        let mut used_segments_len = 0;
                        for segment in segments {
                            let (this, rest) = data.split_at(data.len().min(segment.len as usize));
                            self.guest_memory.read_to_atomic(segment.gpa, this)?;
                            data = rest;
                            if this.len() < segment.len as usize {
                                break;
                            }
                            used_segments += 1;
                            used_segments_len += segment.len;
                        }
                        if !data.is_empty() {
                            tracelimit::error_ratelimited!(
                                header_len,
                                missing_header_bytes = data.len(),
                                "Invalid split header"
                            );
                            // Drop the packet
                            return Ok(None);
                        };
                        let ContiguousBufferInUse { gpa, .. } = copy.reserve();
                        (gpa, used_segments, used_segments_len)
                    } else if header_len < head.len {
                        (self.guest_memory.iova(head.gpa).unwrap(), 0, 0)
                    } else {
                        (self.guest_memory.iova(head.gpa).unwrap(), 1, header_len)
                    };

                // Drop the LSO packet if it only has a header segment.
                // In production builds, this check always runs.
                // For tests, use test hooks to bypass this check for allowing code coverage.
                #[cfg(not(test))]
                let check_lso_segment_count = true;
                #[cfg(test)]
                let check_lso_segment_count = !self.test_configuration.allow_lso_pkt_with_one_sge;
                if check_lso_segment_count && used_segments == segments.len() {
                    return Ok(None);
                }

                // With LSO, GDMA requires that the first segment should only contain
                // the header and should not exceed 256 bytes. Otherwise, it treats
                // the WQE as "corrupt", disables the queue and return GDMA error.
                builder.push_sge(Sge {
                    address: head_iova,
                    mem_key: self.mem_key,
                    size: header_len,
                });
                (&segments[used_segments..], header_len - used_segments_len)
            } else {
                // Just send the segments as they are.
                (segments, 0)
            };

            // Hardware limit for short oob is 31. Max WQE size is 512 bytes.
            // Hardware limit for long oob is 30.
            let hardware_segment_limit = if short_format { 31 } else { 30 };
            let segment_count = builder.sge_count() + segments.len() as u8;
            if segment_count <= hardware_segment_limit {
                let mut segment_offset = segment_offset;
                for tail in segments {
                    builder.push_sge(Sge {
                        address: self
                            .guest_memory
                            .iova(tail.gpa.wrapping_add(segment_offset.into()))
                            .unwrap(),
                        mem_key: self.mem_key,
                        size: tail.len.wrapping_sub(segment_offset),
                    });
                    segment_offset = 0;
                }
            } else {
                let gpa0 = segments[0].gpa.wrapping_add(segment_offset.into());
                let mut sge = Sge {
                    address: self.guest_memory.iova(gpa0).unwrap(),
                    mem_key: self.mem_key,
                    size: segments[0].len.wrapping_sub(segment_offset),
                };

                let mut last_segment_bounced = false;
                let mut segment_count = segment_count;
                let mut last_segment_gpa = gpa0;
                for tail in &segments[1..] {
                    // Try to coalesce segments together if there are more than the hardware allows.
                    // TODO: Could use more expensive techniques such as
                    //       copying portions of segments to fill an entire
                    //       bounce page if the simple algorithm of coalescing
                    //       full segments together fails.
                    // TODO: If the header was not bounced, we could search the segments for the
                    //       longest sequence that can be coalesced, instead of the first sequence.
                    let coalesce_possible = sge.size + tail.len < PAGE_SIZE32;
                    if segment_count > hardware_segment_limit {
                        if !last_segment_bounced
                            && coalesce_possible
                            && bounce_buffer.allocate(sge.size + tail.len).is_ok()
                        {
                            // There is enough room to coalesce the current
                            // segment with the previous. The previous segment
                            // is not yet bounced, so bounce it now.
                            let mut copy = bounce_buffer.allocate(sge.size).unwrap();
                            self.guest_memory
                                .read_to_atomic(last_segment_gpa, copy.as_slice())?;
                            let ContiguousBufferInUse { gpa, .. } = copy.reserve();
                            sge.address = gpa;
                            last_segment_bounced = true;
                        }
                        if last_segment_bounced {
                            if let Some(mut copy) = bounce_buffer.try_extend(tail.len) {
                                // Combine current segment with previous one using bounce buffer.
                                self.guest_memory
                                    .read_to_atomic(tail.gpa, copy.as_slice())?;
                                let ContiguousBufferInUse {
                                    len_with_padding, ..
                                } = copy.reserve();
                                assert_eq!(tail.len, len_with_padding);
                                sge.size += len_with_padding;
                                segment_count -= 1;
                                continue;
                            }
                        }
                        last_segment_bounced = false;
                    }

                    builder.push_sge(sge);
                    if builder.sge_count() == hardware_segment_limit {
                        tracelimit::error_ratelimited!(
                            segments_remaining = segment_count - builder.sge_count(),
                            hardware_segment_limit,
                            "Failed to bounce buffer the packet too many segments"
                        );
                        // Drop the packet, no need to free bounce buffer
                        return Ok(None);
                    }

                    sge = Sge {
                        address: self.guest_memory.iova(tail.gpa).unwrap(),
                        mem_key: self.mem_key,
                        size: tail.len,
                    };
                    last_segment_gpa = tail.gpa;
                }
                builder.push_sge(sge);
                self.stats.tx_packets_coalesced.increment();
            };

            assert!(builder.sge_count() <= hardware_segment_limit);
        }

        let wqe_len = builder
            .finish()
            .expect("caller ensured enough space for a max sized WQE");
        let tx = PostedTx {
            id: meta.id,
            wqe_len,
            bounced_len_with_padding: bounce_buffer.commit(),
        };
        Ok(Some(tx))
    }
}

struct ContiguousBufferInUse {
    pub gpa: u64,
    pub offset: u32,
    pub len_with_padding: u32,
}

struct ContiguousBuffer<'a, 'b> {
    parent: &'a mut ContiguousBufferManagerTransaction<'b>,
    offset: u32,
    len: u32,
    padding_len: u32,
}

impl<'a, 'b> ContiguousBuffer<'a, 'b> {
    pub fn new(
        parent: &'a mut ContiguousBufferManagerTransaction<'b>,
        offset: u32,
        len: u32,
        padding_len: u32,
    ) -> Self {
        Self {
            parent,
            offset,
            len,
            padding_len,
        }
    }

    pub fn as_slice(&mut self) -> &[AtomicU8] {
        &self.parent.as_slice()[self.offset as usize..(self.offset + self.len) as usize]
    }

    pub fn reserve(self) -> ContiguousBufferInUse {
        let page = self.offset / PAGE_SIZE32;
        let offset_in_page = self.offset - page * PAGE_SIZE32;
        let gpa = self.parent.page_gpa(page as usize) + offset_in_page as u64;
        let len_with_padding = self.len + self.padding_len;
        self.parent.head = self.parent.head.wrapping_add(len_with_padding);
        ContiguousBufferInUse {
            gpa,
            offset: self.offset,
            len_with_padding,
        }
    }
}

struct ContiguousBufferManagerTransaction<'a> {
    parent: &'a mut ContiguousBufferManager,
    pub head: u32,
}

impl<'a> ContiguousBufferManagerTransaction<'a> {
    pub fn new(parent: &'a mut ContiguousBufferManager) -> Self {
        let head = parent.head;
        Self { parent, head }
    }

    /// Allocates from next section of available ring buffer.
    pub fn allocate<'b>(&'b mut self, len: u32) -> Result<ContiguousBuffer<'b, 'a>, OutOfMemory> {
        assert!(len < PAGE_SIZE32);
        let mut len_with_padding = len;
        let mut allocated_offset = self.head;
        let bytes_remaining_on_page = PAGE_SIZE32 - (self.head & (PAGE_SIZE32 - 1));
        if len > bytes_remaining_on_page {
            allocated_offset = allocated_offset.wrapping_add(bytes_remaining_on_page);
            len_with_padding += bytes_remaining_on_page;
        }
        if len_with_padding > self.parent.tail.wrapping_sub(self.head) {
            self.parent.failed_allocations += 1;
            return Err(OutOfMemory);
        }
        Ok(ContiguousBuffer::new(
            self,
            allocated_offset % self.parent.len,
            len,
            len_with_padding - len,
        ))
    }

    pub fn try_extend<'b>(&'b mut self, len: u32) -> Option<ContiguousBuffer<'b, 'a>> {
        let bytes_remaining_on_page = PAGE_SIZE32 - (self.head & (PAGE_SIZE32 - 1));
        if bytes_remaining_on_page == PAGE_SIZE32 {
            // Used the entire previous page. Cannot extend onto a new page.
            return None;
        }
        if len <= bytes_remaining_on_page {
            self.allocate(len).ok()
        } else {
            None
        }
    }

    pub fn commit(self) -> u32 {
        self.parent.split_headers += 1;
        let len_with_padding = self.head.wrapping_sub(self.parent.head);
        self.parent.head = self.head;
        len_with_padding
    }

    pub fn as_slice(&self) -> &[AtomicU8] {
        self.parent.as_slice()
    }

    pub fn page_gpa(&self, page_idx: usize) -> u64 {
        self.parent.mem.pfns()[page_idx] * PAGE_SIZE64
    }
}

struct ContiguousBufferManager {
    len: u32,
    head: u32,
    tail: u32,
    mem: MemoryBlock,
    // Counters
    split_headers: u64,
    failed_allocations: u64,
}

#[derive(Debug, Error)]
#[error("out of bounce buffer memory")]
struct OutOfMemory;

impl ContiguousBufferManager {
    pub fn new(dma_client: Arc<dyn DmaClient>, page_limit: u32) -> anyhow::Result<Self> {
        let len = PAGE_SIZE32 * page_limit;
        let mem = dma_client.allocate_dma_buffer(len as usize)?;
        Ok(Self {
            len,
            head: 0,
            tail: len - 1,
            mem,
            split_headers: 0,
            failed_allocations: 0,
        })
    }

    pub fn start_allocation(&mut self) -> ContiguousBufferManagerTransaction<'_> {
        ContiguousBufferManagerTransaction::new(self)
    }

    /// Frees oldest reserved range by advancing the tail of the ring buffer to
    /// account for that range. This requires entries to be consumed FIFO.
    pub fn free(&mut self, len_with_padding: u32) {
        self.tail = self.tail.wrapping_add(len_with_padding);
    }

    pub fn as_slice(&self) -> &[AtomicU8] {
        self.mem.as_slice()
    }
}

impl Inspect for ContiguousBufferManager {
    fn inspect(&self, req: inspect::Request<'_>) {
        req.respond()
            .counter("split_headers", self.split_headers)
            .counter("failed_allocations", self.failed_allocations);
    }
}
