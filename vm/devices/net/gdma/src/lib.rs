// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

#![forbid(unsafe_code)]
#![expect(missing_docs)]

mod bnic;
mod dma;
mod hwc;
mod queues;
pub mod resolver;

use chipset_device::ChipsetDevice;
use chipset_device::io::IoResult;
use chipset_device::mmio::MmioIntercept;
use chipset_device::mmio::RegisterMmioIntercept;
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
        let (msix, msix_capability) = MsixEmulator::new(4, 64, msi_target);

        let hardware_ids = HardwareIds {
            vendor_id: gdma_defs::VENDOR_ID,
            device_id: gdma_defs::DEVICE_ID,
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
                bnic: bnic::BasicNic::new(vports),
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
        todo!()
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
    type SavedState = SavedStateNotSupported;

    fn save(&mut self) -> Result<Self::SavedState, SaveError> {
        todo!()
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
    fn pci_cfg_read(&mut self, offset: u16, value: &mut u32) -> IoResult {
        self.config.read_u32(offset, value)
    }

    fn pci_cfg_write(&mut self, offset: u16, value: u32) -> IoResult {
        self.config.write_u32(offset, value)
    }
}
