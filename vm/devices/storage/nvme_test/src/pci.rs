// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! The NVMe (Fault Injection) PCI device implementation.

use crate::BAR0_LEN;
use crate::DOORBELL_STRIDE_BITS;
use crate::IOCQES;
use crate::IOSQES;
use crate::MAX_QES;
use crate::NVME_VERSION;
use crate::NvmeFaultControllerClient;
use crate::PAGE_MASK;
use crate::VENDOR_ID;
use crate::spec;
use crate::workers::IoQueueEntrySizes;
use crate::workers::NvmeWorkers;
use chipset_device::ChipsetDevice;
use chipset_device::io::IoError;
use chipset_device::io::IoError::InvalidRegister;
use chipset_device::io::IoResult;
use chipset_device::mmio::MmioIntercept;
use chipset_device::mmio::RegisterMmioIntercept;
use chipset_device::pci::PciConfigSpace;
use device_emulators::ReadWriteRequestType;
use device_emulators::read_as_u32_chunks;
use device_emulators::write_as_u32_chunks;
use guestmem::GuestMemory;
use guid::Guid;
use inspect::Inspect;
use inspect::InspectMut;
use nvme_resources::fault::FaultConfiguration;
use nvme_resources::fault::PciFaultBehavior;
use nvme_resources::fault::PciFaultConfig;
use parking_lot::Mutex;
use pci_core::capabilities::msix::MsixEmulator;
use pci_core::cfg_space_emu::BarMemoryKind;
use pci_core::cfg_space_emu::ConfigSpaceType0Emulator;
use pci_core::cfg_space_emu::DeviceBars;
use pci_core::msi::MsiTarget;
use pci_core::spec::hwid::ClassCode;
use pci_core::spec::hwid::HardwareIds;
use pci_core::spec::hwid::ProgrammingInterface;
use pci_core::spec::hwid::Subclass;
use std::sync::Arc;
use tdisp::TdispHostDeviceTarget;
use vmcore::device_state::ChangeDeviceState;
use vmcore::save_restore::SaveError;
use vmcore::save_restore::SaveRestore;
use vmcore::save_restore::SavedStateNotSupported;
use vmcore::vm_task::VmTaskDriverSource;

/// An NVMe controller.
#[derive(InspectMut)]
pub struct NvmeFaultController {
    cfg_space: ConfigSpaceType0Emulator,
    #[inspect(skip)]
    msix: MsixEmulator,
    registers: RegState,
    #[inspect(skip)]
    qe_sizes: Arc<Mutex<IoQueueEntrySizes>>,
    #[inspect(flatten, mut)]
    workers: NvmeWorkers,
    #[inspect(skip)]
    pci_fault_config: PciFaultConfig,
    #[inspect(skip)]
    fault_active: mesh::Cell<bool>,
    /// The NVMe fault controller is repurposed for use in TDISP tests.
    #[inspect(skip)]
    tdisp_interface: Option<Box<dyn TdispHostDeviceTarget>>,
}

#[derive(Inspect)]
struct RegState {
    #[inspect(hex)]
    interrupt_mask: u32,
    cc: spec::Cc,
    csts: spec::Csts,
    aqa: spec::Aqa,
    #[inspect(hex)]
    asq: u64,
    #[inspect(hex)]
    acq: u64,
}

impl RegState {
    fn new() -> Self {
        Self {
            interrupt_mask: 0,
            cc: spec::Cc::new(),
            csts: spec::Csts::new(),
            aqa: spec::Aqa::new(),
            asq: 0,
            acq: 0,
        }
    }
}

const CAP: spec::Cap = spec::Cap::new()
    .with_dstrd(DOORBELL_STRIDE_BITS - 2)
    .with_mqes_z(MAX_QES - 1)
    .with_cqr(true)
    .with_css_nvm(true)
    .with_to(!0);

/// The NVMe controller's capabilities.
#[derive(Debug, Copy, Clone)]
pub struct NvmeFaultControllerCaps {
    /// The number of entries in the MSI-X table.
    pub msix_count: u16,
    /// The maximum number of IO submission and completion queues.
    pub max_io_queues: u16,
    /// The subsystem ID, used as part of the subnqn field of the identify
    /// controller response.
    pub subsystem_id: Guid,
}

