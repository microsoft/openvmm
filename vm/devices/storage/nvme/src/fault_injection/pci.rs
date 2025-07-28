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
use crate::queue::ILLEGAL_DOORBELL_VALUE;
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
use std::any::Any;
use std::sync::Arc;
use task_control::TaskControl;
use tracing::debug;
use vmcore::device_state::ChangeDeviceState;
use vmcore::save_restore::SaveError;
use vmcore::save_restore::SaveRestore;
use vmcore::save_restore::SavedStateNotSupported;
use vmcore::vm_task::VmTaskDriver;
use vmcore::vm_task::VmTaskDriverSource;

// The function can respond with two types of actions.
#[derive(Debug, Clone)]
pub enum FaultInjectionAction {
    /// No-fault. Will always run the underlying function with the given input to the function. A direct passthrough
    No_Op,
    /// Drops the request to the underlying function but expects to be given some output for the caller.
    Drop,
    /// Underlying function is called. Given output is passed along to the caller. Output can be different from that of the function that was run.
    Fault,
    // TODO: There are many other types of faults that can be added in the long run. For eg, change output of the underlying function,
    // change input to the underlying function, call the underlying function several times, etc. This is meant to be a flexible model for invoking faults
    // Not every scenario needs to be supported when calling the underlying function for faults.
    // FaultInjectionAction::Delay is a special case that should be implemented by the custom fault injection function. Keep in mind that
    // delay can also be modeled as FaultInjectionAction::Drop for a given duration of time depending on the required outcome.
}

#[derive(InspectMut)]
pub struct NvmeControllerFaultInjection {
    #[inspect(skip)]
    inner: Arc<Mutex<NvmeController>>,
    /// Fault injection callback for NVMe controller operations
    #[inspect(skip)]
    fi: Box<
        dyn Fn(&str, Vec<Box<dyn Any>>) -> (FaultInjectionAction, Vec<Box<dyn Any>>) + Send + Sync,
    >,
    mem: GuestMemory,
    driver: VmTaskDriver,
    #[inspect(skip)]
    doorbells: Vec<Arc<DoorbellRegister>>,
    regs: Regs,
    #[inspect(skip)]
    admin: TaskControl<AdminHandlerFaultInjection, AdminStateFaultInjection>,
    cfg_space: ConfigSpaceType0Emulator,
    // #[inspect(skip)]
    // admin_sq_latest_tail: mesh::CellUpdater<u32>,
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
        fi: Box<
            dyn Fn(&str, Vec<Box<dyn Any>>) -> (FaultInjectionAction, Vec<Box<dyn Any>>)
                + Send
                + Sync,
        >,
    ) -> Self {
        // Deal with Doorbell registers copy
        let num_qids = 2 + caps.max_io_queues * 2; // Assumes that max_sqs == max_cqs
        let doorbells: Vec<_> = (0..num_qids)
            .map(|_| Arc::new(DoorbellRegister::new()))
            .collect();

        let inner = Arc::new(Mutex::new(NvmeController::new(
            driver_source,
            guest_memory.clone(), // Communication with the inner controller will always be through the inner memory component.
            register_msi,
            register_mmio,
            caps,
        )));

        let qe_sizes = Arc::new(Default::default());
        let mut doorbell_write = mesh::CellUpdater::new(ILLEGAL_DOORBELL_VALUE);
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
            },
        );

        // TODO: This is hack because we don't want to register the interrupt vectors here. In a future implementation this could prove quite useful actually!
        let mut msi_set_dummy = MsiInterruptSet::new();
        let register_msi_dummy: &mut dyn RegisterMsi = &mut msi_set_dummy;
        let (msix, msix_cap) = MsixEmulator::new(4, caps.msix_count, register_msi_dummy);
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
            fi,
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
            // admin_sq_latest_tail: doorbell_write,
        }
    }

    /// Returns a client for manipulating the NVMe controller at runtime. This is mainly to add more namespaces
    pub fn client(&self) -> NvmeControllerClient {
        let inner = self.inner.lock();
        inner.client()
    }

    /// Reads from the virtual BAR 0.
    pub fn read_bar0(&mut self, addr: u16, data: &mut [u8]) -> IoResult {
        // Normal Behaviour.
        let mut inner = self.inner.lock();
        inner.read_bar0(addr, data)
    }

    fn is_admin_sq_doorbell_write(&mut self, addr: u16) -> bool {
        if addr >= 0x1000 {
            // Doorbell write.
            let base = addr - 0x1000;
            let index = base >> DOORBELL_STRIDE_BITS;
            if (index << DOORBELL_STRIDE_BITS) != base {
                return false;
            }
            if index != 0 {
                return false;
            }
            if let Some(_) = self.doorbells.get(index as usize) {
                return true;
            } else {
                tracelimit::warn_ratelimited!(index, "unknown doorbell");
            }
        }
        false
    }

    /// Writes to the virtual BAR 0.
    pub fn write_bar0(&mut self, addr: u16, data: &[u8]) -> IoResult {
        if addr >= 0x1000 {
            // Doorbell write.
            let base = addr - 0x1000;
            let index = base >> DOORBELL_STRIDE_BITS;
            if (index << DOORBELL_STRIDE_BITS) != base {
                return IoResult::Err(InvalidRegister);
            }
            let Ok(data) = data.try_into() else {
                return IoResult::Err(IoError::InvalidAccessSize);
            };
            let data = u32::from_ne_bytes(data);
            if let Some(doorbell) = self.doorbells.get(index as usize) {
                // self.admin_sq_latest_tail.set(data);
                debug!(
                    "Writing doobell data: {:#x} to doorbell index: {}",
                    data, index
                );
                doorbell.write(data);
            } else {
                tracelimit::warn_ratelimited!(index, data, "unknown doorbell");
            }
            return IoResult::Ok;
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

        // Intercept and duplicate admin queue setup!
        match spec::Register(addr & !7) {
            spec::Register::ASQ => {
                if !self.regs.cc.en() {
                    self.regs.asq = update_reg(self.regs.asq) & PAGE_MASK;
                    tracing::debug!("ASQ set to {:#x}", self.regs.asq);
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

        let Ok(data) = data.try_into() else {
            return IoResult::Err(IoError::InvalidAccessSize);
        };
        let data = u32::from_ne_bytes(data);
        // Handle 32-bit registers.
        match spec::Register(addr) {
            spec::Register::CC => self.set_cc(data.into()),
            spec::Register::AQA => self.regs.aqa = data.into(),
            _ => {}
        }

        // Don't pass this function call down since that will be handled by the mmio_write function.
        IoResult::Ok
    }

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
        let mut cc: spec::Cc = (u32::from(cc) & mask).into();

        // Admin queue has not yet been started, set it up here.
        if !self.admin.is_running() {
            let state = AdminStateFaultInjection::new(
                self.admin.task(),
                self.regs.asq,
                self.regs.aqa.asqs_z().max(1) + 1,
                // self.admin_sq_latest_tail.cell(),
            );
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
            let ret_bar0 = self.write_bar0(offset, data); // TODO: This means that we are calling the inner write_bar0 function twice, fix that later.
            if self.is_admin_sq_doorbell_write(addr as u16) {
                return ret_bar0;
            }
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
        self.cfg_space.write_u32(offset, value);
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
