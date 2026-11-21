// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

#![forbid(unsafe_code)]
#![expect(missing_docs)]

mod bnic;
mod dma;
mod hwc;
mod queues;
pub mod resolver;
#[cfg(test)]
mod test_helpers;

use chipset_device::ChipsetDevice;
use chipset_device::io::IoResult;
use chipset_device::mmio::MmioIntercept;
use chipset_device::mmio::RegisterMmioIntercept;
use chipset_device::pci::ByteEnabledDwordRead;
use chipset_device::pci::ByteEnabledDwordWrite;
use chipset_device::pci::PciConfigSpace;
use device_emulators::ReadWriteRequestType;
use device_emulators::read_as_u32_chunks;
use device_emulators::write_as_u32_chunks;
use futures::FutureExt;
use gdma_defs::CqEqDoorbellValue;
use gdma_defs::DB_CQ;
use gdma_defs::DB_EQ;
use gdma_defs::DB_RQ;
use gdma_defs::DB_RQ_CLIENT_DATA;
use gdma_defs::DB_SQ;
use gdma_defs::PAGE_SIZE64;
use gdma_defs::RegMap;
use gdma_defs::SMC_MSG_TYPE_DESTROY_HWC_VERSION;
use gdma_defs::SMC_MSG_TYPE_ESTABLISH_HWC_VERSION;
use gdma_defs::SMC_MSG_TYPE_REPORT_HWC_TIMEOUT_VERSION;
use gdma_defs::SmcMessageType;
use gdma_defs::SmcProtoHdr;
use gdma_defs::WqDoorbellValue;
use guestmem::GuestMemory;
use hwc::Devices;
use hwc::HwControl;
use inspect::InspectMut;
use net_backend::Endpoint;
use net_backend_resources::mac_address::MacAddress;
use pci_core::capabilities::msix::MsixEmulator;
use pci_core::cfg_space_emu::BarMemoryKind;
use pci_core::cfg_space_emu::ConfigSpaceType0Emulator;
use pci_core::cfg_space_emu::DeviceBars;
use pci_core::msi::MsiTarget;
use pci_core::spec::hwid::ClassCode;
use pci_core::spec::hwid::HardwareIds;
use pci_core::spec::hwid::ProgrammingInterface;
use pci_core::spec::hwid::Subclass;
use queues::Queues;
use std::ops::Range;
use std::sync::Arc;
use task_control::TaskControl;
use thiserror::Error;
use vmcore::device_state::ChangeDeviceState;
use vmcore::save_restore::RestoreError;
use vmcore::save_restore::SaveError;
use vmcore::save_restore::SaveRestore;
use vmcore::save_restore::SavedStateNotSupported;
use vmcore::vm_task::VmTaskDriverSource;
use zerocopy::FromZeros;
use zerocopy::IntoBytes;

const REGMAP: Range<usize> = 0..40;
const SHMEM: Range<usize> = 40..72;
const SHMEM_LEN: usize = SHMEM.end - SHMEM.start;
const DOORBELLS: Range<usize> = 4096..8192;

pub struct GdmaDevice {
    config: ConfigSpaceType0Emulator,
    msix: MsixEmulator,
    regmap: RegMap,
    shmem: Shmem,
    destroying_hwc: bool,
    queues: Arc<Queues>,
    hwc: TaskControl<Devices, HwControl>,
}

impl InspectMut for GdmaDevice {
    fn inspect_mut(&mut self, req: inspect::Request<'_>) {
        req.respond()
            .field("config", &self.config)
            .field("queues", &self.queues)
            .merge(&mut self.hwc);
    }
}

struct Shmem([u32; SHMEM_LEN / 4]);

trait ContainsRange<T> {
    fn contains_range(&self, _: &Range<T>) -> bool;
    fn overlaps_range(&self, _: &Range<T>) -> bool;
}

impl<T: Ord> ContainsRange<T> for Range<T> {
    fn contains_range(&self, r: &Range<T>) -> bool {
        r.start >= self.start && r.end <= self.end
    }

    fn overlaps_range(&self, r: &Range<T>) -> bool {
        r.end >= self.start && self.end >= r.start
    }
}

