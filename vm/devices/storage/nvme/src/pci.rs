// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! The NVMe PCI device implementation.

mod vf;

use crate::BAR0_LEN;
use crate::DEVICE_ID;
use crate::DOORBELL_STRIDE_BITS;
use crate::IOCQES;
use crate::IOSQES;
use crate::MAX_QES;
use crate::NVME_VERSION;
use crate::NvmeControllerClient;
use crate::PAGE_MASK;
use crate::PAGE_SIZE;
use crate::VENDOR_ID;
use crate::VF_DEVICE_ID;
use crate::spec;
use crate::workers::EnablePoll;
use crate::workers::EnableStateKind;
use crate::workers::IoQueueEntrySizes;
use crate::workers::NvmeWorkers;
use chipset_device::ChipsetDevice;
use chipset_device::io::IoError;
use chipset_device::io::IoError::InvalidRegister;
use chipset_device::io::IoResult;
use chipset_device::io::deferred::DeferredWrite;
use chipset_device::io::deferred::defer_write;
use chipset_device::mmio::MmioIntercept;
use chipset_device::mmio::RegisterMmioIntercept;
use chipset_device::pci::PciConfigSpace;
use chipset_device::poll_device::PollDevice;
use device_emulators::ReadWriteRequestType;
use device_emulators::read_as_u32_chunks;
use device_emulators::write_as_u32_chunks;
use futures::future::join_all;
use guestmem::GuestMemory;
use guid::Guid;
use inspect::Inspect;
use inspect::InspectMut;
use parking_lot::Mutex;
use pci_core::capabilities::extended::sriov::SriovBarDecode;
use pci_core::capabilities::extended::sriov::SriovConfig;
use pci_core::capabilities::extended::sriov::SriovExtendedCapability;
use pci_core::capabilities::extended::sriov::VfBarConfig;
use pci_core::capabilities::msix::MsixEmulator;
use pci_core::capabilities::pci_express::PciExpressCapability;
use pci_core::cfg_space_emu::BarMemoryKind;
use pci_core::cfg_space_emu::ConfigSpaceType0Emulator;
use pci_core::cfg_space_emu::DeviceBars;
use pci_core::dma::DmaTarget;
use pci_core::spec::hwid::ClassCode;
use pci_core::spec::hwid::HardwareIds;
use pci_core::spec::hwid::ProgrammingInterface;
use pci_core::spec::hwid::Subclass;
use std::sync::Arc;
use std::task::Context;
use vf::NvmeVirtualFunction;
use vmcore::device_state::ChangeDeviceState;
use vmcore::save_restore::SaveError;
use vmcore::save_restore::SaveRestore;
use vmcore::save_restore::SavedStateNotSupported;
use vmcore::vm_task::VmTaskDriverSource;

/// An NVMe controller.
#[derive(InspectMut)]
pub struct NvmeController {
    cfg_space: ConfigSpaceType0Emulator,
    #[inspect(skip)]
    msix: MsixEmulator,

    #[inspect(flatten)]
    core: ControllerCore,

    // Inputs captured at construction, cloned to lazily build VFs in
    // `enable_vfs`. Stored unconditionally (even without SR-IOV) so VF
    // creation doesn't depend on threading them through `SriovState` — a
    // little wasteful for non-SR-IOV controllers, but simpler to reason
    // about.
    #[inspect(skip)]
    driver_source: VmTaskDriverSource,
    #[inspect(skip)]
    guest_memory: GuestMemory,
    #[inspect(skip)]
    msi_target: MsiTarget,
    #[inspect(display)]
    subsystem_id: Guid,

    // SR-IOV support — None when SR-IOV is not configured.
    sriov: Option<SriovState>,
    #[inspect(iter_by_index)]
    vfs: Vec<NvmeVirtualFunction>,
    /// Pending VF drain — set when VF_Enable is cleared, completed by
    /// `poll_device` when all VF IOs have drained. Stalls the VCPU that
    /// wrote VF_Enable=0 until drain is complete.
    #[inspect(with = "Option::is_some")]
    vf_drain: Option<VfDrainState>,
}

