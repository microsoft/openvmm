// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use crate::QUEUE_MAX_SIZE;
use crate::QueueResources;
use crate::Resources;
use crate::VirtioDevice;
use crate::VirtioDoorbells;
use crate::queue::QueueParams;
use crate::spec::*;
use chipset_device::ChipsetDevice;
use chipset_device::io::IoResult;
use chipset_device::mmio::MmioIntercept;
use chipset_device::poll_device::PollDevice;
use device_emulators::ReadWriteRequestType;
use device_emulators::read_as_u32_chunks;
use device_emulators::write_as_u32_chunks;
use guestmem::DoorbellRegistration;
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
pub struct VirtioMmioDevice {
    fixed_mmio_region: (&'static str, RangeInclusive<u64>),

    device: Box<dyn VirtioDevice>,
    device_id: u32,
    vendor_id: u32,
    device_feature: VirtioDeviceFeatures,
    device_feature_select: u32,
    driver_feature: VirtioDeviceFeatures,
    driver_feature_select: u32,
    queue_select: u32,
    events: Vec<pal_event::Event>,
    queues: Vec<QueueParams>,
    device_status: VirtioDeviceStatus,
    disabling: bool,
    config_generation: u32,
    doorbells: VirtioDoorbells,
    interrupt_state: Arc<Mutex<InterruptState>>,
}

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

impl InspectMut for VirtioMmioDevice {
    fn inspect_mut(&mut self, _req: inspect::Request<'_>) {
        // TODO
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
        match offset {
            // Magic value
            0 => u32::from_le_bytes(*b"virt"),
            // Version
            4 => 2,
            // Device ID
            8 => self.device_id,
            // Vendor ID
            12 => self.vendor_id,
            // Device feature bank
            16 => {
                let feature_select = self.device_feature_select as usize;
                self.device_feature.bank(feature_select)
            }
            // Device feature bank index
            20 => self.device_feature_select,
            //
            // 8-byte padding
            //
            // Driver feature bank
            32 => {
                let feature_select = self.driver_feature_select as usize;
                self.driver_feature.bank(feature_select)
            }
            // Driver feature bank index
            36 => self.driver_feature_select,
            //
            // 8-byte padding
            //
            // Queue select index
            48 => self.queue_select,
            // Current queue max supported size. A value of zero indicates the queue is not available.
            52 => {
                let queue_select = self.queue_select as usize;
                if queue_select < self.queues.len() {
                    QUEUE_MAX_SIZE.into()
                } else {
                    0
                }
            }
            // Current queue size
            56 => {
                let queue_select = self.queue_select as usize;
                if queue_select < self.queues.len() {
                    self.queues[queue_select].size as u32
                } else {
                    0
                }
            }
            //
            // 8-byte padding
            //
            // Current queue enabled
            68 => {
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
            //
            // 8-byte padding
            //
            // Queue notification register
            80 => 0,
            //
            // 12-byte padding
            //
            // Interrupt status
            96 => self.interrupt_state.lock().status,
            // Interrupt ACK
            100 => 0,
            //
            // 8-byte padding
            //
            // Device status
            112 => self.device_status.as_u32(),
            // 12-byte padding
            //
            // Queue descriptor table address (low part)
            128 => {
                let queue_select = self.queue_select as usize;
                if queue_select < self.queues.len() {
                    self.queues[queue_select].desc_addr as u32
                } else {
                    0
                }
            }
            // Queue descriptor table address (high part)
            132 => {
                let queue_select = self.queue_select as usize;
                if queue_select < self.queues.len() {
                    (self.queues[queue_select].desc_addr >> 32) as u32
                } else {
                    0
                }
            }
            //
            // 8-byte padding
            //
            // Queue descriptor available ring address (low part)
            144 => {
                let queue_select = self.queue_select as usize;
                if queue_select < self.queues.len() {
                    self.queues[queue_select].avail_addr as u32
                } else {
                    0
                }
            }
            // Queue descriptor available ring address (high part)
            148 => {
                let queue_select = self.queue_select as usize;
                if queue_select < self.queues.len() {
                    (self.queues[queue_select].avail_addr >> 32) as u32
                } else {
                    0
                }
            }
            //
            // 8-byte padding
            //
            // Queue descriptor used ring address (low part)
            160 => {
                let queue_select = self.queue_select as usize;
                if queue_select < self.queues.len() {
                    self.queues[queue_select].used_addr as u32
                } else {
                    0
                }
            }
            // Queue descriptor used ring address (high part)
            164 => {
                let queue_select = self.queue_select as usize;
                if queue_select < self.queues.len() {
                    (self.queues[queue_select].used_addr >> 32) as u32
                } else {
                    0
                }
            }
            0xfc => self.config_generation,
            offset if offset >= 0x100 => self.device.read_registers_u32(offset - 0x100),
            _ => 0xffffffff,
        }
    }

    pub(crate) fn write_u32(&mut self, address: u64, val: u32) {
        let offset = (address & 0xfff) as u16;
        assert!(offset & 3 == 0);
        let queue_select = self.queue_select as usize;
        let queues_locked = self.device_status.driver_ok();
        let features_locked = queues_locked || self.device_status.features_ok();
        match offset {
            // Device feature bank index
            20 => self.device_feature_select = val,
            // Driver feature bank
            32 => {
                let bank = self.driver_feature_select as usize;
                if features_locked || bank >= self.device_feature.len() {
                    // Update is not persisted.
                } else {
                    self.driver_feature
                        .set_bank(bank, val & self.device_feature.bank(bank));
                }
            }
            // Driver feature bank index
            36 => self.driver_feature_select = val,
            // Queue select index
            48 => self.queue_select = val,
            // Queue current size
            56 => {
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
            // Current queue enabled
            68 => {
                if !queues_locked && queue_select < self.queues.len() {
                    let queue = &mut self.queues[queue_select];
                    queue.enable = val != 0;
                }
            }
            // Queue notification register
            80 => {
                if (val as usize) < self.events.len() {
                    self.events[val as usize].signal();
                }
            }
            // Interrupt ACK
            100 => {
                self.interrupt_state.lock().update(false, val);
            }
            // Device status
            112 => {
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
                    let notification_address = (address & !0xfff) + 80;
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

                    self.device.enable(Resources {
                        features: self.driver_feature.clone(),
                        queues,
                        shared_memory_region: None,
                        shared_memory_size: 0,
                    });

                    self.device_status.set_driver_ok(true);
                    self.update_config_generation();
                }
            }
            // Queue descriptor table address (low part)
            128 => {
                if !queues_locked && queue_select < self.queues.len() {
                    let queue = &mut self.queues[queue_select];
                    queue.desc_addr = queue.desc_addr & 0xffffffff00000000 | val as u64;
                }
            }
            // Queue descriptor table address (high part)
            132 => {
                if !queues_locked && queue_select < self.queues.len() {
                    let queue = &mut self.queues[queue_select];
                    queue.desc_addr = (val as u64) << 32 | queue.desc_addr & 0xffffffff;
                }
            }
            // Queue descriptor available ring address (low part)
            144 => {
                if !queues_locked && queue_select < self.queues.len() {
                    let queue = &mut self.queues[queue_select];
                    queue.avail_addr = queue.avail_addr & 0xffffffff00000000 | val as u64;
                }
            }
            // Queue descriptor available ring address (high part)
            148 => {
                if !queues_locked && queue_select < self.queues.len() {
                    let queue = &mut self.queues[queue_select];
                    queue.avail_addr = (val as u64) << 32 | queue.avail_addr & 0xffffffff;
                }
            }
            // Queue descriptor used ring address (low part)
            160 => {
                if !queues_locked && (queue_select) < self.queues.len() {
                    let queue = &mut self.queues[queue_select];
                    queue.used_addr = queue.used_addr & 0xffffffff00000000 | val as u64;
                }
            }
            // Queue descriptor used ring address (high part)
            164 => {
                if !queues_locked && queue_select < self.queues.len() {
                    let queue = &mut self.queues[queue_select];
                    queue.used_addr = (val as u64) << 32 | queue.used_addr & 0xffffffff;
                }
            }
            offset if offset >= 0x100 => self.device.write_registers_u32(offset - 0x100, val),
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
