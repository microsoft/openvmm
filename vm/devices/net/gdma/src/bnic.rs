// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use self::bnic_defs::CQE_RX_TRUNCATED;
use self::bnic_defs::CQE_TX_GDMA_ERR;
use self::bnic_defs::CQE_TX_INVALID_OOB;
use self::bnic_defs::CQE_TX_OKAY;
use self::bnic_defs::MANA_CQE_COMPLETION;
use self::bnic_defs::MANA_LONG_PKT_FMT;
use self::bnic_defs::ManaCommandCode;
use self::bnic_defs::ManaCqeHeader;
use self::bnic_defs::ManaQueryVportCfgReq;
use self::bnic_defs::ManaRxcompOob;
use self::bnic_defs::ManaRxcompOobFlags;
use self::bnic_defs::ManaSetVportSerialNo;
use self::bnic_defs::ManaTxCompOob;
use self::bnic_defs::ManaTxCompOobOffsets;
use crate::VportConfig;
use crate::bnic::bnic_defs::CQE_RX_OKAY;
use crate::bnic::bnic_defs::ManaCfgRxSteerReq;
use crate::bnic::bnic_defs::ManaConfigVportReq;
use crate::bnic::bnic_defs::ManaConfigVportResp;
use crate::bnic::bnic_defs::ManaCreateWqobjReq;
use crate::bnic::bnic_defs::ManaCreateWqobjResp;
use crate::bnic::bnic_defs::ManaQueryDeviceCfgReq;
use crate::bnic::bnic_defs::ManaQueryDeviceCfgResp;
use crate::bnic::bnic_defs::ManaQueryVportCfgResp;
use crate::bnic::bnic_defs::ManaTxOob;
use crate::hwc::HwState;
use crate::queues::Queues;
use anyhow::Context;
use anyhow::anyhow;
use gdma_defs::GdmaQueueType;
use gdma_defs::GdmaReqHdr;
use gdma_defs::Sge;
use gdma_defs::Wqe;
use gdma_defs::access::WqeAccess;
use gdma_defs::bnic as bnic_defs;
use gdma_defs::bnic::ManaDestroyWqobjReq;
use gdma_defs::bnic::ManaTxShortOob;
use gdma_defs::bnic::Tristate;
use guestmem::GuestMemory;
use guestmem::Limit;
use guestmem::MemoryRead;
use guestmem::MemoryWrite;
use inspect::InspectMut;
use net_backend::BufferAccess;
use net_backend::Endpoint;
use net_backend::Queue;
use net_backend::QueueConfig;
use net_backend::RxBufferSegment;
use net_backend::RxChecksumState;
use net_backend::RxId;
use net_backend::RxMetadata;
use net_backend::TxFlags;
use net_backend::TxId;
use net_backend::TxMetadata;
use net_backend::TxSegment;
use net_backend::TxSegmentType;
use net_backend_resources::mac_address::MacAddress;
use slab::Slab;
use std::collections::VecDeque;
use std::future::poll_fn;
use std::sync::Arc;
use std::task::Poll;
use task_control::AsyncRun;
use task_control::InspectTaskMut;
use task_control::StopTask;
use task_control::TaskControl;
use zerocopy::FromBytes;
use zerocopy::FromZeros;
use zerocopy::IntoBytes;

pub struct GuestBuffers {
    gm: GuestMemory,
    rx_packets: Slab<RxPacket>,
}

struct RxPacket {
    segments: Vec<RxBufferSegment>,
    len: u32,
    wqe_offset: u32,
    oob: ManaRxcompOob,
}

impl BufferAccess for GuestBuffers {
    fn guest_memory(&self) -> &GuestMemory {
        &self.gm
    }

    fn write_data(&mut self, id: RxId, mut data: &[u8]) {
        let mut addrs = self.rx_packets[id.0 as usize].segments.iter();
        while !data.is_empty() {
            let Some(addr) = addrs.next() else {
                // Packet exceeds buffer capacity; will be reported as
                // CQE_RX_TRUNCATED by write_header.
                break;
            };
            let len = data.len().min(addr.len as usize);
            let (this, next) = data.split_at(len);
            if let Err(err) = self.gm.write_at(addr.gpa, this) {
                tracing::warn!(
                    gpa = addr.gpa,
                    len,
                    error = &err as &dyn std::error::Error,
                    "rx memory write failure"
                );
            }
            data = next;
        }
    }

    fn push_guest_addresses(&self, id: RxId, buf: &mut Vec<RxBufferSegment>) {
        buf.extend_from_slice(&self.rx_packets[id.0 as usize].segments);
    }

    fn capacity(&self, id: RxId) -> u32 {
        self.rx_packets[id.0 as usize].len
    }

    fn write_header(&mut self, id: RxId, metadata: &RxMetadata) {
        assert_eq!(metadata.offset, 0);

        let mut flags = ManaRxcompOobFlags::new();
        match metadata.ip_checksum {
            RxChecksumState::Unknown => {}
            RxChecksumState::Good => flags.set_rx_iphdr_csum_succeed(true),
            RxChecksumState::Bad => flags.set_rx_iphdr_csum_fail(true),
            RxChecksumState::ValidatedButWrong => {}
        }
        match metadata.l4_protocol {
            net_backend::L4Protocol::Unknown => {}
            net_backend::L4Protocol::Tcp => match metadata.l4_checksum {
                RxChecksumState::Unknown => {}
                RxChecksumState::Good => flags.set_rx_tcp_csum_succeed(true),
                RxChecksumState::Bad => flags.set_rx_tcp_csum_fail(true),
                RxChecksumState::ValidatedButWrong => {}
            },
            net_backend::L4Protocol::Udp => match metadata.l4_checksum {
                RxChecksumState::Unknown => {}
                RxChecksumState::Good => flags.set_rx_udp_csum_succeed(true),
                RxChecksumState::Bad => flags.set_rx_udp_csum_fail(true),
                RxChecksumState::ValidatedButWrong => {}
            },
        }

        if let Some(vlan) = &metadata.vlan {
            flags.set_rx_vlantag_present(true);
            flags.set_rx_vlan_id(vlan.vlan_id() as u32);
        }

        let packet = &mut self.rx_packets[id.0 as usize];

        let cqe_type = if metadata.len > packet.len as usize {
            CQE_RX_TRUNCATED
        } else {
            CQE_RX_OKAY
        };

        packet.oob = ManaRxcompOob {
            cqe_hdr: ManaCqeHeader::new()
                .with_cqe_type(cqe_type)
                .with_client_type(MANA_CQE_COMPLETION),
            rx_wqe_offset: packet.wqe_offset,
            flags,
            ..FromZeros::new_zeroed()
        };
        packet.oob.ppi[0].pkt_len = metadata.len as u16;
    }
}

/// Configuration for the emulated BNIC device.
#[derive(Default)]
pub struct BnicConfig {
    /// Adapter link speed in megabits per second.
    pub adapter_link_speed_mbps: u32,
}

pub struct BasicNic {
    vports: Vec<Vport>,
    config: BnicConfig,
}

impl InspectMut for BasicNic {
    fn inspect_mut(&mut self, req: inspect::Request<'_>) {
        req.respond()
            .fields_mut("vports", self.vports.iter_mut().enumerate());
    }
}

struct Vport {
    mac_address: MacAddress,
    endpoint: Box<dyn Endpoint>,
    task: TaskControl<TxRxState, TxRxTask>,
    queue_cfg: QueueCfg,
    serial_no: u32,
}

impl InspectMut for Vport {
    fn inspect_mut(&mut self, req: inspect::Request<'_>) {
        req.respond()
            .field("mac_address", self.mac_address)
            .field_mut("endpoint", self.endpoint.as_mut())
            .field("tx_wq", self.queue_cfg.tx.map(|(wq, _cq)| wq))
            .field("tx_cq", self.queue_cfg.tx.map(|(_wq, cq)| cq))
            .field("rx_wq", self.queue_cfg.rx.map(|(wq, _cq)| wq))
            .field("rx_cq", self.queue_cfg.rx.map(|(_wq, cq)| cq))
            .merge(&mut self.task);
    }
}

struct QueueCfg {
    tx: Option<(u32, u32)>,
    rx: Option<(u32, u32)>,
}

impl BasicNic {
    pub fn new(vports: Vec<VportConfig>, config: BnicConfig) -> Self {
        assert!(!vports.is_empty());

        let vports = vports
            .into_iter()
            .map(
                |VportConfig {
                     mac_address,
                     endpoint,
                 }| {
                    assert!(endpoint.is_ordered());
                    Vport {
                        mac_address,
                        endpoint,
                        task: TaskControl::new(TxRxState),
                        queue_cfg: QueueCfg { tx: None, rx: None },
                        serial_no: 0,
                    }
                },
            )
            .collect();

        Self { vports, config }
    }