#[derive(Debug, Error)]
enum SmcError {
    #[error("request is a response")]
    RequestIsResponse,
    #[error("unsupported request version")]
    UnsupportedVersion,
    #[error("hwc is already active")]
    HwcAlreadyActive,
    #[error("failed to allocate queues")]
    QueueAlloc(#[source] queues::QueueAllocError),
    #[error("unsupported request {0:#x?}")]
    UnsupportedRequest(SmcMessageType),
}

pub use bnic::BnicConfig;

pub struct VportConfig {
    pub mac_address: MacAddress,
    pub endpoint: Box<dyn Endpoint>,
}

impl GdmaDevice {
    pub fn new(
        driver_source: &VmTaskDriverSource,
        gm: GuestMemory,
        msi_target: &MsiTarget,
        vports: Vec<VportConfig>,
        mmio_registration: &mut dyn RegisterMmioIntercept,
    ) -> Self {
        Self::new_with_config(
            driver_source,
            gm,
            msi_target,
            vports,
            mmio_registration,
            BnicConfig::default(),
        )
    }

    pub fn new_with_config(
        driver_source: &VmTaskDriverSource,
        gm: GuestMemory,
        msi_target: &MsiTarget,
        vports: Vec<VportConfig>,
        mmio_registration: &mut dyn RegisterMmioIntercept,
        bnic_config: BnicConfig,
    ) -> Self {
        let (msix, msix_capability) = MsixEmulator::new(4, 64, msi_target);

        let hardware_ids = HardwareIds {
            vendor_id: pci_core::microsoft::VENDOR_ID,
            device_id: pci_core::microsoft::DeviceId::GDMA.0,
            revision_id: 1,
            prog_if: ProgrammingInterface::NETWORK_CONTROLLER_ETHERNET_GDMA,
            sub_class: Subclass::NETWORK_CONTROLLER_ETHERNET,
            base_class: ClassCode::NETWORK_CONTROLLER,
            type0_sub_vendor_id: 0,
            type0_sub_system_id: 0,
        };

        let capabilities = vec![Box::new(msix_capability) as _];

        let bar0_mem = mmio_registration.new_io_region("regs", 8192);
        let bar2_mem = mmio_registration.new_io_region("msix", msix.bar_len());

        let config = ConfigSpaceType0Emulator::new(
            hardware_ids,
            capabilities,
            Vec::new(),
            DeviceBars::new()
                .bar0(8192, BarMemoryKind::Intercept(bar0_mem))
                .bar4(msix.bar_len(), BarMemoryKind::Intercept(bar2_mem)),
        );

        let regmap = RegMap {
            micro_version_number: 1,
            minor_version_number: 0,
            major_version_number: 1,
            reserved: 0,
            vf_db_pages_zone_offset: DOORBELLS.start as u64,
            vf_db_page_sz: DOORBELLS.len() as u16,
            reserved2: 0,
            reserved3: 0,
            vf_gdma_sriov_shared_reg_start: SHMEM.start as u64,
            vf_gdma_sriov_shared_sz: SHMEM.len() as u16,
            reserved4: 0,
            reserved5: 0,
        };

        let queues = Arc::new(Queues::new(gm, driver_source.simple(), &msix));

        Self {
            config,
            msix,
            shmem: Shmem(FromZeros::new_zeroed()),
            regmap,
            queues,
            destroying_hwc: false,
            hwc: TaskControl::new(Devices {
                bnic: bnic::BasicNic::new(vports, bnic_config),
            }),
        }
    }

    fn read_regmap(&self, offset: usize, data: &mut [u8]) {
        data.copy_from_slice(&self.regmap.as_bytes()[offset..offset + data.len()]);
    }

    fn read_shmem(&mut self, offset: usize, data: &mut [u8]) {
        // If there is a pending DESTROY_HWC request, then poll whether the HWC
        // task has stopped.
        if self.destroying_hwc && self.hwc.stop().now_or_never().is_some() {
            if self.hwc.has_state() {
                let _ = self.hwc.remove();
            }
            self.destroying_hwc = false;
            self.complete_smc(0);
        }
        data.copy_from_slice(&self.shmem.0.as_bytes()[offset..offset + data.len()]);
    }

    fn write_shmem(&mut self, offset: usize, data: &[u8]) {
        self.shmem.0.as_mut_bytes()[offset..offset + data.len()].copy_from_slice(data);
        if (SHMEM_LEN - 4..SHMEM_LEN).overlaps_range(&(offset..offset + data.len())) {
            let status = match self.handle_smc() {
                Ok(true) => 0,
                Ok(false) => return,
                Err(err) => {
                    tracing::error!(error = &err as &dyn std::error::Error, "smc error");
                    1
                }
            };
            self.complete_smc(status);
        }
    }

