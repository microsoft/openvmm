// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! PCI transport for virtio devices

use self::capabilities::*;
use super::StalledIo;
use super::core::TransportOps;
use super::core::VirtioTransportCore;
use super::task::ConfigReadCompletion;
use super::task::defer_config_read;
use super::task::defer_config_write;
use crate::DynVirtioDevice;
use crate::MAX_QUEUE_SIZE;
use crate::spec::VirtioDeviceType;
use crate::spec::pci::VIRTIO_PCI_COMMON_CFG_SIZE;
use crate::spec::pci::VIRTIO_PCI_DEVICE_ID_BASE;
use crate::spec::pci::VIRTIO_VENDOR_ID;
use crate::spec::pci::VirtioPciCapType;
use crate::spec::pci::VirtioPciCommonCfg;
use chipset_device::ChipsetDevice;
use chipset_device::io::IoError;
use chipset_device::io::IoResult;
use chipset_device::io::deferred::defer_read;
use chipset_device::io::deferred::defer_write;
use chipset_device::mmio::MmioIntercept;
use chipset_device::mmio::RegisterMmioIntercept;
use chipset_device::pci::ByteEnabledDwordRead;
use chipset_device::pci::ByteEnabledDwordWrite;
use chipset_device::pci::PciConfigSpace;
use chipset_device::poll_device::PollDevice;
use device_emulators::ReadWriteRequestType;
use device_emulators::read_as_u32_chunks;
use device_emulators::write_as_u32_chunks;
use guestmem::DoorbellRegistration;
use guestmem::GuestMemory;
use guestmem::MemoryMapper;
use inspect::Inspect;
use inspect::InspectMut;
use pal_async::task::Spawn;
use parking_lot::Mutex;
use pci_core::PciInterruptPin;
use pci_core::capabilities::PciCapability;
use pci_core::capabilities::ReadOnlyCapability;
use pci_core::capabilities::msix::MsixEmulator;
use pci_core::cfg_space_emu::BarMemoryKind;
use pci_core::cfg_space_emu::ConfigSpaceType0Emulator;
use pci_core::cfg_space_emu::DeviceBars;
use pci_core::cfg_space_emu::IntxInterrupt;
use pci_core::msi::MsiTarget;
use pci_core::spec::caps::COMMON_HEADER_END;
use pci_core::spec::caps::CapabilityId;
use pci_core::spec::hwid::ClassCode;
use pci_core::spec::hwid::HardwareIds;
use pci_core::spec::hwid::ProgrammingInterface;
use pci_core::spec::hwid::Subclass;
use std::io;
use std::sync::Arc;
use vmcore::device_state::ChangeDeviceState;
use vmcore::interrupt::Interrupt;
use vmcore::line_interrupt::LineInterrupt;
use vmcore::save_restore::RestoreError;
use vmcore::save_restore::SaveError;

/// What kind of PCI interrupts [`VirtioPciDevice`] should use.
pub enum PciInterruptModel<'a> {
    Msix(&'a MsiTarget),
    IntX(PciInterruptPin, LineInterrupt),
}

enum InterruptKind {
    Msix(MsixEmulator),
    IntX(Arc<IntxInterrupt>),
}

/// BAR0 layout: common cfg is at offset 0, followed by notify, ISR, and
/// device-specific config regions.
const BAR0_NOTIFY_OFFSET: u16 = VIRTIO_PCI_COMMON_CFG_SIZE;
const BAR0_NOTIFY_SIZE: u16 = 4;
const BAR0_ISR_OFFSET: u16 = BAR0_NOTIFY_OFFSET + BAR0_NOTIFY_SIZE;
const BAR0_ISR_SIZE: u16 = 4;
const BAR0_DEVICE_CFG_OFFSET: u16 = BAR0_ISR_OFFSET + BAR0_ISR_SIZE;

/// Map a virtio device type to its PCI class/subclass.
///
/// The virtio spec does not require a particular class code — drivers bind on
/// vendor/device ID — but reporting an accurate class lets the guest OS
/// categorize the device correctly.
fn virtio_class_code(device_id: VirtioDeviceType) -> (ClassCode, Subclass) {
    match device_id {
        VirtioDeviceType::NET => (
            ClassCode::NETWORK_CONTROLLER,
            Subclass::NETWORK_CONTROLLER_ETHERNET,
        ),
        VirtioDeviceType::BLK => (
            ClassCode::MASS_STORAGE_CONTROLLER,
            Subclass::MASS_STORAGE_CONTROLLER_SCSI,
        ),
        VirtioDeviceType::CONSOLE => (
            ClassCode::SIMPLE_COMMUNICATION_CONTROLLER,
            Subclass::SIMPLE_COMMUNICATION_CONTROLLER_OTHER,
        ),
        // These device types have no well-established class code; report a
        // generic base system peripheral.
        VirtioDeviceType::RNG
        | VirtioDeviceType::P9
        | VirtioDeviceType::VSOCK
        | VirtioDeviceType::FS
        | VirtioDeviceType::PMEM => (
            ClassCode::BASE_SYSTEM_PERIPHERAL,
            Subclass::BASE_SYSTEM_PERIPHERAL_OTHER,
        ),
        _ => {
            tracelimit::warn_ratelimited!(
                device_id = device_id.0,
                "unknown virtio device type; reporting generic class code"
            );
            (
                ClassCode::BASE_SYSTEM_PERIPHERAL,
                Subclass::BASE_SYSTEM_PERIPHERAL_OTHER,
            )
        }
    }
}