/// The common state machine shared by PF ([`super::NvmeController`]) and VF
/// ([`super::vf::NvmeVirtualFunction`]) NVMe controllers.
///
/// Both controllers have identical register layouts and an identical
/// [`NvmeWorkers`] coordinator; only their surrounding PCI/SR-IOV state
/// differs. Each embeds one of these and forwards BAR0 reads/writes to
/// [`ControllerCore::read_bar0`] / [`ControllerCore::write_bar0`].
#[derive(Inspect)]
pub(crate) struct ControllerCore {
    registers: RegState,
    #[inspect(skip)]
    qe_sizes: Arc<Mutex<IoQueueEntrySizes>>,
    #[inspect(flatten)]
    workers: NvmeWorkers,
}

/// NVMe controller register state.
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

/// NVMe CAP register value shared by PF and VF controllers.
const CAP: spec::Cap = spec::Cap::new()
    .with_dstrd(DOORBELL_STRIDE_BITS - 2)
    .with_mqes_z(MAX_QES - 1)
    .with_cqr(true)
    .with_css_nvm(true)
    .with_to(!0);

/// Internal SR-IOV state held by the PF.
#[derive(Inspect)]
struct SriovState {
    /// Shared VF BAR decode state — updated by the SR-IOV capability,
    /// read by the MMIO handler for VF address routing, and used to
    /// receive pending VF_Enable changes.
    #[inspect(skip)]
    bar_decode: Arc<SriovBarDecode>,
    /// SR-IOV configuration.
    #[inspect(flatten)]
    config: NvmeSriovCaps,
    /// Routing table mapping VF index to that VF's controller client.
    /// Populated by `enable_vfs`, cleared by `disable_vfs`; the PF admin
    /// handler uses it to route online/offline and namespace attach/detach.
    #[inspect(skip)]
    vf_clients: crate::workers::VfClientTable,
}

/// State for an in-progress VF drain operation.
///
/// When VF_Enable is cleared, VFs are moved here and their workers are
/// drained asynchronously via `poll_device`. When all VFs finish
/// draining, the `DeferredWrite` is completed to resume the stalled VCPU.
struct VfDrainState {
    /// The VFs being drained.
    vfs: Vec<NvmeVirtualFunction>,
    /// Completes the deferred config-space write when drain is done.
    deferred: DeferredWrite,
}

/// The NVMe controller's capabilities.
#[derive(Debug, Copy, Clone)]
pub struct NvmeControllerCaps {
    /// The number of entries in the MSI-X table.
    pub msix_count: u16,
    /// The maximum number of IO submission and completion queues.
    pub max_io_queues: u16,
    /// The subsystem ID, used as part of the subnqn field of the identify
    /// controller response.
    pub subsystem_id: Guid,
    /// Optional SR-IOV configuration. When set, the controller exposes an
    /// SR-IOV extended capability and can create VFs.
    pub sriov: Option<NvmeSriovCaps>,
}

/// SR-IOV configuration for the NVMe controller.
#[derive(Debug, Copy, Clone, Inspect)]
pub struct NvmeSriovCaps {
    /// Total number of VFs the PF can support (1..=7 without ARI).
    pub total_vfs: u16,
    /// Number of MSI-X vectors per VF.
    pub vf_msix_count: u16,
    /// Maximum number of IO queues per VF.
    pub vf_max_io_queues: u16,
}

