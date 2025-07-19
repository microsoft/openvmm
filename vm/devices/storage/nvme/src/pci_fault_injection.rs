use crate::NvmeController;
use crate::NvmeControllerCaps;
use crate::NvmeControllerClient;
use crate::PAGE_MASK;
use crate::namespace::Namespace;
use crate::queue::DoorbellRegister;
use crate::queue::QueueError;
use crate::spec;
use crate::workers::admin::AdminConfig;
use crate::workers::admin::AdminHandler;
use crate::workers::admin::AdminState;
use chipset_device::ChipsetDevice;
use chipset_device::io::IoError;
use chipset_device::io::IoResult;
use chipset_device::mmio::MmioIntercept;
use chipset_device::mmio::RegisterMmioIntercept;
use chipset_device::pci::PciConfigSpace;
use guestmem::GuestMemory;
use inspect::Inspect;
use inspect::InspectMut;
use pci_core::msi::RegisterMsi;
use std::any::Any;
use std::collections::BTreeMap;
use std::sync::Arc;
use task_control::AsyncRun;
use task_control::Cancelled;
use task_control::InspectTask;
use task_control::StopTask;
use task_control::TaskControl;
use user_driver_emulated_mock::DeviceTestMemory;
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
    inner: NvmeController,
    /// Fault injection callback for NVMe controller operations
    #[inspect(skip)]
    fi: Box<
        dyn Fn(&str, Vec<Box<dyn Any>>) -> (FaultInjectionAction, Vec<Box<dyn Any>>) + Send + Sync,
    >,
    memory_inner: GuestMemory,
    memory_outer: GuestMemory,
    driver_source: VmTaskDriver,
    #[inspect(skip)]
    doorbells: Vec<Arc<DoorbellRegister>>,
    regs: Regs,
    #[inspect(skip)]
    admin: TaskControl<AdminHandlerFaultInjection, AdminState>,
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
        pages: u64,
    ) -> Self {
        // Create memory for inner
        let memory_inner =
            DeviceTestMemory::new(pages * 2, false, "test_nvme_driver").guest_memory();

        // Deal with Doorbell registers copy
        let num_qids = 2 + caps.max_io_queues * 2; // Assumes that max_sqs == max_cqs
        let doorbells: Vec<_> = (0..num_qids)
            .map(|_| Arc::new(DoorbellRegister::new()))
            .collect();

        // TODO: Unused, but will probably need to update the AdminConfig -> AdminConfigFaultInjection and not pass this in anymore!
        let interrupts = vec![];

        let qe_sizes = Arc::new(Default::default());
        let handler: AdminHandlerFaultInjection = AdminHandlerFaultInjection::new(
            driver_source.simple(),
            AdminConfig {
                driver_source: driver_source.clone(),
                mem: guest_memory.clone(),
                interrupts,
                doorbells: doorbells.clone(),
                subsystem_id: caps.subsystem_id,
                max_sqs: caps.max_io_queues,
                max_cqs: caps.max_io_queues,
                qe_sizes: Arc::clone(&qe_sizes),
            },
        );

        Self {
            inner: NvmeController::new(
                driver_source,
                guest_memory.clone(),
                register_msi,
                register_mmio,
                caps,
            ),
            fi,
            memory_inner,
            memory_outer: guest_memory.clone(),
            driver_source: driver_source.simple(),
            doorbells,
            regs: Regs {
                asq: 0,
                acq: 0,
                aqa: spec::Aqa::default(),
                cc: spec::Cc::default(),
            },
            admin: TaskControl::new(handler),
        }
    }

    /// Returns a client for manipulating the NVMe controller at runtime.
    pub fn client(&self) -> NvmeControllerClient {
        self.inner.client()
    }

    /// Reads from the virtual BAR 0.
    pub fn read_bar0(&mut self, addr: u16, data: &mut [u8]) -> IoResult {
        // FI Input
        let mut input: Vec<Box<dyn Any>> = vec![Box::new(addr as u32), Box::new(data.to_vec())];
        let (action, mut responses) = (self.fi)("read_bar0", input);

        // Act on input Fault Injection
        match action {
            FaultInjectionAction::Drop => {
                // Returns an I/O result.
                // TODO: Since IoResult doesn't implement Copy it cannot be sent from the fault injection function.
                // This is a limitation of the current design. We can change this in the future.
                return IoResult::Ok;
            }
            _ => {
                // Not every action needs to be handled everywhere, only the relevant ones.
                tracing::warn!("FaultInjectionAction {:?} not handled in read_bar0", action);
            }
        }

        // Normal Behaviour.
        self.inner.read_bar0(addr, data)
    }

    /// Writes to the virtual BAR 0.
    pub fn write_bar0(&mut self, addr: u16, data: &[u8]) -> IoResult {
        let data_original = data.clone();

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
        let handled = match spec::Register(addr & !7) {
            spec::Register::ASQ => {
                if !self.regs.cc.en() {
                    self.regs.asq = update_reg(self.regs.asq) & PAGE_MASK;
                } else {
                    tracelimit::warn_ratelimited!("attempt to set asq while enabled");
                }
                true
            }
            spec::Register::ACQ => {
                if !self.regs.cc.en() {
                    self.regs.acq = update_reg(self.regs.acq) & PAGE_MASK;
                } else {
                    tracelimit::warn_ratelimited!("attempt to set acq while enabled");
                }
                true
            }
            _ => false,
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

        // Handled all queue related jargon, let the inner controller handle the rest of the jargon
        self.inner.write_bar0(addr, data_original)
    }

    pub fn fatal_error(&mut self) {
        self.inner.fatal_error();
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

        self.regs.cc = cc;
    }
}