/// PCI-specific transport state.
#[derive(Inspect)]
struct PciTransport {
    config_space: ConfigSpaceType0Emulator,
    #[inspect(skip)]
    interrupt_kind: InterruptKind,
    #[inspect(skip)]
    interrupt_status: Arc<Mutex<u32>>,
    msix_config_vector: u16,
    #[inspect(hex)]
    shared_memory_size: u64,
    /// Shared window state for the `VIRTIO_PCI_CAP_PCI_CFG` capability.
    #[inspect(skip)]
    pci_cfg_access: Arc<Mutex<PciCfgAccessState>>,
    /// Config-space offset of the `pci_cfg_data` window.
    #[inspect(hex)]
    pci_cfg_data_offset: u16,
    /// Length of the device-specific config region within BAR0, used to
    /// bound `pci_cfg_data` accesses.
    #[inspect(hex)]
    device_register_length: u32,
}

impl TransportOps for PciTransport {
    fn create_queue_interrupt(&mut self, _idx: usize, msix_vector: u16) -> Interrupt {
        match &self.interrupt_kind {
            InterruptKind::Msix(msix) => {
                if let Some(interrupt) = msix.interrupt(msix_vector) {
                    interrupt
                } else {
                    tracelimit::warn_ratelimited!(msix_vector, "invalid MSIx vector specified");
                    Interrupt::null()
                }
            }
            InterruptKind::IntX(line) => {
                let interrupt_status = self.interrupt_status.clone();
                let line = line.clone();
                Interrupt::from_fn(move || {
                    *interrupt_status.lock() |= 1;
                    line.set_level(true);
                })
            }
        }
    }

    fn signal_config_change(&mut self) {
        *self.interrupt_status.lock() |= 2;
        match &self.interrupt_kind {
            InterruptKind::Msix(msix) => {
                if let Some(interrupt) = msix.interrupt(self.msix_config_vector) {
                    interrupt.deliver();
                }
            }
            InterruptKind::IntX(line) => line.set_level(true),
        }
    }

    fn reset_interrupts(&mut self) {
        *self.interrupt_status.lock() = 0;
        if let InterruptKind::IntX(line) = &self.interrupt_kind {
            line.set_level(false);
        }
        self.msix_config_vector = 0;
    }

    fn doorbell_region(&mut self) -> Option<(u64, u32)> {
        self.config_space
            .bar_address(0)
            .map(|base| (base + BAR0_NOTIFY_OFFSET as u64, 2))
    }
}

/// Run a virtio device over PCI
#[derive(InspectMut)]
pub struct VirtioPciDevice {
    #[inspect(flatten)]
    core: VirtioTransportCore,
    #[inspect(flatten)]
    pci: PciTransport,
}

impl VirtioPciDevice {
    pub fn new(
        mut device: Box<dyn DynVirtioDevice>,
        driver: &impl Spawn,
        guest_memory: GuestMemory,
        interrupt_model: PciInterruptModel<'_>,
        doorbell_registration: Option<Arc<dyn DoorbellRegistration>>,
        mmio_registration: &mut dyn RegisterMmioIntercept,
        shared_mem_mapper: Option<&dyn MemoryMapper>,
    ) -> io::Result<Self> {
        let traits = device.traits();

        let (base_class, sub_class) = virtio_class_code(traits.device_id);
        let hardware_ids = HardwareIds {
            vendor_id: VIRTIO_VENDOR_ID,
            device_id: VIRTIO_PCI_DEVICE_ID_BASE + traits.device_id.0,
            revision_id: 1,
            prog_if: ProgrammingInterface::NONE,
            base_class,
            sub_class,
            type0_sub_vendor_id: pci_core::microsoft::VENDOR_ID,
            type0_sub_system_id: pci_core::microsoft::DEFAULT_SUBSYSTEM_ID,
        };

        let mut caps: Vec<Box<dyn PciCapability>> = vec![
            Box::new(ReadOnlyCapability::new(
                "virtio-common",
                VirtioCapability::new(
                    VirtioPciCapType::COMMON_CFG.0,
                    0,
                    0,
                    0,
                    VIRTIO_PCI_COMMON_CFG_SIZE as u32,
                ),
            )),
            Box::new(ReadOnlyCapability::new(
                "virtio-notify",
                VirtioNotifyCapability::new(
                    0,
                    0,
                    BAR0_NOTIFY_OFFSET as u32,
                    BAR0_NOTIFY_SIZE as u32,
                ),
            )),
            Box::new(ReadOnlyCapability::new(
                "virtio-pci-isr",
                VirtioCapability::new(
                    VirtioPciCapType::ISR_CFG.0,
                    0,
                    0,
                    BAR0_ISR_OFFSET as u32,
                    BAR0_ISR_SIZE as u32,
                ),
            )),
        ];

        // Only advertise a device-specific config capability when the device
        // actually has device-specific config registers. Per the virtio spec
        // (v1.2 section 4.1.4.6), a VIRTIO_PCI_CAP_DEVICE_CFG capability is
        // required only for device types that have a device-specific
        // configuration; devices without one (e.g. the entropy device) must
        // not advertise a zero-length capability.
        if traits.device_register_length > 0 {
            caps.push(Box::new(ReadOnlyCapability::new(
                "virtio-pci-device",
                VirtioCapability::new(
                    VirtioPciCapType::DEVICE_CFG.0,
                    0,
                    0,
                    BAR0_DEVICE_CFG_OFFSET as u32,
                    traits.device_register_length,
                ),
            )));
        }

        let mut bars = DeviceBars::new().bar0(
            BAR0_DEVICE_CFG_OFFSET as u64 + traits.device_register_length as u64,
            BarMemoryKind::Intercept(mmio_registration.new_io_region(
                "config",
                BAR0_DEVICE_CFG_OFFSET as u64 + traits.device_register_length as u64,
            )),
        );

        let msix: Option<MsixEmulator> = if let PciInterruptModel::Msix(msi_target) =
            interrupt_model
        {
            let (msix, msix_capability) = MsixEmulator::new(2, 64, msi_target);
            caps.insert(0, Box::new(msix_capability));
            bars = bars.bar2(
                msix.bar_len(),
                BarMemoryKind::Intercept(mmio_registration.new_io_region("msix", msix.bar_len())),
            );
            Some(msix)
        } else {
            None
        };

        let shared_memory_size = traits.shared_memory.size;
        if shared_memory_size > 0 {
            let (control, region) = shared_mem_mapper
                .expect("must provide mapper for shmem")
                .new_region(
                    shared_memory_size.try_into().expect("region too big"),
                    "virtio-pci-shmem".into(),
                )?;

            caps.push(Box::new(ReadOnlyCapability::new(
                "virtio-pci-shm",
                VirtioCapability64::new(
                    VirtioPciCapType::SHARED_MEMORY_CFG.0,
                    4, // BAR 4
                    traits.shared_memory.id,
                    0,
                    shared_memory_size,
                ),
            )));

            bars = bars.bar4(shared_memory_size, BarMemoryKind::SharedMem(control));

            device
                .set_shared_memory_region(&region)
                .map_err(io::Error::other)?;
        }

        // Add the VIRTIO_PCI_CAP_PCI_CFG capability last. It provides an
        // alternative access path to the virtio BAR regions through PCI
        // configuration space. The `pci_cfg_data` window is serviced by
        // `pci_cfg_read`/`pci_cfg_write` below, using the shared window state.
        let pci_cfg_access = Arc::new(Mutex::new(PciCfgAccessState::default()));
        caps.push(Box::new(VirtioPciCfgCapability {
            state: pci_cfg_access.clone(),
        }));
        // Capabilities are laid out consecutively starting at the end of the
        // common PCI header in the order they appear in `caps`; the pci_cfg
        // capability is last, so its offset is the header end plus the total
        // length of all preceding capabilities.
        let pci_cfg_cap_offset = COMMON_HEADER_END
            + caps[..caps.len() - 1]
                .iter()
                .map(|c| c.len() as u16)
                .sum::<u16>();
        let pci_cfg_data_offset = pci_cfg_cap_offset + VIRTIO_PCI_CFG_DATA_OFFSET;

        let mut config_space = ConfigSpaceType0Emulator::new(hardware_ids, caps, Vec::new(), bars);
        let interrupt_kind = match interrupt_model {
            PciInterruptModel::Msix(_) => InterruptKind::Msix(msix.unwrap()),
            PciInterruptModel::IntX(pin, line) => {
                InterruptKind::IntX(config_space.set_interrupt_pin(pin, line))
            }
        };

        let core = VirtioTransportCore::new(device, driver, guest_memory, doorbell_registration)?;

        Ok(VirtioPciDevice {
            core,
            pci: PciTransport {
                config_space,
                interrupt_kind,
                interrupt_status: Arc::new(Mutex::new(0)),
                msix_config_vector: 0,
                shared_memory_size,
                pci_cfg_access,
                pci_cfg_data_offset,
                device_register_length: traits.device_register_length,
            },
        })
    }

