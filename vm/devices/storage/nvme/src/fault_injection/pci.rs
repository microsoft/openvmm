// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use crate::BAR0_LEN;
use crate::DOORBELL_STRIDE_BITS;
use crate::FaultFn;
use crate::IOCQES;
use crate::IOSQES;
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
use std::sync::Arc;
use task_control::TaskControl;
use vmcore::device_state::ChangeDeviceState;
use vmcore::save_restore::SaveError;
use vmcore::save_restore::SaveRestore;
use vmcore::save_restore::SavedStateNotSupported;
use vmcore::vm_task::VmTaskDriver;
use vmcore::vm_task::VmTaskDriverSource;

/// Fault injection for the NVMe controller meant for testing. Allows for delaying and changing admin queue commands
/// This is a minimally implemented controller that relies on the inner controller for most functionality.
#[derive(InspectMut)]
pub struct NvmeControllerFaultInjection {
    #[inspect(skip)]
    inner: Arc<Mutex<NvmeController>>,
    mem: GuestMemory,
    driver: VmTaskDriver,
    #[inspect(skip)]
    sq_doorbell: Arc<DoorbellRegister>,
    registers: Registers,
    #[inspect(skip)]
    admin: TaskControl<AdminHandlerFaultInjection, AdminStateFaultInjection>,
    cfg_space: ConfigSpaceType0Emulator,
}

// Need to only track a subset of registers for the fault controller.
#[derive(Inspect)]
struct Registers {
    #[inspect(hex)]
    asq: u64,
    #[inspect(hex)]
    acq: u64,
    aqa: spec::Aqa,
    cc: spec::Cc,
    csts: spec::Csts,
}

/// NvmeController with fault injection capabilities for testing and validation. This implementation
/// wraps a standard NVMe controller and provides the ability to inject faults into submission queue
/// commands for testing purposes.
impl NvmeControllerFaultInjection {
    /// The `FaultFn` is provided `nvme_spec::Command` instances before they
    /// are processed by the inner controller. Returning `Some(command)` will overwrite the provided command in guest memory,
    /// while returning `None` will leave the command unchanged.
    pub fn new(
        driver_source: &VmTaskDriverSource,
        guest_memory: GuestMemory,
        register_msi: &mut dyn RegisterMsi,
        register_mmio: &mut dyn RegisterMmioIntercept,
        caps: NvmeControllerCaps,
        sq_fault_injector: FaultFn,
    ) -> Self {
        let sq_doorbell = Arc::new(DoorbellRegister::new());

        // async AdminHandler uses this too so locking is required.
        let inner = Arc::new(Mutex::new(NvmeController::new(
            driver_source,
            guest_memory.clone(), // Communication with the inner controller will always be through the inner memory component.
            register_msi,
            register_mmio,
            caps,
        )));

        let handler: AdminHandlerFaultInjection = AdminHandlerFaultInjection::new(
            driver_source.simple(),
            AdminConfigFaultInjection {
                mem: guest_memory.clone(),
                controller: inner.clone(),
                admin_sq_doorbell_addr: 0x1000, // The address of the submission queue doorbell in the device's BAR0.
                sq_fault_injector,
            },
        );

        // Only inner needs to register the interrupt vectors.
        let register_msi_dummy: &mut dyn RegisterMsi = &mut MsiInterruptSet::new();
        let (_, msix_cap) = MsixEmulator::new(4, caps.msix_count, register_msi_dummy);

        let bars = DeviceBars::new().bar0(
            BAR0_LEN,
            BarMemoryKind::Intercept(register_mmio.new_io_region("bar0", BAR0_LEN)),
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
            sq_doorbell,
            registers: Registers {
                asq: 0,
                acq: 0,
                aqa: spec::Aqa::default(),
                cc: spec::Cc::default(),
                csts: spec::Csts::default(),
            },
            admin: TaskControl::new(handler),
            cfg_space,
        }
    }

    /// Passthrough
    pub fn client(&self) -> NvmeControllerClient {
        let inner = self.inner.lock();
        inner.client()
    }

    /// Passthrough
    pub fn read_bar0(&mut self, addr: u16, data: &mut [u8]) -> IoResult {
        let mut inner = self.inner.lock();
        inner.read_bar0(addr, data)
    }

    /// If doorbell intercept succeeds, returns Ok
    fn try_intercept_doorbell(&mut self, addr: u16, data: &[u8]) -> Result<IoResult, ()> {
        if addr >= 0x1000 {
            // Doorbell write.
            let base = addr - 0x1000;
            let index = base >> DOORBELL_STRIDE_BITS;
            if (index << DOORBELL_STRIDE_BITS) != base || index != 0 {
                return Err(());
            }
            if index != 0 {
                // As of now the driver only supports a single doorbell (For the Admin Submission Queue).
                return Err(());
            }
            let Ok(data) = data.try_into() else {
                return Err(());
            };
            let data = u32::from_ne_bytes(data);
            self.sq_doorbell.write(data);
            return Ok(IoResult::Ok);
        }
        Err(())
    }

    /// Writes to the virtual BAR 0.
    /// This does NOT handle doorbell writes, use mmio_write instead.
    /// this does NOT handle write_bar0() to inner, use mmio_write instead.
    pub fn write_bar0(&mut self, addr: u16, data: &[u8]) -> IoResult {
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
                if !self.registers.cc.en() {
                    self.registers.asq = update_reg(self.registers.asq) & PAGE_MASK;
                } else {
                    tracelimit::warn_ratelimited!("attempt to set asq while enabled");
                }
            }
            spec::Register::ACQ => {
                if !self.registers.cc.en() {
                    self.registers.acq = update_reg(self.registers.acq) & PAGE_MASK;
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
        match spec::Register(addr) {
            spec::Register::CC => self.set_cc(data.into()),
            spec::Register::AQA => self.registers.aqa = data.into(),
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
        let mut cc: spec::Cc = (u32::from(cc) & mask).into();

        if cc.en() != self.registers.cc.en() {
            if cc.en() {
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

                // Enable the admin fault injection queue
                if !self.admin.is_running() {
                    let state = AdminStateFaultInjection::new(
                        self.registers.asq,
                        self.registers.aqa.asqs_z().max(1) + 1,
                        self.sq_doorbell.clone(), // Admin Submission Queue Doorbell
                    );
                    self.admin
                        .insert(&self.driver, "nvme-admin-fault-injection", state);
                    self.admin.start();
                    self.registers.csts.set_rdy(true);
                }
            } else if self.registers.csts.rdy() {
                // Fault Controller does not yet support controller resets. This functionality will be coming in the future.
            } else {
                tracelimit::warn_ratelimited!("disabling while not ready");
                return;
            }
        }

        self.registers.cc = cc;
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

            // Duplicate admin queue setup and write to the inner controller.
            let _ = self.write_bar0(offset, data);
        }

        let mut inner = self.inner.lock();
        inner.mmio_write(addr, data)
    }
}

impl PciConfigSpace for NvmeControllerFaultInjection {
    fn pci_cfg_read(&mut self, offset: u16, value: &mut u32) -> IoResult {
        let mut inner = self.inner.lock();
        inner.pci_cfg_read(offset, value)
    }

    fn pci_cfg_write(&mut self, offset: u16, value: u32) -> IoResult {
        // Write to self to duplicate admin queue setup.
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
