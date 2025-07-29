use crate::BAR0_LEN;
use crate::DOORBELL_STRIDE_BITS;
use crate::NvmeController;
use crate::NvmeControllerCaps;
use crate::NvmeControllerClient;
use crate::PAGE_MASK;
use crate::VENDOR_ID;
use crate::fault_injection::admin::AdminConfigFaultInjection;
use crate::fault_injection::admin::AdminHandlerFaultInjection;
use crate::fault_injection::admin::AdminStateFaultInjection;
use crate::queue::DoorbellRegister;
use crate::spec;
use chipset_device::ChipsetDevice;
use chipset_device::io::IoError;
use chipset_device::io::IoError::InvalidRegister;
use chipset_device::io::IoResult;
use chipset_device::mmio::MmioIntercept;
use chipset_device::mmio::RegisterMmioIntercept;
use chipset_device::pci::PciConfigSpace;
use guestmem::GuestMemory;
use inspect::Inspect;
use inspect::InspectMut;
use parking_lot::Mutex;
use pci_core::capabilities::msix::MsixEmulator;
use pci_core::cfg_space_emu::BarMemoryKind;
use pci_core::cfg_space_emu::ConfigSpaceType0Emulator;
use pci_core::cfg_space_emu::DeviceBars;
use pci_core::msi::MsiInterruptSet;
use pci_core::msi::RegisterMsi;
use pci_core::spec::hwid::ClassCode;
use pci_core::spec::hwid::HardwareIds;
use pci_core::spec::hwid::ProgrammingInterface;
use pci_core::spec::hwid::Subclass;
use std::collections::HashSet;
use std::sync::Arc;
use task_control::TaskControl;
use vmcore::device_state::ChangeDeviceState;
use vmcore::save_restore::SaveError;
use vmcore::save_restore::SaveRestore;
use vmcore::save_restore::SavedStateNotSupported;
use vmcore::vm_task::VmTaskDriver;
use vmcore::vm_task::VmTaskDriverSource;

/// Fault injection for the NVMe controller. Allows for intercepting and changing admin queue commands
#[derive(InspectMut)]
pub struct NvmeControllerFaultInjection {
    #[inspect(skip)]
    inner: Arc<Mutex<NvmeController>>,
    mem: GuestMemory,
    driver: VmTaskDriver,
    #[inspect(skip)]
    doorbells: Vec<Arc<DoorbellRegister>>,
    #[inspect(iter_by_index)]
    doorbells_intercept: HashSet<u16>,
    regs: Regs,
    #[inspect(skip)]
    admin: TaskControl<AdminHandlerFaultInjection, AdminStateFaultInjection>,
    cfg_space: ConfigSpaceType0Emulator,
}

#[derive(Inspect)]
struct Regs {
    asq: u64,
    acq: u64,
    aqa: spec::Aqa,
    cc: spec::Cc,
}

