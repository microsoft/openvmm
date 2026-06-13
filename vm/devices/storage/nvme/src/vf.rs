// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! NVMe Virtual Function (VF) state for SR-IOV support.
//!
//! Each VF is an independent NVMe controller with its own register state,
//! admin/IO queues, and worker tasks. VFs are owned by the PF
//! ([`super::NvmeController`]) and created/destroyed when the SR-IOV
//! capability's VF Enable bit changes.
//!
//! VF resource limits (max queues, interrupts) and namespace assignments
//! are managed by the PF admin handler via Virtualization Management and
//! Namespace Attachment commands. The VF reads its configuration from a
//! shared [`VfControllerConfig`](crate::VfControllerConfig) at CC.EN time.

use crate::VENDOR_ID;
use crate::VF_DEVICE_ID;
use crate::VfControllerConfig;
use crate::registers::RegState;
use crate::workers::IoQueueEntrySizes;
use crate::workers::NvmeWorkers;
use chipset_device::io::IoResult;
use device_emulators::ReadWriteRequestType;
use device_emulators::read_as_u32_chunks;
use device_emulators::write_as_u32_chunks;
use guestmem::GuestMemory;
use guid::Guid;
use inspect::Inspect;
use parking_lot::Mutex;
use pci_core::capabilities::msix::MsixEmulator;
use pci_core::capabilities::pci_express::PciExpressCapability;
use pci_core::cfg_space_emu::ConfigSpaceType0Emulator;
use pci_core::cfg_space_emu::DeviceBars;
use pci_core::msi::MsiTarget;
use pci_core::spec::hwid::ClassCode;
use pci_core::spec::hwid::HardwareIds;
use pci_core::spec::hwid::ProgrammingInterface;
use pci_core::spec::hwid::Subclass;
use std::sync::Arc;
use vmcore::vm_task::VmTaskDriverSource;

/// An NVMe Virtual Function (VF) with its own PCI config space and NVMe
/// controller state machine.
///
/// VFs do **not** have their own BAR registers. BAR addresses are defined by
/// the PF's SR-IOV extended capability VF BAR registers — each VF's address
/// is computed as `VF_BAR_base + vf_index * bar_size`. The PF manages MMIO
/// intercepts on behalf of VFs and routes MMIO accesses to VF methods.
#[derive(Inspect)]
pub(crate) struct NvmeVirtualFunction {
    cfg_space: ConfigSpaceType0Emulator,
    #[inspect(skip)]
    msix: MsixEmulator,

    registers: RegState,
    #[inspect(skip)]
    qe_sizes: Arc<Mutex<IoQueueEntrySizes>>,
    #[inspect(skip)]
    workers: Option<NvmeWorkers>,

    // Worker creation dependencies — stored at VF creation, used at CC.EN.
    #[inspect(skip)]
    driver_source: VmTaskDriverSource,
    #[inspect(skip)]
    guest_memory: GuestMemory,
    #[inspect(display)]
    subsystem_id: Guid,
    msix_count: u16,

    /// 0-based VF index.
    vf_index: u16,
    /// NVMe controller ID for this VF (secondary controller ID).
    cntlid: u16,
    /// Fixed maximum number of IO queues for this VF.
    max_io_queues: u16,

    /// Shared configuration from PF admin — online state and namespaces.
    #[inspect(skip)]
    shared_config: Arc<Mutex<VfControllerConfig>>,
}