    fn complete_smc(&mut self, status: u8) {
        let hdr = SmcProtoHdr::from(self.shmem.0[SHMEM_LEN / 4 - 1])
            .with_status(status)
            .with_is_response(true);
        self.shmem.0[SHMEM_LEN / 4 - 1] = hdr.into();
    }

    /// Returns Ok(false) if the operation should remain pending.
    fn handle_smc(&mut self) -> Result<bool, SmcError> {
        let hdr = SmcProtoHdr::from(self.shmem.0[SHMEM_LEN / 4 - 1]);
        if hdr.is_response() {
            return Err(SmcError::RequestIsResponse);
        }
        match SmcMessageType(hdr.msg_type()) {
            SmcMessageType::SMC_MSG_TYPE_ESTABLISH_HWC => {
                if hdr.msg_version() != SMC_MSG_TYPE_ESTABLISH_HWC_VERSION {
                    return Err(SmcError::UnsupportedVersion);
                }
                if self.hwc.has_state() {
                    return Err(SmcError::HwcAlreadyActive);
                }
                let packed = self.shmem.0.as_bytes();
                let high = self.shmem.0[6] as u64;
                let msix = self.shmem.0[6] >> 16;
                let low_mask = 0xffff_ffff_ffff;
                let high_mask = 0xf_0000_0000_0000;
                let eq_gpn = (u64::from_ne_bytes(packed[0..8].try_into().unwrap()) & low_mask)
                    | ((high << 48) & high_mask);
                let cq_gpn = (u64::from_ne_bytes(packed[6..14].try_into().unwrap()) & low_mask)
                    | ((high << 44) & high_mask);
                let rq_gpn = (u64::from_ne_bytes(packed[12..20].try_into().unwrap()) & low_mask)
                    | ((high << 40) & high_mask);
                let sq_gpn = (u64::from_ne_bytes(packed[18..26].try_into().unwrap()) & low_mask)
                    | ((high << 36) & high_mask);
                let hwc = HwControl::new(
                    self.queues.clone(),
                    sq_gpn * PAGE_SIZE64,
                    rq_gpn * PAGE_SIZE64,
                    cq_gpn * PAGE_SIZE64,
                    eq_gpn * PAGE_SIZE64,
                    msix,
                )
                .map_err(SmcError::QueueAlloc)?;
                self.hwc.insert(&self.queues.driver, "gdma-hwc", hwc);
                self.hwc.start();
                Ok(true)
            }
            SmcMessageType::SMC_MSG_TYPE_DESTROY_HWC => {
                if hdr.msg_version() != SMC_MSG_TYPE_DESTROY_HWC_VERSION {
                    return Err(SmcError::UnsupportedVersion);
                }
                // Tell HWC to stop. When the guest reads shared memory, we will
                // poll whether it has stopped yet.
                self.hwc.stop().now_or_never();
                self.destroying_hwc = true;
                Ok(false)
            }
            SmcMessageType::SMC_MSG_TYPE_REPORT_HWC_TIMEOUT => {
                if hdr.msg_version() < SMC_MSG_TYPE_REPORT_HWC_TIMEOUT_VERSION {
                    return Err(SmcError::UnsupportedVersion);
                }
                let rqt = self.shmem.0[0];
                let sqt = self.shmem.0[1];
                let cqn = self.shmem.0[2];
                let eqn = self.shmem.0[3];
                let flags_wait = self.shmem.0[6];
                let wait_time_mask = 0xff_ffff;
                let wait_time = flags_wait & wait_time_mask;
                let cmd_failed_mask = 0x01_u32;
                let cmd_failed_shift = 24_u32;
                let cmd_failed = (flags_wait >> cmd_failed_shift) & cmd_failed_mask;
                tracing::warn!(
                    cmd_failed,
                    wait_time,
                    rqt,
                    sqt,
                    cqn,
                    eqn,
                    wait_time,
                    "report_hwc_timeout"
                );
                Ok(true)
            }
            req => Err(SmcError::UnsupportedRequest(req)),
        }
    }

    fn write_doorbell(&mut self, offset: usize, data: u64) {
        tracing::trace!(offset, value = ?CqEqDoorbellValue::from(data), "doorbell");
        match offset as u32 {
            DB_SQ => {
                self.queues.doorbell_sq(WqDoorbellValue::from(data));
            }
            DB_RQ => {
                self.queues.doorbell_rq(WqDoorbellValue::from(data));
            }
            DB_RQ_CLIENT_DATA => {}
            DB_CQ => {
                self.queues.doorbell_cq(CqEqDoorbellValue::from(data));
            }
            DB_EQ => {
                self.queues.doorbell_eq(CqEqDoorbellValue::from(data));
            }
            _ => {
                tracing::warn!(offset, data, "bad doorbell write");
            }
        }
    }