impl NvmeController {
    /// Creates a new NVMe controller.
    pub fn new(
        driver_source: &VmTaskDriverSource,
        dma_target: &DmaTarget,
        register_mmio: &mut dyn RegisterMmioIntercept,
        caps: NvmeControllerCaps,
    ) -> Self {
        let msi_target = dma_target.msi_target();
        let guest_memory = dma_target.guest_memory().clone();
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

        // Build extended capabilities — add SR-IOV if configured.
        let (extended_caps, sriov, multi_function) = if let Some(sriov_caps) = caps.sriov {
            // Allocate one MMIO intercept region per VF BAR, each spanning
            // all VFs contiguously. The SR-IOV capability owns these and
            // maps/unmaps them directly when BAR/MSE/VF_Enable change.
            //
            // Only BAR0 (NVMe registers + doorbells) and BAR4 (MSI-X table)
            // are used; both are 64-bit prefetchable, so their upper halves
            // (BAR1/BAR5) are consumed by the 64-bit BARs.
            let vf_bar0_len = BAR0_LEN;
            let vf_bar4_len = Self::vf_msix_bar_size(sriov_caps.vf_msix_count);
            let total_vfs = sriov_caps.total_vfs as u64;

            let vf_bar = |size| {
                Some(VfBarConfig {
                    size,
                    is_64bit: true,
                    prefetchable: true,
                })
            };
            let mut vf_bar_cfg: [Option<VfBarConfig>; 6] = [None; 6];
            vf_bar_cfg[0] = vf_bar(vf_bar0_len);
            vf_bar_cfg[4] = vf_bar(vf_bar4_len);

            let mut vf_bars: [Option<BarMemoryKind>; 6] = Default::default();
            vf_bars[0] = Some(BarMemoryKind::Intercept(
                register_mmio.new_io_region("vf_bar0", total_vfs * vf_bar0_len),
            ));
            vf_bars[4] = Some(BarMemoryKind::Intercept(
                register_mmio.new_io_region("vf_msix", total_vfs * vf_bar4_len),
            ));

            // VFs start at function 1 with stride 1 (no ARI).
            // VFs use a distinct VF device ID (VF_DEVICE_ID), separate from
            // the PF's DEVICE_ID.
            let (sriov_cap, bar_decode) = SriovExtendedCapability::new(
                SriovConfig {
                    total_vfs: sriov_caps.total_vfs,
                    vf_device_id: VF_DEVICE_ID,
                    first_vf_offset: 1,
                    vf_stride: 1,
                    vf_bars: vf_bar_cfg,
                },
                vf_bars,
            );

            let extended_caps: Vec<
                Box<dyn pci_core::capabilities::extended::PciExtendedCapability>,
            > = vec![Box::new(sriov_cap)];

            // Routing table mapping VF index to that VF's controller client.
            // Empty until VFs are enabled.
            let vf_clients: crate::workers::VfClientTable = Arc::new(Mutex::new(Vec::new()));

            let state = SriovState {
                bar_decode,
                config: sriov_caps,
                vf_clients,
            };

            (extended_caps, Some(state), true)
        } else {
            (Vec::new(), None, false)
        };

        let mut cfg_space = ConfigSpaceType0Emulator::new(
            HardwareIds {
                vendor_id: VENDOR_ID,
                device_id: DEVICE_ID,
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
            extended_caps,
            bars,
        );

        if multi_function {
            cfg_space = cfg_space.with_multi_function_bit(true);
        }

        let interrupts = (0..caps.msix_count)
            .map(|i| msix.interrupt(i).unwrap())
            .collect();

        let qe_sizes = Arc::new(Default::default());
        let sriov_admin_config = sriov.as_ref().map(|s| crate::workers::SriovAdminConfig {
            total_vfs: s.config.total_vfs,
            vf_clients: s.vf_clients.clone(),
        });
        let controller_id = if sriov.is_some() {
            crate::workers::PF_CONTROLLER_ID
        } else {
            0
        };
        let admin = NvmeWorkers::new(
            driver_source,
            guest_memory.clone(),
            interrupts,
            caps.max_io_queues,
            caps.max_io_queues,
            Arc::clone(&qe_sizes),
            caps.subsystem_id,
            sriov_admin_config,
            controller_id,
            true, // PF (and standalone) controllers are always online
        );

        Self {
            cfg_space,
            msix,
            core: ControllerCore::new(qe_sizes, admin),
            driver_source: driver_source.clone(),
            guest_memory,
            msi_target: msi_target.clone(),
            subsystem_id: caps.subsystem_id,
            sriov,
            vfs: Vec::new(),
            vf_drain: None,
        }
    }

    /// Compute the size of a VF's MSI-X BAR (BAR4).
    ///
    /// Each vector needs 16 bytes for its table entry plus a pending-bits
    /// array. Round up to a power of two and at least `PAGE_SIZE` so each VF
    /// gets its own page in the contiguous MMIO region.
    fn vf_msix_bar_size(vf_msix_count: u16) -> u64 {
        let table_size = vf_msix_count as u64 * 16;
        let pending_bits_size = vf_msix_count.div_ceil(32) as u64 * 4;
        (table_size + pending_bits_size)
            .next_power_of_two()
            .max(PAGE_SIZE as u64)
    }

    /// Returns a client for manipulating the NVMe controller at runtime.
    pub fn client(&self) -> NvmeControllerClient {
        self.core.workers.client()
    }

    /// Reads from the virtual BAR 0.
    pub fn read_bar0(&mut self, addr: u64, data: &mut [u8]) -> IoResult {
        self.core.read_bar0(addr, data)
    }

    /// Writes to the virtual BAR 0.
    pub fn write_bar0(&mut self, addr: u64, data: &[u8]) -> IoResult {
        self.core.write_bar0(addr, data)
    }

    /// Enable VFs by creating VF instances.
    fn enable_vfs(&mut self, num_vfs: u16) {
        let sriov = self.sriov.as_ref().expect("SR-IOV must be configured");
        let config = &sriov.config;

        // VFs must already be disabled and drained before enabling. The
        // caller (`drain_sriov_pending`) only invokes this on a VF_Enable
        // 0->1 transition, and refuses to re-enable while a drain is in
        // progress, so `vfs` is always empty here.
        assert!(
            self.vfs.is_empty(),
            "enable_vfs called with {} VFs still present",
            self.vfs.len()
        );
        self.vfs.reserve(num_vfs as usize);

        for i in 0..num_vfs {
            // VF function number: first_vf_offset + i * vf_stride.
            // With offset=1, stride=1, VFs are at functions 1, 2, 3, ...
            let vf_devfn = 1 + i as u8; // first_vf_offset=1, vf_stride=1
            let vf_msi_target = self.msi_target.with_devfn(vf_devfn);
            let vf_index = i;
            let cntlid = crate::workers::PF_CONTROLLER_ID + 1 + vf_index;

            let vf = NvmeVirtualFunction::new(vf::NvmeVirtualFunctionParams {
                msix_count: config.vf_msix_count,
                max_io_queues: config.vf_max_io_queues,
                msi_target: &vf_msi_target,
                driver_source: self.driver_source.clone(),
                guest_memory: self.guest_memory.clone(),
                subsystem_id: self.subsystem_id,
                vf_index,
                cntlid,
            });
            self.vfs.push(vf);
        }

        // Populate the routing table so the PF admin handler can deliver
        // online/offline and namespace attach/detach messages to each VF.
        let clients: Vec<NvmeControllerClient> = self.vfs.iter().map(|vf| vf.client()).collect();
        *sriov.vf_clients.lock() = clients;

        tracing::info!(num_vfs, "SR-IOV: enabled VFs");
    }

    /// Disable all VFs.
    ///
    /// Unmaps MMIO intercepts so VFs stop receiving new work, initiates
    /// controller resets, and returns the VFs for async draining.
    fn disable_vfs(&mut self) -> Vec<NvmeVirtualFunction> {
        let mut draining = Vec::new();
        for mut vf in self.vfs.drain(..) {
            vf.initiate_reset();
            draining.push(vf);
        }
        // Clear the routing table; these VFs are going away.
        if let Some(sriov) = &self.sriov {
            sriov.vf_clients.lock().clear();
        }
        tracing::info!(draining = draining.len(), "SR-IOV: disabled VFs");
        draining
    }

    /// Drain any pending SR-IOV callbacks and handle VF lifecycle / BAR changes.
    ///
    /// Returns `Some(IoResult::Defer(...))` if VF_Enable was cleared and the
    /// config write must stall until VF IOs drain.
    fn drain_sriov_pending(&mut self) -> Option<IoResult> {
        let change = self.sriov.as_ref()?.bar_decode.take_pending_vf_change()?;

        if change.enabled {
            // Don't allow VF_Enable=1 while a drain is in progress.
            if self.vf_drain.is_some() {
                tracelimit::warn_ratelimited!(
                    "SR-IOV: ignoring VF_Enable=1 while VF drain is in progress"
                );
            } else {
                self.enable_vfs(change.num_vfs);
            }
        } else {
            let draining_vfs = self.disable_vfs();
            if !draining_vfs.is_empty() {
                let (deferred, token) = defer_write();
                self.vf_drain = Some(VfDrainState {
                    vfs: draining_vfs,
                    deferred,
                });
                return Some(IoResult::Defer(token));
            }
        }

        None
    }

    /// Compute VF index from a PCI function number.
    /// Returns `None` if the function does not correspond to a valid,
    /// enabled VF.
    fn vf_index_from_function(
        &self,
        secondary_bus: u8,
        target_bus: u8,
        function: u8,
    ) -> Option<usize> {
        if secondary_bus == target_bus && function == 0 {
            return None; // Function 0 is the PF.
        }
        if secondary_bus != target_bus {
            // TODO: ARI support.
            return None;
        }
        // first_vf_offset = 1, vf_stride = 1
        let idx = function.checked_sub(1)? as usize;
        if idx < self.vfs.len() {
            Some(idx)
        } else {
            None
        }
    }
}

impl ControllerCore {
    pub fn new(qe_sizes: Arc<Mutex<IoQueueEntrySizes>>, workers: NvmeWorkers) -> Self {
        Self {
            registers: RegState::new(),
            qe_sizes,
            workers,
        }
    }
}

/// Read from NVMe BAR0 (controller registers).
impl ControllerCore {
    fn read_bar0(&mut self, addr: u64, data: &mut [u8]) -> IoResult {
        if data.len() < 4 {
            return IoResult::Err(IoError::InvalidAccessSize);
        }
        if addr & (data.len() as u64 - 1) != 0 {
            return IoResult::Err(IoError::UnalignedAccess);
        }

        // Check for 64-bit registers.
        let d: Option<u64> = match spec::Register(addr & !7) {
            spec::Register::CAP => Some(CAP.into()),
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

    /// Write to NVMe BAR0 (controller registers + doorbells).
    fn write_bar0(&mut self, addr: u64, data: &[u8]) -> IoResult {
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
            let db_id = match u16::try_from(db_id) {
                Ok(id) => id,
                Err(_) => return IoResult::Err(InvalidRegister),
            };
            self.workers.doorbell(db_id, value);
            return IoResult::Ok;
        }

        if data.len() < 4 {
            return IoResult::Err(IoError::InvalidAccessSize);
        }
        if addr & (data.len() as u64 - 1) != 0 {
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
            match self.workers.poll_enabled() {
                EnablePoll::Enabled => self.registers.csts.set_rdy(true),
                EnablePoll::Pending => {}
                EnablePoll::Rejected => {
                    // The controller is offline (an SR-IOV VF whose secondary
                    // controller is not online). Per NVMe Base 2.1 §8.2.6.3, a
                    // secondary controller "is able to be enabled only when in
                    // the Online state"; in the Offline state "CSTS.CFS shall
                    // be set to '1'". So set CFS and never reach RDY. The PF is
                    // always online, so it never lands here; if it somehow did,
                    // CFS makes the failure observable rather than hanging
                    // silently.
                    tracelimit::warn_ratelimited!("controller enable rejected: offline");
                    self.fatal_error();
                }
            }
        }

        let csts = self.registers.csts;
        tracing::debug!(?csts, "get csts");
        csts.into()
    }

    /// Sets the CFS bit in the controller status register (CSTS), indicating
    /// that the controller has experienced "undefined" behavior.
    fn fatal_error(&mut self) {
        self.registers.csts.set_cfs(true);
    }

    /// Initiates a controller reset and resets the register state.
    ///
    /// If the controller is enabled, this kicks off a controller reset on the
    /// workers; the register state is reset to its initial values immediately.
    /// This does **not** wait for in-flight IO to complete — the caller must
    /// follow up with [`ControllerCore::drain`] (async) or
    /// [`ControllerCore::poll_drain`] (non-blocking) before the controller is
    /// dropped, so that IOs holding guest memory references finish first.
    fn initiate_reset(&mut self) {
        // BUGBUG: this EnableStateKind only exists for this check, seems dumb.
        if self.workers.enable_state() == EnableStateKind::Enabled {
            self.workers.controller_reset();
        }
        self.registers = RegState::new();
        *self.qe_sizes.lock() = Default::default();
    }

    /// Drives the workers to the disabled state from whatever state they are
    /// in, awaiting any in-flight IO. Must be preceded by
    /// [`ControllerCore::initiate_reset`], which resets the register state.
    async fn drain(&mut self) {
        self.workers.reset().await;
    }

    /// Non-blocking poll for drain completion. Returns `true` once the workers
    /// have reached the disabled state.
    ///
    /// Registers `cx.waker()` with the underlying channel so the caller is
    /// woken when the drain makes progress.
    fn poll_drain(&mut self, cx: &mut Context<'_>) -> bool {
        self.workers.poll_drain(cx)
    }
}

impl ChangeDeviceState for NvmeController {
    fn start(&mut self) {}

