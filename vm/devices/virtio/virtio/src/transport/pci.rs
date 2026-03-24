// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! PCI transport for virtio devices

use self::capabilities::*;
use super::task::DeviceCommand;
use super::task::StartParams;
use super::task::TransportState;
use super::task::TransportStateResult;
use super::task::defer_config_read;
use super::task::defer_config_write;
use super::task::run_device_task;
use crate::DynVirtioDevice;
use crate::QUEUE_MAX_SIZE;
use crate::QueueResources;
use crate::VirtioDoorbells;
use crate::queue::QueueParams;
use crate::queue::QueueState;
use crate::spec::pci::VIRTIO_PCI_COMMON_CFG_SIZE;
use crate::spec::pci::VIRTIO_PCI_DEVICE_ID_BASE;
use crate::spec::pci::VIRTIO_VENDOR_ID;
use crate::spec::pci::VirtioPciCapType;
use crate::spec::pci::VirtioPciCommonCfg;
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
use guestmem::GuestMemory;
use guestmem::MemoryMapper;
use inspect::InspectMut;
use mesh::rpc::Rpc;
use mesh::rpc::RpcSend;
use pal_async::task::Spawn;
use pal_async::task::Task;
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
use std::task::Poll;
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

/// Run a virtio device over PCI
#[derive(InspectMut)]
pub struct VirtioPciDevice {
    #[inspect(rename = "device", send = "DeviceCommand::Inspect")]
    device_sender: mesh::Sender<DeviceCommand>,
    #[inspect(skip)]
    _device_task: Task<()>,
    state: TransportState,
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
    #[inspect(iter_by_index)]
    queues: Vec<QueueParams>,
    #[inspect(skip)]
    msix_vectors: Vec<u16>,
    #[inspect(skip)]
    interrupt_status: Arc<Mutex<u32>>,
    #[inspect(hex)]
    device_status: VirtioDeviceStatus,
    #[inspect(skip)]
    poll_waker: Option<std::task::Waker>,
    config_generation: u32,
    config_space: ConfigSpaceType0Emulator,

    #[inspect(skip)]
    interrupt_kind: InterruptKind,
    #[inspect(skip)]
    doorbells: VirtioDoorbells,
    #[inspect(hex)]
    shared_memory_size: u64,
    /// Cached queue states from `ChangeDeviceState::stop()` for resume.
    #[inspect(skip)]
    saved_queue_states: Vec<Option<QueueState>>,
    supports_save_restore: bool,
    #[inspect(skip)]
    guest_memory: GuestMemory,
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

        let mut config_space = ConfigSpaceType0Emulator::new(hardware_ids, caps, bars);
        let interrupt_kind = match interrupt_model {
            PciInterruptModel::Msix(_) => InterruptKind::Msix(msix.unwrap()),
            PciInterruptModel::IntX(pin, line) => {
                InterruptKind::IntX(config_space.set_interrupt_pin(pin, line))
            }
        };

        let device_feature = traits
            .device_features
            .clone()
            .with_bank0(
                traits
                    .device_features
                    .bank0()
                    .with_ring_event_idx(true)
                    .with_ring_indirect_desc(true),
            )
            .with_bank1(
                traits
                    .device_features
                    .bank1()
                    .with_version_1(true)
                    .with_ring_packed(true),
            );
        let supports_save_restore = device.supports_save_restore();

        let (sender, receiver) = mesh::channel();
        let _device_task = driver.spawn("virtio-device-task", async move {
            run_device_task(device, receiver).await;
        });