    fn read_reg(&mut self, offset: usize, data: &mut [u8]) {
        let range = offset..offset + data.len();
        if REGMAP.contains_range(&range) {
            self.read_regmap(offset, data);
        } else if SHMEM.contains_range(&range) {
            self.read_shmem(offset - SHMEM.start, data);
        } else {
            tracing::warn!(offset, len = data.len(), "bad read");
            data.fill(!0);
        }
    }

    fn write_reg(&mut self, offset: usize, data: &[u8]) {
        let range = offset..offset + data.len();
        if SHMEM.contains_range(&range) {
            self.write_shmem(offset - SHMEM.start, data);
        } else if DOORBELLS.contains_range(&range) && data.len() == 8 {
            self.write_doorbell(
                offset - DOORBELLS.start,
                u64::from_ne_bytes(data.try_into().unwrap()),
            );
        } else {
            tracing::warn!(offset, len = data.len(), "bad write");
        }
    }
}

impl ChangeDeviceState for GdmaDevice {
    fn start(&mut self) {}

    async fn stop(&mut self) {}

    async fn reset(&mut self) {
        let Self {
            config,
            msix: _,
            regmap: _,
            shmem,
            destroying_hwc,
            queues,
            hwc,
        } = self;
        hwc.stop().await;
        let Devices { bnic } = hwc.task_mut();
        bnic.reset().await;
        if hwc.has_state() {
            hwc.remove();
        }
        queues.reset();
        *shmem = Shmem(FromZeros::new_zeroed());
        *destroying_hwc = false;
        config.reset();
    }
}

impl ChipsetDevice for GdmaDevice {
    fn supports_mmio(&mut self) -> Option<&mut dyn MmioIntercept> {
        Some(self)
    }

    fn supports_pci(&mut self) -> Option<&mut dyn PciConfigSpace> {
        Some(self)
    }
}

impl SaveRestore for GdmaDevice {
    // This device should be constructed with `omit_saved_state`.
    type SavedState = SavedStateNotSupported;

    fn save(&mut self) -> Result<Self::SavedState, SaveError> {
        Err(SaveError::NotSupported)
    }

    fn restore(&mut self, state: Self::SavedState) -> Result<(), RestoreError> {
        match state {}
    }
}

impl MmioIntercept for GdmaDevice {
    fn mmio_read(&mut self, address: u64, data: &mut [u8]) -> IoResult {
        if let Some((bar, offset)) = self.config.find_bar(address) {
            match bar {
                0 => self.read_reg(offset as usize, data),
                4 => read_as_u32_chunks(offset, data, |offset| self.msix.read_u32(offset)),
                _ => unreachable!(),
            }
        }
        IoResult::Ok
    }

    fn mmio_write(&mut self, address: u64, data: &[u8]) -> IoResult {
        if let Some((bar, offset)) = self.config.find_bar(address) {
            match bar {
                0 => self.write_reg(offset as usize, data),
                4 => write_as_u32_chunks(offset, data, |offset, ty| match ty {
                    ReadWriteRequestType::Read => Some(self.msix.read_u32(offset)),
                    ReadWriteRequestType::Write(val) => {
                        self.msix.write_u32(offset, val);
                        None
                    }
                }),
                _ => unreachable!(),
            }
        }
        IoResult::Ok
    }
}

impl PciConfigSpace for GdmaDevice {
    fn pci_cfg_read(&mut self, offset: u16, value: ByteEnabledDwordRead<'_>) -> IoResult {
        self.config.read_byte_enabled(offset, value)
    }

