// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use crate::QUEUE_MAX_SIZE;
use crate::QueueResources;
use crate::Resources;
use crate::VirtioDevice;
use crate::VirtioDoorbells;
use crate::queue::QueueParams;
use crate::spec::VIRTIO_MMIO_INTERRUPT_STATUS_CONFIG_CHANGE;
use crate::spec::VIRTIO_MMIO_INTERRUPT_STATUS_USED_BUFFER;
use crate::spec::VirtioDeviceFeatures;
use crate::spec::VirtioDeviceStatus;
use crate::spec::mmio::VirtioMmioRegister;
use chipset_device::ChipsetDevice;
use chipset_device::io::IoResult;
use chipset_device::mmio::MmioIntercept;
use chipset_device::poll_device::PollDevice;
use device_emulators::ReadWriteRequestType;
use device_emulators::read_as_u32_chunks;
use device_emulators::write_as_u32_chunks;
use guestmem::DoorbellRegistration;
use inspect::Inspect;
use inspect::InspectMut;
use parking_lot::Mutex;
use std::fmt;
use std::ops::RangeInclusive;
use std::sync::Arc;
use vmcore::device_state::ChangeDeviceState;
use vmcore::interrupt::Interrupt;
use vmcore::line_interrupt::LineInterrupt;
use vmcore::save_restore::NoSavedState;
use vmcore::save_restore::RestoreError;
use vmcore::save_restore::SaveError;
use vmcore::save_restore::SaveRestore;

/// Run a virtio device over MMIO
#[derive(InspectMut)]
pub struct VirtioMmioDevice {
    #[inspect(skip)]
    fixed_mmio_region: (&'static str, RangeInclusive<u64>),

    #[inspect(mut)]
    device: Box<dyn VirtioDevice>,
    #[inspect(hex)]
    device_id: u32,
    #[inspect(hex)]
    vendor_id: u32,
    #[inspect(skip)]
    device_feature: VirtioDeviceFeatures,
    device_feature_select: u32,
    #[inspect(skip)]
    driver_feature: VirtioDeviceFeatures,
    driver_feature_select: u32,
    queue_select: u32,
    #[inspect(skip)]
    events: Vec<pal_event::Event>,
    #[inspect(skip)]
    queues: Vec<QueueParams>,
    device_status: VirtioDeviceStatus,
    disabling: bool,
    #[inspect(skip)]
    poll_waker: Option<std::task::Waker>,
    config_generation: u32,
    #[inspect(skip)]
    doorbells: VirtioDoorbells,
    interrupt_state: Arc<Mutex<InterruptState>>,
}

#[derive(Inspect)]
struct InterruptState {
    interrupt: LineInterrupt,
    status: u32,
}

impl InterruptState {
    fn update(&mut self, is_set: bool, bits: u32) {
        if is_set {
            self.status |= bits;
        } else {
            self.status &= !bits;
        }
        self.interrupt.set_level(self.status != 0);
    }
}

impl fmt::Debug for VirtioMmioDevice {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // TODO: implement debug print
        f.debug_struct("VirtioMmioDevice").finish()
    }
}

impl VirtioMmioDevice {
    pub fn new(
        device: Box<dyn VirtioDevice>,
        interrupt: LineInterrupt,
        doorbell_registration: Option<Arc<dyn DoorbellRegistration>>,
        mmio_gpa: u64,
        mmio_len: u64,
    ) -> Self {
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
        let interrupt_state = Arc::new(Mutex::new(InterruptState {
            interrupt,
            status: 0,
        }));

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
        Self {
            fixed_mmio_region: ("virtio-chipset", mmio_gpa..=(mmio_gpa + mmio_len - 1)),
            device,
            device_id: traits.device_id as u32,
            vendor_id: 0x1af4,
            device_feature,
            device_feature_select: 0,
            driver_feature: VirtioDeviceFeatures::new(),
            driver_feature_select: 0,
            queue_select: 0,
            events,
            queues,
            device_status: VirtioDeviceStatus::new(),
            disabling: false,
            poll_waker: None,
            config_generation: 0,
            doorbells: VirtioDoorbells::new(doorbell_registration),
            interrupt_state,
        }
    }