impl ChangeDeviceState for NvmeControllerFaultInjection {
    fn start(&mut self) {
        self.inner.start();
    }

    async fn stop(&mut self) {
        self.inner.stop().await;
    }

    async fn reset(&mut self) {
        self.inner.reset().await;
    }
}

impl ChipsetDevice for NvmeControllerFaultInjection {
    fn supports_mmio(&mut self) -> Option<&mut dyn MmioIntercept> {
        self.inner.supports_mmio()
    }

    fn supports_pci(&mut self) -> Option<&mut dyn PciConfigSpace> {
        self.inner.supports_pci()
    }
}

impl MmioIntercept for NvmeControllerFaultInjection {
    fn mmio_read(&mut self, addr: u64, data: &mut [u8]) -> IoResult {
        self.inner.mmio_read(addr, data)
    }

    fn mmio_write(&mut self, addr: u64, data: &[u8]) -> IoResult {
        self.inner.mmio_write(addr, data)
    }
}

impl PciConfigSpace for NvmeControllerFaultInjection {
    fn pci_cfg_read(&mut self, offset: u16, value: &mut u32) -> IoResult {
        self.inner.pci_cfg_read(offset, value)
    }

    fn pci_cfg_write(&mut self, offset: u16, value: u32) -> IoResult {
        self.inner.pci_cfg_write(offset, value)
    }
}

impl SaveRestore for NvmeControllerFaultInjection {
    type SavedState = SavedStateNotSupported;

    fn save(&mut self) -> Result<Self::SavedState, SaveError> {
        self.inner.save()
    }

    fn restore(
        &mut self,
        state: Self::SavedState,
    ) -> Result<(), vmcore::save_restore::RestoreError> {
        self.inner.restore(state)
    }
}

/// An admin handler shim layer for fault injection.
#[derive(Inspect)]
pub struct AdminHandlerFaultInjection {
    driver: VmTaskDriver,
    config: AdminConfig,
}

impl AdminHandlerFaultInjection {
    pub fn new(driver: VmTaskDriver, config: AdminConfig) -> Self {
        Self { driver, config }
    }
}

impl AsyncRun<AdminState> for AdminHandlerFaultInjection {
    async fn run(
        &mut self,
        stop: &mut StopTask<'_>,
        state: &mut AdminState,
    ) -> Result<(), Cancelled> {
        loop {
            let event = stop.until_stopped(self.next_event(state)).await?;
            tracing::debug!(
                "THIS IS YOUR CAPTAIN SPEAKING: admin handler is intercepting, I repeat, admin handler is intercepting"
            );
        }
        Ok(())
    }
}

impl AdminHandlerFaultInjection {
    async fn next_event(&mut self, state: &mut AdminState) {
        let _ = state.admin_cq.wait_ready(&self.config.mem);
    }
}

impl InspectTask<AdminState> for AdminHandlerFaultInjection {
    fn inspect(&self, req: inspect::Request<'_>, state: Option<&AdminState>) {
        req.respond().merge(self).merge(state);
    }
}