    fn pci_cfg_write(&mut self, offset: u16, value: ByteEnabledDwordWrite) -> IoResult {
        self.config.write_byte_enabled(offset, value)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_helpers::CQ;
    use crate::test_helpers::EQ;
    use crate::test_helpers::RQ;
    use crate::test_helpers::SQ;
    use crate::test_helpers::write_wqe;
    use chipset_device::mmio::ExternallyManagedMmioIntercepts;
    use gdma_defs::GdmaCreateDmaRegionReq;
    use gdma_defs::GdmaCreateDmaRegionResp;
    use gdma_defs::GdmaDevId;
    use gdma_defs::GdmaDevType;
    use gdma_defs::GdmaMsgHdr;
    use gdma_defs::GdmaQueueType;
    use gdma_defs::GdmaReqHdr;
    use gdma_defs::GdmaRequestType;
    use gdma_defs::GdmaRespHdr;
    use gdma_defs::HWC_DEV_ID;
    use gdma_defs::HwcInitTypeData;
    use gdma_defs::HwcTxOob;
    use gdma_defs::bnic::ManaCfgRxSteerReq;
    use gdma_defs::bnic::ManaCommandCode;
    use gdma_defs::bnic::ManaCreateWqobjReq;
    use gdma_defs::bnic::Tristate;
    use net_backend::Queue;
    use net_backend::QueueConfig;
    use net_backend::RssConfig;
    use net_backend::null::NullEndpoint;
    use pal_async::DefaultDriver;
    use pal_async::async_test;
    use std::future::poll_fn;
    use std::pin::pin;
    use std::sync::atomic::AtomicUsize;
    use std::sync::atomic::Ordering;
    use std::task::Poll;
    use test_with_tracing::test;
    use vmcore::vm_task::SingleDriverBackend;
    use zerocopy::FromBytes;
    use zerocopy::Immutable;
    use zerocopy::KnownLayout;

    const BNIC_DEV_ID: GdmaDevId = GdmaDevId {
        ty: GdmaDevType::GDMA_DEVICE_MANA,
        instance: 1,
    };
    const REQUESTS: u64 = 0x5000;
    const RESPONSES: u64 = 0x6000;
    const SLOT: u64 = 0x100;

    fn device(driver: DefaultDriver, endpoint: Box<dyn Endpoint>) -> GdmaDevice {
        let driver_source = VmTaskDriverSource::new(SingleDriverBackend::new(driver));
        GdmaDevice::new(
            &driver_source,
            GuestMemory::allocate(0x10000),
            &MsiTarget::disconnected(),
            vec![VportConfig {
                mac_address: [0, 1, 2, 3, 4, 5].into(),
                endpoint,
            }],
            &mut ExternallyManagedMmioIntercepts,
        )
    }

    /// Asks the device to establish the HWC and returns the SMC status.
    fn establish_hwc(device: &mut GdmaDevice) -> u8 {
        let hdr = SmcProtoHdr::new()
            .with_msg_type(SmcMessageType::SMC_MSG_TYPE_ESTABLISH_HWC.0)
            .with_msg_version(SMC_MSG_TYPE_ESTABLISH_HWC_VERSION);
        device.write_shmem(SHMEM_LEN - 4, u32::from(hdr).as_bytes());
        SmcProtoHdr::from(device.shmem.0[SHMEM_LEN / 4 - 1]).status()
    }

    async fn yield_once() {
        let mut yielded = false;
        poll_fn(|cx| {
            if yielded {
                Poll::Ready(())
            } else {
                yielded = true;
                cx.waker().wake_by_ref();
                Poll::Pending
            }
        })
        .await
    }

    async fn until(mut done: impl FnMut() -> bool) {
        for _ in 0..1000 {
            if done() {
                return;
            }
            yield_once().await;
        }
        panic!("timed out");
    }

    /// Talks to the device through the HWC, as the guest driver does.
    struct Guest {
        queues: Arc<Queues>,
        sq_id: u32,
        rq_id: u32,
        cq_id: u32,
        sq_tail: u32,
        rq_tail: u32,
        requests: u32,
    }

    impl Guest {
        /// Establishes the HWC and registers the bnic.
        async fn new(device: &mut GdmaDevice) -> Self {
            let queues = device.queues.clone();
            // Responses from a previous boot must not satisfy this one's waits.
            queues.gm.write_at(RESPONSES, &[0; 0x1000]).unwrap();
            let mut shmem = [0u8; SHMEM_LEN - 4];
            for (offset, gpa) in [(0, EQ), (6, CQ), (12, RQ), (18, SQ)] {
                shmem[offset..offset + 6].copy_from_slice(&(gpa / PAGE_SIZE64).to_ne_bytes()[..6]);
            }
            device.write_shmem(0, &shmem);
            assert_eq!(establish_hwc(device), 0);

            let mut ids = [0; 3];
            for i in 0..16 {
                let eqe: gdma_defs::Eqe = queues.gm.read_plain(EQ + i * 16).unwrap();
                if eqe.params.event_type() == gdma_defs::GDMA_EQE_HWC_INIT_DATA {
                    let data = HwcInitTypeData::read_from_prefix(&eqe.data).unwrap().0;
                    match data.ty() {
                        gdma_defs::HWC_INIT_DATA_SQID => ids[0] = data.value(),
                        gdma_defs::HWC_INIT_DATA_RQID => ids[1] = data.value(),
                        gdma_defs::HWC_INIT_DATA_CQID => ids[2] = data.value(),
                        _ => {}
                    }
                }
            }
            let [sq_id, rq_id, cq_id] = ids;
            let mut guest = Self {
                queues,
                sq_id,
                rq_id,
                cq_id,
                sq_tail: 0,
                rq_tail: 0,
                requests: 0,
            };
            let register = guest.post(BNIC_DEV_ID, GdmaRequestType::GDMA_REGISTER_DEVICE.0, &[]);
            guest.wait(register).await;
            guest
        }

        /// Posts a request and returns its id.
        fn post(&mut self, dev_id: GdmaDevId, msg_type: u32, body: &[u8]) -> u32 {
            self.requests += 1;
            let id = self.requests;
            let request = REQUESTS + id as u64 * SLOT;
            let hdr = GdmaReqHdr {
                req: GdmaMsgHdr {
                    msg_type,
                    msg_size: (size_of::<GdmaReqHdr>() + body.len()) as u32,
                    ..FromZeros::new_zeroed()
                },
                resp: GdmaMsgHdr {
                    msg_size: SLOT as u32,
                    ..FromZeros::new_zeroed()
                },
                dev_id,
                activity_id: id,
            };
            let gm = &self.queues.gm;
            gm.write_plain(request, &hdr).unwrap();
            gm.write_at(request + size_of_val(&hdr) as u64, body)
                .unwrap();
            let oob = HwcTxOob {
                flags3: gdma_defs::HwcTxOobFlags3::new().with_vscq_id(self.cq_id),
                flags4: gdma_defs::HwcTxOobFlags4::new().with_vsq_id(self.sq_id),
                ..FromZeros::new_zeroed()
            };
            self.rq_tail += write_wqe(
                &self.queues,
                RQ + self.rq_tail as u64,
                &[],
                RESPONSES + id as u64 * SLOT,
                SLOT as u32,
            );
            self.sq_tail += write_wqe(
                &self.queues,
                SQ + self.sq_tail as u64,
                oob.as_bytes(),
                request,
                hdr.req.msg_size,
            );
            self.queues.doorbell_rq(
                WqDoorbellValue::new()
                    .with_id(self.rq_id)
                    .with_tail(self.rq_tail),
            );
            self.queues.doorbell_sq(
                WqDoorbellValue::new()
                    .with_id(self.sq_id)
                    .with_tail(self.sq_tail),
            );
            id
        }

        /// Returns the response to request `id`, if the device has written it.
        fn response(&self, id: u32) -> Option<GdmaRespHdr> {
            let resp: GdmaRespHdr = self
                .queues
                .gm
                .read_plain(RESPONSES + id as u64 * SLOT)
                .unwrap();
            (resp.activity_id == id).then_some(resp)
        }

        /// Waits for the response to request `id` and checks it succeeded.
        async fn wait(&self, id: u32) {
            until(|| self.response(id).is_some()).await;
            assert_eq!(self.response(id).unwrap().status, 0);
        }

        /// Sends a request and returns its response payload.
        async fn request<T: FromBytes + Immutable + KnownLayout>(
            &mut self,
            dev_id: GdmaDevId,
            msg_type: u32,
            body: &[u8],
        ) -> T {
            let id = self.post(dev_id, msg_type, body);
            self.wait(id).await;
            self.queues
                .gm
                .read_plain(RESPONSES + id as u64 * SLOT + size_of::<GdmaRespHdr>() as u64)
                .unwrap()
        }

        async fn dma_region(&mut self, gpa: u64) -> u64 {
            let req = GdmaCreateDmaRegionReq {
                length: PAGE_SIZE64,
                offset_in_page: 0,
                gdma_page_type: 0,
                page_count: 1,
                page_addr_list_len: 1,
            };
            let mut body = req.as_bytes().to_vec();
            body.extend_from_slice(gpa.as_bytes());
            let resp: GdmaCreateDmaRegionResp = self
                .request(HWC_DEV_ID, GdmaRequestType::GDMA_CREATE_DMA_REGION.0, &body)
                .await;
            resp.gdma_region
        }

        /// Creates the vport's send and receive queues.
        async fn create_queues(&mut self) {
            for (wq_type, gpa) in [
                (GdmaQueueType::GDMA_SQ, 0x8000),
                (GdmaQueueType::GDMA_RQ, 0xa000),
            ] {
                let req = ManaCreateWqobjReq {
                    vport: 0,
                    wq_type,
                    reserved: 0,
                    wq_gdma_region: self.dma_region(gpa).await,
                    cq_gdma_region: self.dma_region(gpa + PAGE_SIZE64).await,
                    wq_size: PAGE_SIZE64 as u32,
                    cq_size: PAGE_SIZE64 as u32,
                    cq_moderation_ctx_id: 0,
                    cq_parent_qid: 0,
                };
                self.request::<[u8; 16]>(
                    BNIC_DEV_ID,
                    ManaCommandCode::MANA_CREATE_WQ_OBJ.0,
                    req.as_bytes(),
                )
                .await;
            }
        }

        /// Posts a request to enable or disable the vport's receive path.
        fn post_rx(&mut self, rx_enable: Tristate) -> u32 {
            let req = ManaCfgRxSteerReq {
                vport: 0,
                rx_enable,
                ..FromZeros::new_zeroed()
            };
            self.post(
                BNIC_DEV_ID,
                ManaCommandCode::MANA_CONFIG_VPORT_RX.0,
                req.as_bytes(),
            )
        }
    }

    /// Counts how the calls to one endpoint method ended.
    #[derive(Default)]
    struct Calls {
        started: AtomicUsize,
        finished: AtomicUsize,
        dropped: AtomicUsize,
    }

    impl Calls {
        fn get(&self) -> [usize; 3] {
            [&self.started, &self.finished, &self.dropped].map(|n| n.load(Ordering::SeqCst))
        }
    }

    /// Counts a call as dropped unless it finished.
    struct Call<'a> {
        calls: &'a Calls,
        finished: bool,
    }