    async fn stop(&mut self) {
        // Drain any pending VF drain — this can happen if the device is
        // stopped while a VF_Enable=0 write is being processed.
        if let Some(mut drain) = self.vf_drain.take() {
            join_all(drain.vfs.iter_mut().map(|vf| vf.drain())).await;
            drain.deferred.complete();
        }
    }

    async fn reset(&mut self) {
        let Self {
            cfg_space,
            msix: _,
            core,
            driver_source: _,
            guest_memory: _,
            msi_target: _,
            subsystem_id: _,
            sriov,
            vfs,
            vf_drain,
        } = self;
        // Initiate reset on all active VFs, then drain them concurrently.
        for vf in vfs.iter_mut() {
            vf.initiate_reset();
        }
        join_all(vfs.iter_mut().map(|vf| vf.drain())).await;
        // Drain any pending VF drain from a VF_Enable=0 write.
        if let Some(mut drain) = vf_drain.take() {
            join_all(drain.vfs.iter_mut().map(|vf| vf.drain())).await;
            drain.deferred.complete();
        }

        // Reset the PF's own controller core: kick off the reset (resetting
        // the register state) and drain in-flight IO.
        core.initiate_reset();
        core.drain().await;
        cfg_space.reset();
        // cfg_space.reset() resets the SR-IOV capability, which unmaps
        // all VF MMIO intercepts. VFs were already drained above.
        if let Some(sriov) = sriov {
            sriov.vf_clients.lock().clear();
        }
        vfs.clear();
    }
}

impl ChipsetDevice for NvmeController {
    fn supports_mmio(&mut self) -> Option<&mut dyn MmioIntercept> {
        Some(self)
    }