impl NvmeFaultController {
    /// Creates a new NVMe controller.
    pub fn new(
        driver_source: &VmTaskDriverSource,
        guest_memory: GuestMemory,
        msi_target: &MsiTarget,
        register_mmio: &mut dyn RegisterMmioIntercept,
        caps: NvmeFaultControllerCaps,
        mut fault_configuration: FaultConfiguration,
        tdisp_interface: Option<Box<dyn TdispHostDeviceTarget>>,
    ) -> Self {
        let (msix, msix_cap) = MsixEmulator::new(4, caps.msix_count, msi_target);
        let bars = DeviceBars::new()
            .bar0(
                BAR0_LEN,
                BarMemoryKind::Intercept(register_mmio.new_io_region("bar0", BAR0_LEN)),
            )
            .bar4(
                msix.bar_len(),
                BarMemoryKind::Intercept(register_mmio.new_io_region("msix", msix.bar_len())),
            );

        let cfg_space = ConfigSpaceType0Emulator::new(
            HardwareIds {
                vendor_id: VENDOR_ID,
                device_id: 0x00a9,
                revision_id: 0,
                prog_if: ProgrammingInterface::MASS_STORAGE_CONTROLLER_NON_VOLATILE_MEMORY_NVME,
                sub_class: Subclass::MASS_STORAGE_CONTROLLER_NON_VOLATILE_MEMORY,
                base_class: ClassCode::MASS_STORAGE_CONTROLLER,
                type0_sub_vendor_id: 0,
                type0_sub_system_id: 0,
            },
            vec![Box::new(msix_cap)],
            bars,
        );

        let interrupts = (0..caps.msix_count)
            .map(|i| msix.interrupt(i).unwrap())
            .collect();

        let pci_fault_config = fault_configuration
            .pci_fault
            .take()
            .unwrap_or(PciFaultConfig::new());

        let fault_active = fault_configuration.fault_active.clone();

        let qe_sizes = Arc::new(Default::default());
        let admin = NvmeWorkers::new(
            driver_source,
            guest_memory,
            interrupts,
            caps.max_io_queues,
            caps.max_io_queues,
            Arc::clone(&qe_sizes),
            caps.subsystem_id,
            fault_configuration,
        );

        Self {
            cfg_space,
            msix,
            registers: RegState::new(),
            workers: admin,
            qe_sizes,
            pci_fault_config,
            fault_active,
            tdisp_interface,
        }
    }

    /// Returns a client for manipulating the NVMe controller at runtime.
    pub fn client(&self) -> NvmeFaultControllerClient {
        self.workers.client()
    }

    /// Reads from the virtual BAR 0.
    pub fn read_bar0(&mut self, addr: u16, data: &mut [u8]) -> IoResult {
        if data.len() < 4 {
            return IoResult::Err(IoError::InvalidAccessSize);
        }
        if addr & (data.len() - 1) as u16 != 0 {
            return IoResult::Err(IoError::UnalignedAccess);
        }

        // Check for 64-bit registers.
        let d: Option<u64> = match spec::Register(addr & !7) {
            spec::Register::CAP => {
                if let Some(mqes) = self.pci_fault_config.max_queue_size {
                    Some(CAP.with_mqes_z(mqes - 1).into())
                } else {
                    Some(CAP.into())
                }
            }
            spec::Register::ASQ => Some(self.registers.asq),
            spec::Register::ACQ => Some(self.registers.acq),
            spec::Register::BPMBL => Some(0),
            _ => None,
        };
        if let Some(d) = d {
            if data.len() == 8 {
                data.copy_from_slice(&d.to_ne_bytes());
            } else if addr & 7 == 0 {
                data.copy_from_slice(&(d as u32).to_ne_bytes());
            } else {
                data.copy_from_slice(&((d >> 32) as u32).to_ne_bytes());
            }
            return IoResult::Ok;
        }

        if data.len() != 4 {
            return IoResult::Err(IoError::InvalidAccessSize);
        }

        // Handle 32-bit registers.
        let d: u32 = match spec::Register(addr) {
            spec::Register::VS => NVME_VERSION,
            spec::Register::INTMS => self.registers.interrupt_mask,
            spec::Register::INTMC => self.registers.interrupt_mask,
            spec::Register::CC => self.registers.cc.into(),
            spec::Register::RESERVED => 0,
            spec::Register::CSTS => self.get_csts(),
            spec::Register::NSSR => 0,
            spec::Register::AQA => self.registers.aqa.into(),
            spec::Register::CMBLOC => 0,
            spec::Register::CMBSZ => 0,
            spec::Register::BPINFO => 0,
            spec::Register::BPRSEL => 0,
            _ => return IoResult::Err(InvalidRegister),
        };
        data.copy_from_slice(&d.to_ne_bytes());
        IoResult::Ok
    }

