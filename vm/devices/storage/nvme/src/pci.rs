// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! The NVMe PCI device implementation.

use crate::BAR0_LEN;
use crate::DEVICE_ID;
use crate::NvmeControllerClient;
use crate::PAGE_SIZE;
use crate::VENDOR_ID;
use crate::VF_DEVICE_ID;
use crate::registers::ControllerCore;
use crate::registers::RegState;
use crate::vf::NvmeVirtualFunction;
use crate::workers::NvmeWorkers;
use chipset_device::ChipsetDevice;
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

    // SR-IOV support — None when SR-IOV is not configured.
    #[inspect(skip)]
    sriov: Option<SriovState>,
    #[inspect(iter_by_index)]
    vfs: Vec<NvmeVirtualFunction>,
    /// Pending VF drain — set when VF_Enable is cleared, completed by
    /// `poll_device` when all VF IOs have drained. Stalls the VCPU that
    /// wrote VF_Enable=0 until drain is complete.
    #[inspect(skip)]
    vf_drain: Option<VfDrainState>,
}

/// Internal SR-IOV state held by the PF.
struct SriovState {
    /// Shared VF BAR decode state — updated by the SR-IOV capability,
    /// read by the MMIO handler for VF address routing, and used to
    /// receive pending VF_Enable changes.
    bar_decode: Arc<SriovBarDecode>,
    /// PF's MSI target, cloned per-VF with different devfn.
    msi_target: MsiTarget,
    /// SR-IOV configuration.
    config: NvmeSriovCaps,
    /// Routing table mapping VF index to that VF's controller client.
    /// Populated by `enable_vfs`, cleared by `disable_vfs`; the PF admin
    /// handler uses it to route online/offline and namespace attach/detach.
    vf_clients: crate::workers::VfClientTable,
    /// Driver source for creating VF workers.
    driver_source: VmTaskDriverSource,
    /// Guest memory for VF DMA.
    guest_memory: GuestMemory,
    /// Subsystem ID for VF NVMe identity.
    subsystem_id: Guid,
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
#[derive(Debug, Copy, Clone)]
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
            let vf_bar_cfg = Self::vf_bar_config(sriov_caps.vf_msix_count);
            let vf_bar0_len = vf_bar_cfg[0].as_ref().expect("VF BAR0 is always set").size;
            let vf_bar4_len = vf_bar_cfg[4].as_ref().expect("VF BAR4 is always set").size;
            let total_vfs = sriov_caps.total_vfs as u64;

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
                msi_target: msi_target.clone(),
                config: sriov_caps,
                vf_clients,
                driver_source: driver_source.clone(),
                guest_memory: guest_memory.clone(),
                subsystem_id: caps.subsystem_id,
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
            guest_memory,
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
            sriov,
            vfs: Vec::new(),
            vf_drain: None,
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

    /// Sets the CFS bit in the controller status register (CSTS), indicating
    /// that the controller has experienced "undefined" behavior.
    pub fn fatal_error(&mut self) {
        self.core.registers.csts.set_cfs(true);
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
            let vf_msi_target = sriov.msi_target.with_devfn(vf_devfn);
            let vf_index = i;
            let cntlid = crate::workers::PF_CONTROLLER_ID + 1 + vf_index;

            let vf = NvmeVirtualFunction::new(crate::vf::NvmeVirtualFunctionParams {
                msix_count: config.vf_msix_count,
                max_io_queues: config.vf_max_io_queues,
                msi_target: &vf_msi_target,
                driver_source: sriov.driver_source.clone(),
                guest_memory: sriov.guest_memory.clone(),
                subsystem_id: sriov.subsystem_id,
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
    fn vf_index_from_function(&self, function: u8) -> Option<usize> {
        if function == 0 {
            return None; // Function 0 is the PF.
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

        core.workers.reset().await;
        cfg_space.reset();
        core.registers = RegState::new();
        *core.qe_sizes.lock() = Default::default();
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
                tracing::info!("SR-IOV: VF drain complete, VCPU resumed");
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
        if secondary_bus != target_bus {
            *value = !0;
            return IoResult::Ok;
        }

        if function == 0 {
            return self.pci_cfg_read(offset, value);
        }

        match self.vf_index_from_function(function) {
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
        if secondary_bus != target_bus {
            return IoResult::Ok;
        }

        if function == 0 {
            return self.pci_cfg_write(offset, value);
        }

        match self.vf_index_from_function(function) {
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