    impl<'a> Call<'a> {
        fn new(calls: &'a Calls) -> Self {
            calls.started.fetch_add(1, Ordering::SeqCst);
            Self {
                calls,
                finished: false,
            }
        }
    }

    impl Drop for Call<'_> {
        fn drop(&mut self) {
            let n = if self.finished {
                &self.calls.finished
            } else {
                &self.calls.dropped
            };
            n.fetch_add(1, Ordering::SeqCst);
        }
    }

    /// A null endpoint that holds its first `get_queues` or `stop` until the
    /// test releases it.
    struct HeldEndpoint {
        inner: NullEndpoint,
        hold_get_queues: Option<mesh::OneshotReceiver<()>>,
        hold_stop: Option<mesh::OneshotReceiver<()>>,
        get_queues: Arc<Calls>,
        stop: Arc<Calls>,
    }

    impl HeldEndpoint {
        fn new(
            hold_get_queues: Option<mesh::OneshotReceiver<()>>,
            hold_stop: Option<mesh::OneshotReceiver<()>>,
        ) -> Self {
            Self {
                inner: NullEndpoint::new(),
                hold_get_queues,
                hold_stop,
                get_queues: Default::default(),
                stop: Default::default(),
            }
        }
    }

    impl InspectMut for HeldEndpoint {
        fn inspect_mut(&mut self, req: inspect::Request<'_>) {
            req.ignore();
        }
    }

