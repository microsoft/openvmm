// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use crate::bnic::BasicNic;
use crate::dma::DmaRegion;
use crate::queues::QueueAllocError;
use crate::queues::Queues;
use anyhow::Context;
use anyhow::anyhow;
use gdma_defs::GDMA_EQE_HWC_INIT_DATA;
use gdma_defs::GDMA_EQE_HWC_INIT_DONE;
use gdma_defs::GDMA_EQE_HWC_INIT_EQ_ID_DB;
use gdma_defs::GDMA_EQE_HWC_RESET_REQUEST;
use gdma_defs::GDMA_EQE_TEST_EVENT;
use gdma_defs::GdmaChangeMsixVectorIndexForEq;
use gdma_defs::GdmaCreateDmaRegionReq;
use gdma_defs::GdmaCreateDmaRegionResp;
use gdma_defs::GdmaCreateQueueReq;
use gdma_defs::GdmaCreateQueueResp;
use gdma_defs::GdmaDevId;
use gdma_defs::GdmaDevType;
use gdma_defs::GdmaDisableQueueReq;
use gdma_defs::GdmaGenerateResetEventReq;
use gdma_defs::GdmaGenerateTestEventReq;
use gdma_defs::GdmaListDevicesResp;
use gdma_defs::GdmaQueryMaxResourcesResp;
use gdma_defs::GdmaQueueType;
use gdma_defs::GdmaRegisterDeviceResp;
use gdma_defs::GdmaReqHdr;
use gdma_defs::GdmaRequestType;
use gdma_defs::GdmaRespHdr;
use gdma_defs::GdmaVerifyVerReq;
use gdma_defs::GdmaVerifyVerResp;
use gdma_defs::HWC_DEV_ID;
use gdma_defs::HWC_INIT_DATA_CQID;
use gdma_defs::HWC_INIT_DATA_GPA_MKEY;
use gdma_defs::HWC_INIT_DATA_MAX_NUM_CQS;
use gdma_defs::HWC_INIT_DATA_MAX_REQUEST;
use gdma_defs::HWC_INIT_DATA_MAX_RESPONSE;
use gdma_defs::HWC_INIT_DATA_PDID;
use gdma_defs::HWC_INIT_DATA_QUEUE_DEPTH;
use gdma_defs::HWC_INIT_DATA_RQID;
use gdma_defs::HWC_INIT_DATA_SQID;
use gdma_defs::HwcInitEqIdDb;
use gdma_defs::HwcInitTypeData;
use gdma_defs::HwcRxOob;
use gdma_defs::HwcTxOob;
use gdma_defs::PAGE_SIZE64;
use gdma_defs::Wqe;
use gdma_defs::access::WqeAccess;
use guestmem::Limit;
use guestmem::MemoryRead;
use guestmem::MemoryWrite;
use slab::Slab;
use std::future::poll_fn;
use std::sync::Arc;
use task_control::AsyncRun;
use task_control::InspectTaskMut;
use task_control::StopTask;
use zerocopy::FromBytes;
use zerocopy::FromZeros;
use zerocopy::IntoBytes;

const BNIC_DEV_ID: GdmaDevId = GdmaDevId {
    ty: GdmaDevType::GDMA_DEVICE_MANA,
    instance: 1,
};

pub struct HwControl {
    state: HwState,
    _eq_id: u32,
    cq_id: u32,
    sq_id: u32,
    rq_id: u32,
    pending_sqe: Option<Wqe>,

    bnic_enabled: bool,
}

impl InspectTaskMut<HwControl> for Devices {
    fn inspect_mut(&mut self, req: inspect::Request<'_>, hwc: Option<&mut HwControl>) {
        let mut resp = req.respond();
        if let Some(hwc) = hwc {
            resp.child("hwc", |req| {
                req.respond()
                    .field("eq_id", hwc._eq_id)
                    .field("cq_id", hwc.cq_id)
                    .field("sq_id", hwc.sq_id)
                    .field("rq_id", hwc.rq_id);
            })
            .field("bnic/enabled", hwc.bnic_enabled);
        }
        resp.field_mut("bnic", &mut self.bnic);
    }
}