    /// Read a transport register as a u32.
    fn read_u32_local(&mut self, offset: u16) -> u32 {
        assert!(offset & 3 == 0);
        let queue_select = self.core.queue_select as usize;
        match VirtioPciCommonCfg(offset) {
            VirtioPciCommonCfg::DEVICE_FEATURE_SELECT => self.core.device_feature_select,
            VirtioPciCommonCfg::DEVICE_FEATURE => self
                .core
                .device_feature
                .bank(self.core.device_feature_select as usize),
            VirtioPciCommonCfg::DRIVER_FEATURE_SELECT => self.core.driver_feature_select,
            VirtioPciCommonCfg::DRIVER_FEATURE => self
                .core
                .driver_feature
                .bank(self.core.driver_feature_select as usize),
            VirtioPciCommonCfg::MSIX_CONFIG => {
                (self.core.queues.len() as u32) << 16 | self.pci.msix_config_vector as u32
            }
            VirtioPciCommonCfg::DEVICE_STATUS => {
                self.core.queue_select << 16
                    | self.core.config_generation << 8
                    | self.core.device_status.as_u32()
            }
            VirtioPciCommonCfg::QUEUE_SIZE => {
                let size = self
                    .core
                    .queues
                    .get(queue_select)
                    .map_or(0, |qd| qd.params.size);
                let msix_vector = self
                    .core
                    .queues
                    .get(queue_select)
                    .map_or(0, |qd| qd.msix_vector);
                (msix_vector as u32) << 16 | size as u32
            }
            VirtioPciCommonCfg::QUEUE_ENABLE => {
                self.core
                    .queues
                    .get(queue_select)
                    .is_some_and(|qd| qd.params.enable) as u32
            }
            VirtioPciCommonCfg::QUEUE_DESC_LO => self
                .core
                .queues
                .get(queue_select)
                .map_or(0, |qd| qd.params.desc_addr as u32),
            VirtioPciCommonCfg::QUEUE_DESC_HI => self
                .core
                .queues
                .get(queue_select)
                .map_or(0, |qd| (qd.params.desc_addr >> 32) as u32),
            VirtioPciCommonCfg::QUEUE_AVAIL_LO => self
                .core
                .queues
                .get(queue_select)
                .map_or(0, |qd| qd.params.avail_addr as u32),
            VirtioPciCommonCfg::QUEUE_AVAIL_HI => self
                .core
                .queues
                .get(queue_select)
                .map_or(0, |qd| (qd.params.avail_addr >> 32) as u32),
            VirtioPciCommonCfg::QUEUE_USED_LO => self
                .core
                .queues
                .get(queue_select)
                .map_or(0, |qd| qd.params.used_addr as u32),
            VirtioPciCommonCfg::QUEUE_USED_HI => self
                .core
                .queues
                .get(queue_select)
                .map_or(0, |qd| (qd.params.used_addr >> 32) as u32),
            VirtioPciCommonCfg(BAR0_NOTIFY_OFFSET) => 0,
            VirtioPciCommonCfg(BAR0_ISR_OFFSET) => {
                let mut interrupt_status = self.pci.interrupt_status.lock();
                let status = *interrupt_status;
                *interrupt_status = 0;
                if let InterruptKind::IntX(line) = &self.pci.interrupt_kind {
                    line.set_level(false)
                }
                status
            }
            _ => {
                tracelimit::warn_ratelimited!(offset, "unknown bar read");
                0xffffffff
            }
        }
    }

