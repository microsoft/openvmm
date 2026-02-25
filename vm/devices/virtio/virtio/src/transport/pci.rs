// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! PCI transport for virtio devices

use self::capabilities::*;
use crate::QUEUE_MAX_SIZE;
use crate::QueueResources;
use crate::Resources;
use crate::VirtioDevice;
use crate::VirtioDoorbells;
use crate::queue::QueueParams;
use crate::spec::pci::*;
use crate::spec::*;
use chipset_device::ChipsetDevice;
use chipset_device::io::IoResult;
use chipset_device::mmio::MmioIntercept;
use chipset_device::mmio::RegisterMmioIntercept;
use chipset_device::pci::PciConfigSpace;
use chipset_device::poll_device::PollDevice;
use device_emulators::ReadWriteRequestType;
use device_emulators::read_as_u32_chunks;
use device_emulators::write_as_u32_chunks;
use guestmem::DoorbellRegistration;
use guestmem::MappedMemoryRegion;
use guestmem::MemoryMapper;
use inspect::InspectMut;
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
use vmcore::save_restore::NoSavedState;
use vmcore::save_restore::RestoreError;
use vmcore::save_restore::SaveError;
use vmcore::save_restore::SaveRestore;

/// What kind of PCI interrupts [`VirtioPciDevice`] should use.
pub enum PciInterruptModel<'a> {
    Msix(&'a MsiTarget),
    IntX(PciInterruptPin, LineInterrupt),
}

enum InterruptKind {
    Msix(MsixEmulator),
    IntX(Arc<IntxInterrupt>),
}

/// Run a virtio device over PCI
#[derive(InspectMut)]
pub struct VirtioPciDevice {
    #[inspect(skip)]
    device: Box<dyn VirtioDevice>,
    #[inspect(skip)]
    device_feature: VirtioDeviceFeatures,
    #[inspect(hex)]
    device_feature_select: u32,
    #[inspect(skip)]
    driver_feature: VirtioDeviceFeatures,
    #[inspect(hex)]
    driver_feature_select: u32,
    msix_config_vector: u16,
    queue_select: u32,
    #[inspect(skip)]
    events: Vec<pal_event::Event>,
    #[inspect(skip)]
    queues: Vec<QueueParams>,
    #[inspect(skip)]
    msix_vectors: Vec<u16>,
    #[inspect(skip)]
    interrupt_status: Arc<Mutex<u32>>,
    #[inspect(hex)]
    device_status: VirtioDeviceStatus,
    disabling: bool,
    config_generation: u32,
    config_space: ConfigSpaceType0Emulator,

    #[inspect(skip)]
    interrupt_kind: InterruptKind,
    #[inspect(skip)]
    doorbells: VirtioDoorbells,
    #[inspect(skip)]
    shared_memory_region: Option<Arc<dyn MappedMemoryRegion>>,
    #[inspect(hex)]
    shared_memory_size: u64,
}

