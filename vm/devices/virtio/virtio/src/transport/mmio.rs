// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

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
use guestmem::GuestMemory;
use inspect::Inspect;
use inspect::InspectMut;
use mesh::rpc::Rpc;
use mesh::rpc::RpcSend;
use pal_async::task::Spawn;
use pal_async::task::Task;
use parking_lot::Mutex;
use std::fmt;
use std::ops::RangeInclusive;
use std::sync::Arc;
use std::task::Poll;
use vmcore::device_state::ChangeDeviceState;
use vmcore::interrupt::Interrupt;
use vmcore::line_interrupt::LineInterrupt;

/// Run a virtio device over MMIO
#[derive(InspectMut)]
pub struct VirtioMmioDevice {
    #[inspect(skip)]
    fixed_mmio_region: (&'static str, RangeInclusive<u64>),

    #[inspect(rename = "device", send = "DeviceCommand::Inspect")]
    device_sender: mesh::Sender<DeviceCommand>,
    #[inspect(skip)]
    _device_task: Task<()>,
    state: TransportState,
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
    #[inspect(iter_by_index)]
    queues: Vec<QueueParams>,
    device_status: VirtioDeviceStatus,
    #[inspect(skip)]
    poll_waker: Option<std::task::Waker>,
    config_generation: u32,
    #[inspect(skip)]
    doorbells: VirtioDoorbells,
    interrupt_state: Arc<Mutex<InterruptState>>,
    /// Cached queue states from `ChangeDeviceState::stop()` for resume.
    #[inspect(skip)]
    saved_queue_states: Vec<Option<QueueState>>,
    supports_save_restore: bool,
    #[inspect(skip)]
    guest_memory: GuestMemory,
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
        device: Box<dyn DynVirtioDevice>,
        driver: &impl Spawn,
        guest_memory: GuestMemory,
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

        let supports_save_restore = device.supports_save_restore();
        let (sender, receiver) = mesh::channel();
        let _device_task = driver.spawn("virtio-device-task", async move {
            run_device_task(device, receiver).await;
        });

        Self {
            fixed_mmio_region: ("virtio-chipset", mmio_gpa..=(mmio_gpa + mmio_len - 1)),
            device_sender: sender,
            _device_task,
            state: TransportState::Ready,
            device_id: traits.device_id.0 as u32,
            vendor_id: 0x1af4,
            device_feature,
            device_feature_select: 0,
            driver_feature: VirtioDeviceFeatures::new(),
            driver_feature_select: 0,
            queue_select: 0,
            events,
            queues,
            device_status: VirtioDeviceStatus::new(),
            poll_waker: None,
            config_generation: 0,
            doorbells: VirtioDoorbells::new(doorbell_registration),
            interrupt_state,
            saved_queue_states: vec![None; traits.max_queues as usize],
            supports_save_restore,
            guest_memory,
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

    /// Create an interrupt for a queue.
    fn create_queue_interrupt(&self) -> Interrupt {
        let interrupt_state = self.interrupt_state.clone();
        Interrupt::from_fn(move || {
            interrupt_state
                .lock()
                .update(true, VIRTIO_MMIO_INTERRUPT_STATUS_USED_BUFFER);
        })
    }

    /// Register doorbells for all queues at the device's notification address.
    fn install_doorbells(&mut self) {
        let notification_address = (*self.fixed_mmio_region.1.start() & !0xfff)
            + VirtioMmioRegister::QUEUE_NOTIFY.0 as u64;
        for i in 0..self.events.len() {
            self.doorbells.add(
                notification_address,
                Some(i as u64),
                Some(4),
                &self.events[i],
            );
        }
    }

    /// Reset transport status and interrupt state after a failed enable or
    /// completed disable.
    fn reset_status(&mut self) {
        self.doorbells.clear();
        self.device_status = VirtioDeviceStatus::new();
        self.config_generation = 0;
        self.interrupt_state.lock().update(false, !0);
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

    /// Synchronous transport register read for tests. Only handles
    /// transport registers — not device config.
    #[cfg(test)]
    pub(crate) fn read_u32(&mut self, address: u64) -> u32 {
        self.read_u32_local((address & 0xfff) as u16)
    }

    /// Synchronous transport register write for tests. Only handles
    /// transport registers — not device config.
    #[cfg(test)]
    pub(crate) fn write_u32(&mut self, address: u64, val: u32) {
        self.write_u32_local((address & 0xfff) as u16, val);
    }
}

impl VirtioMmioDevice {
    /// Read a transport register as a u32. Does not handle device-config
    /// registers — those are dispatched to the device task by `mmio_read`.
    fn read_u32_local(&mut self, offset: u16) -> u32 {
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
            _ => 0xffffffff,
        }
    }

    /// Write a transport register as a u32. Does not handle device-config
    /// registers — those are dispatched to the device task by `mmio_write`.
    fn write_u32_local(&mut self, offset: u16, val: u32) {
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
                            let notify = self.create_queue_interrupt();
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
            _ => (),
        }
    }
}

impl ChangeDeviceState for VirtioMmioDevice {
    fn start(&mut self) {
        if self.device_status.driver_ok() {
            let features = self.driver_feature.clone();
            let mut queues = Vec::new();
            for (i, q) in self.queues.iter().enumerate() {
                if !q.enable {
                    continue;
                }
                let notify = self.create_queue_interrupt();
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
        // doorbells, and interrupt_state.
        self.reset_status();

        // Destructure to ensure every field is handled; the compiler will
        // flag new fields that are not addressed here.
        let Self {
            fixed_mmio_region: _,
            device_sender: _,
            _device_task,
            state: _,
            device_id: _,
            vendor_id: _,
            device_feature: _,
            device_feature_select,
            driver_feature,
            driver_feature_select,
            queue_select,
            events: _,
            queues,
            // Handled by reset_status() above.
            device_status: _,
            poll_waker: _,
            config_generation: _,
            doorbells: _,
            interrupt_state: _,
            saved_queue_states,
            supports_save_restore: _,
            guest_memory: _,
        } = self;

        *device_feature_select = 0;
        *driver_feature = VirtioDeviceFeatures::new();
        *driver_feature_select = 0;
        *queue_select = 0;
        for q in queues {
            *q = QueueParams {
                size: QUEUE_MAX_SIZE,
                ..Default::default()
            };
        }
        for s in saved_queue_states {
            *s = None;
        }
    }
}

impl PollDevice for VirtioMmioDevice {
    fn poll_device(&mut self, cx: &mut std::task::Context<'_>) {
        self.poll_waker = Some(cx.waker().clone());

        if let Poll::Ready(result) = self.state.poll(cx, &self.device_sender) {
            self.apply_transport_result(result);
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

        /// MMIO per-queue saved state. Wraps the common queue state so
        /// MMIO-specific per-queue fields can be added later if needed.
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
        }
    }

    use super::*;
    use crate::transport::saved_state::state as common_state;
    use vmcore::save_restore::RestoreError;
    use vmcore::save_restore::SaveError;
    use vmcore::save_restore::SaveRestore;

    impl SaveRestore for VirtioMmioDevice {
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
                    interrupt_status: self.interrupt_state.lock().status,
                },
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
            {
                let mut is = self.interrupt_state.lock();
                is.status = common.interrupt_status;
                is.interrupt.set_level(is.status != 0);
            }

            // Restore per-queue transport parameters.
            for (i, sq) in state.queues.iter().enumerate() {
                self.queues[i] = QueueParams {
                    size: sq.common.size,
                    enable: sq.common.enable,
                    desc_addr: sq.common.desc_addr,
                    avail_addr: sq.common.avail_addr,
                    used_addr: sq.common.used_addr,
                };
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

impl MmioIntercept for VirtioMmioDevice {
    fn mmio_read(&mut self, address: u64, data: &mut [u8]) -> IoResult {
        let offset = (address & 0xfff) as u16;
        // Device config — defer the entire access to the device task.
        if offset >= VirtioMmioRegister::CONFIG.0 {
            return defer_config_read(
                &self.device_sender,
                offset - VirtioMmioRegister::CONFIG.0,
                data.len() as u8,
            );
        }
        // Transport registers — handle locally.
        read_as_u32_chunks(address, data, |address| {
            self.read_u32_local((address & 0xfff) as u16)
        });
        IoResult::Ok
    }

    fn mmio_write(&mut self, address: u64, data: &[u8]) -> IoResult {
        let offset = (address & 0xfff) as u16;
        // Device config — defer the entire access to the device task.
        if offset >= VirtioMmioRegister::CONFIG.0 {
            return defer_config_write(
                &self.device_sender,
                offset - VirtioMmioRegister::CONFIG.0,
                data,
            );
        }
        // Transport registers — handle locally.
        write_as_u32_chunks(address, data, |address, request_type| match request_type {
            ReadWriteRequestType::Write(value) => {
                self.write_u32_local((address & 0xfff) as u16, value);
                None
            }
            ReadWriteRequestType::Read => Some(self.read_u32_local((address & 0xfff) as u16)),
        });
        IoResult::Ok
    }

    fn get_static_regions(&mut self) -> &[(&str, RangeInclusive<u64>)] {
        std::slice::from_ref(&self.fixed_mmio_region)
    }
}
