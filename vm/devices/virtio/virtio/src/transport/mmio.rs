// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use super::StalledIo;
use super::core::TransportOps;
use super::core::VirtioTransportCore;
use super::task::defer_config_read;
use super::task::defer_config_write;
use crate::DynVirtioDevice;
use crate::MAX_QUEUE_SIZE;
use crate::spec::VIRTIO_MMIO_INTERRUPT_STATUS_CONFIG_CHANGE;
use crate::spec::VIRTIO_MMIO_INTERRUPT_STATUS_USED_BUFFER;
use crate::spec::mmio::VirtioMmioRegister;
use chipset_device::ChipsetDevice;
use chipset_device::io::IoResult;
use chipset_device::io::deferred::defer_read;
use chipset_device::io::deferred::defer_write;
use chipset_device::mmio::MmioIntercept;
use chipset_device::poll_device::PollDevice;
use device_emulators::ReadWriteRequestType;
use device_emulators::read_as_u32_chunks;
use device_emulators::write_as_u32_chunks;
use guestmem::DoorbellRegistration;
use guestmem::GuestMemory;
use inspect::Inspect;
use inspect::InspectMut;
use pal_async::task::Spawn;
use parking_lot::Mutex;
use std::fmt;
use std::ops::RangeInclusive;
use std::sync::Arc;
use vmcore::device_state::ChangeDeviceState;
use vmcore::interrupt::Interrupt;
use vmcore::line_interrupt::LineInterrupt;

/// MMIO-specific transport state.
#[derive(Inspect)]
struct MmioTransport {
    #[inspect(skip)]
    fixed_mmio_region: (&'static str, RangeInclusive<u64>),
    #[inspect(hex)]
    device_id: u32,
    #[inspect(hex)]
    vendor_id: u32,
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

impl TransportOps for MmioTransport {
    fn create_queue_interrupt(&mut self, _idx: usize, _msix_vector: u16) -> Interrupt {
        let interrupt_state = self.interrupt_state.clone();
        Interrupt::from_fn(move || {
            interrupt_state
                .lock()
                .update(true, VIRTIO_MMIO_INTERRUPT_STATUS_USED_BUFFER);
        })
    }

    fn signal_config_change(&mut self) {
        self.interrupt_state
            .lock()
            .update(true, VIRTIO_MMIO_INTERRUPT_STATUS_CONFIG_CHANGE);
    }

    fn reset_interrupts(&mut self) {
        self.interrupt_state.lock().update(false, !0);
    }

    fn doorbell_region(&mut self) -> Option<(u64, u32)> {
        let base = (*self.fixed_mmio_region.1.start() & !0xfff)
            + VirtioMmioRegister::QUEUE_NOTIFY.0 as u64;
        Some((base, 4))
    }
}

/// Run a virtio device over MMIO
#[derive(InspectMut)]
pub struct VirtioMmioDevice {
    #[inspect(flatten)]
    core: VirtioTransportCore,
    #[inspect(flatten)]
    mmio: MmioTransport,
}

impl fmt::Debug for VirtioMmioDevice {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("VirtioMmioDevice").finish()
    }
}

impl VirtioMmioDevice {
    pub fn new(
        device: Box<dyn DynVirtioDevice>,
        driver: &impl Spawn,
        guest_memory: GuestMemory,
        interrupt: LineInterrupt,
        doorbell_registration: Option<Arc<dyn DoorbellRegistration>>,
        mmio_gpa: u64,
        mmio_len: u64,
    ) -> std::io::Result<Self> {
        let traits = device.traits();
        let interrupt_state = Arc::new(Mutex::new(InterruptState {
            interrupt,
            status: 0,
        }));

        let core = VirtioTransportCore::new(device, driver, guest_memory, doorbell_registration)?;

        Ok(Self {
            core,
            mmio: MmioTransport {
                fixed_mmio_region: ("virtio-chipset", mmio_gpa..=(mmio_gpa + mmio_len - 1)),
                device_id: traits.device_id.0 as u32,
                vendor_id: 0x1af4,
                interrupt_state,
            },
        })
    }