pub struct Devices {
    pub bnic: BasicNic,
}

pub struct HwState {
    pub queues: Arc<Queues>,
    pub dma_regions: Slab<DmaRegion>,
}

impl HwState {
    pub fn get_dma_region(
        &self,
        gdma_region: u64,
        expected_size: u32,
    ) -> anyhow::Result<&DmaRegion> {
        let region = self
            .dma_regions
            .get(gdma_region.wrapping_sub(1) as usize)
            .context("dma region not found")?;
        if region.len() != expected_size as usize {
            anyhow::bail!("dma region size does not match");
        }
        Ok(region)
    }

    pub fn remove_dma_region(&mut self, gdma_region: u64) -> anyhow::Result<()> {
        self.dma_regions
            .try_remove(gdma_region.wrapping_sub(1) as usize)
            .context("invalid gdma region")?;
        Ok(())
    }
}

impl HwControl {
    pub fn new(
        queues: Arc<Queues>,
        sq_gpa: u64,
        rq_gpa: u64,
        cq_gpa: u64,
        eq_gpa: u64,
        eq_msix: u32,
    ) -> Result<Self, QueueAllocError> {
        tracing::info!(sq_gpa, rq_gpa, cq_gpa, eq_gpa, eq_msix, "enabling hwc");

        let sq_region = DmaRegion::new(vec![sq_gpa], 0, PAGE_SIZE64).unwrap();
        let rq_region = DmaRegion::new(vec![rq_gpa], 0, PAGE_SIZE64).unwrap();
        let eq_region = DmaRegion::new(vec![eq_gpa], 0, PAGE_SIZE64).unwrap();
        let cq_region = DmaRegion::new(vec![cq_gpa], 0, PAGE_SIZE64).unwrap();

        let sq_id = queues.alloc_wq(true, sq_region)?;
        let rq_id = queues.alloc_wq(false, rq_region)?;
        let eq_id = queues.alloc_eq(eq_region, eq_msix)?;
        let cq_id = queues.alloc_cq(cq_region, eq_id)?;

        queues.post_eq(
            eq_id,
            GDMA_EQE_HWC_INIT_EQ_ID_DB,
            HwcInitEqIdDb::new()
                .with_eq_id(eq_id as u16)
                .with_doorbell(0)
                .as_bytes(),
        );

        let data = [
            (HWC_INIT_DATA_CQID, cq_id),
            (HWC_INIT_DATA_RQID, rq_id),
            (HWC_INIT_DATA_SQID, sq_id),
            (HWC_INIT_DATA_QUEUE_DEPTH, 1),
            (HWC_INIT_DATA_MAX_REQUEST, 0x1000),
            (HWC_INIT_DATA_MAX_RESPONSE, 0x1000),
            (HWC_INIT_DATA_MAX_NUM_CQS, queues.max_cqs()),
            (HWC_INIT_DATA_PDID, 0),
            (HWC_INIT_DATA_GPA_MKEY, 0),
        ];

        for (ty, val) in data {
            queues.post_eq(
                eq_id,
                GDMA_EQE_HWC_INIT_DATA,
                HwcInitTypeData::new()
                    .with_ty(ty)
                    .with_value(val)
                    .as_bytes(),
            );
        }

        queues.post_eq(eq_id, GDMA_EQE_HWC_INIT_DONE, &[]);

        Ok(Self {
            state: HwState {
                queues,
                dma_regions: Slab::new(),
            },
            _eq_id: eq_id,
            cq_id,
            sq_id,
            rq_id,
            pending_sqe: None,

            bnic_enabled: false,
        })
    }

    async fn process(
        &mut self,
        devices: &mut Devices,
        check: &task_control::StopCheck<'_>,
    ) -> Result<(), task_control::Cancelled> {
        tracing::info!("starting hwc");

        loop {
            check.check()?;
            if let Err(err) = self.process_request(devices).await {
                tracing::error!(
                    error = err.as_ref() as &dyn std::error::Error,
                    "hwc failure"
                );
                return Ok(());
            }
        }
    }