    fn supports_pci(&mut self) -> Option<&mut dyn PciConfigSpace> {
        Some(self)
    }

    fn supports_poll_device(&mut self) -> Option<&mut dyn PollDevice> {
        Some(self)
    }
}

impl PollDevice for NvmeController {
    fn poll_device(&mut self, cx: &mut Context<'_>) {
        if let Some(drain) = &mut self.vf_drain {
            // Poll every VF — must not short-circuit so that each VF
            // registers cx.waker() with its underlying mesh channel.
            let mut all_drained = true;
            for vf in &mut drain.vfs {
                all_drained &= vf.poll_drain(cx);
            }
            if all_drained {
                let drain = self.vf_drain.take().unwrap();
                drain.deferred.complete();
                tracing::debug!("SR-IOV: VF drain complete, VCPU resumed");
            }
        }
    }
}

impl MmioIntercept for NvmeController {
    fn mmio_read(&mut self, addr: u64, data: &mut [u8]) -> IoResult {
        // Check PF BARs first.
        match self.cfg_space.find_bar(addr) {
            Some((0, offset)) => return self.read_bar0(offset, data),
            Some((4, offset)) => {
                read_as_u32_chunks(offset, data, |offset| self.msix.read_u32(offset));
                return IoResult::Ok;
            }
            _ => {}
        }

        // Check VF BARs using the shared address decode.
        if let Some(sriov) = &self.sriov {
            if let Some((vf_idx, offset)) = sriov.bar_decode.decode(0, addr) {
                if let Some(vf) = self.vfs.get_mut(vf_idx) {
                    return vf.read_bar0(offset, data);
                }
            }
            if let Some((vf_idx, offset)) = sriov.bar_decode.decode(4, addr) {
                if let Some(vf) = self.vfs.get_mut(vf_idx) {
                    return vf.read_msix(offset, data);
                }
            }
        }

        IoResult::Err(InvalidRegister)
    }