    /// Write a transport register as a u32.
    fn write_u32_local(&mut self, offset: u16, val: u32) {
        assert!(offset & 3 == 0);
        let queues_locked = self.core.device_status.driver_ok();
        let features_locked = queues_locked || self.core.device_status.features_ok();
        let queue_select = self.core.queue_select as usize;
        match VirtioPciCommonCfg(offset) {
            VirtioPciCommonCfg::DEVICE_FEATURE_SELECT => self.core.device_feature_select = val,
            VirtioPciCommonCfg::DRIVER_FEATURE_SELECT => self.core.driver_feature_select = val,
            VirtioPciCommonCfg::DRIVER_FEATURE => {
                let bank = self.core.driver_feature_select as usize;
                if !features_locked && bank < 2 {
                    self.core
                        .driver_feature
                        .set_bank(bank, val & self.core.device_feature.bank(bank));
                }
            }
            VirtioPciCommonCfg::MSIX_CONFIG => self.pci.msix_config_vector = val as u16,
            VirtioPciCommonCfg::DEVICE_STATUS => {
                self.core.queue_select = val >> 16;
                self.core.write_device_status(&mut self.pci, val as u8);
            }
            VirtioPciCommonCfg::QUEUE_SIZE => {
                let msix_vector = (val >> 16) as u16;
                if !queues_locked && queue_select < self.core.queues.len() {
                    let val = val as u16;
                    let qd = &mut self.core.queues[queue_select];
                    if val > MAX_QUEUE_SIZE {
                        qd.params.size = MAX_QUEUE_SIZE;
                    } else {
                        qd.params.size = val;
                    }
                    qd.msix_vector = msix_vector;
                }
            }
            VirtioPciCommonCfg::QUEUE_ENABLE => {
                let val = val & 0xffff;
                if !queues_locked && queue_select < self.core.queues.len() {
                    self.core.queues[queue_select].params.enable = val != 0;
                }
            }
            VirtioPciCommonCfg::QUEUE_DESC_LO => {
                if !queues_locked && queue_select < self.core.queues.len() {
                    let queue = &mut self.core.queues[queue_select].params;
                    queue.desc_addr = queue.desc_addr & 0xffffffff00000000 | val as u64;
                }
            }
            VirtioPciCommonCfg::QUEUE_DESC_HI => {
                if !queues_locked && queue_select < self.core.queues.len() {
                    let queue = &mut self.core.queues[queue_select].params;
                    queue.desc_addr = (val as u64) << 32 | queue.desc_addr & 0xffffffff;
                }
            }
            VirtioPciCommonCfg::QUEUE_AVAIL_LO => {
                if !queues_locked && queue_select < self.core.queues.len() {
                    let queue = &mut self.core.queues[queue_select].params;
                    queue.avail_addr = queue.avail_addr & 0xffffffff00000000 | val as u64;
                }
            }
            VirtioPciCommonCfg::QUEUE_AVAIL_HI => {
                if !queues_locked && queue_select < self.core.queues.len() {
                    let queue = &mut self.core.queues[queue_select].params;
                    queue.avail_addr = (val as u64) << 32 | queue.avail_addr & 0xffffffff;
                }
            }
            VirtioPciCommonCfg::QUEUE_USED_LO => {
                if !queues_locked && queue_select < self.core.queues.len() {
                    let queue = &mut self.core.queues[queue_select].params;
                    queue.used_addr = queue.used_addr & 0xffffffff00000000 | val as u64;
                }
            }
            VirtioPciCommonCfg::QUEUE_USED_HI => {
                if !queues_locked && queue_select < self.core.queues.len() {
                    let queue = &mut self.core.queues[queue_select].params;
                    queue.used_addr = (val as u64) << 32 | queue.used_addr & 0xffffffff;
                }
            }
            VirtioPciCommonCfg(BAR0_NOTIFY_OFFSET) => {
                self.core.notify_queue(val);
            }
            _ => {
                tracelimit::warn_ratelimited!(offset, "unknown bar write at offset");
            }
        }
    }

    /// Read transport registers via sub-word chunk handling.
    fn read_transport(&mut self, offset: u16, data: &mut [u8]) {
        read_as_u32_chunks(offset, data, |offset| self.read_u32_local(offset));
    }

    /// Write transport registers via sub-word chunk handling.
    fn write_transport(&mut self, offset: u16, data: &[u8]) {
        write_as_u32_chunks(offset, data, |offset, request_type| match request_type {
            ReadWriteRequestType::Write(value) => {
                self.write_u32_local(offset, value);
                None
            }
            ReadWriteRequestType::Read => Some(self.read_u32_local(offset)),
        });
    }

    /// Validate a guest-programmed `pci_cfg_data` access against the selected
    /// BAR, returning the offset as a `u16`, or `None` if `bar`, `offset`, and
    /// `length` do not address `length` bytes within a reachable BAR.
    fn pci_cfg_bar_offset(&self, bar: u8, offset: u32, length: u32) -> Option<u16> {
        let bar_len = match bar {
            0 => u32::from(BAR0_DEVICE_CFG_OFFSET) + self.pci.device_register_length,
            2 => match &self.pci.interrupt_kind {
                InterruptKind::Msix(msix) => msix.bar_len() as u32,
                _ => return None,
            },
            _ => return None,
        };
        // The access must fit entirely within the BAR, and the offset must be
        // addressable as a `u16` (all reachable regions live within the first
        // 64 KiB of their BARs).
        if offset.checked_add(length)? > bar_len {
            return None;
        }
        u16::try_from(offset).ok()
    }