    pub async fn handle_req(
        &mut self,
        state: &mut HwState,
        hdr: &GdmaReqHdr,
        mut read: Limit<WqeAccess<'_>>,
        mut write: Limit<WqeAccess<'_>>,
    ) -> anyhow::Result<usize> {
        tracing::debug!(msg_type = ?ManaCommandCode(hdr.req.msg_type), "bnic request");

        // Zero the guest response buffer before writing the actual response
        // to maintain forward compatibility: a newer VF driver may request
        // fields added in a later protocol version that this emulator does
        // not yet populate. Zeroing ensures those fields read as zero rather
        // than containing undefined data.
        let guest_resp_size = MemoryWrite::len(&write);
        let mut zero_write = write.clone();
        zero_write.write(&vec![0u8; guest_resp_size])?;

        match ManaCommandCode(hdr.req.msg_type) {
            ManaCommandCode::MANA_QUERY_DEV_CONFIG => {
                let _req: ManaQueryDeviceCfgReq = read
                    .read_plain()
                    .context("reading query dev config request")?;

                let resp = ManaQueryDeviceCfgResp {
                    pf_cap_flags1: 0.into(),
                    pf_cap_flags2: 0,
                    pf_cap_flags3: 0,
                    pf_cap_flags4: 0,
                    max_num_vports: self.vports.len() as u16,
                    reserved: 0,
                    max_num_eqs: 64,
                    adapter_mtu: 0,
                    reserved2: 0,
                    adapter_link_speed_mbps: self.config.adapter_link_speed_mbps,
                };

                let resp_bytes = resp.as_bytes();
                let write_len = guest_resp_size.min(resp_bytes.len());
                write.write(&resp_bytes[..write_len])?;
            }
            ManaCommandCode::MANA_CONFIG_VPORT_TX => {
                let req: ManaConfigVportReq = read
                    .read_plain()
                    .context("reading config vport tx request")?;
                let _vport = self
                    .vports
                    .get_mut(req.vport as usize)
                    .context("invalid vport")?;

                let resp = ManaConfigVportResp {
                    tx_vport_offset: 0,
                    short_form_allowed: 1,
                    reserved: 0,
                };
                write.write(resp.as_bytes())?;
            }
            ManaCommandCode::MANA_CREATE_WQ_OBJ => {
                let req: ManaCreateWqobjReq =
                    read.read_plain().context("reading create wq obj request")?;
                let vport = self
                    .vports
                    .get_mut(req.vport as usize)
                    .context("invalid vport")?;

                let is_send = match req.wq_type {
                    GdmaQueueType::GDMA_RQ => false,
                    GdmaQueueType::GDMA_SQ => true,
                    ty => anyhow::bail!("unsupported queue type: {:?}", ty),
                };

                if (is_send && vport.queue_cfg.tx.is_some())
                    || (!is_send && vport.queue_cfg.rx.is_some())
                {
                    anyhow::bail!("queue already created");
                }

                let wq_region = state.get_dma_region(req.wq_gdma_region, req.wq_size)?;
                let cq_region = state.get_dma_region(req.cq_gdma_region, req.cq_size)?;

                let wq_id = state
                    .queues
                    .alloc_wq(is_send, wq_region.clone())
                    .context("failed to allocate wq")?;

                let cq_id = state
                    .queues
                    .alloc_cq(cq_region.clone(), req.cq_parent_qid)
                    .context("failed to allocate cq")?;

                let resp = ManaCreateWqobjResp {
                    wq_id,
                    cq_id,
                    wq_obj: req.vport, // use the vport # as the handle
                };

                *if is_send {
                    &mut vport.queue_cfg.tx
                } else {
                    &mut vport.queue_cfg.rx
                } = Some((wq_id, cq_id));

                write.write(resp.as_bytes())?;

                // Take ownership of the DMA regions.
                state.remove_dma_region(req.wq_gdma_region).unwrap();
                state.remove_dma_region(req.cq_gdma_region).unwrap();
            }
            ManaCommandCode::MANA_DESTROY_WQ_OBJ => {
                let req: ManaDestroyWqobjReq = read
                    .read_plain()
                    .context("failed to read destroy wq obj request")?;
                let vport = self
                    .vports
                    .get_mut(req.wq_obj_handle as usize)
                    .context("invalid obj handle")?;

                if vport.task.has_state() {
                    anyhow::bail!("queue still in use");
                }
                let (is_send, queues) = match req.wq_type {
                    GdmaQueueType::GDMA_RQ => (false, &mut vport.queue_cfg.rx),
                    GdmaQueueType::GDMA_SQ => (true, &mut vport.queue_cfg.tx),
                    ty => anyhow::bail!("unsupported queue type: {:?}", ty),
                };
                let (wq_id, cq_id) = queues.take().context("specified queue does not exist")?;
                state.queues.free_wq(is_send, wq_id).unwrap();
                state.queues.free_cq(cq_id).unwrap();
            }
            ManaCommandCode::MANA_CONFIG_VPORT_RX => {
                let req: ManaCfgRxSteerReq = read
                    .read_plain()
                    .context("reading config vport rx request")?;
                tracing::debug!(?req, "rx config");
                let vport = self
                    .vports
                    .get_mut(req.vport as usize)
                    .context("invalid vport")?;

                match req.rx_enable {
                    Tristate::FALSE if vport.task.is_running() => {
                        vport.task.stop().await;
                        vport.task.remove();
                        vport.endpoint.stop().await;
                    }
                    Tristate::TRUE if !vport.task.is_running() => {
                        if let (Some((sq_id, sq_cq_id)), Some((rq_id, rq_cq_id))) =
                            (vport.queue_cfg.tx, vport.queue_cfg.rx)
                        {
                            let mut queues = vec![];
                            vport
                                .endpoint
                                .get_queues(
                                    vec![QueueConfig {
                                        driver: Box::new(state.queues.driver.clone()),
                                        enabled_rx_offloads: Default::default(),
                                    }],
                                    None,
                                    &mut queues,
                                )
                                .await?;

                            vport.task.insert(
                                &state.queues.driver,
                                "gdma-bnic",
                                TxRxTask {
                                    queues: state.queues.clone(),
                                    epqueue: queues.drain(..).next().unwrap(),
                                    pool: GuestBuffers {
                                        gm: state.queues.gm.clone(),
                                        rx_packets: Default::default(),
                                    },
                                    sq_id,
                                    sq_cq_id,
                                    rq_id,
                                    rq_cq_id,
                                    tx_segment_buffer: Vec::new(),
                                    rx_buf_count: 0,
                                    pending_tx: VecDeque::new(),
                                },
                            );
                            vport.task.start();
                        } else {
                            anyhow::bail!("queues not configured");
                        }
                    }
                    _ => {}
                }
            }
            ManaCommandCode::MANA_VTL2_MOVE_FILTER => {
                anyhow::bail!("unsupported command MANA_VTL2_MOVE_FILTER");
            }
            ManaCommandCode::MANA_VTL2_QUERY_FILTER_STATE => {
                let req: gdma_defs::bnic::ManaQueryFilterStateReq = read
                    .read_plain()
                    .context("reading query vport filter state request")?;
                let _ = self
                    .vports
                    .get_mut(req.vport as usize)
                    .context("invalid vport")?;

                let resp = gdma_defs::bnic::ManaQueryFilterStateResponse {
                    direction_to_vtl0: 0,
                    reserved: [0; 7],
                };

                write.write(resp.as_bytes())?;
            }
            ManaCommandCode::MANA_QUERY_VPORT_CONFIG => {
                let req: ManaQueryVportCfgReq = read
                    .read_plain()
                    .context("reading query vport config request")?;
                let vport = self
                    .vports
                    .get_mut(req.vport_index as usize)
                    .context("invalid vport")?;

                let resp = ManaQueryVportCfgResp {
                    max_num_sq: 1,
                    max_num_rq: 1,
                    num_indirection_ent: 128,
                    reserved1: 0,
                    mac_addr: vport.mac_address.to_bytes(),
                    reserved2: [0; 2],
                    vport: req.vport_index.into(),
                };

                write.write(resp.as_bytes())?;
            }
            ManaCommandCode::MANA_VTL2_ASSIGN_SERIAL_NUMBER => {
                let req: ManaSetVportSerialNo =
                    read.read_plain().context("set vport serial number")?;
                let vport = self
                    .vports
                    .get_mut(req.vport as usize)
                    .context("invalid vport")?;
                vport.serial_no = req.serial_no;
            }
            n => anyhow::bail!("unsupported request {:?}", n),
        }
        Ok(guest_resp_size)
    }
}

pub struct TxRxTask {
    queues: Arc<Queues>,
    epqueue: Box<dyn Queue>,
    pool: GuestBuffers,
    sq_id: u32,
    sq_cq_id: u32,
    rq_id: u32,
    rq_cq_id: u32,
    tx_segment_buffer: Vec<TxSegment>,
    rx_buf_count: u32,
    /// Sends not yet completed to the guest, oldest first.
    pending_tx: VecDeque<PendingTx>,
}

/// A send whose completion has not been posted yet.
#[derive(Debug, Clone, Copy)]
enum PendingTx {
    /// The backend still has it.
    InBackend,
    /// Finished with this completion type, behind a send the backend still has.
    Done(u8),
}

impl InspectTaskMut<TxRxTask> for TxRxState {
    fn inspect_mut(&mut self, req: inspect::Request<'_>, task: Option<&mut TxRxTask>) {
        let mut resp = req.respond();
        if let Some(task) = task {
            resp.field_mut("queue", &mut task.epqueue)
                .field("rx_bufs", task.pool.rx_packets.len());
        }
    }
}

