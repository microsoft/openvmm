use crate::BAR0_LEN;
use crate::DOORBELL_STRIDE_BITS;
use crate::NvmeController;
use crate::NvmeControllerCaps;
use crate::NvmeControllerClient;
use crate::PAGE_MASK;
use crate::VENDOR_ID;
use crate::queue::DoorbellRegister;
use crate::queue::ILLEGAL_DOORBELL_VALUE;
use crate::queue::QueueError;
use crate::queue::ShadowDoorbell;
use crate::queue::SubmissionQueue;
use crate::spec;
use crate::workers::IoQueueEntrySizes;
use chipset_device::ChipsetDevice;
use chipset_device::io::IoError;
use chipset_device::io::IoError::InvalidRegister;
use chipset_device::io::IoResult;
use chipset_device::mmio::MmioIntercept;
use chipset_device::mmio::RegisterMmioIntercept;
use chipset_device::pci::PciConfigSpace;
use futures::FutureExt;
use guestmem::GuestMemory;
use guid::Guid;
use inspect::Inspect;
use inspect::InspectMut;
use parking_lot::Mutex;
use pci_core::capabilities::msix::MsixEmulator;
use pci_core::cfg_space_emu::BarMemoryKind;
use pci_core::cfg_space_emu::ConfigSpaceType0Emulator;
use pci_core::cfg_space_emu::DeviceBars;
use pci_core::msi::RegisterMsi;
use pci_core::spec::hwid::ClassCode;
use pci_core::spec::hwid::HardwareIds;
use pci_core::spec::hwid::ProgrammingInterface;
use pci_core::spec::hwid::Subclass;
use std::any::Any;
use std::sync::Arc;
use task_control::AsyncRun;
use task_control::Cancelled;
use task_control::InspectTask;
use task_control::StopTask;
use task_control::TaskControl;
use user_driver_emulated_mock::DeviceTestMemory;
use vmcore::device_state::ChangeDeviceState;
use vmcore::interrupt::Interrupt;
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
    memory_inner: GuestMemory,
    memory_outer: GuestMemory,
    driver: VmTaskDriver,
    #[inspect(skip)]
    doorbells: Vec<Arc<DoorbellRegister>>,
    regs: Regs,
    #[inspect(skip)]
    admin: TaskControl<AdminHandlerFaultInjection, AdminStateFaultInjection>,
    cfg_space: ConfigSpaceType0Emulator,
    #[inspect(skip)]
    doorbell_write: mesh::CellUpdater<(u16, u32)>,
}

#[derive(Inspect)]
pub struct AdminConfigFaultInjection {
    #[inspect(skip)]
    pub driver_source: VmTaskDriverSource,
    #[inspect(skip)]
    pub mem: GuestMemory,
    pub inner_mem: GuestMemory,
    #[inspect(skip)]
    pub interrupts: Vec<Interrupt>,
    #[inspect(skip)]
    pub doorbells: Vec<Arc<DoorbellRegister>>,
    #[inspect(display)]
    pub subsystem_id: Guid,
    pub max_sqs: u16,
    pub max_cqs: u16,
    pub qe_sizes: Arc<Mutex<IoQueueEntrySizes>>,
    #[inspect(skip)]
    pub controller: Arc<Mutex<NvmeController>>,
    #[inspect(skip)]
    pub doorbell_write: mesh::Cell<(u16, u32)>,
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
        let inner = Arc::new(Mutex::new(NvmeController::new(
            driver_source,
            guest_memory.clone(), // Communication with the inner controller will always be through the inner memory component.
            register_msi,
            register_mmio,
            caps,
        )));

        let qe_sizes = Arc::new(Default::default());
        let mut doorbell_write = mesh::CellUpdater::new((0, ILLEGAL_DOORBELL_VALUE));
        let handler: AdminHandlerFaultInjection = AdminHandlerFaultInjection::new(
            driver_source.simple(),
            AdminConfigFaultInjection {
                driver_source: driver_source.clone(),
                mem: guest_memory.clone(),
                inner_mem: memory_inner.clone(),
                interrupts,
                doorbells: doorbells.clone(),
                subsystem_id: caps.subsystem_id,
                max_sqs: caps.max_io_queues,
                max_cqs: caps.max_io_queues,
                qe_sizes: Arc::clone(&qe_sizes),
                controller: inner.clone(),
                doorbell_write: doorbell_write.cell(),
            },
        );