    /// Service a read of the `VIRTIO_PCI_CAP_PCI_CFG` `pci_cfg_data` window.
    ///
    /// Per the virtio spec (4.1.4.9.1) the accessed bytes are stored in the
    /// *first* `cap.length` bytes of `pci_cfg_data`, regardless of how
    /// `cap.offset` is aligned within its dword. The device-config region is
    /// owned by the async device task and is therefore deferred; that path
    /// reads the dword containing the access, so the completed size always
    /// matches the 4-byte buffer the PCI config bus polls deferred reads with.
    fn read_pci_cfg_data(&mut self, value: &mut u32) -> IoResult {
        let (bar, offset, length) = {
            let state = self.pci.pci_cfg_access.lock();
            (state.bar, state.offset, state.length)
        };
        // Per the virtio spec, length MUST be 1, 2, or 4 and offset MUST be a
        // multiple of length. Reject anything else without panicking.
        if !matches!(length, 1 | 2 | 4) {
            return IoResult::Err(IoError::InvalidAccessSize);
        }
        if !offset.is_multiple_of(length) {
            return IoResult::Err(IoError::UnalignedAccess);
        }
        // Reject accesses that do not address `length` bytes within the
        // selected BAR.
        let Some(offset) = self.pci_cfg_bar_offset(bar, offset, length) else {
            return IoResult::Err(IoError::InvalidRegister);
        };
        let len = length as usize;
        if bar == 0 && offset >= BAR0_DEVICE_CFG_OFFSET {
            // The device-config region is owned by the async device task, so
            // the access is deferred. The PCI config bus polls the deferred
            // read with a 4-byte dword buffer, so the completion must be a full
            // dword with the accessed `cap.length` bytes left-aligned into the
            // low bytes of `pci_cfg_data`, as required by the spec.
            let dev_offset = offset - BAR0_DEVICE_CFG_OFFSET;
            return defer_config_read(
                &self.core.device_sender,
                dev_offset,
                len as u8,
                ConfigReadCompletion::LeftAlignedDword,
            );
        }
        // Store the accessed bytes in the first `cap.length` bytes of
        // `pci_cfg_data`, as required by the spec.
        let mut buf = [0u8; 4];
        let result = self.read_pci_cfg_bar(bar, offset, &mut buf[..len]);
        if let IoResult::Ok = result {
            *value = u32::from_le_bytes(buf);
        }
        result
    }

    /// Service a write of the `VIRTIO_PCI_CAP_PCI_CFG` `pci_cfg_data` window.
    fn write_pci_cfg_data(&mut self, value: u32) -> IoResult {
        let (bar, offset, length) = {
            let state = self.pci.pci_cfg_access.lock();
            (state.bar, state.offset, state.length)
        };
        if !matches!(length, 1 | 2 | 4) {
            return IoResult::Err(IoError::InvalidAccessSize);
        }
        if !offset.is_multiple_of(length) {
            return IoResult::Err(IoError::UnalignedAccess);
        }
        // Reject accesses that do not address `length` bytes within the
        // selected BAR (see `read_pci_cfg_data`).
        let Some(offset) = self.pci_cfg_bar_offset(bar, offset, length) else {
            return IoResult::Err(IoError::InvalidRegister);
        };
        let len = length as usize;
        // Take the value from the first `cap.length` bytes of `pci_cfg_data`,
        // as required by the spec.
        let bytes = value.to_le_bytes();
        let data = &bytes[..len];
        if bar == 0 && offset >= BAR0_DEVICE_CFG_OFFSET {
            return defer_config_write(
                &self.core.device_sender,
                offset - BAR0_DEVICE_CFG_OFFSET,
                data,
            );
        }
        self.write_pci_cfg_bar(bar, offset, data)
    }

    /// Read `data.len()` bytes from a synchronous BAR region (BAR0 transport or
    /// BAR2 MSI-X) on behalf of a `pci_cfg_data` access. The device-config
    /// region must be handled separately via deferral by the caller.
    fn read_pci_cfg_bar(&mut self, bar: u8, offset: u16, data: &mut [u8]) -> IoResult {
        match bar {
            0 => self.read_transport(offset, data),
            2 => read_as_u32_chunks(offset, data, |offset| {
                if let InterruptKind::Msix(msix) = &self.pci.interrupt_kind {
                    msix.read_u32(offset as u64)
                } else {
                    !0
                }
            }),
            _ => return IoResult::Err(IoError::InvalidRegister),
        }
        IoResult::Ok
    }

    /// Write `data` to a synchronous BAR region (BAR0 transport or BAR2 MSI-X)
    /// on behalf of a `pci_cfg_data` access. The device-config region must be
    /// handled separately via deferral by the caller.
    fn write_pci_cfg_bar(&mut self, bar: u8, offset: u16, data: &[u8]) -> IoResult {
        match bar {
            0 => self.write_transport(offset, data),
            2 => {
                write_as_u32_chunks(offset, data, |offset, request_type| match request_type {
                    ReadWriteRequestType::Write(value) => {
                        if let InterruptKind::Msix(msix) = &mut self.pci.interrupt_kind {
                            msix.write_u32(offset as u64, value)
                        }
                        None
                    }
                    ReadWriteRequestType::Read => {
                        if let InterruptKind::Msix(msix) = &self.pci.interrupt_kind {
                            Some(msix.read_u32(offset as u64))
                        } else {
                            Some(!0)
                        }
                    }
                });
            }
            _ => return IoResult::Err(IoError::InvalidRegister),
        }
        IoResult::Ok
    }