impl TxRxTask {
    async fn process(
        &mut self,
        check: &task_control::StopCheck<'_>,
    ) -> Result<(), task_control::Cancelled> {
        let max_rx_buf = 256;

        enum Event {
            Sqe(Wqe),
            Rqe(u32, Wqe),
            Ready,
        }

        loop {
            check.check()?;
            let event = poll_fn(|cx| {
                // Fill rx before transmitting to avoid rx buffer starvation
                // (particularly in tests, but seems reasonable in general).
                if self.rx_buf_count < max_rx_buf {
                    if let Poll::Ready((wqe_offset, wqe)) = self.queues.poll_rq(self.rq_id, cx) {
                        self.rx_buf_count += 1;
                        return Poll::Ready(Event::Rqe(wqe_offset, wqe));
                    }
                }
                if self.pending_tx.len() < MAX_PENDING_TX {
                    if let Poll::Ready(wqe) = self.queues.poll_sq(self.sq_id, cx) {
                        return Poll::Ready(Event::Sqe(wqe));
                    }
                }
                if self.epqueue.poll_ready(cx, &mut self.pool).is_ready() {
                    return Poll::Ready(Event::Ready);
                }
                Poll::Pending
            })
            .await;
            let result = match event {
                Event::Sqe(sqe) => self.process_sqe(sqe),
                Event::Rqe(wqe_offset, wqe) => self.process_rqe(wqe, wqe_offset),
                Event::Ready => self.process_backend(),
            };
            if let Err(err) = result {
                tracing::error!(err = err.as_ref() as &dyn std::error::Error, "bnic failure");
                return Ok(());
            }
        }
    }

    fn process_sqe(&mut self, sqe: Wqe) -> anyhow::Result<()> {
        tracing::trace!("tx wqe");
        let oob = sqe.oob();
        let oob = if oob.len() >= size_of::<ManaTxOob>() {
            ManaTxOob::read_from_prefix(oob).unwrap().0
        } else {
            ManaTxOob {
                // TODO: zerocopy: use details from SizeError in the returned context (https://github.com/microsoft/openvmm/issues/759)
                s_oob: ManaTxShortOob::read_from_prefix(oob)
                    .map_err(|_| anyhow!("oob too small"))?
                    .0,
                ..FromZeros::new_zeroed()
            }
        };

        let sge0 = sqe.sgl().first().context("no sgl")?;
        let total_len: usize = sqe.sgl().iter().map(|sge| sge.size as usize).sum();
        let (l2_len, vlan) =
            if oob.s_oob.pkt_fmt() == MANA_LONG_PKT_FMT && oob.l_oob.inject_vlan_pri_tag() {
                (
                    net_backend::ETHERNET_VLAN_HEADER_LEN,
                    Some(
                        net_backend::VlanMetadata::new()
                            .with_priority(oob.l_oob.pcp())
                            .with_drop_eligible_indicator(oob.l_oob.dei())
                            .with_vlan_id(oob.l_oob.vlan_id()),
                    ),
                )
            } else {
                (net_backend::ETHERNET_HEADER_LEN, None)
            };

        let lso = sqe.header.params.client_oob_in_sgl();
        // A checksum send gets its header layout from the frame, the way the NIC parses
        // it; see `checksum_send_headers`. Every other send keeps the driver's `trans_off`.
        let (l2_len, trans_off) =
            if !lso && (oob.s_oob.comp_tcp_csum() || oob.s_oob.comp_udp_csum()) {
                let mut headers = [0; MAX_CHECKSUM_HEADER_LEN];
                let read = read_frame_prefix(self.pool.guest_memory(), sqe.sgl(), &mut headers);
                match checksum_send_headers(&headers[..read], &oob.s_oob) {
                    // An injected tag is not in the frame the backend gets, so the offsets
                    // are the frame's own: the backend fixes up the IPv4 header at `l2_len`.
                    Ok(layout) => (layout.l2_len, layout.transport_offset()),
                    Err(err) => {
                        // The NIC completes a send whose OOB does not match how it parsed
                        // the frame with CQE_TX_INVALID_OOB, which costs that packet only.
                        // Guessing a checksum location instead would have the backend write
                        // the checksum over whatever bytes sit there.
                        tracelimit::warn_ratelimited!(
                            error = &err as &dyn std::error::Error,
                            "dropping a checksum send the device cannot offload"
                        );
                        self.complete_tx(CQE_TX_INVALID_OOB);
                        return Ok(());
                    }
                }
            } else {
                (l2_len as u16, oob.s_oob.trans_off())
            };
        let mut meta = TxMetadata {
            id: TxId(0),
            segment_count: sqe.sgl().len().try_into().unwrap(),
            len: total_len.try_into().unwrap(),
            flags: tx_flags(&oob.s_oob, lso),
            l2_len: l2_len as u8,
            l3_len: trans_off.clamp(l2_len, 255) - l2_len,
            l4_len: 0,
            transport_header_offset: trans_off,
            max_segment_size: 0,
            vlan,
        };

        if lso {
            meta.l4_len =
                sge0.size
                    .saturating_sub(meta.l2_len as u32 + meta.l3_len as u32) as u8;
            meta.max_segment_size = sqe.header.params.gd_client_unit_data();
        }

        // With LSO, the first SGE is the header and the rest are the payload.
        // For LSO, the requirements by the GDMA hardware are:
        // - The first SGE must be the header and must be <= 256 bytes.
        // - There should be at least two SGEs.
        // Possible test improvement: Disable the Queue to mimick the hardware behavior.
        if meta.flags.offload_tcp_segmentation() {
            if sqe.sgl().len() < 2 {
                tracelimit::error_ratelimited!(
                    sgl_count = sqe.sgl().len(),
                    "LSO enabled, but only one SGE"
                );
                self.complete_tx(CQE_TX_GDMA_ERR);
                return Ok(());
            }
            if sge0.size > 256 {
                tracelimit::error_ratelimited!(
                    sge0_size = sge0.size,
                    "LSO enabled and SGE[0] size > 256 bytes"
                );
                self.complete_tx(CQE_TX_GDMA_ERR);
                return Ok(());
            }
        }

        let tx_segments = &mut self.tx_segment_buffer;
        tx_segments.clear();
        tx_segments.push(TxSegment {
            ty: TxSegmentType::Head(meta),
            gpa: sge0.address,
            len: sge0.size,
        });
        for sge in &sqe.sgl()[1..] {
            tx_segments.push(TxSegment {
                ty: TxSegmentType::Tail,
                gpa: sge.address,
                len: sge.size,
            });
        }
        let (sync, count) = self.epqueue.tx_avail(&mut self.pool, tx_segments)?;
        if sync || count == 0 {
            tracing::trace!("tx sync complete");
            self.complete_tx(CQE_TX_OKAY);
        } else {
            self.pending_tx.push_back(PendingTx::InBackend);
        }
        Ok(())
    }

    /// Completes a send. A TX completion does not say which send it is for: the driver
    /// matches it to its oldest outstanding send, so it waits behind any earlier send the
    /// backend still has.
    fn complete_tx(&mut self, cqe_type: u8) {
        if self.pending_tx.is_empty() {
            self.post_tx_completion(cqe_type);
        } else {
            self.pending_tx.push_back(PendingTx::Done(cqe_type));
        }
    }

    /// Completes the oldest send the backend had, then the finished sends behind it.
    fn complete_backend_tx(&mut self) -> anyhow::Result<()> {
        if !matches!(self.pending_tx.pop_front(), Some(PendingTx::InBackend)) {
            anyhow::bail!("tx completion with no send outstanding in the backend");
        }
        self.post_tx_completion(CQE_TX_OKAY);
        while let Some(&PendingTx::Done(cqe_type)) = self.pending_tx.front() {
            self.pending_tx.pop_front();
            self.post_tx_completion(cqe_type);
        }
        Ok(())
    }

    // Possible test improvement: provide proper OOB data for a GDMA error.
    fn post_tx_completion(&mut self, cqe_type: u8) {
        let tx_oob = ManaTxCompOob {
            cqe_hdr: ManaCqeHeader::new()
                .with_client_type(MANA_CQE_COMPLETION)
                .with_cqe_type(cqe_type),
            tx_data_offset: 0,
            offsets: ManaTxCompOobOffsets::new(),
            reserved: [0; 12],
        };
        self.queues
            .post_cq(self.sq_cq_id, tx_oob.as_bytes(), self.sq_id, true);
    }

    fn process_rqe(&mut self, wqe: Wqe, wqe_offset: u32) -> anyhow::Result<()> {
        let segments = wqe
            .sgl()
            .iter()
            .map(|sge| RxBufferSegment {
                gpa: sge.address,
                len: sge.size,
            })
            .collect();

        let len = wqe.sgl().iter().map(|sge| sge.size).sum();
        tracing::trace!(?segments, len, "rx wqe");
        let packet = RxPacket {
            segments,
            len,
            wqe_offset,
            oob: FromZeros::new_zeroed(),
        };
        let id = RxId(self.pool.rx_packets.insert(packet) as u32);
        self.epqueue.rx_avail(&mut self.pool, &[id]);
        Ok(())
    }

    fn process_backend(&mut self) -> anyhow::Result<()> {
        let mut packets = [RxId(0)];
        if self.epqueue.rx_poll(&mut self.pool, &mut packets)? > 0 {
            tracing::trace!("rx complete");
            let packet = self
                .pool
                .rx_packets
                .try_remove(packets[0].0 as usize)
                .context("invalid rx id")?;

            self.queues
                .post_cq(self.rq_cq_id, packet.oob.as_bytes(), self.rq_id, false);

            self.rx_buf_count -= 1;
        }

        let mut packets = [TxId(0)];
        if self.epqueue.tx_poll(&mut self.pool, &mut packets)? > 0 {
            tracing::trace!("tx async complete");
            self.complete_backend_tx()?;
        }

        Ok(())
    }
}