    async fn process_request(&mut self, devices: &mut Devices) -> anyhow::Result<()> {
        // Retain accepted work if cancellation interrupts the wait for an RQE.
        if self.pending_sqe.is_none() {
            self.pending_sqe = Some(poll_fn(|cx| self.state.queues.poll_sq(self.sq_id, cx)).await);
        }
        let (rqe_offset, rqe) = poll_fn(|cx| self.state.queues.poll_rq(self.rq_id, cx)).await;
        let sqe = self.pending_sqe.take().unwrap();

        let queues = self.state.queues.clone();
        let tx_oob = HwcTxOob::read_from_prefix(sqe.oob())
            .map_err(|_| anyhow!("reading tx oob"))?
            .0; // TODO: zerocopy: map_err, use-rest-of-range, use error details in the returned `anyhow!` (https://github.com/microsoft/openvmm/issues/759)
        if tx_oob.flags3.vscq_id() != self.cq_id {
            anyhow::bail!(
                "mismatched cq id: {} != {}",
                tx_oob.flags3.vscq_id(),
                self.cq_id
            );
        }

        if tx_oob.flags4.vsq_id() != self.sq_id {
            anyhow::bail!(
                "mismatched sq id: {} != {}",
                tx_oob.flags4.vsq_id(),
                self.sq_id
            );
        }

        let read = sqe.access(&queues.gm);
        let hdr: GdmaReqHdr = read
            .clone()
            .read_plain()
            .context("reading request message header")?;

        if hdr.req.msg_size as u64 > PAGE_SIZE64 {
            anyhow::bail!(
                "request message size {} exceeds page size {PAGE_SIZE64}",
                hdr.req.msg_size
            );
        }
        if hdr.resp.msg_size as u64 > PAGE_SIZE64 {
            anyhow::bail!(
                "response message size {} exceeds page size {PAGE_SIZE64}",
                hdr.resp.msg_size
            );
        }

        let mut read = MemoryRead::limit(read, hdr.req.msg_size as usize);
        read.skip(size_of_val(&hdr))
            .context("message size too small")?;

        let mut write = MemoryWrite::limit(rqe.access(&queues.gm), hdr.resp.msg_size as usize);
        let mut header_write = write.clone();
        write
            .skip(size_of::<GdmaRespHdr>())
            .context("response message too small")?;

        let r = match hdr.req.msg_type >> 16 {
            0 => self.handle_req(&hdr, read, write),
            _ => {
                // Device specific.
                if hdr.dev_id == BNIC_DEV_ID && self.bnic_enabled {
                    devices
                        .bnic
                        .handle_req(&mut self.state, &hdr, read, write)
                        .await
                } else {
                    Err(anyhow!("unknown device {:?}", hdr.dev_id))
                }
            }
        };

        let (status, response_len) = match r {
            Ok(response_len) => (0, response_len),
            Err(err) => {
                tracing::warn!(msg_type = hdr.req.msg_type, dev_id = ?hdr.dev_id, error = err.as_ref() as &dyn std::error::Error, "req error");
                (1, 0)
            }
        };

        self.state.queues.post_cq(self.cq_id, &[], self.sq_id, true);

        let resp = GdmaRespHdr {
            response: hdr.resp,
            dev_id: hdr.dev_id,
            activity_id: hdr.activity_id,
            status,
            reserved: 0,
        };

        header_write
            .write(resp.as_bytes())
            .context("writing response message header")?;

        let rx_oob = HwcRxOob {
            wqe_addr_low_or_offset: rqe_offset,
            tx_oob_data_size: (size_of_val(&resp) + response_len) as u32,
            ..FromZeros::new_zeroed()
        };

        self.state
            .queues
            .post_cq(self.cq_id, rx_oob.as_bytes(), self.rq_id, false);
        Ok(())
    }