impl NvmeControllerFaultInjection {
    /// Creates a new NVMe controller with fault injection.
    pub fn new(
        driver_source: &VmTaskDriverSource,
        guest_memory: GuestMemory,
        register_msi: &mut dyn RegisterMsi,
        register_mmio: &mut dyn RegisterMmioIntercept,
        caps: NvmeControllerCaps,
    ) -> Self {
        // Setup Doorbell intercept.
        let num_qids = 2 + caps.max_io_queues * 2; // Assumes that max_sqs == max_cqs
        let doorbells: Vec<_> = (0..num_qids)
            .map(|_| Arc::new(DoorbellRegister::new()))
            .collect();

        // We want to be able to share the inner controller with the admin handler.
        let inner = Arc::new(Mutex::new(NvmeController::new(
            driver_source,
            guest_memory.clone(), // Communication with the inner controller will always be through the inner memory component.
            register_msi,
            register_mmio,
            caps,
        )));

        let qe_sizes = Arc::new(Default::default());
        let handler: AdminHandlerFaultInjection = AdminHandlerFaultInjection::new(
            driver_source.simple(),
            AdminConfigFaultInjection {
                mem: guest_memory.clone(),
                doorbells: doorbells.clone(),
                subsystem_id: caps.subsystem_id,
                max_sqs: caps.max_io_queues,
                max_cqs: caps.max_io_queues,
                qe_sizes: Arc::clone(&qe_sizes),
                controller: inner.clone(),
                sq_doorbell_addr: 0x1000, // The address of the submission queue doorbell in the device's BAR0.
            },
        );

        // Don't register the interrupt vectors multiple times. Fixes issues calculating max queues.
        let register_msi_dummy: &mut dyn RegisterMsi = &mut MsiInterruptSet::new();
        let (msix, msix_cap) = MsixEmulator::new(4, caps.msix_count, register_msi_dummy);

        // TODO: Do we need to set up the bars here?
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

        Self {
            inner: inner.clone(),
            mem: guest_memory.clone(),
            driver: driver_source.simple(),
            doorbells,
            regs: Regs {
                asq: 0,
                acq: 0,
                aqa: spec::Aqa::default(),
                cc: spec::Cc::default(),
            },
            admin: TaskControl::new(handler),
            cfg_space,
            doorbells_intercept: HashSet::new(),
        }
    }

    /// Passthrough
    pub fn client(&self) -> NvmeControllerClient {
        let inner = self.inner.lock();
        inner.client()
    }

    /// Passthrough
    pub fn read_bar0(&mut self, addr: u16, data: &mut [u8]) -> IoResult {
        // Normal Behaviour.
        let mut inner = self.inner.lock();
        inner.read_bar0(addr, data)
    }

    // Tries to write to the admin submission queue doorbell register. If write is successful, return the IoResult.
    fn try_intercept_doorbell(&mut self, addr: u16, data: &[u8]) -> Result<IoResult, ()> {
        if addr >= 0x1000 {
            // Doorbell write.
            let base = addr - 0x1000;
            let index = base >> DOORBELL_STRIDE_BITS;
            if (index << DOORBELL_STRIDE_BITS) != base || index != 0 {
                return Err(());
            }
            if !self.doorbells_intercept.contains(&index) {
                return Err(());
            }
            let Ok(data) = data.try_into() else {
                return Err(());
            };
            let data = u32::from_ne_bytes(data);
            if let Some(doorbell) = self.doorbells.get(index as usize) {
                tracelimit::warn_ratelimited!(index, data, "Intercepted doorbell write");
                doorbell.write(data);
                return Ok(IoResult::Ok);
            } else {
                tracelimit::warn_ratelimited!(index, data, "unknown doorbell");
            }
        }
        Err(())
    }

    /// Writes to the virtual BAR 0. Does NOT handle doorbell writes. Those should be handled through mmio_write.
    /// This does NOT handle doorbell writes, use mmio_write instead.
    /// this does NOT handle write_bar0() to inner, use mmio_write instead.
    pub fn write_bar0(&mut self, addr: u16, data: &[u8]) -> IoResult {
        // Doorbell writes should be handled through mmio_write.
        if addr >= 0x1000 {
            return IoResult::Err(InvalidRegister);
        }

        // Duplicate admin queue setup!
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

        match spec::Register(addr & !7) {
            spec::Register::ASQ => {
                if !self.regs.cc.en() {
                    self.regs.asq = update_reg(self.regs.asq) & PAGE_MASK;
                } else {
                    tracelimit::warn_ratelimited!("attempt to set asq while enabled");
                }
            }
            spec::Register::ACQ => {
                if !self.regs.cc.en() {
                    self.regs.acq = update_reg(self.regs.acq) & PAGE_MASK;
                } else {
                    tracelimit::warn_ratelimited!("attempt to set acq while enabled");
                }
            }
            _ => {}
        };

        // Admin Queue setup flow
        let Ok(data) = data.try_into() else {
            return IoResult::Err(IoError::InvalidAccessSize);
        };

        let data = u32::from_ne_bytes(data);
        match spec::Register(addr) {
            spec::Register::CC => self.set_cc(data.into()),
            spec::Register::AQA => self.regs.aqa = data.into(),
            _ => {}
        }

        // Don't passthrough, that will be handled by mmio_write.
        IoResult::Ok
    }