/// Builds the backend transmit flags for a send WQE.
///
/// An LSO send implies TCP and IPv4 header checksum offload, whatever
/// per-packet checksum bits the guest set, because the NIC writes both
/// checksums into every segment it produces.
fn tx_flags(s_oob: &ManaTxShortOob, lso: bool) -> TxFlags {
    let is_ipv4 = s_oob.is_outer_ipv4();
    TxFlags::new()
        .with_offload_ip_header_checksum(s_oob.comp_iphdr_csum() || (lso && is_ipv4))
        .with_offload_tcp_checksum(s_oob.comp_tcp_csum() || lso)
        .with_offload_udp_checksum(s_oob.comp_udp_csum())
        .with_offload_tcp_segmentation(lso)
        .with_is_ipv4(is_ipv4)
        .with_is_ipv6(s_oob.is_outer_ipv6() && !is_ipv4)
}

/// The 802.1Q customer tag's EtherType.
const ETHERTYPE_VLAN: u16 = 0x8100;
/// The 802.1ad service tag's EtherType, the outer tag of a QinQ stack.
const ETHERTYPE_QINQ: u16 = 0x88a8;
const ETHERTYPE_IPV4: u16 = 0x0800;
const ETHERTYPE_IPV6: u16 = 0x86dd;
const IP_PROTOCOL_TCP: u8 = 6;
const IP_PROTOCOL_UDP: u8 = 17;
/// The length of an in-band 802.1Q or 802.1ad tag.
const VLAN_TAG_LEN: u16 = 4;
/// In-band tags a checksum send may carry: a single tag or a QinQ pair.
const MAX_IN_BAND_TAGS: u16 = 2;
const TCP_HEADER_MIN_LEN: u16 = 20;
const TCP_HEADER_MAX_LEN: u16 = 60;
const UDP_HEADER_LEN: u16 = 8;
/// IPv4 flags and fragment offset: the MF flag or a nonzero offset marks a fragment.
const IPV4_MORE_FRAGMENTS_AND_OFFSET: u16 = 0x3fff;
/// The most sends whose completions may wait at once, behind the oldest send the backend
/// still has. A driver frees an SQ slot only when it reads that send's completion, so a
/// guest that keeps posting past it gets no further sends taken until the backend drains.
const MAX_PENDING_TX: usize = 256;

/// The most header bytes a checksum send can need: Ethernet, two in-band tags, an IPv4
/// header with every option, and a TCP header with every option. A frame is never read
/// past this.
const MAX_CHECKSUM_HEADER_LEN: usize = net_backend::ETHERNET_HEADER_LEN as usize
    + (MAX_IN_BAND_TAGS * VLAN_TAG_LEN) as usize
    + 60
    + TCP_HEADER_MAX_LEN as usize;

/// The header layout of a checksum send, as read from its frame.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct ChecksumHeaders {
    /// The Ethernet header with any in-band tags, as it is in the frame.
    l2_len: u16,
    /// The IP header, with IPv4 options.
    l3_len: u16,
}

impl ChecksumHeaders {
    fn transport_offset(&self) -> u16 {
        self.l2_len + self.l3_len
    }
}

/// Why a checksum send cannot be offloaded.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
enum ChecksumSendError {
    #[error("the send asks for both a TCP and a UDP checksum")]
    BothProtocols,
    #[error("the frame ends inside its headers")]
    Truncated,
    #[error("the frame is not IPv4 or IPv6 (EtherType {0:#06x})")]
    NotIp(u16),
    #[error("the IP header is malformed")]
    MalformedIpHeader,
    #[error("the TCP data offset is below the minimum header length")]
    MalformedTcpHeader,
    #[error("the OOB names a different IP version than the frame carries")]
    IpVersionMismatch,
    #[error("the frame is an IP fragment")]
    Fragment,
    #[error("an IPv6 extension header (next header {0}) precedes the transport header")]
    Ipv6ExtensionHeader(u8),
    #[error("the frame carries IP protocol {found}, not the requested {requested}")]
    ProtocolMismatch { requested: u8, found: u8 },
    #[error("trans_off {stated} does not match the transport header at {parsed}")]
    OffsetMismatch { stated: u16, parsed: u16 },
}

/// Copies the start of a frame described by `sgl` into `buf`, returning how many bytes it
/// copied. It stops early where the frame ends or a guest address cannot be read, so a
/// short count means a frame shorter than `buf`.
fn read_frame_prefix(gm: &GuestMemory, sgl: &[Sge], buf: &mut [u8]) -> usize {
    let mut copied = 0;
    for sge in sgl {
        if copied == buf.len() {
            break;
        }
        let len = (sge.size as usize).min(buf.len() - copied);
        if gm
            .read_at(sge.address, &mut buf[copied..copied + len])
            .is_err()
        {
            break;
        }
        copied += len;
    }
    copied
}

fn be16(bytes: &[u8], offset: u16) -> Result<u16, ChecksumSendError> {
    let offset = offset as usize;
    bytes
        .get(offset..offset + 2)
        .map(|b| u16::from_be_bytes([b[0], b[1]]))
        .ok_or(ChecksumSendError::Truncated)
}

fn byte_at(bytes: &[u8], offset: u16) -> Result<u8, ChecksumSendError> {
    bytes
        .get(offset as usize)
        .copied()
        .ok_or(ChecksumSendError::Truncated)
}

/// Whether an IPv6 next-header value names an extension header (RFC 8200 section 4,
/// plus the IANA registry's later entries) rather than an upper-layer protocol.
fn is_ipv6_extension_header(next_header: u8) -> bool {
    matches!(
        next_header,
        0 | 43 | 44 | 50 | 51 | 60 | 135 | 139 | 140 | 253 | 254
    )
}

/// Reads the header layout of a TCP or UDP checksum send from the start of its frame, and
/// checks it against what the OOB asks for.
///
/// The NIC parses the frame itself, which is why a driver may leave `trans_off` at 0:
/// Linux's mana driver does on every UDP checksum send (`mana_start_xmit`), and that
/// driver works on hardware. So a `trans_off` of 0 means "not stated"; a stated one must
/// match the parsed offset. A send the device cannot parse into a layout it can checksum
/// is refused rather than handed to the backend with a guessed checksum location, which
/// would write the checksum over whatever bytes sit there.
///
/// In-band tags are walked (they are in the frame, so they lengthen `l2_len`); a tag
/// injected from the OOB is not in the frame and changes nothing here. IPv4 options are
/// counted. IPv6 extension headers are not walked: a send that carries one is refused,
/// as is any IP fragment, since a fragment's transport checksum covers data the frame
/// does not hold. Linux's driver only offloads a frame whose transport header follows
/// the IP header directly (`mana_checksum_info` looks at `protocol`/`nexthdr` alone), so
/// none of these refusals reaches a send that driver offloads.
fn checksum_send_headers(
    frame: &[u8],
    s_oob: &ManaTxShortOob,
) -> Result<ChecksumHeaders, ChecksumSendError> {
    let requested = match (s_oob.comp_tcp_csum(), s_oob.comp_udp_csum()) {
        (true, false) => IP_PROTOCOL_TCP,
        (false, true) => IP_PROTOCOL_UDP,
        _ => return Err(ChecksumSendError::BothProtocols),
    };

    let mut l2_len = net_backend::ETHERNET_HEADER_LEN as u16;
    let mut ethertype = be16(frame, l2_len - 2)?;
    for _ in 0..MAX_IN_BAND_TAGS {
        if ethertype != ETHERTYPE_VLAN && ethertype != ETHERTYPE_QINQ {
            break;
        }
        ethertype = be16(frame, l2_len + 2)?;
        l2_len += VLAN_TAG_LEN;
    }

    let (l3_len, found) = match ethertype {
        ETHERTYPE_IPV4 => {
            if s_oob.is_outer_ipv6() {
                return Err(ChecksumSendError::IpVersionMismatch);
            }
            let version_ihl = byte_at(frame, l2_len)?;
            let l3_len = u16::from(version_ihl & 0xf) * 4;
            if version_ihl >> 4 != 4 || l3_len < net_backend::IPV4_MIN_HEADER_LEN {
                return Err(ChecksumSendError::MalformedIpHeader);
            }
            if be16(frame, l2_len + 6)? & IPV4_MORE_FRAGMENTS_AND_OFFSET != 0 {
                return Err(ChecksumSendError::Fragment);
            }
            (l3_len, byte_at(frame, l2_len + 9)?)
        }
        ETHERTYPE_IPV6 => {
            if s_oob.is_outer_ipv4() {
                return Err(ChecksumSendError::IpVersionMismatch);
            }
            if byte_at(frame, l2_len)? >> 4 != 6 {
                return Err(ChecksumSendError::MalformedIpHeader);
            }
            let next_header = byte_at(frame, l2_len + 6)?;
            if is_ipv6_extension_header(next_header) {
                return Err(ChecksumSendError::Ipv6ExtensionHeader(next_header));
            }
            (net_backend::IPV6_MIN_HEADER_LEN, next_header)
        }
        other => return Err(ChecksumSendError::NotIp(other)),
    };
    if found != requested {
        return Err(ChecksumSendError::ProtocolMismatch { requested, found });
    }

    let layout = ChecksumHeaders { l2_len, l3_len };
    let l4_len = if requested == IP_PROTOCOL_TCP {
        // The data offset, the high nibble of byte 12, counts the header in 32-bit words.
        let l4_len = u16::from(byte_at(frame, layout.transport_offset() + 12)? >> 4) * 4;
        if l4_len < TCP_HEADER_MIN_LEN {
            return Err(ChecksumSendError::MalformedTcpHeader);
        }
        l4_len
    } else {
        UDP_HEADER_LEN
    };
    // The whole transport header must be in the frame, not just its checksum field.
    byte_at(frame, layout.transport_offset() + l4_len - 1)?;
    let stated = s_oob.trans_off();
    if stated != 0 && stated != layout.transport_offset() {
        return Err(ChecksumSendError::OffsetMismatch {
            stated,
            parsed: layout.transport_offset(),
        });
    }
    Ok(layout)
}