    fn mmio_write(&mut self, addr: u64, data: &[u8]) -> IoResult {
        // Check PF BARs first.
        match self.cfg_space.find_bar(addr) {
            Some((0, offset)) => return self.write_bar0(offset, data),
            Some((4, offset)) => {
                write_as_u32_chunks(offset, data, |offset, ty| match ty {
                    ReadWriteRequestType::Read => Some(self.msix.read_u32(offset)),
                    ReadWriteRequestType::Write(val) => {
                        self.msix.write_u32(offset, val);
                        None
                    }
                });
                return IoResult::Ok;
            }
            _ => {}
        }

        // Check VF BARs using the shared address decode.
        if let Some(sriov) = &self.sriov {
            if let Some((vf_idx, offset)) = sriov.bar_decode.decode(0, addr) {
                if let Some(vf) = self.vfs.get_mut(vf_idx) {
                    return vf.write_bar0(offset, data);
                }
            }
            if let Some((vf_idx, offset)) = sriov.bar_decode.decode(4, addr) {
                if let Some(vf) = self.vfs.get_mut(vf_idx) {
                    return vf.write_msix(offset, data);
                }
            }
        }

        IoResult::Err(InvalidRegister)
    }
}

impl PciConfigSpace for NvmeController {
    fn pci_cfg_read(&mut self, offset: u16, value: &mut u32) -> IoResult {
        self.cfg_space.read_u32(offset, value)
    }