    /// Replay MMIO accesses that were stalled while the transport was busy.
    fn replay_stalled_io(&mut self) {
        let stalled = std::mem::take(&mut self.core.stalled_io);
        let mut iter = stalled.into_iter();
        for io in &mut iter {
            match io {
                StalledIo::Read {
                    address,
                    len,
                    deferred,
                } => {
                    if let Some((_, offset)) = self.pci.config_space.find_bar(address) {
                        let mut buf = vec![0u8; len];
                        self.read_transport(offset as u16, &mut buf);
                        deferred.complete(&buf);
                    } else {
                        // BAR was remapped via PCI config write while
                        // the IO was stalled.
                        deferred.complete_error(IoError::InvalidRegister);
                    }
                }
                StalledIo::Write {
                    address,
                    data,
                    len,
                    deferred,
                } => {
                    if let Some((_, offset)) = self.pci.config_space.find_bar(address) {
                        self.write_transport(offset as u16, &data[..len]);
                        if self.core.state.is_busy() {
                            self.core.pending_status_deferred = Some(deferred);
                            break;
                        }
                        deferred.complete();
                    } else {
                        deferred.complete_error(IoError::InvalidRegister);
                    }
                }
            }
        }
        self.core.stalled_io = iter.collect();
    }

    #[cfg(test)]
    pub(crate) fn read_u32(&mut self, offset: u16) -> u32 {
        self.read_u32_local(offset)
    }

    #[cfg(test)]
    pub(crate) fn write_u32(&mut self, offset: u16, val: u32) {
        self.write_u32_local(offset, val);
    }
}

impl ChangeDeviceState for VirtioPciDevice {
    fn start(&mut self) {
        self.core.start(&mut self.pci);
    }

    async fn stop(&mut self) {
        self.core.stop(&mut self.pci).await;
    }

    async fn reset(&mut self) {
        self.core.reset(&mut self.pci).await;
        self.pci.config_space.reset();
    }
}

impl PollDevice for VirtioPciDevice {
    fn poll_device(&mut self, cx: &mut std::task::Context<'_>) {
        self.core.poll_device(&mut self.pci, cx);
        // Replay any stalled IO after the state machine advances.
        if !self.core.stalled_io.is_empty() && !self.core.state.is_busy() {
            self.replay_stalled_io();
        }
    }
}

impl ChipsetDevice for VirtioPciDevice {
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

mod saved_state {
    mod state {
        use crate::transport::saved_state::state::CommonQueueState;
        use crate::transport::saved_state::state::CommonSavedState;
        use mesh::payload::Protobuf;
        use pci_core::cfg_space_emu::ConfigSpaceType0Emulator;
        use vmcore::save_restore::SaveRestore;
        use vmcore::save_restore::SavedStateRoot;

        #[derive(Protobuf)]
        #[mesh(package = "virtio.transport.pci")]
        pub struct SavedQueueState {
            #[mesh(1)]
            pub common: CommonQueueState,
            #[mesh(2)]
            pub msix_vector: u16,
        }

        #[derive(Protobuf, SavedStateRoot)]
        #[mesh(package = "virtio.transport.pci")]
        pub struct SavedState {
            #[mesh(1)]
            pub common: CommonSavedState,
            #[mesh(2)]
            pub msix_config_vector: u16,
            #[mesh(3)]
            pub queues: Vec<SavedQueueState>,
            #[mesh(4)]
            pub interrupt_status: u32,
            /// PCI configuration space, including the BAR base addresses. This
            /// must be preserved so that BARs pre-assigned by the host (before
            /// the guest firmware runs) survive a save/restore cycle.
            #[mesh(5)]
            pub cfg_space: <ConfigSpaceType0Emulator as SaveRestore>::SavedState,
        }

        #[derive(Protobuf, SavedStateRoot)]
        #[mesh(package = "virtio.transport.pci")]
        pub struct CfgCapSavedState {
            #[mesh(1)]
            pub bar: u8,
            #[mesh(2)]
            pub offset: u32,
            #[mesh(3)]
            pub length: u32,
        }
    }

    use super::*;
    use vmcore::save_restore::SaveRestore;

    impl SaveRestore for VirtioPciDevice {
        type SavedState = state::SavedState;

        fn save(&mut self) -> Result<Self::SavedState, SaveError> {
            Ok(state::SavedState {
                common: self.core.save_common()?,
                msix_config_vector: self.pci.msix_config_vector,
                queues: self
                    .core
                    .queues
                    .iter()
                    .enumerate()
                    .map(|(i, qd)| state::SavedQueueState {
                        common: self.core.save_queue_common(i),
                        msix_vector: qd.msix_vector,
                    })
                    .collect(),
                interrupt_status: *self.pci.interrupt_status.lock(),
                cfg_space: self.pci.config_space.save()?,
            })
        }

        fn restore(&mut self, state: Self::SavedState) -> Result<(), RestoreError> {
            let saved_queue_count = state.queues.len();
            self.core.restore_common(
                &mut self.pci,
                &state.common,
                state
                    .queues
                    .into_iter()
                    .map(|sq| (sq.common, sq.msix_vector)),
                saved_queue_count,
            )?;

            // Restore PCI-specific interrupt state.
            *self.pci.interrupt_status.lock() = state.interrupt_status;
            if let InterruptKind::IntX(line) = &self.pci.interrupt_kind {
                line.set_level(state.interrupt_status != 0);
            }
            self.pci.msix_config_vector = state.msix_config_vector;

            // Restore the PCI config space (BARs, command register, etc.) so
            // that host pre-assigned BARs are preserved across the cycle.
            self.pci.config_space.restore(state.cfg_space)?;

            Ok(())
        }
    }

    impl SaveRestore for VirtioPciCfgCapability {
        type SavedState = state::CfgCapSavedState;

        fn save(&mut self) -> Result<Self::SavedState, SaveError> {
            let state = self.state.lock();
            Ok(state::CfgCapSavedState {
                bar: state.bar,
                offset: state.offset,
                length: state.length,
            })
        }