struct TxRxState;

impl AsyncRun<TxRxTask> for TxRxState {
    async fn run(
        &mut self,
        stop: &mut StopTask<'_>,
        task: &mut TxRxTask,
    ) -> Result<(), task_control::Cancelled> {
        let check = stop.check_handle()?;
        stop.until_stopped(task.process(&check)).await?
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dma::DmaRegion;
    use crate::test_helpers as helper;
    use gdma_defs::CLIENT_OOB_24;
    use gdma_defs::Cqe;
    use gdma_defs::WqDoorbellValue;
    use gdma_defs::WqeParams;
    use net_backend::TxError;
    use pal_async::DefaultPool;
    use parking_lot::Mutex;
    use std::task::Context;
    use test_with_tracing::test;

    #[derive(Default)]
    struct BackendState {
        ready_budget: usize,
        ready_processed: usize,
        tx_accepted: usize,
        tx_done: bool,
        rx: Option<RxId>,
    }

    #[derive(InspectMut)]
    #[inspect(skip)]
    struct TestQueue {
        state: Arc<Mutex<BackendState>>,
        tx_hook: Arc<helper::StopHook>,
        rx_hook: Arc<helper::StopHook>,
        ready_hook: Arc<helper::StopHook>,
        /// Fires when the task finds the backend not ready, that is, when it has no SQE or
        /// RQE it will take.
        idle_hook: Arc<helper::StopHook>,
        synchronous_tx: bool,
    }

    impl Queue for TestQueue {
        fn poll_ready(&mut self, _: &mut Context<'_>, _: &mut dyn BufferAccess) -> Poll<()> {
            let mut state = self.state.lock();
            if state.ready_budget == 0 {
                drop(state);
                self.idle_hook.fire();
                return Poll::Pending;
            }
            state.ready_budget -= 1;
            Poll::Ready(())
        }

        fn rx_avail(&mut self, _: &mut dyn BufferAccess, ids: &[RxId]) {
            assert_eq!(ids.len(), 1);
            assert!(self.state.lock().rx.replace(ids[0]).is_none());
            self.rx_hook.fire();
        }

        fn rx_poll(
            &mut self,
            pool: &mut dyn BufferAccess,
            packets: &mut [RxId],
        ) -> anyhow::Result<usize> {
            let mut state = self.state.lock();
            state.ready_processed += 1;
            let count = if let Some(id) = state.rx.take() {
                let data = [0x5a; 60];
                pool.write_packet(
                    id,
                    &RxMetadata {
                        len: data.len(),
                        ..Default::default()
                    },
                    &data,
                );
                packets[0] = id;
                1
            } else {
                0
            };
            drop(state);
            self.ready_hook.fire();
            Ok(count)
        }

        fn tx_avail(
            &mut self,
            _: &mut dyn BufferAccess,
            segments: &[TxSegment],
        ) -> anyhow::Result<(bool, usize)> {
            let mut state = self.state.lock();
            state.tx_accepted += 1;
            if !self.synchronous_tx {
                assert!(!std::mem::replace(&mut state.tx_done, true));
            }
            drop(state);
            self.tx_hook.fire();
            Ok((self.synchronous_tx, segments.len()))
        }

        fn tx_poll(
            &mut self,
            _: &mut dyn BufferAccess,
            packets: &mut [TxId],
        ) -> Result<usize, TxError> {
            if std::mem::take(&mut self.state.lock().tx_done) {
                packets[0] = TxId(0);
                Ok(1)
            } else {
                Ok(0)
            }
        }
    }

    fn task(queues: Arc<Queues>, epqueue: TestQueue) -> TxRxTask {
        let sq_id = queues.alloc_wq(true, helper::region(helper::SQ)).unwrap();
        let rq_id = queues.alloc_wq(false, helper::region(helper::RQ)).unwrap();
        let eq_id = queues.alloc_eq(helper::region(helper::EQ), 0).unwrap();
        let cq_id = queues.alloc_cq(helper::region(helper::CQ), eq_id).unwrap();
        TxRxTask {
            pool: GuestBuffers {
                gm: queues.gm.clone(),
                rx_packets: Slab::new(),
            },
            queues,
            epqueue: Box::new(epqueue),
            sq_id,
            sq_cq_id: cq_id,
            rq_id,
            rq_cq_id: cq_id,
            tx_segment_buffer: Vec::new(),
            rx_buf_count: 0,
            pending_tx: VecDeque::new(),
        }
    }

    fn backend() -> TestQueue {
        TestQueue {
            state: Arc::new(Mutex::new(BackendState {
                ready_budget: 8,
                ..Default::default()
            })),
            tx_hook: Arc::new(helper::StopHook::default()),
            rx_hook: Arc::new(helper::StopHook::default()),
            ready_hook: Arc::new(helper::StopHook::default()),
            idle_hook: Arc::new(helper::StopHook::default()),
            synchronous_tx: true,
        }
    }

    fn completion(queues: &Queues, index: u64) -> Cqe {
        queues
            .gm
            .read_plain(helper::CQ + index * size_of::<Cqe>() as u64)
            .unwrap()
    }

    /// An Ethernet header with `tags` in-band tags (their EtherTypes, outermost first) ahead of
    /// `ethertype`.
    fn eth(tags: &[u16], ethertype: u16) -> Vec<u8> {
        let mut bytes = vec![0x02, 0, 0, 0, 0, 1, 0x02, 0, 0, 0, 0, 2];
        for &tag in tags {
            bytes.extend_from_slice(&tag.to_be_bytes());
            // The TCI: priority 1, VLAN 0x123.
            bytes.extend_from_slice(&0x2123u16.to_be_bytes());
        }
        bytes.extend_from_slice(&ethertype.to_be_bytes());
        bytes
    }

    /// An IPv4 header with `option_words` 32-bit option words, carrying `protocol`, whose
    /// flags-and-fragment-offset field is `fragment`.
    fn ipv4(option_words: u8, protocol: u8, fragment: u16) -> Vec<u8> {
        let mut bytes = vec![0x45 + option_words, 0, 0, 0, 0x12, 0x34];
        bytes.extend_from_slice(&fragment.to_be_bytes());
        bytes.extend_from_slice(&[64, protocol, 0, 0, 10, 0, 0, 1, 10, 0, 0, 2]);
        // Options: a run of NOPs.
        bytes.extend(std::iter::repeat_n(1, usize::from(option_words) * 4));
        bytes
    }

    /// An IPv6 header whose next header is `next_header`.
    fn ipv6(next_header: u8) -> Vec<u8> {
        let mut bytes = vec![0x60, 0, 0, 0, 0, 0, next_header, 64];
        bytes.extend_from_slice(&[0xfd; 32]);
        bytes
    }

    /// A TCP header with no options, ports 1234 -> 80.
    fn tcp() -> Vec<u8> {
        tcp_with_options(0)
    }

    /// A TCP header with `option_words` 32-bit option words, ports 1234 -> 80.
    fn tcp_with_options(option_words: u8) -> Vec<u8> {
        let mut bytes = vec![
            0x04,
            0xd2,
            0,
            80,
            0,
            0,
            0,
            1,
            0,
            0,
            0,
            0,
            (5 + option_words) << 4,
            0x18,
            0xff,
            0xff,
        ];
        bytes.extend_from_slice(&[0; 4]);
        // Options: a run of NOPs.
        bytes.extend(std::iter::repeat_n(1, usize::from(option_words) * 4));
        bytes
    }

    /// A UDP header, ports 1234 -> 53.
    fn udp() -> Vec<u8> {
        vec![0x04, 0xd2, 0, 53, 0, 8, 0, 0]
    }

    fn udp_csum() -> ManaTxShortOob {
        ManaTxShortOob::new().with_comp_udp_csum(true)
    }

    fn tcp_csum() -> ManaTxShortOob {
        ManaTxShortOob::new().with_comp_tcp_csum(true)
    }

    fn task_with(queues: Arc<Queues>, epqueue: Box<dyn Queue>) -> TxRxTask {
        let mut task = task(queues, backend());
        task.epqueue = epqueue;
        task
    }

    /// What became of a send: the metadata the backend got, if any, and the completion type.
    struct SendOutcome {
        sent: Option<TxMetadata>,
        cqe_type: u8,
    }

    /// Sends `frame` through the real `process_sqe` in two SGEs, the first `split` bytes long,
    /// with a backend standing in for the tap.
    fn send_frame(s_oob: ManaTxShortOob, frame: &[u8], split: usize) -> SendOutcome {
        send_frame_with(s_oob, FromZeros::new_zeroed(), frame, split)
    }

    fn send_frame_with(
        s_oob: ManaTxShortOob,
        l_oob: bnic_defs::ManaTxLongOob,
        frame: &[u8],
        split: usize,
    ) -> SendOutcome {
        let pool = DefaultPool::new();
        let queues = helper::queues(&pool, None);
        let sent = Arc::new(Mutex::new(Vec::new()));
        let mut task = task_with(
            queues.clone(),
            Box::new(RecordingQueue { sent: sent.clone() }),
        );
        let sqe = send_wqe(&queues, s_oob, l_oob, frame, split, 0x6000);
        task.process_sqe(sqe).unwrap();

        let cqe = completion(&queues, 0);
        assert_eq!(cqe.params.owner_count(), 1, "one completion per send");
        let sent = sent.lock().pop();
        SendOutcome {
            sent,
            cqe_type: cqe_type(&cqe),
        }
    }

    fn cqe_type(cqe: &Cqe) -> u8 {
        ManaTxCompOob::read_from_prefix(&cqe.data)
            .unwrap()
            .0
            .cqe_hdr
            .cqe_type()
    }

    /// Writes `frame` to guest memory at `gpa` in two SGEs, the first `split` bytes long, and
    /// returns a send WQE for it.
    fn send_wqe(
        queues: &Queues,
        s_oob: ManaTxShortOob,
        l_oob: bnic_defs::ManaTxLongOob,
        frame: &[u8],
        split: usize,
        gpa: u64,
    ) -> Wqe {
        let (head, tail) = frame.split_at(split);
        queues.gm.write_at(gpa, head).unwrap();
        queues.gm.write_at(gpa + 0x2000, tail).unwrap();
        let sgl = [
            frame_sge(gpa, head.len() as u32),
            frame_sge(gpa + 0x2000, tail.len() as u32),
        ];

        let mut sqe = Wqe::new_zeroed();
        sqe.header.params = WqeParams::new()
            .with_num_sgl_entries(2)
            .with_inline_client_oob_size(CLIENT_OOB_24);
        let oob = ManaTxOob {
            s_oob,
            l_oob,
            reserved3: 0,
            reserved4: 0,
        };
        oob.write_to_prefix(&mut sqe.data[..]).unwrap();
        let sgl_offset = sqe.header.sgl_offset();
        sgl.write_to_prefix(&mut sqe.data[sgl_offset..]).unwrap();
        sqe
    }

    /// A refused send's completion waits behind an earlier send the backend still has: the
    /// driver frees its oldest outstanding send for each completion it reads.
    #[test]
    fn a_refused_send_completes_after_an_earlier_send_in_flight() {
        let pool = DefaultPool::new();
        let queues = helper::queues(&pool, None);
        let mut epqueue = backend();
        epqueue.synchronous_tx = false;
        let mut task = task(queues.clone(), epqueue);
        let accepted = [eth(&[], 0x0800), ipv4(0, 17, 0), udp()].concat();
        let refused = [eth(&[], 0x0800), ipv4(0, 6, 0), tcp()].concat();
        for (frame, gpa) in [(&accepted, 0x6000), (&refused, 0xa000)] {
            let sqe = send_wqe(&queues, udp_csum(), FromZeros::new_zeroed(), frame, 14, gpa);
            task.process_sqe(sqe).unwrap();
        }
        assert_eq!(
            completion(&queues, 0).params.owner_count(),
            0,
            "nothing completes ahead of the send in flight"
        );

        task.process_backend().unwrap();
        assert_eq!(cqe_type(&completion(&queues, 0)), CQE_TX_OKAY);
        assert_eq!(cqe_type(&completion(&queues, 1)), CQE_TX_INVALID_OOB);
    }

    /// Refused sends queued behind one the backend still has stop being taken at
    /// `MAX_PENDING_TX`, and the task takes the rest once the backend completes it.
    #[test]
    fn sends_behind_one_in_flight_stop_at_the_completion_backlog_limit() {
        let mut pool = DefaultPool::new();
        let queues = helper::queues(&pool, None);
        let mut epqueue = backend();
        epqueue.synchronous_tx = false;
        epqueue.state.lock().ready_budget = 0;
        let state = epqueue.state.clone();
        let idle_hook = epqueue.idle_hook.clone();
        let mut task = task(queues.clone(), epqueue);

        // Eight pages hold 512 of these 64-byte WQEs, more than the limit.
        let sq_gpa = 0x8000;
        let sq_pages = (0..8).map(|i| sq_gpa + i * 0x1000).collect();
        task.sq_id = queues
            .alloc_wq(true, DmaRegion::new(sq_pages, 0, 0x8000).unwrap())
            .unwrap();
        let accepted = [eth(&[], 0x0800), ipv4(0, 17, 0), udp()].concat();
        let refused = [eth(&[], 0x0800), ipv4(0, 6, 0), tcp()].concat();
        let sends = MAX_PENDING_TX + 44;
        let mut tail = 0;
        for i in 0..sends {
            let (frame, gpa) = if i == 0 {
                (&accepted, 0x5000)
            } else {
                (&refused, 0x5800)
            };
            let sqe = send_wqe(&queues, udp_csum(), FromZeros::new_zeroed(), frame, 14, gpa);
            let len = sqe.header.total_len();
            queues
                .gm
                .write_at(sq_gpa + tail as u64, &sqe.as_bytes()[..len])
                .unwrap();
            tail += len as u32;
        }
        queues.doorbell_sq(WqDoorbellValue::new().with_id(task.sq_id).with_tail(tail));

        let control = Arc::new(Mutex::new(TaskControl::new(TxRxState)));
        control.lock().insert(pool.driver(), "gdma-tx", task);
        let signal = idle_hook.arm(&control);
        assert!(control.lock().start());
        helper::stopped(&mut pool, &control, signal);
        assert_eq!(
            control.lock().state().unwrap().pending_tx.len(),
            MAX_PENDING_TX
        );

        state.lock().ready_budget = 1;
        let signal = idle_hook.arm(&control);
        assert!(control.lock().start());
        helper::stopped(&mut pool, &control, signal);
        assert!(control.lock().state().unwrap().pending_tx.is_empty());
        assert_eq!(state.lock().tx_accepted, 1);
    }

    /// A backend that reports a send it was never given must not complete one of the guest's:
    /// the driver would free its oldest outstanding send for it.
    #[test]
    fn a_backend_completion_with_no_send_outstanding_is_refused() {
        let pool = DefaultPool::new();
        let queues = helper::queues(&pool, None);
        let epqueue = backend();
        epqueue.state.lock().tx_done = true;
        let mut task = task(queues.clone(), epqueue);
        assert!(task.process_backend().is_err());
        assert_eq!(completion(&queues, 0).params.owner_count(), 0);
    }

    /// Linux's UDP checksum send: `trans_off` left at 0, the IP header split from the
    /// Ethernet header across SGEs, and an IPv4 option in it.
    #[test]
    fn a_udp_checksum_send_without_trans_off_gets_it_from_the_frame() {
        let frame = [eth(&[], 0x0800), ipv4(1, 17, 0), udp()].concat();
        for split in [14, 20, 30, frame.len() - 1] {
            let outcome = send_frame(udp_csum().with_is_outer_ipv4(true), &frame, split);
            assert_eq!(outcome.cqe_type, CQE_TX_OKAY);
            let meta = outcome.sent.unwrap();
            assert_eq!(meta.l2_len, 14);
            assert_eq!(meta.l3_len, 24);
            assert_eq!(meta.transport_header_offset, 38);
        }
    }

    /// The device refuses the send the way the NIC does, and the backend never sees it.
    #[test]
    fn a_checksum_send_the_device_cannot_parse_is_refused_per_packet() {
        // A UDP checksum asked for on a TCP frame.
        let frame = [eth(&[], 0x0800), ipv4(0, 6, 0), tcp()].concat();
        let outcome = send_frame(udp_csum(), &frame, 14);
        assert_eq!(outcome.cqe_type, CQE_TX_INVALID_OOB);
        assert!(outcome.sent.is_none());
    }

    /// A send with no checksum asked for is not parsed: its `trans_off` goes through as is,
    /// whatever the frame holds.
    #[test]
    fn a_send_without_a_checksum_keeps_trans_off_and_is_not_parsed() {
        let frame = [eth(&[], 0x0806), vec![0; 28]].concat();
        let plain = ManaTxShortOob::new().with_trans_off(7);
        let outcome = send_frame(plain, &frame, 14);
        assert_eq!(outcome.cqe_type, CQE_TX_OKAY);
        assert_eq!(outcome.sent.unwrap().transport_header_offset, 7);
    }

    fn layout(s_oob: ManaTxShortOob, frame: &[u8]) -> Result<(u16, u16), ChecksumSendError> {
        checksum_send_headers(frame, &s_oob).map(|h| (h.l2_len, h.transport_offset()))
    }

    #[test]
    fn checksum_send_layouts_the_device_accepts() {
        let v4_udp = [eth(&[], 0x0800), ipv4(0, 17, 0), udp()].concat();
        assert_eq!(layout(udp_csum(), &v4_udp), Ok((14, 34)));
        // IPv4 options, up to the 60-byte maximum.
        let v4_opts = [eth(&[], 0x0800), ipv4(10, 6, 0), tcp()].concat();
        assert_eq!(layout(tcp_csum(), &v4_opts), Ok((14, 74)));
        // Don't Fragment is not a fragment.
        let v4_df = [eth(&[], 0x0800), ipv4(0, 17, 0x4000), udp()].concat();
        assert_eq!(layout(udp_csum(), &v4_df), Ok((14, 34)));
        let v6_tcp = [eth(&[], 0x86dd), ipv6(6), tcp()].concat();
        assert_eq!(
            layout(tcp_csum().with_is_outer_ipv6(true), &v6_tcp),
            Ok((14, 54))
        );
        let v6_udp = [eth(&[], 0x86dd), ipv6(17), udp()].concat();
        assert_eq!(layout(udp_csum(), &v6_udp), Ok((14, 54)));
    }

    /// An in-band tag is in the frame, so it lengthens the L2 header; QinQ has two.
    #[test]
    fn in_band_tags_lengthen_the_l2_header() {
        let one = [eth(&[0x8100], 0x0800), ipv4(0, 17, 0), udp()].concat();
        assert_eq!(layout(udp_csum(), &one), Ok((18, 38)));
        let qinq = [eth(&[0x88a8, 0x8100], 0x86dd), ipv6(6), tcp()].concat();
        assert_eq!(layout(tcp_csum(), &qinq), Ok((22, 62)));
        // A third tag is past what the device walks.
        let three = [
            eth(&[0x88a8, 0x8100, 0x8100], 0x0800),
            ipv4(0, 17, 0),
            udp(),
        ]
        .concat();
        assert_eq!(
            layout(udp_csum(), &three),
            Err(ChecksumSendError::NotIp(0x8100))
        );
    }

    /// A stated offset must be the one the frame has. This replaces counting any offset short
    /// of a minimal IP header as omitted: only 0 is.
    #[test]
    fn a_stated_trans_off_must_match_the_frame() {
        let frame = [eth(&[], 0x0800), ipv4(1, 6, 0), tcp()].concat();
        assert_eq!(layout(tcp_csum().with_trans_off(38), &frame), Ok((14, 38)));
        for stated in [1, 33, 34, 39] {
            assert_eq!(
                layout(tcp_csum().with_trans_off(stated), &frame),
                Err(ChecksumSendError::OffsetMismatch { stated, parsed: 38 })
            );
        }
    }

    #[test]
    fn checksum_send_layouts_the_device_refuses() {
        use ChecksumSendError::*;
        let v4 = |protocol, fragment| [eth(&[], 0x0800), ipv4(0, protocol, fragment)].concat();
        let cases: Vec<(ManaTxShortOob, Vec<u8>, ChecksumSendError)> = vec![
            (
                udp_csum().with_comp_tcp_csum(true),
                [v4(17, 0), udp()].concat(),
                BothProtocols,
            ),
            (
                udp_csum(),
                [v4(6, 0), tcp()].concat(),
                ProtocolMismatch {
                    requested: 17,
                    found: 6,
                },
            ),
            (
                tcp_csum(),
                [eth(&[], 0x86dd), ipv6(17), udp()].concat(),
                ProtocolMismatch {
                    requested: 6,
                    found: 17,
                },
            ),
            // More Fragments, and a later fragment with MF clear.
            (udp_csum(), [v4(17, 0x2000), udp()].concat(), Fragment),
            (udp_csum(), [v4(17, 0x00b9), udp()].concat(), Fragment),
            // Hop-by-hop options and a fragment header ahead of the transport header.
            (
                udp_csum(),
                [eth(&[], 0x86dd), ipv6(0), udp()].concat(),
                Ipv6ExtensionHeader(0),
            ),
            (
                tcp_csum(),
                [eth(&[], 0x86dd), ipv6(44), tcp()].concat(),
                Ipv6ExtensionHeader(44),
            ),
            (
                udp_csum(),
                [eth(&[], 0x0806), vec![0; 28]].concat(),
                NotIp(0x0806),
            ),
            // An IHL below the minimum, and IPv6 bytes behind an IPv4 EtherType.
            (
                udp_csum(),
                [
                    eth(&[], 0x0800),
                    vec![0x44],
                    ipv4(0, 17, 0)[1..].to_vec(),
                    udp(),
                ]
                .concat(),
                MalformedIpHeader,
            ),
            (
                udp_csum(),
                [eth(&[], 0x0800), ipv6(17), udp()].concat(),
                MalformedIpHeader,
            ),
            // The OOB names the other IP version.
            (
                udp_csum().with_is_outer_ipv6(true),
                [v4(17, 0), udp()].concat(),
                IpVersionMismatch,
            ),
            (
                tcp_csum().with_is_outer_ipv4(true),
                [eth(&[], 0x86dd), ipv6(6), tcp()].concat(),
                IpVersionMismatch,
            ),
        ];
        for (s_oob, frame, expected) in cases {
            assert_eq!(layout(s_oob, &frame), Err(expected), "{frame:02x?}");
        }
    }

    /// A frame that ends anywhere inside its headers is refused, whichever header it ends in.
    #[test]
    fn a_frame_ending_inside_its_headers_is_refused() {
        for (s_oob, frame) in [
            (
                udp_csum(),
                [eth(&[0x8100], 0x0800), ipv4(2, 17, 0), udp()].concat(),
            ),
            (tcp_csum(), [eth(&[], 0x86dd), ipv6(6), tcp()].concat()),
        ] {
            assert!(layout(s_oob, &frame).is_ok());
            for len in 0..frame.len() {
                assert_eq!(
                    layout(s_oob, &frame[..len]),
                    Err(ChecksumSendError::Truncated),
                    "length {len}"
                );
            }
        }
    }

    /// A TCP header is as long as its data offset says, so a frame must hold all of it, and a
    /// data offset below the minimum header is refused.
    #[test]
    fn a_tcp_header_is_as_long_as_its_data_offset() {
        let frame = [eth(&[], 0x0800), ipv4(0, 6, 0), tcp_with_options(10)].concat();
        assert_eq!(layout(tcp_csum(), &frame), Ok((14, 34)));
        for len in [54, frame.len() - 1] {
            assert_eq!(
                layout(tcp_csum(), &frame[..len]),
                Err(ChecksumSendError::Truncated),
                "length {len}"
            );
        }
        for data_offset in 0..5 {
            let mut short = frame.clone();
            short[34 + 12] = data_offset << 4;
            assert_eq!(
                layout(tcp_csum(), &short),
                Err(ChecksumSendError::MalformedTcpHeader)
            );
        }
    }

    /// The largest layout the device accepts, two in-band tags, an IPv4 header and a TCP header
    /// both with every option, fits in the bytes it reads from the frame.
    #[test]
    fn the_largest_checksum_layout_fits_the_frame_prefix() {
        let frame = [
            eth(&[0x88a8, 0x8100], 0x0800),
            ipv4(10, 6, 0),
            tcp_with_options(10),
            vec![0xee; 100],
        ]
        .concat();
        let outcome = send_frame(tcp_csum(), &frame, 14);
        assert_eq!(outcome.cqe_type, CQE_TX_OKAY);
        assert_eq!(outcome.sent.unwrap().transport_header_offset, 82);
    }

    /// The device never reads past what the largest layout needs, and stops at the frame's end
    /// or at an address it cannot read.
    #[test]
    fn the_frame_prefix_is_bounded_by_the_frame_and_the_buffer() {
        let gm = GuestMemory::allocate(0x2000);
        gm.write_at(0x10, &[0xaa; 4]).unwrap();
        gm.write_at(0x1000, &[0xbb; 0x200]).unwrap();
        let mut buf = [0; MAX_CHECKSUM_HEADER_LEN];
        let n = read_frame_prefix(
            &gm,
            &[frame_sge(0x10, 4), frame_sge(0x1000, 0x200)],
            &mut buf,
        );
        assert_eq!(n, MAX_CHECKSUM_HEADER_LEN);
        assert_eq!(&buf[..5], &[0xaa, 0xaa, 0xaa, 0xaa, 0xbb]);
        let n = read_frame_prefix(&gm, &[frame_sge(0x10, 4), frame_sge(0x1000, 3)], &mut buf);
        assert_eq!(n, 7);
        let n = read_frame_prefix(
            &gm,
            &[frame_sge(0x10, 4), frame_sge(u64::MAX, 32)],
            &mut buf,
        );
        assert_eq!(n, 4);
    }

    /// A backend that keeps what it was asked to send, standing in for the tap or the VF.
    #[derive(InspectMut)]
    struct RecordingQueue {
        #[inspect(skip)]
        sent: Arc<Mutex<Vec<TxMetadata>>>,
    }

    impl Queue for RecordingQueue {
        fn poll_ready(&mut self, _cx: &mut Context<'_>, _pool: &mut dyn BufferAccess) -> Poll<()> {
            Poll::Pending
        }

        fn rx_avail(&mut self, _pool: &mut dyn BufferAccess, _done: &[RxId]) {}

        fn rx_poll(
            &mut self,
            _pool: &mut dyn BufferAccess,
            _packets: &mut [RxId],
        ) -> anyhow::Result<usize> {
            Ok(0)
        }

        fn tx_avail(
            &mut self,
            _pool: &mut dyn BufferAccess,
            segments: &[TxSegment],
        ) -> anyhow::Result<(bool, usize)> {
            let TxSegmentType::Head(meta) = &segments[0].ty else {
                panic!("the first segment is the head");
            };
            self.sent.lock().push(meta.clone());
            Ok((true, segments.len()))
        }

        fn tx_poll(
            &mut self,
            _pool: &mut dyn BufferAccess,
            _done: &mut [TxId],
        ) -> Result<usize, TxError> {
            Ok(0)
        }
    }

    fn frame_sge(address: u64, size: u32) -> Sge {
        Sge {
            address,
            mem_key: 0,
            size,
        }
    }

    /// A tagged IPv4 checksum send as Linux lays it out: the tag in the OOB, none in the
    /// frame, `trans_off` 34. Byte 18 of the frame is the IP ID's high byte, here 0x60, so
    /// counting the tag in the frame would read an IPv6 version nibble there.
    #[test]
    fn an_injected_tag_is_not_looked_for_in_the_frame() {
        let mut v4 = ipv4(0, 17, 0);
        v4[4] = 0x60;
        let frame = [eth(&[], 0x0800), v4, udp()].concat();
        let s_oob = udp_csum()
            .with_is_outer_ipv4(true)
            .with_pkt_fmt(MANA_LONG_PKT_FMT);
        let l_oob = bnic_defs::ManaTxLongOob::new()
            .with_inject_vlan_pri_tag(true)
            .with_vlan_id(5);
        for s_oob in [s_oob, s_oob.with_trans_off(34)] {
            let outcome = send_frame_with(s_oob, l_oob, &frame, 14);
            assert_eq!(outcome.cqe_type, CQE_TX_OKAY);
            let meta = outcome.sent.unwrap();
            assert_eq!(meta.vlan.unwrap().vlan_id(), 5);
            assert_eq!(meta.l2_len, 14);
            assert_eq!(meta.l3_len, 20);
            assert_eq!(meta.transport_header_offset, 34);
        }
    }

    /// With a tag in the frame and another in the OOB, the IP header follows the in-band
    /// tag: the backend fixes up the IPv4 header at `l2_len`.
    #[test]
    fn an_injected_tag_does_not_move_the_ip_header_of_a_tagged_frame() {
        let frame = [eth(&[0x8100], 0x0800), ipv4(0, 17, 0), udp()].concat();
        let s_oob = udp_csum()
            .with_is_outer_ipv4(true)
            .with_pkt_fmt(MANA_LONG_PKT_FMT);
        let l_oob = bnic_defs::ManaTxLongOob::new()
            .with_inject_vlan_pri_tag(true)
            .with_vlan_id(5);
        let outcome = send_frame_with(s_oob, l_oob, &frame, 18);
        assert_eq!(outcome.cqe_type, CQE_TX_OKAY);
        let meta = outcome.sent.unwrap();
        assert_eq!(meta.l2_len, 18);
        assert_eq!(meta.l3_len, 20);
        assert_eq!(meta.transport_header_offset, 38);
    }

    #[test]
    fn ready_backend_cancellation_stops_before_the_next_iteration_and_resumes() {
        let mut pool = DefaultPool::new();
        let queues = helper::queues(&pool, None);
        let epqueue = backend();
        let state = epqueue.state.clone();
        let hook = epqueue.ready_hook.clone();
        let control = Arc::new(Mutex::new(TaskControl::new(TxRxState)));
        control
            .lock()
            .insert(pool.driver(), "gdma-ready", task(queues, epqueue));
        for expected in 1..=2 {
            let signal = hook.arm(&control);
            assert!(control.lock().start());
            helper::stopped(&mut pool, &control, signal);
            assert_eq!(state.lock().ready_processed, expected);
        }
    }

    #[test]
    fn accepted_transmit_is_completed_across_cancellation() {
        for synchronous_tx in [true, false] {
            let mut pool = DefaultPool::new();
            let queues = helper::queues(&pool, None);
            let mut epqueue = backend();
            epqueue.synchronous_tx = synchronous_tx;
            let state = epqueue.state.clone();
            let tx_hook = epqueue.tx_hook.clone();
            let ready_hook = epqueue.ready_hook.clone();
            let task = task(queues.clone(), epqueue);
            let len = helper::write_wqe(
                &queues,
                helper::SQ,
                ManaTxShortOob::new().as_bytes(),
                0x5000,
                60,
            );
            queues.doorbell_sq(WqDoorbellValue::new().with_id(task.sq_id).with_tail(len));
            let control = Arc::new(Mutex::new(TaskControl::new(TxRxState)));
            control.lock().insert(pool.driver(), "gdma-tx", task);
            let signal = tx_hook.arm(&control);
            assert!(control.lock().start());
            helper::stopped(&mut pool, &control, signal);
            assert_eq!(state.lock().tx_accepted, 1);
            assert_eq!(state.lock().ready_processed, 0);
            assert_eq!(
                completion(&queues, 0).params.owner_count(),
                u8::from(synchronous_tx)
            );

            let signal = ready_hook.arm(&control);
            assert!(control.lock().start());
            helper::stopped(&mut pool, &control, signal);
            assert_eq!(state.lock().tx_accepted, 1);
            let cqe = completion(&queues, 0);
            assert_eq!(cqe.params.owner_count(), 1);
            assert!(cqe.params.is_send_wq());
            let oob = ManaTxCompOob::read_from_prefix(&cqe.data).unwrap().0;
            assert_eq!(oob.cqe_hdr.cqe_type(), CQE_TX_OKAY);
            assert_eq!(completion(&queues, 1).params.owner_count(), 0);
        }
    }

    #[test]
    fn accepted_receive_survives_stop_and_publishes_completion_before_cancelling() {
        let mut pool = DefaultPool::new();
        let queues = helper::queues(&pool, None);
        let epqueue = backend();
        let rx_hook = epqueue.rx_hook.clone();
        let ready_hook = epqueue.ready_hook.clone();
        let task = task(queues.clone(), epqueue);
        let len = helper::write_wqe(&queues, helper::RQ, &[], 0x5000, 128);
        queues.doorbell_rq(WqDoorbellValue::new().with_id(task.rq_id).with_tail(len));
        let control = Arc::new(Mutex::new(TaskControl::new(TxRxState)));
        control.lock().insert(pool.driver(), "gdma-rx", task);
        let signal = rx_hook.arm(&control);
        assert!(control.lock().start());
        helper::stopped(&mut pool, &control, signal);
        {
            let control = control.lock();
            let task = control.state().unwrap();
            assert_eq!(task.rx_buf_count, 1);
            assert_eq!(task.pool.rx_packets.len(), 1);
        }
        assert_eq!(completion(&queues, 0).params.owner_count(), 0);

        let signal = ready_hook.arm(&control);
        assert!(control.lock().start());
        helper::stopped(&mut pool, &control, signal);
        let control = control.lock();
        let task = control.state().unwrap();
        assert_eq!(task.rx_buf_count, 0);
        assert!(task.pool.rx_packets.is_empty());
        let cqe = completion(&queues, 0);
        assert_eq!(cqe.params.owner_count(), 1);
        assert!(!cqe.params.is_send_wq());
        assert_eq!(cqe.params.wq_number(), task.rq_id);
        let oob = ManaRxcompOob::read_from_prefix(&cqe.data).unwrap().0;
        assert_eq!(oob.cqe_hdr.cqe_type(), CQE_RX_OKAY);
        assert_eq!(oob.ppi[0].pkt_len, 60);
        assert_eq!(oob.rx_wqe_offset, 0);
        let mut data = [0; 60];
        queues.gm.read_at(0x5000, &mut data).unwrap();
        assert_eq!(data, [0x5a; 60]);
        assert_eq!(completion(&queues, 1).params.owner_count(), 0);
    }

    #[test]
    fn lso_ipv4_offloads_header_checksums_the_guest_did_not_request() {
        let s_oob = ManaTxShortOob::new().with_is_outer_ipv4(true);
        let flags = tx_flags(&s_oob, true);
        assert!(flags.offload_tcp_segmentation());
        assert!(flags.offload_ip_header_checksum());
        assert!(flags.offload_tcp_checksum());
        assert!(!flags.offload_udp_checksum());
        assert!(flags.is_ipv4());
        assert!(!flags.is_ipv6());
    }

    #[test]
    fn lso_ipv6_has_no_ip_header_checksum() {
        let s_oob = ManaTxShortOob::new().with_is_outer_ipv6(true);
        let flags = tx_flags(&s_oob, true);
        assert!(flags.offload_tcp_segmentation());
        assert!(!flags.offload_ip_header_checksum());
        assert!(flags.offload_tcp_checksum());
        assert!(flags.is_ipv6());
        assert!(!flags.is_ipv4());
    }

    #[test]
    fn non_lso_send_offloads_only_what_the_guest_requested() {
        let s_oob = ManaTxShortOob::new().with_is_outer_ipv4(true);
        assert_eq!(
            tx_flags(&s_oob, false).into_bits(),
            TxFlags::new().with_is_ipv4(true).into_bits()
        );

        let s_oob = s_oob.with_comp_iphdr_csum(true).with_comp_udp_csum(true);
        let flags = tx_flags(&s_oob, false);
        assert!(flags.offload_ip_header_checksum());
        assert!(flags.offload_udp_checksum());
        assert!(!flags.offload_tcp_checksum());
        assert!(!flags.offload_tcp_segmentation());
    }
}