    fn pci_cfg_write(&mut self, offset: u16, value: u32) -> IoResult {
        let result = self.cfg_space.write_u32(offset, value);
        if let Some(defer) = self.drain_sriov_pending() {
            return defer;
        }
        result
    }

    fn pci_cfg_read_with_routing(
        &mut self,
        secondary_bus: u8,
        target_bus: u8,
        function: u8,
        offset: u16,
        value: &mut u32,
    ) -> IoResult {
        if secondary_bus == target_bus && function == 0 {
            return self.pci_cfg_read(offset, value);
        }

        match self.vf_index_from_function(secondary_bus, target_bus, function) {
            Some(idx) => self.vfs[idx].pci_cfg_read(offset, value),
            None => {
                *value = !0; // No device present.
                IoResult::Ok
            }
        }
    }

    fn pci_cfg_write_with_routing(
        &mut self,
        secondary_bus: u8,
        target_bus: u8,
        function: u8,
        offset: u16,
        value: u32,
    ) -> IoResult {
        if secondary_bus == target_bus && function == 0 {
            return self.pci_cfg_write(offset, value);
        }

        match self.vf_index_from_function(secondary_bus, target_bus, function) {
            Some(idx) => self.vfs[idx].pci_cfg_write(offset, value),
            None => IoResult::Ok, // Silently drop writes to absent functions.
        }
    }
}

impl SaveRestore for NvmeController {
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