impl NvmeVirtualFunction {
    /// Creates a new VF with the given identity and worker dependencies.
    ///
    /// The VF starts disabled (CC.EN=0). The guest driver enables it by
    /// writing to the CC register via BAR0. At that point, the VF reads
    /// its resource limits and namespaces from `shared_config`.
    pub fn new(
        msix_count: u16,
        max_io_queues: u16,
        msi_target: &MsiTarget,
        driver_source: VmTaskDriverSource,
        guest_memory: GuestMemory,
        subsystem_id: Guid,
        vf_index: u16,
        cntlid: u16,
        shared_config: Arc<Mutex<VfControllerConfig>>,
    ) -> Self {
        let (msix, msix_cap) = MsixEmulator::new(4, msix_count, msi_target);

        // VFs have no BARs in their own config space. BAR addresses come
        // from the PF's SR-IOV extended capability VF BAR registers.
        let bars = DeviceBars::new();

        let cfg_space = ConfigSpaceType0Emulator::new(
            HardwareIds {
                vendor_id: VENDOR_ID,
                device_id: VF_DEVICE_ID,
                revision_id: 0,
                prog_if: ProgrammingInterface::MASS_STORAGE_CONTROLLER_NON_VOLATILE_MEMORY_NVME,
                sub_class: Subclass::MASS_STORAGE_CONTROLLER_NON_VOLATILE_MEMORY,
                base_class: ClassCode::MASS_STORAGE_CONTROLLER,
                type0_sub_vendor_id: 0,
                type0_sub_system_id: 0,
            },
            vec![
                Box::new(msix_cap),
                Box::new(PciExpressCapability::new(
                    pci_core::spec::caps::pci_express::DevicePortType::Endpoint,
                    None,
                )),
            ],
            Vec::new(),
            bars,
        );

        Self {
            cfg_space,
            msix,
            registers: RegState::new(),
            qe_sizes: Arc::new(Default::default()),
            workers: None,
            driver_source,
            guest_memory,
            subsystem_id,
            msix_count,
            vf_index,
            cntlid,
            max_io_queues,
            shared_config,
        }
    }

    /// Read from this VF's PCI config space.
    pub fn pci_cfg_read(&mut self, offset: u16, value: &mut u32) -> IoResult {
        self.cfg_space.read_u32(offset, value)
    }

    /// Write to this VF's PCI config space.
    pub fn pci_cfg_write(&mut self, offset: u16, value: u32) -> IoResult {
        self.cfg_space.write_u32(offset, value)
    }

    /// Reads from the VF's BAR0 (NVMe registers + doorbells).
    pub fn read_bar0(&mut self, addr: u64, data: &mut [u8]) -> IoResult {
        crate::registers::read_bar0(self, addr, data)
    }

    /// Writes to the VF's BAR0 (NVMe registers + doorbells).
    pub fn write_bar0(&mut self, addr: u64, data: &[u8]) -> IoResult {
        crate::registers::write_bar0(self, addr, data)
    }

    /// Create NVMe workers and enable the VF controller.
    ///
    /// Reads resource limits and namespaces from the shared PF admin config.
    fn do_enable_controller(&mut self) {
        let config = self.shared_config.lock();

        if !config.online {
            tracelimit::warn_ratelimited!(
                vf = self.vf_index,
                "VF: cannot enable — secondary controller is offline"
            );
            drop(config);
            self.registers.csts.set_cfs(true);
            return;
        }

        // VF queue limits are fixed at construction time (CRT=0).
        let max_sqs = self.max_io_queues;
        let max_cqs = self.max_io_queues;

        // Clone the namespace disks for VF worker creation.
        let initial_namespaces = config.attached_namespaces.clone();
        drop(config);

        // Create VF interrupts from MSI-X.
        let interrupt_count = self.msix_count.min(max_cqs + 1); // admin + IO CQs
        let interrupts: Vec<_> = (0..interrupt_count)
            .map(|i| self.msix.interrupt(i).unwrap())
            .collect();

        let qe_sizes = Arc::clone(&self.qe_sizes);

        // Create VF workers — VFs don't have SR-IOV of their own.
        let mut workers = NvmeWorkers::new(
            &self.driver_source,
            self.guest_memory.clone(),
            interrupts,
            max_sqs,
            max_cqs,
            qe_sizes,
            self.subsystem_id,
            None, // VFs don't manage other VFs
            self.cntlid,
            initial_namespaces,
        );

        workers.enable(
            self.registers.asq,
            self.registers.aqa.asqs_z().max(1) + 1,
            self.registers.acq,
            self.registers.aqa.acqs_z().max(1) + 1,
        );

        self.workers = Some(workers);

        tracing::info!(
            vf = self.vf_index,
            cntlid = self.cntlid,
            max_sqs,
            max_cqs,
            "VF: controller enabled"
        );
    }
}