    /// Synchronous transport register read for tests.
    #[cfg(test)]
    pub(crate) fn read_u32(&mut self, address: u64) -> u32 {
        self.read_u32_local((address & 0xfff) as u16)
    }

    /// Synchronous transport register write for tests.
    #[cfg(test)]
    pub(crate) fn write_u32(&mut self, address: u64, val: u32) {
        self.write_u32_local((address & 0xfff) as u16, val);
    }

    /// Read a transport register as a u32.
    fn read_u32_local(&mut self, offset: u16) -> u32 {
        assert!(offset & 3 == 0);
        let queue_select = self.core.queue_select as usize;
        match VirtioMmioRegister(offset) {
            VirtioMmioRegister::MAGIC_VALUE => u32::from_le_bytes(*b"virt"),
            VirtioMmioRegister::VERSION => 2,
            VirtioMmioRegister::DEVICE_ID => self.mmio.device_id,
            VirtioMmioRegister::VENDOR_ID => self.mmio.vendor_id,
            VirtioMmioRegister::DEVICE_FEATURES => self
                .core
                .device_feature
                .bank(self.core.device_feature_select as usize),
            VirtioMmioRegister::DEVICE_FEATURES_SEL => self.core.device_feature_select,
            VirtioMmioRegister::DRIVER_FEATURES => self
                .core
                .driver_feature
                .bank(self.core.driver_feature_select as usize),
            VirtioMmioRegister::DRIVER_FEATURES_SEL => self.core.driver_feature_select,
            VirtioMmioRegister::QUEUE_SEL => self.core.queue_select,
            VirtioMmioRegister::QUEUE_NUM_MAX => self
                .core
                .queues
                .get(queue_select)
                .map_or(0, |qd| qd.initial_size.into()),
            VirtioMmioRegister::QUEUE_NUM => self
                .core
                .queues
                .get(queue_select)
                .map_or(0, |qd| qd.params.size as u32),
            VirtioMmioRegister::QUEUE_READY => {
                self.core
                    .queues
                    .get(queue_select)
                    .is_some_and(|qd| qd.params.enable) as u32
            }
            VirtioMmioRegister::QUEUE_NOTIFY => 0,
            VirtioMmioRegister::INTERRUPT_STATUS => self.mmio.interrupt_state.lock().status,
            VirtioMmioRegister::INTERRUPT_ACK => 0,
            VirtioMmioRegister::STATUS => self.core.device_status.as_u32(),
            VirtioMmioRegister::QUEUE_DESC_LOW => self
                .core
                .queues
                .get(queue_select)
                .map_or(0, |qd| qd.params.desc_addr as u32),
            VirtioMmioRegister::QUEUE_DESC_HIGH => self
                .core
                .queues
                .get(queue_select)
                .map_or(0, |qd| (qd.params.desc_addr >> 32) as u32),
            VirtioMmioRegister::QUEUE_AVAIL_LOW => self
                .core
                .queues
                .get(queue_select)
                .map_or(0, |qd| qd.params.avail_addr as u32),
            VirtioMmioRegister::QUEUE_AVAIL_HIGH => self
                .core
                .queues
                .get(queue_select)
                .map_or(0, |qd| (qd.params.avail_addr >> 32) as u32),
            VirtioMmioRegister::QUEUE_USED_LOW => self
                .core
                .queues
                .get(queue_select)
                .map_or(0, |qd| qd.params.used_addr as u32),
            VirtioMmioRegister::QUEUE_USED_HIGH => self
                .core
                .queues
                .get(queue_select)
                .map_or(0, |qd| (qd.params.used_addr >> 32) as u32),
            VirtioMmioRegister::CONFIG_GENERATION => self.core.config_generation,
            _ => 0xffffffff,
        }
    }