    #[async_trait::async_trait]
    impl Endpoint for HeldEndpoint {
        fn endpoint_type(&self) -> &'static str {
            "held"
        }

        async fn get_queues(
            &mut self,
            config: Vec<QueueConfig>,
            rss: Option<&RssConfig<'_>>,
            queues: &mut Vec<Box<dyn Queue>>,
        ) -> anyhow::Result<()> {
            let calls = self.get_queues.clone();
            let mut call = Call::new(&calls);
            if let Some(hold) = self.hold_get_queues.take() {
                hold.await.unwrap();
            }
            self.inner.get_queues(config, rss, queues).await?;
            call.finished = true;
            Ok(())
        }

        async fn stop(&mut self) {
            let calls = self.stop.clone();
            let mut call = Call::new(&calls);
            if let Some(hold) = self.hold_stop.take() {
                hold.await.unwrap();
            }
            self.inner.stop().await;
            call.finished = true;
        }

        fn is_ordered(&self) -> bool {
            true
        }
    }

    /// Asserts that `stop` stays pending while the device runs.
    async fn assert_pending(mut stop: std::pin::Pin<&mut impl Future>) {
        for _ in 0..10 {
            assert!(futures::poll!(stop.as_mut()).is_pending());
            yield_once().await;
        }
    }

    #[async_test]
    async fn hwc_stop_waits_for_get_queues(driver: DefaultDriver) {
        let (release, hold) = mesh::oneshot();
        let endpoint = HeldEndpoint::new(Some(hold), None);
        let (get_queues, stop) = (endpoint.get_queues.clone(), endpoint.stop.clone());
        let mut device = device(driver, Box::new(endpoint));
        let mut guest = Guest::new(&mut device).await;
        guest.create_queues().await;
        let enable = guest.post_rx(Tristate::TRUE);
        until(|| get_queues.get()[0] == 1).await;

        {
            let mut hwc_stop = pin!(device.hwc.stop());
            assert_pending(hwc_stop.as_mut()).await;
            release.send(());
            hwc_stop.await;
        }
        assert_eq!(guest.response(enable).unwrap().status, 0);
        assert_eq!(get_queues.get(), [1, 1, 0]);

        // The data path is up: disabling it stops the endpoint.
        device.hwc.start();
        let disable = guest.post_rx(Tristate::FALSE);
        guest.wait(disable).await;
        assert_eq!(stop.get(), [1, 1, 0]);
    }

    #[async_test]
    async fn hwc_stop_waits_for_endpoint_stop(driver: DefaultDriver) {
        let (release, hold) = mesh::oneshot();
        let endpoint = HeldEndpoint::new(None, Some(hold));
        let (get_queues, stop) = (endpoint.get_queues.clone(), endpoint.stop.clone());
        let mut device = device(driver, Box::new(endpoint));
        let mut guest = Guest::new(&mut device).await;
        guest.create_queues().await;
        let enable = guest.post_rx(Tristate::TRUE);
        guest.wait(enable).await;
        let disable = guest.post_rx(Tristate::FALSE);
        until(|| stop.get()[0] == 1).await;

        {
            let mut hwc_stop = pin!(device.hwc.stop());
            assert_pending(hwc_stop.as_mut()).await;
            release.send(());
            hwc_stop.await;
        }
        assert_eq!(guest.response(disable).unwrap().status, 0);
        assert_eq!(stop.get(), [1, 1, 0]);

        // The endpoint can hand out queues again.
        device.hwc.start();
        let enable = guest.post_rx(Tristate::TRUE);
        guest.wait(enable).await;
        assert_eq!(get_queues.get(), [2, 2, 0]);
    }

    /// More boots than the device has queues of any kind.
    #[async_test]
    async fn reset_lets_each_boot_establish_the_hwc(driver: DefaultDriver) {
        let mut device = device(driver, Box::new(NullEndpoint::new()));
        for boot in 0..129 {
            assert_eq!(establish_hwc(&mut device), 0, "boot {boot}");
            device.reset().await;
        }
    }

    #[async_test]
    async fn reset_clears_registers_and_config_space(driver: DefaultDriver) {
        let mut device = device(driver, Box::new(NullEndpoint::new()));
        assert_eq!(establish_hwc(&mut device), 0);
        device.destroying_hwc = true;
        let command = pci_core::spec::cfg_space::HeaderType00::STATUS_COMMAND.0;
        device
            .pci_cfg_write(command, ByteEnabledDwordWrite::with_all_bytes_enabled(2))
            .unwrap();

        device.reset().await;

        assert_eq!(device.shmem.0, [0; SHMEM_LEN / 4]);
        assert!(!device.destroying_hwc);
        let mut value = 0;
        device
            .pci_cfg_read(
                command,
                ByteEnabledDwordRead::with_all_bytes_enabled(&mut value),
            )
            .unwrap();
        assert_eq!(value & 2, 0);
    }

    #[async_test]
    async fn reset_waits_for_get_queues_and_stops_the_endpoint(driver: DefaultDriver) {
        let (release, hold) = mesh::oneshot();
        let endpoint = HeldEndpoint::new(Some(hold), None);
        let (get_queues, stop) = (endpoint.get_queues.clone(), endpoint.stop.clone());
        let mut device = device(driver, Box::new(endpoint));
        let mut guest = Guest::new(&mut device).await;
        guest.create_queues().await;
        guest.post_rx(Tristate::TRUE);
        until(|| get_queues.get()[0] == 1).await;

        {
            let mut reset = pin!(device.reset());
            assert_pending(reset.as_mut()).await;
            release.send(());
            reset.await;
        }
        assert_eq!(get_queues.get(), [1, 1, 0]);
        assert_eq!(stop.get(), [1, 1, 0]);

        // The next boot brings the vport up from scratch.
        let mut guest = Guest::new(&mut device).await;
        guest.create_queues().await;
        let enable = guest.post_rx(Tristate::TRUE);
        guest.wait(enable).await;
        assert_eq!(get_queues.get(), [2, 2, 0]);
    }
}
