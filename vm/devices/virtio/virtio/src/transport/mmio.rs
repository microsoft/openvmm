// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use crate::queue::QueueParams;
use crate::spec::*;
use crate::QueueResources;
use crate::Resources;
use crate::VirtioDevice;
use crate::VirtioDoorbells;
use crate::QUEUE_MAX_SIZE;
use chipset_device::io::IoResult;
use chipset_device::mmio::MmioIntercept;
use chipset_device::ChipsetDevice;
use device_emulators::read_as_u32_chunks;
use device_emulators::write_as_u32_chunks;
use device_emulators::ReadWriteRequestType;
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
    device_feature: [u32; 2],
    device_feature_select: u32,
    driver_feature: [u32; 2],
    driver_feature_select: u32,
    queue_select: u32,
    events: Vec<pal_event::Event>,
    queues: Vec<QueueParams>,
    device_status: u32,
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

        Self {
            fixed_mmio_region: ("virtio-chipset", mmio_gpa..=(mmio_gpa + mmio_len - 1)),
            device,
            device_id: traits.device_id as u32,
            vendor_id: 0x1af4,
            device_feature: [
                traits.device_features as u32
                    | VIRTIO_F_RING_EVENT_IDX
                    | VIRTIO_F_RING_INDIRECT_DESC,
                (traits.device_features >> 32) as u32 | VIRTIO_F_VERSION_1,
            ],
            device_feature_select: 0,
            driver_feature: [0; 2],
            driver_feature_select: 0,
            queue_select: 0,
            events,
            queues,
            device_status: 0,
            config_generation: 0,
            doorbells: VirtioDoorbells::new(doorbell_registration),
            interrupt_state,
        }
    }

    fn update_config_generation(&mut self) {
        self.config_generation = self.config_generation.wrapping_add(1);
        if self.device_status & VIRTIO_DRIVER_OK != 0 {
            self.interrupt_state
                .lock()
                .update(true, VIRTIO_MMIO_INTERRUPT_STATUS_CONFIG_CHANGE);
        }
    }
}

impl Drop for VirtioMmioDevice {
    fn drop(&mut self) {
        self.device.disable();
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
                if feature_select < self.device_feature.len() {
                    self.device_feature[feature_select]
                } else {
                    0
                }
            }
            // Device feature bank index
            20 => self.device_feature_select,
            //
            // 8-byte padding
            //
            // Driver feature bank
            32 => {
                let feature_select = self.driver_feature_select as usize;
                if feature_select < self.driver_feature.len() {
                    self.driver_feature[feature_select]
                } else {
                    0
                }
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
            112 => self.device_status,
            //
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
        let queues_locked = self.device_status & VIRTIO_DRIVER_OK != 0;
        let features_locked = queues_locked || self.device_status & VIRTIO_FEATURES_OK != 0;
        match offset {
            // Device feature bank index
            20 => self.device_feature_select = val,
            // Driver feature bank
            32 => {
                let bank = self.driver_feature_select as usize;
                if !features_locked && bank < self.driver_feature.len() {
                    self.driver_feature[bank] = val & self.device_feature[bank];
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
                    let started = (self.device_status & VIRTIO_DRIVER_OK) != 0;
                    self.device_status = 0;
                    self.config_generation = 0;
                    if started {
                        self.doorbells.clear();
                        self.device.disable();
                    }
                    self.interrupt_state.lock().update(false, !0);
                }

                self.device_status |= val & (VIRTIO_ACKNOWLEDGE | VIRTIO_DRIVER | VIRTIO_FAILED);

                if self.device_status & VIRTIO_FEATURES_OK == 0 && val & VIRTIO_FEATURES_OK != 0 {
                    self.device_status |= VIRTIO_FEATURES_OK;
                    self.update_config_generation();
                }

                if self.device_status & VIRTIO_DRIVER_OK == 0 && val & VIRTIO_DRIVER_OK != 0 {
                    let features =
                        ((self.driver_feature[1] as u64) << 32) | self.driver_feature[0] as u64;

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
                        features,
                        queues,
                        shared_memory_region: None,
                        shared_memory_size: 0,
                    });

                    self.device_status |= VIRTIO_DRIVER_OK;
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
        // TODO
    }
}

impl ChipsetDevice for VirtioMmioDevice {
    fn supports_mmio(&mut self) -> Option<&mut dyn MmioIntercept> {
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