        fn restore(&mut self, saved_state: Self::SavedState) -> Result<(), RestoreError> {
            let state::CfgCapSavedState {
                bar,
                offset,
                length,
            } = saved_state;
            let mut state = self.state.lock();
            state.bar = bar;
            state.offset = offset;
            state.length = length;
            Ok(())
        }
    }
}

impl MmioIntercept for VirtioPciDevice {
    fn mmio_read(&mut self, address: u64, data: &mut [u8]) -> IoResult {
        let Some((bar, offset)) = self.pci.config_space.find_bar(address) else {
            return IoResult::Err(IoError::InvalidRegister);
        };
        let offset = offset as u16;
        if bar == 0 && offset >= BAR0_DEVICE_CFG_OFFSET {
            return defer_config_read(
                &self.core.device_sender,
                offset - BAR0_DEVICE_CFG_OFFSET,
                data.len() as u8,
                ConfigReadCompletion::Exact,
            );
        }
        if bar == 0 && self.core.state.is_busy() {
            let (deferred, token) = defer_read();
            self.core.stalled_io.push(StalledIo::Read {
                address,
                len: data.len(),
                deferred,
            });
            return IoResult::Defer(token);
        }
        match bar {
            0 => self.read_transport(offset, data),
            2 => read_as_u32_chunks(offset, data, |offset| {
                if let InterruptKind::Msix(msix) = &self.pci.interrupt_kind {
                    msix.read_u32(offset as u64)
                } else {
                    !0
                }
            }),
            _ => return IoResult::Err(IoError::InvalidRegister),
        }
        IoResult::Ok
    }

    fn mmio_write(&mut self, address: u64, data: &[u8]) -> IoResult {
        let Some((bar, offset)) = self.pci.config_space.find_bar(address) else {
            return IoResult::Err(IoError::InvalidRegister);
        };
        let offset = offset as u16;
        if bar == 0 && offset >= BAR0_DEVICE_CFG_OFFSET {
            return defer_config_write(
                &self.core.device_sender,
                offset - BAR0_DEVICE_CFG_OFFSET,
                data,
            );
        }
        if bar == 0 && self.core.state.is_busy() {
            let (deferred, token) = defer_write();
            let mut buf = [0u8; 8];
            buf[..data.len()].copy_from_slice(data);
            self.core.stalled_io.push(StalledIo::Write {
                address,
                data: buf,
                len: data.len(),
                deferred,
            });
            return IoResult::Defer(token);
        }
        match bar {
            0 => self.write_transport(offset, data),
            2 => {
                write_as_u32_chunks(offset, data, |offset, request_type| match request_type {
                    ReadWriteRequestType::Write(value) => {
                        if let InterruptKind::Msix(msix) = &mut self.pci.interrupt_kind {
                            msix.write_u32(offset as u64, value)
                        }
                        None
                    }
                    ReadWriteRequestType::Read => {
                        if let InterruptKind::Msix(msix) = &self.pci.interrupt_kind {
                            Some(msix.read_u32(offset as u64))
                        } else {
                            Some(!0)
                        }
                    }
                });
            }
            _ => return IoResult::Err(IoError::InvalidRegister),
        }
        if bar == 0 && self.core.state.is_busy() {
            let (deferred, token) = defer_write();
            self.core.pending_status_deferred = Some(deferred);
            return IoResult::Defer(token);
        }
        IoResult::Ok
    }
}

impl PciConfigSpace for VirtioPciDevice {
    fn pci_cfg_read(&mut self, offset: u16, mut value: ByteEnabledDwordRead<'_>) -> IoResult {
        if offset == self.pci.pci_cfg_data_offset {
            let mut dword = 0;
            let result = self.read_pci_cfg_data(&mut dword);
            if let IoResult::Ok = result {
                value.set(dword);
            }
            return result;
        }
        self.pci.config_space.read_byte_enabled(offset, value)
    }

    fn pci_cfg_write(&mut self, offset: u16, value: ByteEnabledDwordWrite) -> IoResult {
        if offset == self.pci.pci_cfg_data_offset {
            return self.write_pci_cfg_data(value.merge(0));
        }
        self.pci.config_space.write_byte_enabled(offset, value)
    }
}

/// Length of the `virtio_pci_cfg_cap` structure: the 16-byte `virtio_pci_cap`
/// header plus the trailing 4-byte `pci_cfg_data` window.
const VIRTIO_PCI_CFG_CAP_LEN: u8 = 20;
/// Byte offset of the `pci_cfg_data` window within `virtio_pci_cfg_cap`.
const VIRTIO_PCI_CFG_DATA_OFFSET: u16 = 16;

/// Shared window state for the `VIRTIO_PCI_CAP_PCI_CFG` capability.
///
/// The driver programs `bar`, `offset`, and `length` by writing the
/// capability's config-space fields, then reads or writes the `pci_cfg_data`
/// window to perform an access into the selected BAR region. Both the
/// capability (which owns the field writes) and [`VirtioPciDevice`] (which
/// services the `pci_cfg_data` window) hold a clone of this state.
#[derive(Debug, Default, Inspect)]
struct PciCfgAccessState {
    bar: u8,
    #[inspect(hex)]
    offset: u32,
    #[inspect(hex)]
    length: u32,
}

/// The `VIRTIO_PCI_CAP_PCI_CFG` capability (`virtio_pci_cfg_cap`).
///
/// Provides an alternative access path to the virtio BAR regions purely
/// through PCI configuration space, for drivers or firmware that cannot map
/// the device's memory BARs. See the virtio 1.x spec, "PCI configuration
/// access capability". The `pci_cfg_data` window itself is serviced by
/// [`VirtioPciDevice`], which has access to the BAR regions; this capability
/// only tracks the programmed `bar`/`offset`/`length` fields.
struct VirtioPciCfgCapability {
    state: Arc<Mutex<PciCfgAccessState>>,
}

impl Inspect for VirtioPciCfgCapability {
    fn inspect(&self, req: inspect::Request<'_>) {
        let state = self.state.lock();
        req.respond()
            .field("label", "virtio-pci-cfg")
            .field("bar", state.bar)
            .hex("offset", state.offset)
            .hex("length", state.length);
    }
}

impl PciCapability for VirtioPciCfgCapability {
    fn label(&self) -> &str {
        "virtio-pci-cfg"
    }

