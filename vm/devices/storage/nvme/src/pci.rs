// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! The NVMe PCI device implementation.

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
use crate::spec;
use crate::vf::NvmeVirtualFunction;
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
use guid::Guid;
use inspect::Inspect;
use inspect::InspectMut;
use parking_lot::Mutex;
use pci_core::capabilities::extended::sriov::SriovCallback;
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

    registers: RegState,
    #[inspect(skip)]
    qe_sizes: Arc<Mutex<IoQueueEntrySizes>>,
    #[inspect(flatten, mut)]
    workers: NvmeWorkers,

    // SR-IOV support — None when SR-IOV is not configured.
    #[inspect(skip)]
    sriov: Option<SriovState>,
    #[inspect(iter_by_index)]
    vfs: Vec<Option<NvmeVirtualFunction>>,
}

/// Internal SR-IOV state held by the PF.
struct SriovState {
    /// Shared callback state — receives VF Enable change notifications
    /// from the SR-IOV extended capability during config space writes.
    callback: Arc<SriovCallbackState>,
    /// PF's MSI target, cloned per-VF with different devfn.
    msi_target: MsiTarget,
    /// SR-IOV configuration.
    config: NvmeSriovCaps,
}

/// Shared state between the SR-IOV callback and the NvmeController.
/// The callback writes a pending change, and the controller drains it
/// after each config space write completes.
struct SriovCallbackState {
    pending: Mutex<Option<SriovPendingChange>>,
}

struct SriovPendingChange {
    enabled: bool,
    num_vfs: u16,
}

impl SriovCallback for SriovCallbackState {
    fn vf_enable_changed(&self, enabled: bool, num_vfs: u16) {
        *self.pending.lock() = Some(SriovPendingChange { enabled, num_vfs });
    }
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
#[derive(Debug, Copy, Clone)]
pub struct NvmeSriovCaps {
    /// Total number of VFs the PF can support (1..=7 without ARI).
    pub total_vfs: u16,
    /// PCI Device ID to report for all VFs.
    pub vf_device_id: u16,
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
            let callback = Arc::new(SriovCallbackState {
                pending: Mutex::new(None),
            });

            // VFs start at function 1 with stride 1 (no ARI).
            let sriov_cap = SriovExtendedCapability::new(
                SriovConfig {
                    total_vfs: sriov_caps.total_vfs,
                    vf_device_id: sriov_caps.vf_device_id,
                    first_vf_offset: 1,
                    vf_stride: 1,
                    vf_bars: Self::vf_bar_config(sriov_caps.vf_msix_count),
                },
                Some(callback.clone()),
            );

            let extended_caps: Vec<
                Box<dyn pci_core::capabilities::extended::PciExtendedCapability>,
            > = vec![Box::new(sriov_cap)];

            let state = SriovState {
                callback,
                msi_target: msi_target.clone(),
                config: sriov_caps,
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
        let admin = NvmeWorkers::new(
            driver_source,
            guest_memory,
            interrupts,
            caps.max_io_queues,
            caps.max_io_queues,
            Arc::clone(&qe_sizes),
            caps.subsystem_id,
        );

        Self {
            cfg_space,
            msix,
            registers: RegState::new(),
            workers: admin,
            qe_sizes,
            sriov,
            vfs: Vec::new(),
        }
    }

    /// Build VF BAR configuration for the SR-IOV capability.
    fn vf_bar_config(vf_msix_count: u16) -> [Option<VfBarConfig>; 6] {
        // Compute MSI-X BAR size: each vector needs 16 bytes for the table
        // entry, plus pending bits array. Round up to power of 2 and at least
        // PAGE_SIZE so that each VF gets its own page in the MMIO region.
        let msix_table_size = vf_msix_count as u64 * 16;
        let pending_bits_size = (vf_msix_count.div_ceil(32)) as u64 * 4;
        let raw_msix_bar_size = msix_table_size + pending_bits_size;
        let msix_bar_size = raw_msix_bar_size.next_power_of_two().max(PAGE_SIZE as u64);

        let mut vf_bars: [Option<VfBarConfig>; 6] = [None; 6];
        // VF BAR0: NVMe registers + doorbells (64-bit, prefetchable).
        vf_bars[0] = Some(VfBarConfig {
            size: BAR0_LEN,
            is_64bit: true,
            prefetchable: true,
        });
        // VF BAR1 consumed by 64-bit BAR0.
        // VF BAR2: unused.
        // VF BAR3: unused.
        // VF BAR4: MSI-X table (64-bit, prefetchable).
        vf_bars[4] = Some(VfBarConfig {
            size: msix_bar_size,
            is_64bit: true,
            prefetchable: true,
        });
        // VF BAR5 consumed by 64-bit BAR4.
        vf_bars
    }