        Ok(VirtioPciDevice {
            device_sender: sender,
            _device_task,
            state: TransportState::Ready,
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
            poll_waker: None,
            config_generation: 0,
            interrupt_kind,
            config_space,
            doorbells: VirtioDoorbells::new(doorbell_registration),
            shared_memory_size,
            saved_queue_states: vec![None; traits.max_queues as usize],
            supports_save_restore,
            guest_memory,
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

    /// Create an interrupt for a specific queue index.
    fn create_queue_interrupt(&self, idx: usize) -> Interrupt {
        let vector = self.msix_vectors[idx];
        match &self.interrupt_kind {
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
        }
    }

    /// Reset transport status and interrupt state after a failed enable or
    /// completed disable.
    fn reset_status(&mut self) {
        self.doorbells.clear();
        self.device_status = VirtioDeviceStatus::new();
        self.config_generation = 0;
        *self.interrupt_status.lock() = 0;
        if let InterruptKind::IntX(line) = &self.interrupt_kind {
            line.set_level(false);
        }
    }

    /// Apply the result of a completed transport state transition.
    /// Used by both `poll_device` and `stop` to avoid duplicating
    /// the side-effect logic.
    fn apply_transport_result(&mut self, result: TransportStateResult) {
        match result {
            TransportStateResult::EnableComplete(true) => {
                self.device_status.set_driver_ok(true);
                self.update_config_generation();
            }
            TransportStateResult::EnableComplete(false) | TransportStateResult::DisableComplete => {
                self.reset_status();
            }
        }
    }

    /// Register doorbells for all queues at BAR0's notification offset.
    fn install_doorbells(&mut self) {
        if let Some(bar0_base) = self.config_space.bar_address(0) {
            let notification_address = bar0_base + BAR0_NOTIFY_OFFSET as u64;
            for i in 0..self.events.len() {
                self.doorbells.add(
                    notification_address,
                    Some(i as u64),
                    Some(2),
                    &self.events[i],
                );
            }
        }
    }

    /// Read a transport register as a u32. Does not handle device-config
    /// registers — those are dispatched to the device task by `mmio_read`.
    fn read_u32_local(&mut self, offset: u16) -> u32 {
        assert!(offset & 3 == 0);
        let queue_select = self.queue_select as usize;
        match VirtioPciCommonCfg(offset) {
            VirtioPciCommonCfg::DEVICE_FEATURE_SELECT => self.device_feature_select,
            VirtioPciCommonCfg::DEVICE_FEATURE => {
                let feature_select = self.device_feature_select as usize;
                self.device_feature.bank(feature_select)
            }
            VirtioPciCommonCfg::DRIVER_FEATURE_SELECT => self.driver_feature_select,
            VirtioPciCommonCfg::DRIVER_FEATURE => {
                let feature_select = self.driver_feature_select as usize;
                self.driver_feature.bank(feature_select)
            }
            VirtioPciCommonCfg::MSIX_CONFIG => {
                (self.queues.len() as u32) << 16 | self.msix_config_vector as u32
            }
            VirtioPciCommonCfg::DEVICE_STATUS => {
                self.queue_select << 24 | self.config_generation << 8 | self.device_status.as_u32()
            }
            VirtioPciCommonCfg::QUEUE_SIZE => {
                let size = if queue_select < self.queues.len() {
                    self.queues[queue_select].size
                } else {
                    0
                };
                let msix_vector = self.msix_vectors.get(queue_select).copied().unwrap_or(0);
                (msix_vector as u32) << 16 | size as u32
            }
            VirtioPciCommonCfg::QUEUE_ENABLE => {
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
            VirtioPciCommonCfg::QUEUE_DESC_LO => {
                if queue_select < self.queues.len() {
                    self.queues[queue_select].desc_addr as u32
                } else {
                    0
                }
            }
            VirtioPciCommonCfg::QUEUE_DESC_HI => {
                if queue_select < self.queues.len() {
                    (self.queues[queue_select].desc_addr >> 32) as u32
                } else {
                    0
                }
            }
            VirtioPciCommonCfg::QUEUE_AVAIL_LO => {
                if queue_select < self.queues.len() {
                    self.queues[queue_select].avail_addr as u32
                } else {
                    0
                }
            }
            VirtioPciCommonCfg::QUEUE_AVAIL_HI => {
                if queue_select < self.queues.len() {
                    (self.queues[queue_select].avail_addr >> 32) as u32
                } else {
                    0
                }
            }
            VirtioPciCommonCfg::QUEUE_USED_LO => {
                if queue_select < self.queues.len() {
                    self.queues[queue_select].used_addr as u32
                } else {
                    0
                }
            }
            VirtioPciCommonCfg::QUEUE_USED_HI => {
                if queue_select < self.queues.len() {
                    (self.queues[queue_select].used_addr >> 32) as u32
                } else {
                    0
                }
            }
            VirtioPciCommonCfg(BAR0_NOTIFY_OFFSET) => 0,
            VirtioPciCommonCfg(BAR0_ISR_OFFSET) => {
                let mut interrupt_status = self.interrupt_status.lock();
                let status = *interrupt_status;
                *interrupt_status = 0;
                if let InterruptKind::IntX(line) = &self.interrupt_kind {
                    line.set_level(false)
                }
                status
            }
            _ => {
                tracing::warn!(offset, "unknown bar read");
                0xffffffff
            }
        }
    }

    /// Write a transport register as a u32. Does not handle device-config
    /// registers — those are dispatched to the device task by `mmio_write`.
    fn write_u32_local(&mut self, offset: u16, val: u32) {
        assert!(offset & 3 == 0);
        let queues_locked = self.device_status.driver_ok();
        let features_locked = queues_locked || self.device_status.features_ok();
        let queue_select = self.queue_select as usize;
        match VirtioPciCommonCfg(offset) {
            VirtioPciCommonCfg::DEVICE_FEATURE_SELECT => self.device_feature_select = val,
            VirtioPciCommonCfg::DRIVER_FEATURE_SELECT => self.driver_feature_select = val,
            VirtioPciCommonCfg::DRIVER_FEATURE => {
                let bank = self.driver_feature_select as usize;
                if features_locked || bank >= self.device_feature.len() {
                    // Update is not persisted.
                } else {
                    self.driver_feature
                        .set_bank(bank, val & self.device_feature.bank(bank));
                }
            }
            VirtioPciCommonCfg::MSIX_CONFIG => self.msix_config_vector = val as u16,
            VirtioPciCommonCfg::DEVICE_STATUS => {
                self.queue_select = val >> 16;
                let val = val & 0xff;
                if val == 0 {
                    if self.state.try_pending_reset() {
                        // An enable or disable is in flight — the
                        // reset will complete asynchronously. STATUS
                        // stays non-zero until then; the guest polls
                        // per virtio spec v1.2 §2.1.
                        return;
                    }

                    if !self.device_status.driver_ok() {
                        // Never reached DRIVER_OK, reset synchronously.
                        self.reset_status();
                    } else {
                        // Queues are active — send async teardown to task.
                        self.doorbells.clear();
                        self.state.start_disable(&self.device_sender);
                        if let Some(waker) = self.poll_waker.take() {
                            waker.wake();
                        }
                    }
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
                    if self.state.is_busy() {
                        return;
                    }
                    self.install_doorbells();

                    let features = self.driver_feature.clone();
                    let queues: Vec<_> = self
                        .queues
                        .iter()
                        .enumerate()
                        .filter(|(_, q)| q.enable)
                        .map(|(i, q)| {
                            let notify = self.create_queue_interrupt(i);
                            (
                                i as u16,
                                QueueResources {
                                    params: *q,
                                    notify,
                                    event: self.events[i].clone(),
                                    guest_memory: self.guest_memory.clone(),
                                },
                            )
                        })
                        .collect();

                    self.state
                        .start_enable(&self.device_sender, queues, features);

                    if let Some(waker) = self.poll_waker.take() {
                        waker.wake();
                    }
                }
            }
            VirtioPciCommonCfg::QUEUE_SIZE => {
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
            VirtioPciCommonCfg::QUEUE_ENABLE => {
                let val = val & 0xffff;
                if !queues_locked && queue_select < self.queues.len() {
                    let queue = &mut self.queues[queue_select];
                    queue.enable = val != 0;
                }
            }
            VirtioPciCommonCfg::QUEUE_DESC_LO => {
                if !queues_locked && queue_select < self.queues.len() {
                    let queue = &mut self.queues[queue_select];
                    queue.desc_addr = queue.desc_addr & 0xffffffff00000000 | val as u64;
                }
            }
            VirtioPciCommonCfg::QUEUE_DESC_HI => {
                if !queues_locked && queue_select < self.queues.len() {
                    let queue = &mut self.queues[queue_select];
                    queue.desc_addr = (val as u64) << 32 | queue.desc_addr & 0xffffffff;
                }
            }
            VirtioPciCommonCfg::QUEUE_AVAIL_LO => {
                if !queues_locked && queue_select < self.queues.len() {
                    let queue = &mut self.queues[queue_select];
                    queue.avail_addr = queue.avail_addr & 0xffffffff00000000 | val as u64;
                }
            }
            VirtioPciCommonCfg::QUEUE_AVAIL_HI => {
                if !queues_locked && queue_select < self.queues.len() {
                    let queue = &mut self.queues[queue_select];
                    queue.avail_addr = (val as u64) << 32 | queue.avail_addr & 0xffffffff;
                }
            }
            VirtioPciCommonCfg::QUEUE_USED_LO => {
                if !queues_locked && (queue_select) < self.queues.len() {
                    let queue = &mut self.queues[queue_select];
                    queue.used_addr = queue.used_addr & 0xffffffff00000000 | val as u64;
                }
            }
            VirtioPciCommonCfg::QUEUE_USED_HI => {
                if !queues_locked && queue_select < self.queues.len() {
                    let queue = &mut self.queues[queue_select];
                    queue.used_addr = (val as u64) << 32 | queue.used_addr & 0xffffffff;
                }
            }
            VirtioPciCommonCfg(BAR0_NOTIFY_OFFSET) => {
                if (val as usize) < self.events.len() {
                    self.events[val as usize].signal();
                }
            }
            _ => {
                tracing::warn!(offset, "unknown bar write at offset");
            }
        }
    }
}

impl VirtioPciDevice {
    /// Read a BAR register as a u32 (transport registers only, not config).
    fn read_bar_local(&mut self, bar: u8, offset: u16) -> u32 {
        match bar {
            0 => self.read_u32_local(offset),
            2 => {
                if let InterruptKind::Msix(msix) = &self.interrupt_kind {
                    msix.read_u32(offset as u64)
                } else {
                    !0
                }
            }
            _ => !0,
        }
    }

    /// Write a BAR register as a u32 (transport registers only, not config).
    fn write_bar_local(&mut self, bar: u8, offset: u16, value: u32) {
        match bar {
            0 => self.write_u32_local(offset, value),
            2 => {
                if let InterruptKind::Msix(msix) = &mut self.interrupt_kind {
                    msix.write_u32(offset as u64, value)
                }
            }
            _ => {
                tracing::warn!(bar, offset, "Unknown write");
            }
        }
    }
}

impl ChangeDeviceState for VirtioPciDevice {
    fn start(&mut self) {
        if self.device_status.driver_ok() {
            let features = self.driver_feature.clone();
            let mut queues = Vec::new();
            for (i, q) in self.queues.iter().enumerate() {
                if !q.enable {
                    continue;
                }
                let notify = self.create_queue_interrupt(i);
                let initial_state = self.saved_queue_states[i].take();
                queues.push((
                    i as u16,
                    QueueResources {
                        params: *q,
                        notify,
                        event: self.events[i].clone(),
                        guest_memory: self.guest_memory.clone(),
                    },
                    initial_state,
                ));
            }

            let params = StartParams { queues, features };

            // Fire and forget — start() is sync, can't await.
            self.device_sender
                .send(DeviceCommand::Start(Rpc::detached(params)));
        }
    }

    async fn stop(&mut self) {
        if let Some(result) = self.state.drain(&self.device_sender).await {
            self.apply_transport_result(result);
        }
        // Always send Stop to the device task; it safely handles
        // the case where no queues are running (returns None for each).
        let states = self
            .device_sender
            .call(DeviceCommand::Stop, ())
            .await
            .expect("device task is gone");
        for (i, state) in states.into_iter().enumerate() {
            self.saved_queue_states[i] = state;
        }
    }

    async fn reset(&mut self) {
        // Drain ignoring result — reset_status() below clears everything.
        let _ = self.state.drain(&self.device_sender).await;
        let _ = self.device_sender.call(DeviceCommand::Reset, ()).await;

        // reset_status() handles device_status, config_generation,
        // interrupt_status, doorbells, and interrupt line level.
        self.reset_status();

        // Destructure to ensure every field is handled; the compiler will
        // flag new fields that are not addressed here.
        let Self {
            device_sender: _,
            _device_task,
            state: _,
            device_feature: _,
            device_feature_select,
            driver_feature,
            driver_feature_select,
            msix_config_vector,
            queue_select,
            events: _,
            queues,
            msix_vectors,
            // Handled by reset_status() above.
            interrupt_status: _,
            device_status: _,
            poll_waker: _,
            config_generation: _,
            interrupt_kind: _,
            doorbells: _,
            config_space,
            shared_memory_size: _,
            saved_queue_states,
            supports_save_restore: _,
            guest_memory: _,
        } = self;

        // Reset PCI config space so BARs and command register return to
        // their power-on defaults.
        config_space.reset();

        *device_feature_select = 0;
        *driver_feature = VirtioDeviceFeatures::new();
        *driver_feature_select = 0;
        *msix_config_vector = 0;
        *queue_select = 0;
        for q in queues {
            *q = QueueParams {
                size: QUEUE_MAX_SIZE,
                ..Default::default()
            };
        }
        for v in msix_vectors {
            *v = 0;
        }
        for s in saved_queue_states {
            *s = None;
        }
    }
}

impl PollDevice for VirtioPciDevice {
    fn poll_device(&mut self, cx: &mut std::task::Context<'_>) {
        self.poll_waker = Some(cx.waker().clone());

        if let Poll::Ready(result) = self.state.poll(cx, &self.device_sender) {
            self.apply_transport_result(result);
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
        }
    }

    use super::*;
    use crate::transport::saved_state::state as common_state;
    use vmcore::save_restore::RestoreError;
    use vmcore::save_restore::SaveError;
    use vmcore::save_restore::SaveRestore;

    impl SaveRestore for VirtioPciDevice {
        type SavedState = state::SavedState;

        fn save(&mut self) -> Result<Self::SavedState, SaveError> {
            if !self.supports_save_restore {
                return Err(SaveError::NotSupported);
            }

            Ok(state::SavedState {
                common: common_state::CommonSavedState {
                    device_status: self.device_status.into(),
                    driver_feature_banks: (0..self.device_feature.len())
                        .map(|i| self.driver_feature.bank(i))
                        .collect(),
                    device_feature_select: self.device_feature_select,
                    driver_feature_select: self.driver_feature_select,
                    queue_select: self.queue_select,
                    config_generation: self.config_generation,
                    interrupt_status: *self.interrupt_status.lock(),
                },
                msix_config_vector: self.msix_config_vector,
                queues: self
                    .queues
                    .iter()
                    .enumerate()
                    .map(|(i, q)| state::SavedQueueState {
                        common: common_state::CommonQueueState {
                            size: q.size,
                            enable: q.enable,
                            desc_addr: q.desc_addr,
                            avail_addr: q.avail_addr,
                            used_addr: q.used_addr,
                            queue_state: self.saved_queue_states[i],
                        },
                        msix_vector: self.msix_vectors[i],
                    })
                    .collect(),
            })
        }

        fn restore(&mut self, state: Self::SavedState) -> Result<(), RestoreError> {
            if !self.supports_save_restore {
                return Err(RestoreError::SavedStateNotSupported);
            }

            let common = &state.common;

            crate::transport::saved_state::validate_restore(
                common,
                &self.device_feature,
                state
                    .queues
                    .iter()
                    .enumerate()
                    .map(|(i, q)| (i, q.common.size)),
                self.queues.len(),
                state.queues.len(),
                QUEUE_MAX_SIZE,
            )?;

            let new_status = VirtioDeviceStatus::from(common.device_status);

            // Restore transport fields.
            self.driver_feature = VirtioDeviceFeatures::new();
            for (i, &bank) in common.driver_feature_banks.iter().enumerate() {
                self.driver_feature.set_bank(i, bank);
            }
            self.device_feature_select = common.device_feature_select;
            self.driver_feature_select = common.driver_feature_select;
            self.queue_select = common.queue_select;
            self.config_generation = common.config_generation;
            *self.interrupt_status.lock() = common.interrupt_status;
            if let InterruptKind::IntX(line) = &self.interrupt_kind {
                line.set_level(common.interrupt_status != 0);
            }
            self.msix_config_vector = state.msix_config_vector;

            // Restore per-queue transport parameters.
            for (i, sq) in state.queues.iter().enumerate() {
                self.queues[i] = QueueParams {
                    size: sq.common.size,
                    enable: sq.common.enable,
                    desc_addr: sq.common.desc_addr,
                    avail_addr: sq.common.avail_addr,
                    used_addr: sq.common.used_addr,
                };
                self.msix_vectors[i] = sq.msix_vector;
                self.saved_queue_states[i] = sq.common.queue_state;
            }

            self.device_status = new_status;

            // Verify ephemeral runtime state.
            assert!(!self.state.is_busy());
            self.poll_waker = None;

            // Reinstall doorbells for the restored device state.
            self.doorbells.clear();
            if new_status.driver_ok() {
                self.install_doorbells();
            }

            Ok(())
        }
    }
}

impl MmioIntercept for VirtioPciDevice {
    fn mmio_read(&mut self, address: u64, data: &mut [u8]) -> IoResult {
        if let Some((bar, offset)) = self.config_space.find_bar(address) {
            let offset = offset as u16;
            // Device config — defer the entire access to the device task.
            if bar == 0 && offset >= BAR0_DEVICE_CFG_OFFSET {
                return defer_config_read(
                    &self.device_sender,
                    offset - BAR0_DEVICE_CFG_OFFSET,
                    data.len() as u8,
                );
            }
            // Transport/MSI-X registers — handle locally.
            read_as_u32_chunks(offset, data, |offset| self.read_bar_local(bar, offset));
        }
        IoResult::Ok
    }

    fn mmio_write(&mut self, address: u64, data: &[u8]) -> IoResult {
        if let Some((bar, offset)) = self.config_space.find_bar(address) {
            let offset = offset as u16;
            // Device config — defer the entire access to the device task.
            if bar == 0 && offset >= BAR0_DEVICE_CFG_OFFSET {
                return defer_config_write(
                    &self.device_sender,
                    offset - BAR0_DEVICE_CFG_OFFSET,
                    data,
                );
            }
            // Transport/MSI-X registers — handle locally.
            write_as_u32_chunks(offset, data, |offset, request_type| match request_type {
                ReadWriteRequestType::Write(value) => {
                    self.write_bar_local(bar, offset, value);
                    None
                }
                ReadWriteRequestType::Read => Some(self.read_bar_local(bar, offset)),
            });
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