    /// Write a transport register as a u32.
    fn write_u32_local(&mut self, offset: u16, val: u32) {
        assert!(offset & 3 == 0);
        let queue_select = self.core.queue_select as usize;
        let queues_locked = self.core.device_status.driver_ok();
        let features_locked = queues_locked || self.core.device_status.features_ok();
        match VirtioMmioRegister(offset) {
            VirtioMmioRegister::DEVICE_FEATURES_SEL => self.core.device_feature_select = val,
            VirtioMmioRegister::DRIVER_FEATURES => {
                let bank = self.core.driver_feature_select as usize;
                if !features_locked && bank < 2 {
                    self.core
                        .driver_feature
                        .set_bank(bank, val & self.core.device_feature.bank(bank));
                }
            }
            VirtioMmioRegister::DRIVER_FEATURES_SEL => self.core.driver_feature_select = val,
            VirtioMmioRegister::QUEUE_SEL => self.core.queue_select = val,
            VirtioMmioRegister::QUEUE_NUM => {
                if !queues_locked && queue_select < self.core.queues.len() {
                    let val = val as u16;
                    let queue = &mut self.core.queues[queue_select].params;
                    if val > MAX_QUEUE_SIZE {
                        queue.size = MAX_QUEUE_SIZE;
                    } else {
                        queue.size = val;
                    }
                }
            }
            VirtioMmioRegister::QUEUE_READY => {
                if !queues_locked && queue_select < self.core.queues.len() {
                    self.core.queues[queue_select].params.enable = val != 0;
                }
            }
            VirtioMmioRegister::QUEUE_NOTIFY => {
                self.core.notify_queue(val);
            }
            VirtioMmioRegister::INTERRUPT_ACK => {
                self.mmio.interrupt_state.lock().update(false, val);
            }
            VirtioMmioRegister::STATUS => {
                self.core.write_device_status(&mut self.mmio, val as u8);
            }
            VirtioMmioRegister::QUEUE_DESC_LOW => {
                if !queues_locked && queue_select < self.core.queues.len() {
                    let queue = &mut self.core.queues[queue_select].params;
                    queue.desc_addr = queue.desc_addr & 0xffffffff00000000 | val as u64;
                }
            }
            VirtioMmioRegister::QUEUE_DESC_HIGH => {
                if !queues_locked && queue_select < self.core.queues.len() {
                    let queue = &mut self.core.queues[queue_select].params;
                    queue.desc_addr = (val as u64) << 32 | queue.desc_addr & 0xffffffff;
                }
            }
            VirtioMmioRegister::QUEUE_AVAIL_LOW => {
                if !queues_locked && queue_select < self.core.queues.len() {
                    let queue = &mut self.core.queues[queue_select].params;
                    queue.avail_addr = queue.avail_addr & 0xffffffff00000000 | val as u64;
                }
            }
            VirtioMmioRegister::QUEUE_AVAIL_HIGH => {
                if !queues_locked && queue_select < self.core.queues.len() {
                    let queue = &mut self.core.queues[queue_select].params;
                    queue.avail_addr = (val as u64) << 32 | queue.avail_addr & 0xffffffff;
                }
            }
            VirtioMmioRegister::QUEUE_USED_LOW => {
                if !queues_locked && queue_select < self.core.queues.len() {
                    let queue = &mut self.core.queues[queue_select].params;
                    queue.used_addr = queue.used_addr & 0xffffffff00000000 | val as u64;
                }
            }
            VirtioMmioRegister::QUEUE_USED_HIGH => {
                if !queues_locked && queue_select < self.core.queues.len() {
                    let queue = &mut self.core.queues[queue_select].params;
                    queue.used_addr = (val as u64) << 32 | queue.used_addr & 0xffffffff;
                }
            }
            _ => (),
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
                    let mut buf = vec![0u8; len];
                    self.read_transport((address & 0xfff) as u16, &mut buf);
                    deferred.complete(&buf);
                }
                StalledIo::Write {
                    address,
                    data,
                    len,
                    deferred,
                } => {
                    self.write_transport((address & 0xfff) as u16, &data[..len]);
                    if self.core.state.is_busy() {
                        self.core.pending_status_deferred = Some(deferred);
                        break;
                    }
                    deferred.complete();
                }
            }
        }
        self.core.stalled_io = iter.collect();
    }
}

impl ChangeDeviceState for VirtioMmioDevice {
    fn start(&mut self) {
        self.core.start(&mut self.mmio);
    }