    /// Returns a client for manipulating the NVMe controller at runtime.
    pub fn client(&self) -> NvmeControllerClient {
        self.workers.client()
    }

    /// Reads from the virtual BAR 0.
    pub fn read_bar0(&mut self, addr: u64, data: &mut [u8]) -> IoResult {
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

    /// Writes to the virtual BAR 0.
    pub fn write_bar0(&mut self, addr: u64, data: &[u8]) -> IoResult {
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

    /// Enable VFs by creating VF instances.
    fn enable_vfs(&mut self, num_vfs: u16) {
        let sriov = self.sriov.as_ref().expect("SR-IOV must be configured");
        let config = &sriov.config;

        self.vfs.clear();
        self.vfs.reserve(num_vfs as usize);

        for i in 0..num_vfs {
            // VF function number: first_vf_offset + i * vf_stride.
            // With offset=1, stride=1, VFs are at functions 1, 2, 3, ...
            let vf_devfn = 1 + i as u8; // first_vf_offset=1, vf_stride=1
            let vf_msi_target = sriov.msi_target.with_devfn(vf_devfn);

            let vf =
                NvmeVirtualFunction::new(config.vf_device_id, config.vf_msix_count, &vf_msi_target);
            self.vfs.push(Some(vf));
        }

        tracing::info!(num_vfs, "SR-IOV: enabled VFs");
    }

    /// Disable all VFs.
    fn disable_vfs(&mut self) {
        let count = self.vfs.len();
        self.vfs.clear();
        tracing::info!(count, "SR-IOV: disabled VFs");
    }

    /// Drain any pending SR-IOV callback and handle VF lifecycle.
    fn drain_sriov_pending(&mut self) {
        let pending = self
            .sriov
            .as_ref()
            .and_then(|s| s.callback.pending.lock().take());

        if let Some(change) = pending {
            if change.enabled {
                self.enable_vfs(change.num_vfs);
            } else {
                self.disable_vfs();
            }
        }
    }

    /// Compute VF index from a PCI function number.
    /// Returns `None` if the function does not correspond to a valid,
    /// enabled VF.
    fn vf_index_from_function(&self, function: u8) -> Option<usize> {
        if function == 0 {
            return None; // Function 0 is the PF.
        }
        // first_vf_offset = 1, vf_stride = 1
        let idx = function.checked_sub(1)? as usize;
        if idx < self.vfs.len() && self.vfs[idx].is_some() {
            Some(idx)
        } else {
            None
        }
    }
}

impl ChangeDeviceState for NvmeController {
    fn start(&mut self) {}

    async fn stop(&mut self) {}

    async fn reset(&mut self) {
        let Self {
            cfg_space,
            msix: _,
            registers,
            qe_sizes,
            workers,
            sriov: _,
            vfs,
        } = self;
        workers.reset().await;
        cfg_space.reset();
        *registers = RegState::new();
        *qe_sizes.lock() = Default::default();
        // cfg_space.reset() will reset the SR-IOV capability, which fires
        // the callback to disable VFs. Drain it.
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
}

impl MmioIntercept for NvmeController {
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

impl PciConfigSpace for NvmeController {
    fn pci_cfg_read(&mut self, offset: u16, value: &mut u32) -> IoResult {
        self.cfg_space.read_u32(offset, value)
    }

    fn pci_cfg_write(&mut self, offset: u16, value: u32) -> IoResult {
        let result = self.cfg_space.write_u32(offset, value);
        self.drain_sriov_pending();
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
        if secondary_bus != target_bus {
            *value = !0;
            return IoResult::Ok;
        }

        if function == 0 {
            return self.pci_cfg_read(offset, value);
        }

        match self.vf_index_from_function(function) {
            Some(idx) => self.vfs[idx].as_mut().unwrap().pci_cfg_read(offset, value),
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
        if secondary_bus != target_bus {
            return IoResult::Ok;
        }

        if function == 0 {
            return self.pci_cfg_write(offset, value);
        }

        match self.vf_index_from_function(function) {
            Some(idx) => self.vfs[idx].as_mut().unwrap().pci_cfg_write(offset, value),
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
