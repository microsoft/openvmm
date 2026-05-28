// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! PCI transport for virtio devices

use self::capabilities::*;
use super::StalledIo;
use super::core::TransportOps;
use super::core::VirtioTransportCore;
use super::task::defer_config_read;
use super::task::defer_config_write;
use crate::DynVirtioDevice;
use crate::MAX_QUEUE_SIZE;
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
use pci_core::spec::hwid::ClassCode;
use pci_core::spec::hwid::HardwareIds;
use pci_core::spec::hwid::ProgrammingInterface;
use pci_core::spec::hwid::Subclass;
use std::io;
use std::sync::Arc;
use vmcore::device_state::ChangeDeviceState;
use vmcore::interrupt::Interrupt;
use vmcore::line_interrupt::LineInterrupt;

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

        let hardware_ids = HardwareIds {
            vendor_id: VIRTIO_VENDOR_ID,
            device_id: VIRTIO_PCI_DEVICE_ID_BASE + traits.device_id.0,
            revision_id: 1,
            prog_if: ProgrammingInterface::NONE,
            base_class: ClassCode::BASE_SYSTEM_PERIPHERAL,
            sub_class: Subclass::BASE_SYSTEM_PERIPHERAL_OTHER,
            type0_sub_vendor_id: VIRTIO_VENDOR_ID,
            type0_sub_system_id: 0x40,
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
            Box::new(ReadOnlyCapability::new(
                "virtio-pci-device",
                VirtioCapability::new(
                    VirtioPciCapType::DEVICE_CFG.0,
                    0,
                    0,
                    BAR0_DEVICE_CFG_OFFSET as u32,
                    traits.device_register_length,
                ),
            )),
        ];

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
        }
    }

    use super::*;
    use vmcore::save_restore::SaveRestore;

    impl SaveRestore for VirtioPciDevice {
        type SavedState = state::SavedState;

        fn save(&mut self) -> Result<Self::SavedState, vmcore::save_restore::SaveError> {
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
            })
        }

        fn restore(
            &mut self,
            state: Self::SavedState,
        ) -> Result<(), vmcore::save_restore::RestoreError> {
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
    fn pci_cfg_read(&mut self, offset: u16, value: &mut u32) -> IoResult {
        self.pci.config_space.read_u32(offset, value)
    }

    fn pci_cfg_write(&mut self, offset: u16, value: u32) -> IoResult {
        self.pci.config_space.write_u32(offset, value)
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
        use pci_core::capabilities::PciCapability;
        use pci_core::capabilities::ReadOnlyCapability;

        #[test]
        fn common_check() {
            let common =
                ReadOnlyCapability::new("common", VirtioCapability::new(0x13, 2, 0, 0x100, 0x200));
            assert_eq!(common.read_u32(0), 0x13100009);
            assert_eq!(common.read_u32(4), 2);
            assert_eq!(common.read_u32(8), 0x100);
            assert_eq!(common.read_u32(12), 0x200);
        }

        #[test]
        fn notify_check() {
            let notify = ReadOnlyCapability::new(
                "notify",
                VirtioNotifyCapability::new(0x123, 2, 0x100, 0x200),
            );
            assert_eq!(notify.read_u32(0), 0x2140009);
            assert_eq!(notify.read_u32(4), 2);
            assert_eq!(notify.read_u32(8), 0x100);
            assert_eq!(notify.read_u32(12), 0x200);
        }
    }
}