    async fn stop(&mut self) {
        self.core.stop(&mut self.mmio).await;
    }

    async fn reset(&mut self) {
        self.core.reset(&mut self.mmio).await;
    }
}

impl PollDevice for VirtioMmioDevice {
    fn poll_device(&mut self, cx: &mut std::task::Context<'_>) {
        self.core.poll_device(&mut self.mmio, cx);
        if !self.core.stalled_io.is_empty() && !self.core.state.is_busy() {
            self.replay_stalled_io();
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

mod saved_state {
    mod state {
        use crate::transport::saved_state::state::CommonQueueState;
        use crate::transport::saved_state::state::CommonSavedState;
        use mesh::payload::Protobuf;
        use vmcore::save_restore::SavedStateRoot;

        #[derive(Protobuf)]
        #[mesh(package = "virtio.transport.mmio")]
        pub struct SavedQueueState {
            #[mesh(1)]
            pub common: CommonQueueState,
        }

        #[derive(Protobuf, SavedStateRoot)]
        #[mesh(package = "virtio.transport.mmio")]
        pub struct SavedState {
            #[mesh(1)]
            pub common: CommonSavedState,
            #[mesh(2)]
            pub queues: Vec<SavedQueueState>,
            #[mesh(3)]
            pub interrupt_status: u32,
        }
    }

    use super::*;
    use vmcore::save_restore::SaveRestore;

    impl SaveRestore for VirtioMmioDevice {
        type SavedState = state::SavedState;

        fn save(&mut self) -> Result<Self::SavedState, vmcore::save_restore::SaveError> {
            Ok(state::SavedState {
                common: self.core.save_common()?,
                queues: (0..self.core.queues.len())
                    .map(|i| state::SavedQueueState {
                        common: self.core.save_queue_common(i),
                    })
                    .collect(),
                interrupt_status: self.mmio.interrupt_state.lock().status,
            })
        }

        fn restore(
            &mut self,
            state: Self::SavedState,
        ) -> Result<(), vmcore::save_restore::RestoreError> {
            let saved_queue_count = state.queues.len();
            self.core.restore_common(
                &mut self.mmio,
                &state.common,
                state.queues.into_iter().map(|sq| (sq.common, 0)),
                saved_queue_count,
            )?;

            // Restore MMIO-specific interrupt state.
            {
                let mut is = self.mmio.interrupt_state.lock();
                is.status = state.interrupt_status;
                is.interrupt.set_level(is.status != 0);
            }

            Ok(())
        }
    }
}

impl MmioIntercept for VirtioMmioDevice {
    fn mmio_read(&mut self, address: u64, data: &mut [u8]) -> IoResult {
        let offset = (address & 0xfff) as u16;
        if offset >= VirtioMmioRegister::CONFIG.0 {
            return defer_config_read(
                &self.core.device_sender,
                offset - VirtioMmioRegister::CONFIG.0,
                data.len() as u8,
            );
        }
        if self.core.state.is_busy() {
            let (deferred, token) = defer_read();
            self.core.stalled_io.push(StalledIo::Read {
                address,
                len: data.len(),
                deferred,
            });
            return IoResult::Defer(token);
        }
        self.read_transport(offset, data);
        IoResult::Ok
    }

    fn mmio_write(&mut self, address: u64, data: &[u8]) -> IoResult {
        let offset = (address & 0xfff) as u16;
        if offset >= VirtioMmioRegister::CONFIG.0 {
            return defer_config_write(
                &self.core.device_sender,
                offset - VirtioMmioRegister::CONFIG.0,
                data,
            );
        }
        if self.core.state.is_busy() {
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
        self.write_transport(offset, data);
        if self.core.state.is_busy() {
            let (deferred, token) = defer_write();
            self.core.pending_status_deferred = Some(deferred);
            return IoResult::Defer(token);
        }
        IoResult::Ok
    }

    fn get_static_regions(&mut self) -> &[(&str, RangeInclusive<u64>)] {
        std::slice::from_ref(&self.mmio.fixed_mmio_region)
    }
}