    fn update_config_generation(&mut self) {
        self.config_generation = self.config_generation.wrapping_add(1);
        if self.device_status.driver_ok() {
            self.interrupt_state
                .lock()
                .update(true, VIRTIO_MMIO_INTERRUPT_STATUS_CONFIG_CHANGE);
        }
    }
}

impl VirtioMmioDevice {
    pub(crate) fn read_u32(&self, address: u64) -> u32 {
        let offset = (address & 0xfff) as u16;
        assert!(offset & 3 == 0);
        match VirtioMmioRegister(offset) {
            VirtioMmioRegister::MAGIC_VALUE => u32::from_le_bytes(*b"virt"),
            VirtioMmioRegister::VERSION => 2,
            VirtioMmioRegister::DEVICE_ID => self.device_id,
            VirtioMmioRegister::VENDOR_ID => self.vendor_id,
            VirtioMmioRegister::DEVICE_FEATURES => {
                let feature_select = self.device_feature_select as usize;
                self.device_feature.bank(feature_select)
            }
            VirtioMmioRegister::DEVICE_FEATURES_SEL => self.device_feature_select,
            VirtioMmioRegister::DRIVER_FEATURES => {
                let feature_select = self.driver_feature_select as usize;
                self.driver_feature.bank(feature_select)
            }
            VirtioMmioRegister::DRIVER_FEATURES_SEL => self.driver_feature_select,
            VirtioMmioRegister::QUEUE_SEL => self.queue_select,
            // A value of zero indicates the queue is not available.
            VirtioMmioRegister::QUEUE_NUM_MAX => {
                let queue_select = self.queue_select as usize;
                if queue_select < self.queues.len() {
                    QUEUE_MAX_SIZE.into()
                } else {
                    0
                }
            }
            VirtioMmioRegister::QUEUE_NUM => {
                let queue_select = self.queue_select as usize;
                if queue_select < self.queues.len() {
                    self.queues[queue_select].size as u32
                } else {
                    0
                }
            }
            VirtioMmioRegister::QUEUE_READY => {
                let queue_select = self.queue_select as usize;
                if queue_select < self.queues.len() {
                    if self.queues[queue_select].enable {
                        1
                    } else {
                        0
                    }
                } else {
                    0
                }
            }
            VirtioMmioRegister::QUEUE_NOTIFY => 0,
            VirtioMmioRegister::INTERRUPT_STATUS => self.interrupt_state.lock().status,
            VirtioMmioRegister::INTERRUPT_ACK => 0,
            VirtioMmioRegister::STATUS => self.device_status.as_u32(),
            VirtioMmioRegister::QUEUE_DESC_LOW => {
                let queue_select = self.queue_select as usize;
                if queue_select < self.queues.len() {
                    self.queues[queue_select].desc_addr as u32
                } else {
                    0
                }
            }
            VirtioMmioRegister::QUEUE_DESC_HIGH => {
                let queue_select = self.queue_select as usize;
                if queue_select < self.queues.len() {
                    (self.queues[queue_select].desc_addr >> 32) as u32
                } else {
                    0
                }
            }
            VirtioMmioRegister::QUEUE_AVAIL_LOW => {
                let queue_select = self.queue_select as usize;
                if queue_select < self.queues.len() {
                    self.queues[queue_select].avail_addr as u32
                } else {
                    0
                }
            }
            VirtioMmioRegister::QUEUE_AVAIL_HIGH => {
                let queue_select = self.queue_select as usize;
                if queue_select < self.queues.len() {
                    (self.queues[queue_select].avail_addr >> 32) as u32
                } else {
                    0
                }
            }
            VirtioMmioRegister::QUEUE_USED_LOW => {
                let queue_select = self.queue_select as usize;
                if queue_select < self.queues.len() {
                    self.queues[queue_select].used_addr as u32
                } else {
                    0
                }
            }
            VirtioMmioRegister::QUEUE_USED_HIGH => {
                let queue_select = self.queue_select as usize;
                if queue_select < self.queues.len() {
                    (self.queues[queue_select].used_addr >> 32) as u32
                } else {
                    0
                }
            }
            VirtioMmioRegister::CONFIG_GENERATION => self.config_generation,
            VirtioMmioRegister(offset) if offset >= VirtioMmioRegister::CONFIG.0 => self
                .device
                .read_registers_u32(offset - VirtioMmioRegister::CONFIG.0),
            _ => 0xffffffff,
        }
    }