impl VirtioPciDevice {
    pub fn new(
        device: Box<dyn VirtioDevice>,
        interrupt_model: PciInterruptModel<'_>,
        doorbell_registration: Option<Arc<dyn DoorbellRegistration>>,
        mmio_registration: &mut dyn RegisterMmioIntercept,
        shared_mem_mapper: Option<&dyn MemoryMapper>,
    ) -> io::Result<Self> {
        let traits = device.traits();
        let queues = (0..traits.max_queues)
            .map(|_| QueueParams {
                size: QUEUE_MAX_SIZE,
                ..Default::default()
            })
            .collect();
        let events = (0..traits.max_queues)
            .map(|_| pal_event::Event::new())
            .collect();
        let msix_vectors = vec![0; traits.max_queues.into()];

        let hardware_ids = HardwareIds {
            vendor_id: VIRTIO_VENDOR_ID,
            device_id: VIRTIO_PCI_DEVICE_ID_BASE + traits.device_id,
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
                VirtioCapability::new(VIRTIO_PCI_CAP_COMMON_CFG, 0, 0, 0, 56),
            )),
            Box::new(ReadOnlyCapability::new(
                "virtio-notify",
                VirtioNotifyCapability::new(0, 0, 56, 4),
            )),
            Box::new(ReadOnlyCapability::new(
                "virtio-pci-isr",
                VirtioCapability::new(VIRTIO_PCI_CAP_ISR_CFG, 0, 0, 60, 4),
            )),
            Box::new(ReadOnlyCapability::new(
                "virtio-pci-device",
                VirtioCapability::new(
                    VIRTIO_PCI_CAP_DEVICE_CFG,
                    0,
                    0,
                    64,
                    traits.device_register_length,
                ),
            )),
        ];

        let mut bars = DeviceBars::new().bar0(
            0x40 + traits.device_register_length as u64,
            BarMemoryKind::Intercept(
                mmio_registration
                    .new_io_region("config", 0x40 + traits.device_register_length as u64),
            ),
        );

        let msix: Option<MsixEmulator> = if let PciInterruptModel::Msix(msi_target) =
            interrupt_model
        {
            let (msix, msix_capability) = MsixEmulator::new(2, 64, msi_target);
            // setting msix as the first cap so that we don't have to update unit tests
            // i.e: there's no reason why this can't be a .push() instead of .insert()
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
        let mut shared_memory_region = None;
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
                    VIRTIO_PCI_CAP_SHARED_MEMORY_CFG,
                    4, // BAR 4
                    traits.shared_memory.id,
                    0,
                    shared_memory_size,
                ),
            )));

            bars = bars.bar4(shared_memory_size, BarMemoryKind::SharedMem(control));
            shared_memory_region = Some(region);
        }

        let mut config_space = ConfigSpaceType0Emulator::new(hardware_ids, caps, bars);
        let interrupt_kind = match interrupt_model {
            PciInterruptModel::Msix(_) => InterruptKind::Msix(msix.unwrap()),
            PciInterruptModel::IntX(pin, line) => {
                InterruptKind::IntX(config_space.set_interrupt_pin(pin, line))
            }
        };

        let mut device_feature = traits.device_features.clone();
        device_feature.set_bank(
            0,
            device_feature
                .bank0()
                .with_ring_event_idx(true)
                .with_ring_indirect_desc(true)
                .into_bits(),
        );
        device_feature.set_bank(1, device_feature.bank1().with_version_1(true).into_bits());
        Ok(VirtioPciDevice {
            device,
            device_feature,
            device_feature_select: 0,
            driver_feature: VirtioDeviceFeatures::new(),
            driver_feature_select: 0,
            msix_config_vector: 0,
            queue_select: 0,
            events,
            queues,
            msix_vectors,
            interrupt_status: Arc::new(Mutex::new(0)),
            device_status: VirtioDeviceStatus::new(),
            disabling: false,
            config_generation: 0,
            interrupt_kind,
            config_space,
            doorbells: VirtioDoorbells::new(doorbell_registration),
            shared_memory_region,
            shared_memory_size,
        })
    }

    fn update_config_generation(&mut self) {
        self.config_generation = self.config_generation.wrapping_add(1);
        if self.device_status.driver_ok() {
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
    }

    fn read_u32(&mut self, offset: u16) -> u32 {
        assert!(offset & 3 == 0);
        let queue_select = self.queue_select as usize;
        match offset {
            // Device feature bank index
            0 => self.device_feature_select,
            // Device feature bank
            4 => {
                let feature_select = self.device_feature_select as usize;
                self.device_feature.bank(feature_select)
            }
            // Driver feature bank index
            8 => self.driver_feature_select,
            // Driver feature bank
            12 => {
                let feature_select = self.driver_feature_select as usize;
                self.driver_feature.bank(feature_select)
            }
            16 => (self.queues.len() as u32) << 16 | self.msix_config_vector as u32,
            20 => {
                self.queue_select << 24 | self.config_generation << 8 | self.device_status.as_u32()
            }
            24 => {
                let size = if queue_select < self.queues.len() {
                    self.queues[queue_select].size
                } else {
                    0
                };
                let msix_vector = self.msix_vectors.get(queue_select).copied().unwrap_or(0);
                (msix_vector as u32) << 16 | size as u32
            }
            // Current queue enabled
            28 => {
                let enable = if queue_select < self.queues.len() {
                    if self.queues[queue_select].enable {
                        1
                    } else {
                        0
                    }
                } else {
                    0
                };
                #[expect(clippy::if_same_then_else)] // fix when TODO is resolved
                let notify_offset = if queue_select < self.queues.len() {
                    0 // TODO: when should this be non-zero? ever?
                } else {
                    0
                };
                (notify_offset as u32) << 16 | enable as u32
            }
            // Queue descriptor table address (low part)
            32 => {
                if queue_select < self.queues.len() {
                    self.queues[queue_select].desc_addr as u32
                } else {
                    0
                }
            }
            // Queue descriptor table address (high part)
            36 => {
                if queue_select < self.queues.len() {
                    (self.queues[queue_select].desc_addr >> 32) as u32
                } else {
                    0
                }
            }
            // Queue descriptor available ring address (low part)
            40 => {
                if queue_select < self.queues.len() {
                    self.queues[queue_select].avail_addr as u32
                } else {
                    0
                }
            }
            // Queue descriptor available ring address (high part)
            44 => {
                if queue_select < self.queues.len() {
                    (self.queues[queue_select].avail_addr >> 32) as u32
                } else {
                    0
                }
            }
            // Queue descriptor used ring address (low part)
            48 => {
                if queue_select < self.queues.len() {
                    self.queues[queue_select].used_addr as u32
                } else {
                    0
                }
            }
            // Queue descriptor used ring address (high part)
            52 => {
                if queue_select < self.queues.len() {
                    (self.queues[queue_select].used_addr >> 32) as u32
                } else {
                    0
                }
            }
            56 => 0, // queue notification register
            // ISR
            60 => {
                let mut interrupt_status = self.interrupt_status.lock();
                let status = *interrupt_status;
                *interrupt_status = 0;
                if let InterruptKind::IntX(line) = &self.interrupt_kind {
                    line.set_level(false)
                }
                status
            }
            offset if offset >= 64 => self.device.read_registers_u32(offset - 64),
            _ => {
                tracing::warn!(offset, "unknown bar read");
                0xffffffff
            }
        }
    }

    fn write_u32(&mut self, address: u64, offset: u16, val: u32) {
        assert!(offset & 3 == 0);
        let queues_locked = self.device_status.driver_ok();
        let features_locked = queues_locked || self.device_status.features_ok();
        let queue_select = self.queue_select as usize;
        match offset {
            // Device feature bank index
            0 => self.device_feature_select = val,
            // Driver feature bank index
            8 => self.driver_feature_select = val,
            // Driver feature bank
            12 => {
                let bank = self.driver_feature_select as usize;
                if features_locked || bank >= self.device_feature.len() {
                    // Update is not persisted.
                } else {
                    self.driver_feature
                        .set_bank(bank, val & self.device_feature.bank(bank));
                }
            }
            16 => self.msix_config_vector = val as u16,
            // Device status
            20 => {
                self.queue_select = val >> 16;
                let val = val & 0xff;
                if val == 0 {
                    if self.disabling {
                        return;
                    }
                    let started = self.device_status.driver_ok();
                    self.config_generation = 0;
                    if started {
                        self.doorbells.clear();
                        let waker = std::task::Waker::noop();
                        let mut cx = std::task::Context::from_waker(&waker);
                        if self.device.poll_disable(&mut cx).is_pending() {
                            self.disabling = true;
                            return;
                        }
                    }
                    // Fast path: disable completed synchronously.
                    self.device_status = VirtioDeviceStatus::new();
                    *self.interrupt_status.lock() = 0;
                    return;
                }

                let new_status = VirtioDeviceStatus::from(val as u8);
                if new_status.acknowledge() {
                    self.device_status.set_acknowledge(true);
                }
                if new_status.driver() {
                    self.device_status.set_driver(true);
                }
                if new_status.failed() {
                    self.device_status.set_failed(true);
                }

                if !self.device_status.features_ok() && new_status.features_ok() {
                    self.device_status.set_features_ok(true);
                    self.update_config_generation();
                }

                if !self.device_status.driver_ok() && new_status.driver_ok() {
                    let notification_address = (address & !0xfff) + 56;
                    for i in 0..self.events.len() {
                        self.doorbells.add(
                            notification_address,
                            Some(i as u64),
                            Some(2),
                            &self.events[i],
                        );
                    }
                    let queues = self
                        .queues
                        .iter()
                        .zip(self.msix_vectors.iter().copied())
                        .zip(self.events.iter().cloned())
                        .map(|((queue, vector), event)| {
                            let notify = match &self.interrupt_kind {
                                InterruptKind::Msix(msix) => {
                                    if let Some(interrupt) = msix.interrupt(vector) {
                                        interrupt
                                    } else {
                                        tracing::warn!(vector, "invalid MSIx vector specified");
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
                            };

                            QueueResources {
                                params: *queue,
                                notify,
                                event,
                            }
                        })
                        .collect();

                    self.device.enable(Resources {
                        features: self.driver_feature.clone(),
                        queues,
                        shared_memory_region: self.shared_memory_region.clone(),
                        shared_memory_size: self.shared_memory_size,
                    });

                    self.device_status.set_driver_ok(true);
                    self.update_config_generation();
                }
            }
            // Queue current size
            24 => {
                let msix_vector = (val >> 16) as u16;
                if !queues_locked && queue_select < self.queues.len() {
                    let val = val as u16;
                    let queue = &mut self.queues[queue_select];
                    if val > QUEUE_MAX_SIZE {
                        queue.size = QUEUE_MAX_SIZE;
                    } else {
                        queue.size = val;
                    }
                    self.msix_vectors[queue_select] = msix_vector;
                }
            }
            // Current queue enabled
            28 => {
                let val = val & 0xffff;
                if !queues_locked && queue_select < self.queues.len() {
                    let queue = &mut self.queues[queue_select];
                    queue.enable = val != 0;
                }
            }
            // Queue descriptor table address (low part)
            32 => {
                if !queues_locked && queue_select < self.queues.len() {
                    let queue = &mut self.queues[queue_select];
                    queue.desc_addr = queue.desc_addr & 0xffffffff00000000 | val as u64;
                }
            }
            // Queue descriptor table address (high part)
            36 => {
                if !queues_locked && queue_select < self.queues.len() {
                    let queue = &mut self.queues[queue_select];
                    queue.desc_addr = (val as u64) << 32 | queue.desc_addr & 0xffffffff;
                }
            }
            // Queue descriptor available ring address (low part)
            40 => {
                if !queues_locked && queue_select < self.queues.len() {
                    let queue = &mut self.queues[queue_select];
                    queue.avail_addr = queue.avail_addr & 0xffffffff00000000 | val as u64;
                }
            }
            // Queue descriptor available ring address (high part)
            44 => {
                if !queues_locked && queue_select < self.queues.len() {
                    let queue = &mut self.queues[queue_select];
                    queue.avail_addr = (val as u64) << 32 | queue.avail_addr & 0xffffffff;
                }
            }
            // Queue descriptor used ring address (low part)
            48 => {
                if !queues_locked && (queue_select) < self.queues.len() {
                    let queue = &mut self.queues[queue_select];
                    queue.used_addr = queue.used_addr & 0xffffffff00000000 | val as u64;
                }
            }
            // Queue descriptor used ring address (high part)
            52 => {
                if !queues_locked && queue_select < self.queues.len() {
                    let queue = &mut self.queues[queue_select];
                    queue.used_addr = (val as u64) << 32 | queue.used_addr & 0xffffffff;
                }
            }
            // Queue notification register
            56 => {
                if (val as usize) < self.events.len() {
                    self.events[val as usize].signal();
                }
            }
            offset if offset >= 64 => self.device.write_registers_u32(offset - 64, val),
            _ => {
                tracing::warn!(offset, "unknown bar write at offset");
            }
        }
    }
}

impl Drop for VirtioPciDevice {
    fn drop(&mut self) {
        if self.device_status.driver_ok() || self.disabling {
            let waker = std::task::Waker::noop();
            let mut cx = std::task::Context::from_waker(&waker);
            let _ = self.device.poll_disable(&mut cx);
        }
    }
}

impl VirtioPciDevice {
    fn read_bar_u32(&mut self, bar: u8, offset: u16) -> u32 {
        match bar {
            0 => self.read_u32(offset),
            2 => {
                if let InterruptKind::Msix(msix) = &self.interrupt_kind {
                    msix.read_u32(offset)
                } else {
                    !0
                }
            }
            _ => !0,
        }
    }

    fn write_bar_u32(&mut self, address: u64, bar: u8, offset: u16, value: u32) {
        match bar {
            0 => self.write_u32(address, offset, value),
            2 => {
                if let InterruptKind::Msix(msix) = &mut self.interrupt_kind {
                    msix.write_u32(offset, value)
                }
            }
            _ => tracing::warn!(bar, offset, "Unknown write"),
        }
    }
}

impl ChangeDeviceState for VirtioPciDevice {
    fn start(&mut self) {}

    async fn stop(&mut self) {}

    async fn reset(&mut self) {
        if self.device_status.driver_ok() || self.disabling {
            self.doorbells.clear();
            std::future::poll_fn(|cx| self.device.poll_disable(cx)).await;
        }
        self.device_status = VirtioDeviceStatus::new();
        self.disabling = false;
        self.config_generation = 0;
        *self.interrupt_status.lock() = 0;
    }
}

impl PollDevice for VirtioPciDevice {
    fn poll_device(&mut self, cx: &mut std::task::Context<'_>) {
        if self.disabling {
            if self.device.poll_disable(cx).is_ready() {
                self.device_status = VirtioDeviceStatus::new();
                self.disabling = false;
                *self.interrupt_status.lock() = 0;
            }
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

impl SaveRestore for VirtioPciDevice {
    type SavedState = NoSavedState; // TODO

    fn save(&mut self) -> Result<Self::SavedState, SaveError> {
        Ok(NoSavedState)
    }

    fn restore(&mut self, NoSavedState: Self::SavedState) -> Result<(), RestoreError> {
        Ok(())
    }
}

impl MmioIntercept for VirtioPciDevice {
    fn mmio_read(&mut self, address: u64, data: &mut [u8]) -> IoResult {
        if let Some((bar, offset)) = self.config_space.find_bar(address) {
            read_as_u32_chunks(offset, data, |offset| self.read_bar_u32(bar, offset))
        }
        IoResult::Ok
    }

    fn mmio_write(&mut self, address: u64, data: &[u8]) -> IoResult {
        if let Some((bar, offset)) = self.config_space.find_bar(address) {
            write_as_u32_chunks(offset, data, |offset, request_type| match request_type {
                ReadWriteRequestType::Write(value) => {
                    self.write_bar_u32(address, bar, offset, value);
                    None
                }
                ReadWriteRequestType::Read => Some(self.read_bar_u32(bar, offset)),
            })
        }
        IoResult::Ok
    }
}

impl PciConfigSpace for VirtioPciDevice {
    fn pci_cfg_read(&mut self, offset: u16, value: &mut u32) -> IoResult {
        self.config_space.read_u32(offset, value)
    }

    fn pci_cfg_write(&mut self, offset: u16, value: u32) -> IoResult {
        self.config_space.write_u32(offset, value)
    }
}

pub(crate) mod capabilities {
    use crate::spec::pci::VIRTIO_PCI_CAP_NOTIFY_CFG;
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
                    VIRTIO_PCI_CAP_NOTIFY_CFG,
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