        let (msix, msix_cap) = MsixEmulator::new(4, caps.msix_count, register_msi);
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
            memory_inner,
            memory_outer: guest_memory.clone(),
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
            doorbell_write,
        }
    }

    /// Returns a client for manipulating the NVMe controller at runtime.
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

    fn is_doorbell_write(&mut self, addr: u16) -> bool {
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
        tracing::debug!("IN THE WRITE BAR 0 FUNCTION");

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
                tracing::debug!(
                    "DINGING MY OWN DOORBELL REAL QUICK WITH {addr}, data: {data}, and index: {index}"
                );
                self.doorbell_write.set((addr, data));
                doorbell.write(data);
            } else {
                tracelimit::warn_ratelimited!(index, data, "unknown doorbell");
            }
            return IoResult::Ok;
        }

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

        // Handled all queue related jargon, let the inner controller handle the rest
        let mut inner = self.inner.lock();
        inner.write_bar0(addr, data_original)
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
        tracing::debug!("IN THE SET CC FUNCTION");

        // Admin queue has not yet been started, set it up here.
        if !self.admin.is_running() {
            let state = AdminStateFaultInjection::new(
                self.admin.task(),
                self.regs.asq,
                self.regs.aqa.asqs_z().max(1) + 1,
                self.doorbell_write.cell(),
            );
            self.admin
                .insert(&self.driver, "nvme-admin-fault-injection", state);
            tracing::debug!("SETTING UP THE ADMIN HANDLER FOR FAULT INJECTION");
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
        // TODO: Leaving this intentionally empty because the inner fn is also empty
    }

    async fn reset(&mut self) {
        // TODO: Leaving this intentionally empty because the inner fn is also empty
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
            if self.is_doorbell_write(addr as u16) {
                tracing::debug!("ISSUING DOORBELL MMIO WRITE");
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

/// An admin handler shim layer for fault injection.
#[derive(Inspect)]
pub struct AdminHandlerFaultInjection {
    driver: VmTaskDriver,
    config: AdminConfigFaultInjection,
}

impl AdminHandlerFaultInjection {
    pub fn new(driver: VmTaskDriver, config: AdminConfigFaultInjection) -> Self {
        Self { driver, config }
    }
}

impl AsyncRun<AdminStateFaultInjection> for AdminHandlerFaultInjection {
    async fn run(
        &mut self,
        stop: &mut StopTask<'_>,
        state: &mut AdminStateFaultInjection,
    ) -> Result<(), Cancelled> {
        loop {
            let event = stop.until_stopped(self.next_event(state)).await?;
            tracing::debug!(
                "THIS IS YOUR CAPTAIN SPEAKING: admin handler is intercepting, I repeat, admin handler is intercepting",
            );
        }
        Ok(())
    }
}

#[derive(Debug)]
enum Event {
    Command(Result<spec::Command, QueueError>),
}

impl AdminHandlerFaultInjection {
    async fn next_event(
        &mut self,
        state: &mut AdminStateFaultInjection,
    ) -> Result<Event, QueueError> {
        // A little bit of an explanation here: From the looks of it, the underlying admin handler
        // is actually handling 3 different types of commands. The sq_delete_response, admin_sq, and changed_namespace.
        // For now we are only concerned with the admin_sq because that is the driver->controller communication that we are interested in.
        let next_command = state
            .admin_sq
            .next(&self.config.mem)
            .map(Event::Command)
            .await;
        Ok(next_command)
    }
}

impl InspectTask<AdminStateFaultInjection> for AdminHandlerFaultInjection {
    fn inspect(&self, req: inspect::Request<'_>, state: Option<&AdminStateFaultInjection>) {
        req.respond().merge(self).merge(state);
    }
}

#[derive(Inspect)]
pub struct AdminStateFaultInjection {
    pub admin_sq: SubmissionQueueFaultInjection,
    // pub admin_cq: CompletionQueue, Coming soon!
    // #[inspect(with = "|x| inspect::iter_by_index(x).map_key(|x| x + 1)")]  TODO: These will be used in the future.
    // io_sqs: Vec<IoSq>,
    // #[inspect(with = "|x| inspect::iter_by_index(x).map_key(|x| x + 1)")]
    // io_cqs: Vec<Option<IoCq>>,
    // #[inspect(skip)]
    // sq_delete_response: mesh::Receiver<u16>,  TODO: What is this for?
    // #[inspect(with = "Option::is_some")]
    // shadow_db_evt_gpa_base: Option<ShadowDoorbell>,  TODO: What is this used for?
    // #[inspect(iter_by_index)]
    // asynchronous_event_requests: Vec<u16>,  TODO: No idea what this is for either
    // #[inspect(
    //     rename = "namespaces",
    //     with = "|x| inspect::iter_by_key(x.iter().map(|v| (v, ChangedNamespace { changed: true })))"
    // )]
    // changed_namespaces: Vec<u32>,
    // notified_changed_namespaces: bool,
    // #[inspect(skip)]
    // recv_changed_namespace: futures::channel::mpsc::Receiver<u32>,
    // #[inspect(skip)]
    // send_changed_namespace: futures::channel::mpsc::Sender<u32>,
    // #[inspect(skip)]
    // poll_namespace_change: BTreeMap<u32, Task<()>>,  All this will be used in the future.
}

impl AdminStateFaultInjection {
    pub fn new(
        handler: &AdminHandlerFaultInjection,
        asq: u64,
        asqs: u16,
        doorbell_write: mesh::Cell<(u16, u32)>,
    ) -> Self {
        Self {
            admin_sq: SubmissionQueueFaultInjection::new(
                handler.config.doorbells[0].clone(),
                asq,
                asqs,
                None,
                handler.config.controller.clone(),
                doorbell_write,
            ),
            // admin_cq: CompletionQueue::new(handler.config.doorbells[1].clone(), acq, acqs, None),
        }
    }
}

#[derive(Inspect)]
pub struct SubmissionQueueFaultInjection {
    inner: SubmissionQueue,
    gpa: u64,
    #[inspect(skip)]
    controller: Arc<Mutex<NvmeController>>,
    #[inspect(skip)]
    doorbell_write: mesh::Cell<(u16, u32)>,
}

impl SubmissionQueueFaultInjection {
    pub fn new(
        tail: Arc<DoorbellRegister>,
        gpa: u64,
        len: u16,
        shadow_db_evt_idx: Option<ShadowDoorbell>,
        controller: Arc<Mutex<NvmeController>>,
        doorbell_write: mesh::Cell<(u16, u32)>,
    ) -> Self {
        Self {
            inner: SubmissionQueue::new(tail, gpa, len, shadow_db_evt_idx),
            gpa,
            controller,
            doorbell_write,
        }
    }

    /// This function returns a future for the next entry in the submission queue.  It also
    /// has a side effect of updating the tail.
    ///
    /// Note that this function returns a future that must be cancellable, which means that the
    /// parts after an await may never run.  The tail update side effect is benign, so
    /// that can happen before the await.
    /// TODO: This approach will only work for a single admin command at a time. If multiple commands
    /// are placed at the same time, this will not work as expected!
    pub async fn next(&mut self, mem: &GuestMemory) -> Result<spec::Command, QueueError> {
        let head_cached = self.inner.sqhd();
        // let mut changed_command = spec::Command::default();
        let command = self.inner.next(mem).await?;
        // changed_command = command.clone();
        // changed_command.cdw0.set_opcode(0x06);
        tracing::debug!(
            "GETTING THE NEXT OPERATION IN THE CHAIN with gpa: {:#x}",
            self.gpa
        );
        // mem.write_plain(
        //     self.gpa.wrapping_add(head_cached as u64 * 64),
        //     &changed_command,
        // )
        // .map_err(QueueError::Memory)?;
        let (addr, data) = self.doorbell_write.get();
        let mut inner_controller = self.controller.lock();
        tracing::debug!("INVOKING INNER WRITE BAR0 FOR DOORBELL WITH DATA {addr}, data: {data:?}");
        let data = u32::to_ne_bytes(data);
        inner_controller.write_bar0(addr, &data);
        Ok(command)
    }

    pub fn sqhd(&self) -> u16 {
        self.inner.sqhd()
    }

    /// This function lets the driver know what doorbell value we consumed, allowing
    /// it to elide the next ring, maybe.
    pub fn advance_evt_idx(&mut self, mem: &GuestMemory) -> Result<(), QueueError> {
        self.inner.advance_evt_idx(mem)
    }

    /// This function updates the shadow doorbell values of a queue that is
    /// potentially already in use.
    pub fn update_shadow_db(&mut self, mem: &GuestMemory, sdb: ShadowDoorbell) {
        self.inner.update_shadow_db(mem, sdb)
    }
}