    pub(crate) fn write_u32(&mut self, address: u64, val: u32) {
        let offset = (address & 0xfff) as u16;
        assert!(offset & 3 == 0);
        let queue_select = self.queue_select as usize;
        let queues_locked = self.device_status.driver_ok();
        let features_locked = queues_locked || self.device_status.features_ok();
        match VirtioMmioRegister(offset) {
            VirtioMmioRegister::DEVICE_FEATURES_SEL => self.device_feature_select = val,
            VirtioMmioRegister::DRIVER_FEATURES => {
                let bank = self.driver_feature_select as usize;
                if features_locked || bank >= self.device_feature.len() {
                    // Update is not persisted.
                } else {
                    self.driver_feature
                        .set_bank(bank, val & self.device_feature.bank(bank));
                }
            }
            VirtioMmioRegister::DRIVER_FEATURES_SEL => self.driver_feature_select = val,
            VirtioMmioRegister::QUEUE_SEL => self.queue_select = val,
            VirtioMmioRegister::QUEUE_NUM => {
                if !queues_locked && queue_select < self.queues.len() {
                    let val = val as u16;
                    let queue = &mut self.queues[queue_select];
                    if val > QUEUE_MAX_SIZE {
                        queue.size = QUEUE_MAX_SIZE;
                    } else {
                        queue.size = val;
                    }
                }
            }
            VirtioMmioRegister::QUEUE_READY => {
                if !queues_locked && queue_select < self.queues.len() {
                    let queue = &mut self.queues[queue_select];
                    queue.enable = val != 0;
                }
            }
            VirtioMmioRegister::QUEUE_NOTIFY => {
                if (val as usize) < self.events.len() {
                    self.events[val as usize].signal();
                }
            }
            VirtioMmioRegister::INTERRUPT_ACK => {
                self.interrupt_state.lock().update(false, val);
            }
            VirtioMmioRegister::STATUS => {
                if val == 0 {
                    if self.disabling {
                        return;
                    }
                    let started = self.device_status.driver_ok();
                    self.config_generation = 0;
                    if started {
                        self.doorbells.clear();
                        // Try the fast path: poll with a noop waker to see if
                        // the device can disable synchronously.
                        let waker = std::task::Waker::noop();
                        let mut cx = std::task::Context::from_waker(waker);
                        if self.device.poll_disable(&mut cx).is_pending() {
                            self.disabling = true;
                            // Wake the real poll waker so that poll_device will
                            // re-poll with a real waker, replacing the noop one.
                            if let Some(waker) = self.poll_waker.take() {
                                waker.wake();
                            }
                            return;
                        }
                    }
                    // Fast path: disable completed synchronously.
                    self.device_status = VirtioDeviceStatus::new();
                    self.interrupt_state.lock().update(false, !0);
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
                    let notification_address =
                        (address & !0xfff) + VirtioMmioRegister::QUEUE_NOTIFY.0 as u64;
                    for i in 0..self.events.len() {
                        self.doorbells.add(
                            notification_address,
                            Some(i as u64),
                            Some(4),
                            &self.events[i],
                        );
                    }
                    let queues = self
                        .queues
                        .iter()
                        .zip(self.events.iter().cloned())
                        .map(|(queue, event)| {
                            let interrupt_state = self.interrupt_state.clone();
                            let notify = Interrupt::from_fn(move || {
                                interrupt_state
                                    .lock()
                                    .update(true, VIRTIO_MMIO_INTERRUPT_STATUS_USED_BUFFER);
                            });
                            QueueResources {
                                params: *queue,
                                notify,
                                event,
                            }
                        })
                        .collect();

                    match self.device.enable(Resources {
                        features: self.driver_feature.clone(),
                        queues,
                        shared_memory_region: None,
                        shared_memory_size: 0,
                    }) {
                        Ok(()) => {
                            self.device_status.set_driver_ok(true);
                        }
                        Err(err) => {
                            self.doorbells.clear();
                            // FUTURE: consider setting DEVICE_NEEDS_RESET and
                            // delivering a config change interrupt so the guest
                            // can detect the failure proactively instead of
                            // waiting for IO timeouts.
                            tracelimit::error_ratelimited!(
                                error = &*err as &dyn std::error::Error,
                                "virtio device enable failed"
                            );
                        }
                    }
                    self.update_config_generation();
                }
            }
            VirtioMmioRegister::QUEUE_DESC_LOW => {
                if !queues_locked && queue_select < self.queues.len() {
                    let queue = &mut self.queues[queue_select];
                    queue.desc_addr = queue.desc_addr & 0xffffffff00000000 | val as u64;
                }
            }
            VirtioMmioRegister::QUEUE_DESC_HIGH => {
                if !queues_locked && queue_select < self.queues.len() {
                    let queue = &mut self.queues[queue_select];
                    queue.desc_addr = (val as u64) << 32 | queue.desc_addr & 0xffffffff;
                }
            }
            VirtioMmioRegister::QUEUE_AVAIL_LOW => {
                if !queues_locked && queue_select < self.queues.len() {
                    let queue = &mut self.queues[queue_select];
                    queue.avail_addr = queue.avail_addr & 0xffffffff00000000 | val as u64;
                }
            }
            VirtioMmioRegister::QUEUE_AVAIL_HIGH => {
                if !queues_locked && queue_select < self.queues.len() {
                    let queue = &mut self.queues[queue_select];
                    queue.avail_addr = (val as u64) << 32 | queue.avail_addr & 0xffffffff;
                }
            }
            VirtioMmioRegister::QUEUE_USED_LOW => {
                if !queues_locked && (queue_select) < self.queues.len() {
                    let queue = &mut self.queues[queue_select];
                    queue.used_addr = queue.used_addr & 0xffffffff00000000 | val as u64;
                }
            }
            VirtioMmioRegister::QUEUE_USED_HIGH => {
                if !queues_locked && queue_select < self.queues.len() {
                    let queue = &mut self.queues[queue_select];
                    queue.used_addr = (val as u64) << 32 | queue.used_addr & 0xffffffff;
                }
            }
            VirtioMmioRegister(offset) if offset >= VirtioMmioRegister::CONFIG.0 => self
                .device
                .write_registers_u32(offset - VirtioMmioRegister::CONFIG.0, val),
            _ => (),
        }
    }
}

