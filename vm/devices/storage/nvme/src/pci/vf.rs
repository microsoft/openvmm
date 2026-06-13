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
//! Namespace Attachment commands, which are routed to the VF's coordinator
//! through its [`NvmeControllerClient`](crate::NvmeControllerClient).

use crate::VENDOR_ID;
use crate::VF_DEVICE_ID;
use crate::pci::ControllerCore;
use crate::workers::IoQueueEntrySizes;
use crate::workers::NvmeControllerClient;
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

    #[inspect(flatten)]
    core: ControllerCore,

    /// 0-based VF index.
    vf_index: u16,
}

/// Parameters for constructing a [`NvmeVirtualFunction`].
pub(crate) struct NvmeVirtualFunctionParams<'a> {
    /// Number of MSI-X vectors for the VF.
    pub msix_count: u16,
    /// Fixed maximum number of IO queues for the VF.
    pub max_io_queues: u16,
    /// MSI target for this VF's interrupts.
    pub msi_target: &'a MsiTarget,
    /// Driver source for creating VF worker tasks.
    pub driver_source: VmTaskDriverSource,
    /// Guest memory for VF DMA.
    pub guest_memory: GuestMemory,
    /// Subsystem ID for VF NVMe identity.
    pub subsystem_id: Guid,
    /// 0-based VF index.
    pub vf_index: u16,
    /// NVMe controller ID for this VF (secondary controller ID).
    pub cntlid: u16,
}

impl NvmeVirtualFunction {
    /// Creates a new VF with the given identity and worker dependencies.
    ///
    /// The VF's coordinator is created here (in the `Disabled` state) and
    /// lives until the VF is destroyed. The VF starts disabled (CC.EN=0);
    /// the guest driver enables it by writing to the CC register via BAR0.
    pub fn new(params: NvmeVirtualFunctionParams<'_>) -> Self {
        let NvmeVirtualFunctionParams {
            msix_count,
            max_io_queues,
            msi_target,
            driver_source,
            guest_memory,
            subsystem_id,
            vf_index,
            cntlid,
        } = params;

        let (msix, msix_cap) = MsixEmulator::new(4, msix_count, msi_target);

        // VF queue limits are fixed at construction time (CRT=0).
        let max_sqs = max_io_queues;
        let max_cqs = max_io_queues;

        // Create VF interrupts from MSI-X (admin + IO CQs).
        let interrupt_count = msix_count.min(max_cqs + 1);
        let interrupts: Vec<_> = (0..interrupt_count)
            .map(|i| msix.interrupt(i).unwrap())
            .collect();

        let qe_sizes = Arc::new(Mutex::new(IoQueueEntrySizes::default()));

        // Create the VF's long-lived coordinator. It starts offline; the PF
        // brings it online via Virtualization Management before the guest
        // enables it. VFs don't manage other VFs.
        let workers = NvmeWorkers::new(
            &driver_source,
            guest_memory,
            interrupts,
            max_sqs,
            max_cqs,
            Arc::clone(&qe_sizes),
            subsystem_id,
            None,
            cntlid,
            false,
        );

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
            core: ControllerCore::new(qe_sizes, workers),
            vf_index,
        }
    }

    /// Returns a client for routing online/offline and namespace
    /// attach/detach to this VF's coordinator.
    pub fn client(&self) -> NvmeControllerClient {
        self.core.workers.client()
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
        self.core.read_bar0(addr, data)
    }

    /// Writes to the VF's BAR0 (NVMe registers + doorbells).
    pub fn write_bar0(&mut self, addr: u64, data: &[u8]) -> IoResult {
        self.core.write_bar0(addr, data)
    }
}

impl NvmeVirtualFunction {
    /// Force-reset the VF controller. Called when PF resets or VF_Enable
    /// is cleared. Resets register state and initiates worker shutdown, but
    /// does NOT drop the workers — call [`drain`] to wait for in-flight IOs
    /// to complete before the VF is dropped.
    pub fn initiate_reset(&mut self) {
        tracing::info!(vf = self.vf_index, "VF: initiating controller reset");
        self.core.initiate_reset();
    }

    /// Asynchronously drain all in-flight IOs, returning the workers to the
    /// disabled state.
    ///
    /// Must be called after [`initiate_reset`] to ensure all IOs holding
    /// guest memory references complete before the VF is dropped.
    pub async fn drain(&mut self) {
        self.core.drain().await;
    }

    /// Non-blocking poll for drain completion. Returns `true` when the
    /// workers have drained back to the disabled state.
    ///
    /// Registers `cx.waker()` with the underlying channel so the caller
    /// is woken when the drain makes progress.
    pub fn poll_drain(&mut self, cx: &mut std::task::Context<'_>) -> bool {
        self.core.poll_drain(cx)
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