    fn handle_req(
        &mut self,
        hdr: &GdmaReqHdr,
        mut read: Limit<WqeAccess<'_>>,
        mut write: Limit<WqeAccess<'_>>,
    ) -> anyhow::Result<usize> {
        tracing::debug!(msg_type = ?GdmaRequestType(hdr.req.msg_type), "hwc request");

        let response_len = match GdmaRequestType(hdr.req.msg_type) {
            GdmaRequestType::GDMA_GENERATE_TEST_EQE => {
                let req: GdmaGenerateTestEventReq =
                    read.read_plain().context("reading test eqe request")?;
                self.state
                    .queues
                    .post_eq(req.queue_index, GDMA_EQE_TEST_EVENT, &[]);

                0
            }
            GdmaRequestType::GDMA_GENERATE_RESET_REQUEST_EQE => {
                let req: GdmaGenerateResetEventReq =
                    read.read_plain().context("reading reset request EQE")?;
                self.state.queues.post_eq(
                    req.queue_index,
                    GDMA_EQE_HWC_RESET_REQUEST,
                    req.data.as_bytes(),
                );
                0
            }
            GdmaRequestType::GDMA_VERIFY_VF_DRIVER_VERSION => {
                let req: GdmaVerifyVerReq = read
                    .read_plain()
                    .context("reading verify vf driver request")?;

                let drv_name = core::ffi::CStr::from_bytes_until_nul(&req.os_ver_str1)
                    .ok()
                    .and_then(|c| c.to_str().ok())
                    .unwrap_or("<invalid>");
                let drv_commit = core::ffi::CStr::from_bytes_until_nul(&req.os_ver_str2)
                    .ok()
                    .and_then(|c| c.to_str().ok())
                    .unwrap_or("<invalid>");
                tracing::info!(
                    drv_name,
                    drv_commit,
                    os_type = format_args!("{:#x}", req.os_type),
                    os_ver_major = req.os_ver_major,
                    os_ver_minor = req.os_ver_minor,
                    os_ver_build = req.os_ver_build,
                    os_ver_platform = req.os_ver_platform,
                    cap_flags1 = format_args!("{:#x}", req.gd_drv_cap_flags1),
                    protocol_ver_min = req.protocol_ver_min,
                    protocol_ver_max = req.protocol_ver_max,
                    "vf driver version",
                );

                let resp = GdmaVerifyVerResp {
                    gdma_protocol_ver: req.protocol_ver_min,
                    pf_cap_flags1: 0,
                    pf_cap_flags2: 0,
                    pf_cap_flags3: 0,
                    pf_cap_flags4: 0,
                };

                write
                    .write(resp.as_bytes())
                    .context("writing verify vf driver response")?;
                size_of_val(&resp)
            }
            GdmaRequestType::GDMA_QUERY_MAX_RESOURCES => {
                let resp = GdmaQueryMaxResourcesResp {
                    status: 0,
                    max_sq: self.state.queues.max_sqs(),
                    max_rq: self.state.queues.max_rqs(),
                    max_cq: self.state.queues.max_cqs(),
                    max_eq: self.state.queues.max_eqs(),
                    max_db: 1,
                    max_mst: 1,
                    max_cq_mod_ctx: 0,
                    max_mod_cq: 0,
                    max_msix: 64,
                };

                write
                    .write(resp.as_bytes())
                    .context("writing query max response")?;
                size_of_val(&resp)
            }
            GdmaRequestType::GDMA_LIST_DEVICES => {
                let mut resp = GdmaListDevicesResp {
                    num_of_devs: 2,
                    ..FromZeros::new_zeroed()
                };
                resp.devs[0] = HWC_DEV_ID;
                resp.devs[1] = BNIC_DEV_ID;

                write
                    .write(resp.as_bytes())
                    .context("writing gdma list response")?;
                size_of_val(&resp)
            }
            GdmaRequestType::GDMA_REGISTER_DEVICE => {
                if hdr.dev_id != BNIC_DEV_ID {
                    anyhow::bail!("invalid device id: {:?}", hdr.dev_id);
                }

                if self.bnic_enabled {
                    anyhow::bail!("bnic already enabled");
                }

                self.bnic_enabled = true;

                let resp = GdmaRegisterDeviceResp {
                    pdid: 0,
                    gpa_mkey: 0,
                    db_id: 0,
                };

                write
                    .write(resp.as_bytes())
                    .context("writing register device response")?;
                size_of_val(&resp)
            }
            GdmaRequestType::GDMA_CREATE_DMA_REGION => {
                let req: GdmaCreateDmaRegionReq =
                    read.read_plain().context("reading dma region request")?;
                if req.page_addr_list_len != req.page_count {
                    anyhow::bail!("large regions not supported");
                }
                let pages: Vec<u64> = read
                    .read_n(req.page_addr_list_len as usize)
                    .context("reading dma region pages")?;

                let dma_region = DmaRegion::new(pages, req.offset_in_page, req.length)
                    .context("failed to parse dma region input")?;
                let gdma_region = self.state.dma_regions.insert(dma_region) as u64 + 1;

                let resp = GdmaCreateDmaRegionResp { gdma_region };
                write
                    .write(resp.as_bytes())
                    .context("writing dma region response")?;
                size_of_val(&resp)
            }
            GdmaRequestType::GDMA_CREATE_QUEUE => {
                let req: GdmaCreateQueueReq = read.read_plain().context("reading queue request")?;
                if req.queue_type != GdmaQueueType::GDMA_EQ {
                    anyhow::bail!("unsupported queue type: {:?}", req.queue_type);
                }

                let region = self.state.get_dma_region(req.gdma_region, req.queue_size)?;

                let eq_id = self
                    .state
                    .queues
                    .alloc_eq(region.clone(), req.eq_pci_msix_index)
                    .context("failed to allocate queue")?;

                let resp = GdmaCreateQueueResp { queue_index: eq_id };
                write
                    .write(resp.as_bytes())
                    .context("writing queue response")?;

                // Take ownership of the DMA region.
                self.state.remove_dma_region(req.gdma_region).unwrap();
                size_of_val(&resp)
            }
            GdmaRequestType::GDMA_DISABLE_QUEUE => {
                let req: GdmaDisableQueueReq = read
                    .read_plain()
                    .context("failed to read disable queue request")?;
                if req.queue_type != GdmaQueueType::GDMA_EQ {
                    anyhow::bail!("unsupported queue type: {:?}", req.queue_type);
                }
                if req.alloc_res_id_on_creation != 1 {
                    tracing::warn!(
                        value = req.alloc_res_id_on_creation,
                        "mystery value not set to 1"
                    );
                }
                self.state.queues.free_eq(req.queue_index)?;
                0
            }
            GdmaRequestType::GDMA_CHANGE_MSIX_FOR_EQ => {
                let req: GdmaChangeMsixVectorIndexForEq = read
                    .read_plain()
                    .context("failed to read change eq msix request")?;
                self.state
                    .queues
                    .update_eq_msix(req.queue_index, req.msix)?;
                0
            }
            GdmaRequestType::GDMA_DEREGISTER_DEVICE => {
                if hdr.dev_id != BNIC_DEV_ID {
                    anyhow::bail!("invalid device id: {:?}", hdr.dev_id);
                }

                if !self.bnic_enabled {
                    anyhow::bail!("bnic not enabled");
                }

                self.bnic_enabled = false;
                0
            }
            ty => {
                anyhow::bail!("unsupported message type: {:x?}", ty);
            }
        };
        Ok(response_len)
    }
}