impl ChangeDeviceState for VirtioMmioDevice {
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
        self.interrupt_state.lock().update(false, !0);
    }
}

impl PollDevice for VirtioMmioDevice {
    fn poll_device(&mut self, cx: &mut std::task::Context<'_>) {
        self.poll_waker = Some(cx.waker().clone());
        if self.disabling {
            if self.device.poll_disable(cx).is_ready() {
                self.device_status = VirtioDeviceStatus::new();
                self.disabling = false;
                self.interrupt_state.lock().update(false, !0);
            }
        }
    }
}

impl ChipsetDevice for VirtioMmioDevice {
    fn supports_mmio(&mut self) -> Option<&mut dyn MmioIntercept> {
        Some(self)
    }

    fn supports_poll_device(&mut self) -> Option<&mut dyn PollDevice> {
        Some(self)
    }
}

impl SaveRestore for VirtioMmioDevice {
    type SavedState = NoSavedState; // TODO

    fn save(&mut self) -> Result<Self::SavedState, SaveError> {
        Ok(NoSavedState)
    }

    fn restore(&mut self, NoSavedState: Self::SavedState) -> Result<(), RestoreError> {
        Ok(())
    }
}

impl MmioIntercept for VirtioMmioDevice {
    fn mmio_read(&mut self, address: u64, data: &mut [u8]) -> IoResult {
        read_as_u32_chunks(address, data, |address| self.read_u32(address));
        IoResult::Ok
    }

    fn mmio_write(&mut self, address: u64, data: &[u8]) -> IoResult {
        write_as_u32_chunks(address, data, |address, request_type| match request_type {
            ReadWriteRequestType::Write(value) => {
                self.write_u32(address, value);
                None
            }
            ReadWriteRequestType::Read => Some(self.read_u32(address)),
        });
        IoResult::Ok
    }

    fn get_static_regions(&mut self) -> &[(&str, RangeInclusive<u64>)] {
        std::slice::from_ref(&self.fixed_mmio_region)
    }
}