impl crate::registers::NvmeRegisterIo for NvmeVirtualFunction {
    fn registers(&self) -> &RegState {
        &self.registers
    }

    fn registers_mut(&mut self) -> &mut RegState {
        &mut self.registers
    }

    fn qe_sizes(&self) -> &Mutex<IoQueueEntrySizes> {
        &self.qe_sizes
    }

    fn doorbell(&self, db_id: u16, value: u32) {
        if let Some(workers) = &self.workers {
            workers.doorbell(db_id, value);
        }
    }

    fn enable_controller(&mut self) {
        self.do_enable_controller();
    }

    fn reset_controller(&mut self) {
        self.workers
            .as_mut()
            .expect("workers must exist when enabled")
            .controller_reset();
    }

    fn poll_enabled(&mut self) -> bool {
        self.workers.as_mut().is_some_and(|w| w.poll_enabled())
    }

    fn poll_reset(&mut self) -> bool {
        let done = self
            .workers
            .as_mut()
            .is_some_and(|w| w.poll_controller_reset());
        if done {
            // Drop workers after reset completes.
            self.workers = None;
        }
        done
    }
}

impl NvmeVirtualFunction {
    /// Force-reset the VF controller. Called when PF resets or VF_Enable
    /// is cleared. Resets register state and initiates worker shutdown, but
    /// does NOT drop workers — call [`drain`] to wait for in-flight IOs
    /// to complete before dropping.
    pub fn initiate_reset(&mut self) {
        if let Some(workers) = &mut self.workers {
            match workers.enable_state() {
                crate::workers::EnableStateKind::Enabled => {
                    tracing::info!(vf = self.vf_index, "VF: initiating controller reset");
                    workers.controller_reset();
                }
                crate::workers::EnableStateKind::Enabling => {
                    // Workers are mid-enable — nothing to drain yet, will
                    // be cleaned up when drain() awaits reset().
                    tracing::info!(vf = self.vf_index, "VF: initiating reset during enable");
                }
                crate::workers::EnableStateKind::Disabled
                | crate::workers::EnableStateKind::Resetting => {
                    // Already disabled or resetting — nothing to do.
                }
            }
        }
        self.registers = RegState::new();
        *self.qe_sizes.lock() = Default::default();
    }

    /// Asynchronously drain all in-flight IOs and drop workers.
    ///
    /// Must be called after [`initiate_reset`] to ensure all IOs holding
    /// guest memory references complete before the VF is dropped.
    pub async fn drain(&mut self) {
        if let Some(workers) = &mut self.workers {
            workers.reset().await;
        }
        self.workers = None;
    }

    /// Non-blocking poll for drain completion. Returns `true` when all
    /// workers have drained. Drops workers when done.
    ///
    /// Registers `cx.waker()` with the underlying channel so the caller
    /// is woken when the drain makes progress.
    pub fn poll_drain(&mut self, cx: &mut std::task::Context<'_>) -> bool {
        let drained = match &mut self.workers {
            Some(workers) => workers.poll_drain(cx),
            None => true,
        };
        if drained {
            self.workers = None;
        }
        drained
    }

    /// Reads from the VF's MSI-X BAR.
    pub fn read_msix(&mut self, offset: u64, data: &mut [u8]) -> IoResult {
        read_as_u32_chunks(offset, data, |offset| self.msix.read_u32(offset));
        IoResult::Ok
    }

    /// Writes to the VF's MSI-X BAR.
    pub fn write_msix(&mut self, offset: u64, data: &[u8]) -> IoResult {
        write_as_u32_chunks(offset, data, |offset, ty| match ty {
            ReadWriteRequestType::Read => Some(self.msix.read_u32(offset)),
            ReadWriteRequestType::Write(val) => {
                self.msix.write_u32(offset, val);
                None
            }
        });
        IoResult::Ok
    }
}