    /// Writes to the virtual BAR 0.
    pub fn write_bar0(&mut self, addr: u16, data: &[u8]) -> IoResult {
        if addr >= 0x1000 {
            // Doorbell write.
            let base = addr - 0x1000;
            let db_id = base >> DOORBELL_STRIDE_BITS;
            if (db_id << DOORBELL_STRIDE_BITS) != base {
                return IoResult::Err(InvalidRegister);
            }
            let Ok(data) = data.try_into() else {
                return IoResult::Err(IoError::InvalidAccessSize);
            };
            let value = u32::from_ne_bytes(data);
            self.workers.doorbell(db_id, value);
            return IoResult::Ok;
        }

        if data.len() < 4 {
            return IoResult::Err(IoError::InvalidAccessSize);
        }
        if addr & (data.len() - 1) as u16 != 0 {
            return IoResult::Err(IoError::UnalignedAccess);
        }

        let update_reg = |x: u64| {
            if data.len() == 8 {
                u64::from_ne_bytes(data.try_into().unwrap())
            } else {
                let data = u32::from_ne_bytes(data.try_into().unwrap()) as u64;
                if addr & 7 == 0 {
                    (x & !(u32::MAX as u64)) | data
                } else {
                    (x & u32::MAX as u64) | (data << 32)
                }
            }
        };

        // Check for 64-bit registers.
        let handled = match spec::Register(addr & !7) {
            spec::Register::ASQ => {
                if !self.registers.cc.en() {
                    self.registers.asq = update_reg(self.registers.asq) & PAGE_MASK;
                } else {
                    tracelimit::warn_ratelimited!("attempt to set asq while enabled");
                }
                true
            }
            spec::Register::ACQ => {
                if !self.registers.cc.en() {
                    self.registers.acq = update_reg(self.registers.acq) & PAGE_MASK;
                } else {
                    tracelimit::warn_ratelimited!("attempt to set acq while enabled");
                }
                true
            }
            _ => false,
        };
        if handled {
            return IoResult::Ok;
        }

        let Ok(data) = data.try_into() else {
            return IoResult::Err(IoError::InvalidAccessSize);
        };
        let data = u32::from_ne_bytes(data);

        // Handle 32-bit registers.
        match spec::Register(addr) {
            spec::Register::INTMS => self.registers.interrupt_mask |= data,
            spec::Register::INTMC => self.registers.interrupt_mask &= !data,
            spec::Register::CC => self.set_cc(data.into()),
            spec::Register::AQA => self.registers.aqa = data.into(),
            _ => return IoResult::Err(InvalidRegister),
        }
        IoResult::Ok
    }

    fn set_cc(&mut self, cc: spec::Cc) {
        tracing::debug!(?cc, "set cc");

        if cc.mps() != 0 {
            tracelimit::warn_ratelimited!(
                "This implementation only supports memory page sizes of 4K."
            );
            self.fatal_error();
            return;
        }

        if cc.css() != 0 {
            tracelimit::warn_ratelimited!("This implementation only supports the NVM command set.");
            self.fatal_error();
            return;
        }

        if let 2..=6 = cc.ams() {
            tracelimit::warn_ratelimited!("Undefined arbitration mechanism.");
            self.fatal_error();
        }

        let mask: u32 = u32::from(
            spec::Cc::new()
                .with_en(true)
                .with_shn(0b11)
                .with_iosqes(0b1111)
                .with_iocqes(0b1111),
        );
        let mut cc: spec::Cc = (u32::from(cc) & mask).into();

        if cc.shn() != 0 {
            // It is unclear in the spec (to me) what guarantees a
            // controller is supposed to make after shutdown. For now, just
            // complete shutdown immediately.
            self.registers.csts.set_shst(0b10);
        }

        if cc.en() != self.registers.cc.en() {
            if cc.en() {
                // If any fault was configured for cc.en() process it here
                if self.fault_active.get() {
                    match &mut self.pci_fault_config.controller_management_fault_enable {
                        PciFaultBehavior::Delay(duration) => {
                            std::thread::sleep(*duration);
                        }
                        PciFaultBehavior::Default => {}
                        PciFaultBehavior::Verify(send) => {
                            if let Some(send) = send.take() {
                                send.send(());
                            }
                        }
                    }
                }

                // Some drivers will write zeros to IOSQES and IOCQES, assuming that the defaults will work.
                if cc.iocqes() == 0 {
                    cc.set_iocqes(IOCQES);
                } else if cc.iocqes() != IOCQES {
                    tracelimit::warn_ratelimited!(
                        "This implementation only supports CQEs of the default size."
                    );
                    self.fatal_error();
                    return;
                }

                if cc.iosqes() == 0 {
                    cc.set_iosqes(IOSQES);
                } else if cc.iosqes() != IOSQES {
                    tracelimit::warn_ratelimited!(
                        "This implementation only supports SQEs of the default size."
                    );
                    self.fatal_error();
                    return;
                }

                if self.registers.csts.rdy() {
                    tracelimit::warn_ratelimited!("enabling during reset");
                    return;
                }
                if cc.shn() == 0 {
                    self.registers.csts.set_shst(0);
                }

                self.workers.enable(
                    self.registers.asq,
                    self.registers.aqa.asqs_z().max(1) + 1,
                    self.registers.acq,
                    self.registers.aqa.acqs_z().max(1) + 1,
                );
            } else if self.registers.csts.rdy() {
                self.workers.controller_reset();
            } else {
                tracelimit::warn_ratelimited!("disabling while not ready");
                return;
            }
        }

        self.registers.cc = cc;
        *self.qe_sizes.lock() = IoQueueEntrySizes {
            sqe_bits: cc.iosqes(),
            cqe_bits: cc.iocqes(),
        };
    }