impl AsyncRun<HwControl> for Devices {
    async fn run(
        &mut self,
        stop: &mut StopTask<'_>,
        hwc: &mut HwControl,
    ) -> Result<(), task_control::Cancelled> {
        let check = stop.check_handle()?;
        stop.until_stopped(hwc.process(self, &check)).await?
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::VportConfig;
    use crate::bnic::BnicConfig;
    use crate::test_helpers as helper;
    use chipset_device::pci::ByteEnabledDwordWrite;
    use gdma_defs::CqEqDoorbellValue;
    use gdma_defs::Cqe;
    use gdma_defs::Eqe;
    use gdma_defs::WqDoorbellValue;
    use net_backend::loopback::LoopbackEndpoint;
    use net_backend_resources::mac_address::MacAddress;
    use pal_async::DefaultPool;
    use parking_lot::Mutex;
    use pci_core::capabilities::PciCapability;
    use pci_core::capabilities::msix::MsixEmulator;
    use pci_core::msi::MsiConnection;
    use pci_core::msi::SignalMsi;
    use task_control::TaskControl;
    use test_with_tracing::test;

    impl SignalMsi for helper::StopHook {
        fn signal_msi(&self, _: Option<u32>, _: u64, _: u32) {
            self.fire();
        }
    }

    struct StopWhenBlocked {
        devices: Devices,
        hook: Arc<helper::StopHook>,
    }

    impl AsyncRun<HwControl> for StopWhenBlocked {
        async fn run(
            &mut self,
            stop: &mut StopTask<'_>,
            hwc: &mut HwControl,
        ) -> Result<(), task_control::Cancelled> {
            let mut run = std::pin::pin!(self.devices.run(stop, hwc));
            poll_fn(|cx| {
                let result = Future::poll(run.as_mut(), cx);
                if result.is_pending() {
                    self.hook.fire();
                }
                result
            })
            .await
        }
    }

    #[test]
    fn accepted_sqe_survives_stop_while_waiting_for_rqe_and_is_processed_once() {
        let mut pool = DefaultPool::new();
        let completed = Arc::new(helper::StopHook::default());
        let blocked = Arc::new(helper::StopHook::default());
        let connection = MsiConnection::new();
        connection.connect(completed.clone());
        let (mut msix, mut capability) = MsixEmulator::new(4, 64, &connection.target());
        let queues = helper::queues(&pool, Some(&msix));
        let hwc = HwControl::new(
            queues.clone(),
            helper::SQ,
            helper::RQ,
            helper::CQ,
            helper::EQ,
            0,
        )
        .unwrap();
        let event_eq = queues.alloc_eq(helper::region(0x6000), 0).unwrap();
        msix.write_u32(12, 0);
        capability.write(
            0,
            ByteEnabledDwordWrite::with_all_bytes_enabled(0x8000_0000),
        );
        let request = GdmaReqHdr {
            req: gdma_defs::GdmaMsgHdr {
                msg_type: GdmaRequestType::GDMA_GENERATE_TEST_EQE.0,
                msg_size: (size_of::<GdmaReqHdr>() + size_of::<GdmaGenerateTestEventReq>()) as u32,
                ..FromZeros::new_zeroed()
            },
            resp: gdma_defs::GdmaMsgHdr {
                hwc_msg_id: 7,
                msg_size: size_of::<GdmaRespHdr>() as u32,
                ..FromZeros::new_zeroed()
            },
            dev_id: HWC_DEV_ID,
            activity_id: 42,
        };
        queues.gm.write_plain(0x5000, &request).unwrap();
        queues
            .gm
            .write_plain(
                0x5000 + size_of::<GdmaReqHdr>() as u64,
                &GdmaGenerateTestEventReq {
                    queue_index: event_eq,
                },
            )
            .unwrap();
        let oob = HwcTxOob {
            flags3: gdma_defs::HwcTxOobFlags3::new().with_vscq_id(hwc.cq_id),
            flags4: gdma_defs::HwcTxOobFlags4::new().with_vsq_id(hwc.sq_id),
            ..FromZeros::new_zeroed()
        };
        let sq_len = helper::write_wqe(
            &queues,
            helper::SQ,
            oob.as_bytes(),
            0x5000,
            request.req.msg_size,
        );
        let sq_id = hwc.sq_id;
        let rq_id = hwc.rq_id;
        queues.doorbell_sq(WqDoorbellValue::new().with_id(sq_id).with_tail(sq_len));
        let worker = StopWhenBlocked {
            devices: Devices {
                bnic: BasicNic::new(
                    vec![VportConfig {
                        mac_address: MacAddress::new([0; 6]),
                        endpoint: Box::new(LoopbackEndpoint::new()),
                    }],
                    BnicConfig::default(),
                ),
            },
            hook: blocked.clone(),
        };
        let control = Arc::new(Mutex::new(TaskControl::new(worker)));
        control
            .lock()
            .insert(pool.driver(), "gdma-hwc-blocked", hwc);
        let signal = blocked.arm(&control);
        assert!(control.lock().start());
        helper::stopped(&mut pool, &control, signal);
        let mut cx = std::task::Context::from_waker(std::task::Waker::noop());
        // The worker really consumed the only SQE, but cannot process it yet.
        assert!(queues.poll_sq(sq_id, &mut cx).is_pending());
        let cqe: Cqe = queues.gm.read_plain(helper::CQ).unwrap();
        assert_eq!(cqe.params.owner_count(), 0);
        let event: Eqe = queues.gm.read_plain(0x6000).unwrap();
        assert_eq!(event.params.owner_count(), 0);

        let rq_len = helper::write_wqe(&queues, helper::RQ, &[], 0x7000, request.resp.msg_size);
        queues.doorbell_rq(WqDoorbellValue::new().with_id(rq_id).with_tail(rq_len));
        let signal = completed.arm(&control);
        assert!(control.lock().start());
        helper::stopped(&mut pool, &control, signal);
        let response: GdmaRespHdr = queues.gm.read_plain(0x7000).unwrap();
        assert_eq!(response.status, 0);
        assert_eq!(response.activity_id, 42);
        assert_eq!(response.response.hwc_msg_id, 7);
        for (index, send, wq) in [(0, true, sq_id), (1, false, rq_id)] {
            let cqe: Cqe = queues
                .gm
                .read_plain(helper::CQ + index * size_of::<Cqe>() as u64)
                .unwrap();
            assert_eq!(cqe.params.owner_count(), 1);
            assert_eq!(cqe.params.is_send_wq(), send);
            assert_eq!(cqe.params.wq_number(), wq);
        }
        let event: Eqe = queues.gm.read_plain(0x6000).unwrap();
        assert_eq!(event.params.owner_count(), 1);
        assert_eq!(event.params.event_type(), GDMA_EQE_TEST_EVENT);

        // A spare RQE and another restart must not replay the accepted SQE.
        let extra_len = helper::write_wqe(
            &queues,
            helper::RQ + rq_len as u64,
            &[],
            0x7100,
            request.resp.msg_size,
        );
        queues.doorbell_rq(
            WqDoorbellValue::new()
                .with_id(rq_id)
                .with_tail(rq_len + extra_len),
        );
        let signal = blocked.arm(&control);
        assert!(control.lock().start());
        helper::stopped(&mut pool, &control, signal);
        let cqe: Cqe = queues
            .gm
            .read_plain(helper::CQ + 2 * size_of::<Cqe>() as u64)
            .unwrap();
        assert_eq!(cqe.params.owner_count(), 0);
        let event: Eqe = queues
            .gm
            .read_plain(0x6000 + size_of::<Eqe>() as u64)
            .unwrap();
        assert_eq!(event.params.owner_count(), 0);
        let response: GdmaRespHdr = queues.gm.read_plain(0x7100).unwrap();
        assert_eq!(response.activity_id, 0);
        assert!(queues.poll_rq(rq_id, &mut cx).is_ready());
    }

    #[test]
    fn ready_requests_finish_publication_before_cancelling_and_resume() {
        let mut pool = DefaultPool::new();
        let hook = Arc::new(helper::StopHook::default());
        let connection = MsiConnection::new();
        connection.connect(hook.clone());
        let (mut msix, mut capability) = MsixEmulator::new(4, 64, &connection.target());
        let queues = helper::queues(&pool, Some(&msix));
        let hwc = HwControl::new(
            queues.clone(),
            helper::SQ,
            helper::RQ,
            helper::CQ,
            helper::EQ,
            0,
        )
        .unwrap();
        let event_eq = queues.alloc_eq(helper::region(0x6000), 0).unwrap();
        // Enable real MSI delivery only after HWC initialization. The test EQ's
        // interrupt requests stop from inside handle_req, before CQ publication.
        msix.write_u32(12, 0);
        capability.write(
            0,
            ByteEnabledDwordWrite::with_all_bytes_enabled(0x8000_0000),
        );

        let oob = HwcTxOob {
            flags3: gdma_defs::HwcTxOobFlags3::new().with_vscq_id(hwc.cq_id),
            flags4: gdma_defs::HwcTxOobFlags4::new().with_vsq_id(hwc.sq_id),
            ..FromZeros::new_zeroed()
        };
        let mut sq_tail = 0;
        let mut rq_tail = 0;
        for index in 0..2 {
            let req = GdmaReqHdr {
                req: gdma_defs::GdmaMsgHdr {
                    msg_type: GdmaRequestType::GDMA_GENERATE_TEST_EQE.0,
                    msg_size: (size_of::<GdmaReqHdr>() + size_of::<GdmaGenerateTestEventReq>())
                        as u32,
                    ..FromZeros::new_zeroed()
                },
                resp: gdma_defs::GdmaMsgHdr {
                    hwc_msg_id: index as u16,
                    msg_size: size_of::<GdmaRespHdr>() as u32,
                    ..FromZeros::new_zeroed()
                },
                dev_id: HWC_DEV_ID,
                activity_id: index as u32 + 1,
            };
            let request_gpa = 0x5000 + index * 0x100;
            queues.gm.write_plain(request_gpa, &req).unwrap();
            queues
                .gm
                .write_plain(
                    request_gpa + size_of::<GdmaReqHdr>() as u64,
                    &GdmaGenerateTestEventReq {
                        queue_index: event_eq,
                    },
                )
                .unwrap();
            sq_tail += helper::write_wqe(
                &queues,
                helper::SQ + sq_tail as u64,
                oob.as_bytes(),
                request_gpa,
                req.req.msg_size,
            );
            rq_tail += helper::write_wqe(
                &queues,
                helper::RQ + rq_tail as u64,
                &[],
                0x7000 + index * 0x100,
                req.resp.msg_size,
            );
        }
        queues.doorbell_sq(WqDoorbellValue::new().with_id(hwc.sq_id).with_tail(sq_tail));
        queues.doorbell_rq(WqDoorbellValue::new().with_id(hwc.rq_id).with_tail(rq_tail));
        let devices = Devices {
            bnic: BasicNic::new(
                vec![VportConfig {
                    mac_address: MacAddress::new([0; 6]),
                    endpoint: Box::new(LoopbackEndpoint::new()),
                }],
                BnicConfig::default(),
            ),
        };
        let control = Arc::new(Mutex::new(TaskControl::new(devices)));
        control.lock().insert(pool.driver(), "gdma-hwc", hwc);

        for index in 0..2 {
            let signal = hook.arm(&control);
            assert!(control.lock().start());
            helper::stopped(&mut pool, &control, signal);
            let control = control.lock();
            let hwc = control.state().unwrap();
            let response: GdmaRespHdr = queues.gm.read_plain(0x7000 + index * 0x100).unwrap();
            assert_eq!(response.status, 0);
            assert_eq!(response.activity_id, index as u32 + 1);
            assert_eq!(response.response.hwc_msg_id, index as u16);
            for (entry, send, wq) in [
                (index * 2, true, hwc.sq_id),
                (index * 2 + 1, false, hwc.rq_id),
            ] {
                let cqe: Cqe = queues
                    .gm
                    .read_plain(helper::CQ + entry * size_of::<Cqe>() as u64)
                    .unwrap();
                assert_eq!(cqe.params.owner_count(), 1);
                assert_eq!(cqe.params.is_send_wq(), send);
                assert_eq!(cqe.params.wq_number(), wq);
                if !send {
                    let rx_oob = HwcRxOob::read_from_prefix(&cqe.data).unwrap().0;
                    assert_eq!(rx_oob.tx_oob_data_size, size_of::<GdmaRespHdr>() as u32);
                    assert_eq!(rx_oob.wqe_addr_low_or_offset, index as u32 * 32);
                }
            }
            let next: Cqe = queues
                .gm
                .read_plain(helper::CQ + (index + 1) * 2 * size_of::<Cqe>() as u64)
                .unwrap();
            assert_eq!(next.params.owner_count(), 0);
            if index == 0 {
                let next_response: GdmaRespHdr = queues.gm.read_plain(0x7100).unwrap();
                assert_eq!(next_response.activity_id, 0);
                queues.doorbell_eq(
                    CqEqDoorbellValue::new()
                        .with_id(event_eq)
                        .with_tail(PAGE_SIZE64 as u32 / size_of::<Eqe>() as u32 + 1)
                        .with_arm(true),
                );
            }
        }
    }
}