    fn capability_id(&self) -> CapabilityId {
        CapabilityId::VENDOR_SPECIFIC
    }

    fn len(&self) -> usize {
        VIRTIO_PCI_CFG_CAP_LEN as usize
    }

    fn read(&self, offset: u16, mut value: ByteEnabledDwordRead<'_>) {
        let state = self.state.lock();
        let dword = match offset {
            // cap_vndr | cap_next (filled in by the config space emulator) |
            // cap_len | cfg_type
            0 => {
                CapabilityId::VENDOR_SPECIFIC.0 as u32
                    | (VIRTIO_PCI_CFG_CAP_LEN as u32) << 16
                    | (VirtioPciCapType::PCI_CFG.0 as u32) << 24
            }
            // bar | id | padding. The id field is read-only and unused for
            // this capability, so it always reads as zero.
            4 => state.bar as u32,
            8 => state.offset,
            12 => state.length,
            // pci_cfg_data is serviced by VirtioPciDevice::pci_cfg_read.
            _ => 0,
        };
        value.set(dword);
    }

    fn write(&mut self, offset: u16, val: ByteEnabledDwordWrite) {
        let mut state = self.state.lock();
        match offset {
            // The header dword (cap_vndr/cap_next/cap_len/cfg_type) is
            // read-only.
            0 => {}
            // Only bar is writable here; the id byte is read-only and unused
            // for this capability.
            4 => state.bar = val.merge(state.bar.into()) as u8,
            8 => state.offset = val.merge(state.offset),
            12 => state.length = val.merge(state.length),
            // pci_cfg_data is serviced by VirtioPciDevice::pci_cfg_write.
            _ => {}
        }
    }

    fn reset(&mut self) {
        *self.state.lock() = PciCfgAccessState::default();
    }
}

pub(crate) mod capabilities {
    use crate::spec::pci::VirtioPciCapType;
    use pci_core::spec::caps::CapabilityId;

    use zerocopy::Immutable;
    use zerocopy::IntoBytes;
    use zerocopy::KnownLayout;

    #[repr(C)]
    #[derive(Debug, IntoBytes, Immutable, KnownLayout)]
    pub struct VirtioCapabilityCommon {
        cap_id: u8,
        cap_next: u8,
        len: u8,
        typ: u8,
        bar: u8,
        unique_id: u8,
        padding: [u8; 2],
        offset: u32,
        length: u32,
    }

    impl VirtioCapabilityCommon {
        pub fn new(len: u8, typ: u8, bar: u8, unique_id: u8, addr_off: u32, addr_len: u32) -> Self {
            Self {
                cap_id: CapabilityId::VENDOR_SPECIFIC.0,
                cap_next: 0,
                len,
                typ,
                bar,
                unique_id,
                padding: [0; 2],
                offset: addr_off,
                length: addr_len,
            }
        }
    }

    #[repr(C)]
    #[derive(Debug, IntoBytes, Immutable, KnownLayout)]
    pub struct VirtioCapability {
        common: VirtioCapabilityCommon,
    }

    impl VirtioCapability {
        pub fn new(typ: u8, bar: u8, unique_id: u8, addr_off: u32, addr_len: u32) -> Self {
            Self {
                common: VirtioCapabilityCommon::new(
                    size_of::<Self>() as u8,
                    typ,
                    bar,
                    unique_id,
                    addr_off,
                    addr_len,
                ),
            }
        }
    }

    #[repr(C)]
    #[derive(Debug, IntoBytes, Immutable, KnownLayout)]
    pub struct VirtioCapability64 {
        common: VirtioCapabilityCommon,
        offset_hi: u32,
        length_hi: u32,
    }

    impl VirtioCapability64 {
        pub fn new(typ: u8, bar: u8, unique_id: u8, addr_off: u64, addr_len: u64) -> Self {
            Self {
                common: VirtioCapabilityCommon::new(
                    size_of::<Self>() as u8,
                    typ,
                    bar,
                    unique_id,
                    addr_off as u32,
                    addr_len as u32,
                ),
                offset_hi: (addr_off >> 32) as u32,
                length_hi: (addr_len >> 32) as u32,
            }
        }
    }

    #[repr(C)]
    #[derive(Debug, IntoBytes, Immutable, KnownLayout)]
    pub struct VirtioNotifyCapability {
        common: VirtioCapabilityCommon,
        offset_multiplier: u32,
    }

    impl VirtioNotifyCapability {
        pub fn new(offset_multiplier: u32, bar: u8, addr_off: u32, addr_len: u32) -> Self {
            Self {
                common: VirtioCapabilityCommon::new(
                    size_of::<Self>() as u8,
                    VirtioPciCapType::NOTIFY_CFG.0,
                    bar,
                    0,
                    addr_off,
                    addr_len,
                ),
                offset_multiplier,
            }
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;
        use pci_core::capabilities::ReadOnlyCapability;
        use pci_core::test_helpers::read_cap_u32;

        #[test]
        fn common_check() {
            let common =
                ReadOnlyCapability::new("common", VirtioCapability::new(0x13, 2, 0, 0x100, 0x200));
            assert_eq!(read_cap_u32(&common, 0), 0x13100009);
            assert_eq!(read_cap_u32(&common, 4), 2);
            assert_eq!(read_cap_u32(&common, 8), 0x100);
            assert_eq!(read_cap_u32(&common, 12), 0x200);
        }

        #[test]
        fn notify_check() {
            let notify = ReadOnlyCapability::new(
                "notify",
                VirtioNotifyCapability::new(0x123, 2, 0x100, 0x200),
            );
            assert_eq!(read_cap_u32(&notify, 0), 0x2140009);
            assert_eq!(read_cap_u32(&notify, 4), 2);
            assert_eq!(read_cap_u32(&notify, 8), 0x100);
            assert_eq!(read_cap_u32(&notify, 12), 0x200);
        }
    }
}