    /// Passthrough
    pub fn fatal_error(&mut self) {
        let mut inner = self.inner.lock();
        inner.fatal_error();
    }

    fn set_cc(&mut self, cc: spec::Cc) {
        let mask: u32 = u32::from(
            spec::Cc::new()
                .with_en(true)
                .with_shn(0b11)
                .with_iosqes(0b1111)
                .with_iocqes(0b1111),
        );
        let cc: spec::Cc = (u32::from(cc) & mask).into();

        // Admin queue has not yet been started, set it up here.
        if !self.admin.is_running() {
            let state = AdminStateFaultInjection::new(
                self.admin.task(),
                self.regs.asq,
                self.regs.aqa.asqs_z().max(1) + 1,
            );
            self.doorbells_intercept
                .insert(state.get_intercept_doorbell());
            self.admin
                .insert(&self.driver, "nvme-admin-fault-injection", state);
            self.admin.start();
        }

        self.regs.cc = cc;
    }
}

impl ChangeDeviceState for NvmeControllerFaultInjection {
    fn start(&mut self) {
        let mut inner = self.inner.lock();
        inner.start();
    }

    async fn stop(&mut self) {
        // NOTE: Left intentionally empty because the inner fn is also empty
    }

    async fn reset(&mut self) {
        // NOTE: Left intentionally empty because the inner fn is also empty
    }
}

impl ChipsetDevice for NvmeControllerFaultInjection {
    fn supports_mmio(&mut self) -> Option<&mut dyn MmioIntercept> {
        Some(self)
    }

    fn supports_pci(&mut self) -> Option<&mut dyn PciConfigSpace> {
        Some(self)
    }
}

impl MmioIntercept for NvmeControllerFaultInjection {
    fn mmio_read(&mut self, addr: u64, data: &mut [u8]) -> IoResult {
        let mut inner = self.inner.lock();
        inner.mmio_read(addr, data)
    }

    fn mmio_write(&mut self, addr: u64, data: &[u8]) -> IoResult {
        if let Some((0, offset)) = self.cfg_space.find_bar(addr) {
            // Don't passthrough admin sq doorbell writes.
            let admin_doorbell_intercept = self.try_intercept_doorbell(offset, data);
            if let Ok(intercept_result) = admin_doorbell_intercept {
                return intercept_result;
            }

            // Not an admin doorbell write, duplicate admin queue setup and write to the inner controller.
            let _ = self.write_bar0(offset, data);
        }

        let mut inner = self.inner.lock();
        inner.mmio_write(addr, data)
    }
}

impl PciConfigSpace for NvmeControllerFaultInjection {
    // DONE: Always read from inner.
    fn pci_cfg_read(&mut self, offset: u16, value: &mut u32) -> IoResult {
        let mut inner = self.inner.lock();
        inner.pci_cfg_read(offset, value)
    }

    // DONE: Copy any writes going to inner.
    fn pci_cfg_write(&mut self, offset: u16, value: u32) -> IoResult {
        let _ = self.cfg_space.write_u32(offset, value);
        let mut inner = self.inner.lock();
        inner.pci_cfg_write(offset, value)
    }
}

impl SaveRestore for NvmeControllerFaultInjection {
    type SavedState = SavedStateNotSupported;

    fn save(&mut self) -> Result<Self::SavedState, SaveError> {
        let mut inner = self.inner.lock();
        inner.save()
    }

    fn restore(
        &mut self,
        state: Self::SavedState,
    ) -> Result<(), vmcore::save_restore::RestoreError> {
        let mut inner = self.inner.lock();
        inner.restore(state)
    }
}