    fn get_csts(&mut self) -> u32 {
        if !self.registers.cc.en() && self.registers.csts.rdy() {
            // Keep trying to disable.
            if self.workers.poll_controller_reset() {
                // AQA, ASQ, and ACQ are not reset by controller reset.
                self.registers.csts = 0.into();
                self.registers.cc = 0.into();
                self.registers.interrupt_mask = 0;
            }
        } else if self.registers.cc.en() && !self.registers.csts.rdy() {
            if self.workers.poll_enabled() {
                self.registers.csts.set_rdy(true);
            }
        }

        let csts = self.registers.csts;
        tracing::debug!(?csts, "get csts");
        csts.into()
    }

    /// Sets the CFS bit in the controller status register (CSTS), indicating
    /// that the controller has experienced "undefined" behavior.
    pub fn fatal_error(&mut self) {
        self.registers.csts.set_cfs(true);
    }
}

impl ChangeDeviceState for NvmeFaultController {
    fn start(&mut self) {}

    async fn stop(&mut self) {}

    async fn reset(&mut self) {
        let Self {
            cfg_space,
            msix: _,
            registers,
            qe_sizes,
            workers,
            pci_fault_config: _,
            fault_active: _,
            tdisp_interface: _,
        } = self;
        workers.reset().await;
        cfg_space.reset();
        *registers = RegState::new();
        *qe_sizes.lock() = Default::default();
    }
}

impl ChipsetDevice for NvmeFaultController {
    fn supports_mmio(&mut self) -> Option<&mut dyn MmioIntercept> {
        Some(self)
    }

    fn supports_pci(&mut self) -> Option<&mut dyn PciConfigSpace> {
        Some(self)
    }

    /// The NVMe fault controller is repurposed for use in TDISP tests.
    fn supports_tdisp(&mut self) -> Option<&mut dyn TdispHostDeviceTarget> {
        tracing::debug!(
            supported = self.tdisp_interface.is_some(),
            "fault controller TDISP support in ChipsetDevice"
        );

        match &mut self.tdisp_interface {
            Some(tdisp) => Some(tdisp.as_mut()),
            None => None,
        }
    }
}

impl MmioIntercept for NvmeFaultController {
    fn mmio_read(&mut self, addr: u64, data: &mut [u8]) -> IoResult {
        match self.cfg_space.find_bar(addr) {
            Some((0, offset)) => self.read_bar0(offset, data),
            Some((4, offset)) => {
                read_as_u32_chunks(offset, data, |offset| self.msix.read_u32(offset));
                IoResult::Ok
            }
            _ => IoResult::Err(InvalidRegister),
        }
    }

    fn mmio_write(&mut self, addr: u64, data: &[u8]) -> IoResult {
        match self.cfg_space.find_bar(addr) {
            Some((0, offset)) => self.write_bar0(offset, data),
            Some((4, offset)) => {
                write_as_u32_chunks(offset, data, |offset, ty| match ty {
                    ReadWriteRequestType::Read => Some(self.msix.read_u32(offset)),
                    ReadWriteRequestType::Write(val) => {
                        self.msix.write_u32(offset, val);
                        None
                    }
                });
                IoResult::Ok
            }
            _ => IoResult::Err(InvalidRegister),
        }
    }
}

impl PciConfigSpace for NvmeFaultController {
    fn pci_cfg_read(&mut self, offset: u16, value: &mut u32) -> IoResult {
        self.cfg_space.read_u32(offset, value)
    }

    fn pci_cfg_write(&mut self, offset: u16, value: u32) -> IoResult {
        self.cfg_space.write_u32(offset, value)
    }
}

impl SaveRestore for NvmeFaultController {
    type SavedState = SavedStateNotSupported;

    fn save(&mut self) -> Result<Self::SavedState, SaveError> {
        Err(SaveError::NotSupported)
    }

    fn restore(
        &mut self,
        state: Self::SavedState,
    ) -> Result<(), vmcore::save_restore::RestoreError> {
        match state {}
    }
}
