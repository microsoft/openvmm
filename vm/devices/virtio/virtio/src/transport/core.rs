// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Shared virtio transport core — state and logic common to PCI and MMIO.

use super::StalledIo;
use super::task::DeviceCommand;
use super::task::StartParams;
use super::task::TransportState;
use super::task::TransportStateResult;
use super::task::run_device_task;
use crate::DynVirtioDevice;
use crate::QueueResources;
use crate::VirtioDoorbells;
use crate::queue::QueueParams;
use crate::queue::QueueState;
use crate::spec::VirtioDeviceFeatures;
use crate::spec::VirtioDeviceStatus;
use chipset_device::io::deferred::DeferredWrite;
use guestmem::DoorbellRegistration;
use guestmem::GuestMemory;
use inspect::Inspect;
use mesh::rpc::Rpc;
use mesh::rpc::RpcSend;
use pal_async::task::Spawn;
use pal_async::task::Task;
use std::sync::Arc;
use std::task::Poll;
use vmcore::interrupt::Interrupt;
use vmcore::save_restore::RestoreError;
use vmcore::save_restore::SaveError;

/// Per-queue transport data shared between PCI and MMIO.
#[derive(Inspect)]
pub(crate) struct QueueData {
    #[inspect(flatten)]
    pub params: QueueParams,
    pub initial_size: u16,
    /// MSI-X vector for this queue (PCI only; always 0 for MMIO).
    pub msix_vector: u16,
    #[inspect(skip)]
    pub event: pal_event::Event,
    #[inspect(skip)]
    pub saved_state: Option<QueueState>,
}

/// Transport-specific operations that the core delegates to PCI or MMIO.
pub(crate) trait TransportOps: Send {
    /// Create an interrupt for queue `idx`.
    ///
    /// `msix_vector` is the per-queue MSI-X vector (PCI) or unused (MMIO).
    fn create_queue_interrupt(&mut self, idx: usize, msix_vector: u16) -> Interrupt;

    /// Signal a config-change interrupt to the guest.
    fn signal_config_change(&mut self);

    /// Reset transport-specific interrupt state (clear status, deassert lines).
    fn reset_interrupts(&mut self);

    /// Return the (base_address, entry_size) for doorbell registration,
    /// or `None` if the address is not yet known.
    fn doorbell_region(&mut self) -> Option<(u64, u32)>;
}

/// State shared by both the PCI and MMIO virtio transports.
#[derive(Inspect)]
pub(crate) struct VirtioTransportCore {
    #[inspect(rename = "device", send = "DeviceCommand::Inspect")]
    pub device_sender: mesh::Sender<DeviceCommand>,
    #[inspect(skip)]
    pub _device_task: Task<()>,
    pub state: TransportState,
    pub device_feature: VirtioDeviceFeatures,
    #[inspect(hex)]
    pub device_feature_select: u32,
    pub driver_feature: VirtioDeviceFeatures,
    #[inspect(hex)]
    pub driver_feature_select: u32,
    pub queue_select: u32,
    #[inspect(iter_by_index)]
    pub queues: Vec<QueueData>,
    #[inspect(hex)]
    pub device_status: VirtioDeviceStatus,
    #[inspect(skip)]
    pub poll_waker: Option<std::task::Waker>,
    pub config_generation: u32,
    #[inspect(skip)]
    pub doorbells: VirtioDoorbells,
    pub supports_save_restore: bool,
    #[inspect(skip)]
    pub guest_memory: GuestMemory,
    #[inspect(with = "Option::is_some")]
    pub pending_status_deferred: Option<DeferredWrite>,
    #[inspect(with = "Vec::len")]
    pub stalled_io: Vec<StalledIo>,
}

impl VirtioTransportCore {
    /// Create a new transport core, spawning the device task.
    pub fn new(
        device: Box<dyn DynVirtioDevice>,
        driver: &impl Spawn,
        guest_memory: GuestMemory,
        doorbell_registration: Option<Arc<dyn DoorbellRegistration>>,
    ) -> std::io::Result<Self> {
        let traits = device.traits();
        let queues: Vec<QueueData> = (0..traits.max_queues)
            .map(|i| {
                let size = device.queue_size(i);
                super::validate_queue_size(i, size)?;
                Ok(QueueData {
                    params: QueueParams {
                        size,
                        ..Default::default()
                    },
                    initial_size: size,
                    msix_vector: 0,
                    event: pal_event::Event::new(),
                    saved_state: None,
                })
            })
            .collect::<std::io::Result<Vec<_>>>()?;

        let device_feature = traits.device_features.with_version_1(true);
        let supports_save_restore = device.supports_save_restore();

        let (sender, receiver) = mesh::channel();
        let _device_task = driver.spawn("virtio-device-task", async move {
            run_device_task(device, receiver).await;
        });

        Ok(Self {
            device_sender: sender,
            _device_task,
            state: TransportState::Ready,
            device_feature,
            device_feature_select: 0,
            driver_feature: VirtioDeviceFeatures::new(),
            driver_feature_select: 0,
            queue_select: 0,
            queues,
            device_status: VirtioDeviceStatus::new(),
            poll_waker: None,
            config_generation: 0,
            doorbells: VirtioDoorbells::new(doorbell_registration),
            supports_save_restore,
            guest_memory,
            pending_status_deferred: None,
            stalled_io: Vec::new(),
        })
    }

    /// Bump config_generation and signal a config-change interrupt if
    /// the device is in DRIVER_OK state.
    pub fn update_config_generation(&mut self, ops: &mut dyn TransportOps) {
        self.config_generation = self.config_generation.wrapping_add(1);
        if self.device_status.driver_ok() {
            ops.signal_config_change();
        }
    }

    /// Register doorbells for all queues using the transport's
    /// notification address.
    pub fn install_doorbells(&mut self, ops: &mut dyn TransportOps) {
        if let Some((base, entry_size)) = ops.doorbell_region() {
            for (i, qd) in self.queues.iter().enumerate() {
                self.doorbells
                    .add(base, Some(i as u64), Some(entry_size), &qd.event);
            }
        }
    }

    /// Reset all virtio transport configuration to power-on defaults.
    ///
    /// Per virtio spec v1.2 §2.4, writing 0 to device_status resets
    /// the device and all its configuration.
    ///
    /// Uses destructuring so the compiler catches any new fields.
    pub fn reset_status(&mut self, ops: &mut dyn TransportOps) {
        let Self {
            // Immutable / long-lived — not reset.
            device_sender: _,
            _device_task: _,
            device_feature: _,
            supports_save_restore: _,
            guest_memory: _,

            // Async state machine — not owned by reset_status.
            state: _,
            poll_waker: _,

            // Deferred IO — drop pending writes and stalled accesses.
            pending_status_deferred,
            stalled_io,

            // Reset below.
            doorbells,
            device_status,
            config_generation,
            device_feature_select,
            driver_feature,
            driver_feature_select,
            queue_select,
            queues,
        } = self;

        drop(pending_status_deferred.take());
        stalled_io.clear();

        doorbells.clear();
        *device_status = VirtioDeviceStatus::new();
        *config_generation = 0;
        ops.reset_interrupts();

        *device_feature_select = 0;
        *driver_feature = VirtioDeviceFeatures::new();
        *driver_feature_select = 0;
        *queue_select = 0;

        for qd in queues.iter_mut() {
            let QueueData {
                params,
                initial_size,
                msix_vector,
                // Event is reused — the device task drains any pending
                // signal during stop_queue before reset_status runs.
                event: _,
                saved_state,
            } = qd;
            *saved_state = None;
            *params = QueueParams {
                size: *initial_size,
                ..Default::default()
            };
            *msix_vector = 0;
        }
    }

    /// Apply the result of a completed transport state transition.
    pub fn apply_transport_result(
        &mut self,
        ops: &mut dyn TransportOps,
        result: TransportStateResult,
    ) {
        match result {
            TransportStateResult::EnableComplete(true) => {
                self.device_status.set_driver_ok(true);
                self.update_config_generation(ops);
            }
            TransportStateResult::EnableComplete(false) | TransportStateResult::DisableComplete => {
                self.reset_status(ops);
            }
        }
    }

    /// Handle a write to the device_status byte.
    ///
    /// Writing 0 resets the device if it is currently initialized.
    /// Writing a non-zero value sets the corresponding status bits.
    ///
    /// # Panics
    ///
    /// Panics if `self.state.is_busy()`.  The PCI and MMIO transports
    /// enforce this by stalling and deferring all guest MMIO writes to
    /// transport registers while an enable or disable is in flight, so
    /// the writing VCPU blocks until the operation completes before this
    /// function is called.
    pub fn write_device_status(&mut self, ops: &mut dyn TransportOps, val: u8) {
        assert!(
            !self.state.is_busy(),
            "caller must not write STATUS while busy"
        );

        if val == 0 {
            if self.device_status.as_u32() == 0 {
                return;
            }
            if !self.device_status.driver_ok() {
                self.reset_status(ops);
            } else {
                self.doorbells.clear();
                self.state.start_disable(&self.device_sender);
                if let Some(waker) = self.poll_waker.take() {
                    waker.wake();
                }
            }
            return;
        }

        let new_status = VirtioDeviceStatus::from(val);
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
            self.update_config_generation(ops);
        }

        if !self.device_status.driver_ok() && new_status.driver_ok() {
            self.install_doorbells(ops);

            let features = self.driver_feature;
            let queues: Vec<_> = self
                .queues
                .iter()
                .enumerate()
                .filter(|(_, qd)| qd.params.enable)
                .map(|(i, qd)| {
                    let notify = ops.create_queue_interrupt(i, qd.msix_vector);
                    (
                        i as u16,
                        QueueResources {
                            params: qd.params,
                            notify,
                            event: qd.event.clone(),
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

    /// Poll the transport state machine and complete deferred IO on
    /// transition.
    pub fn poll_device(&mut self, ops: &mut dyn TransportOps, cx: &mut std::task::Context<'_>) {
        self.poll_waker = Some(cx.waker().clone());

        if let Poll::Ready(result) = self.state.poll(cx) {
            // Complete the deferred STATUS write before applying the
            // result, since apply_transport_result may call reset_status
            // which would drop the deferred (giving the VCPU NoResponse).
            if let Some(deferred) = self.pending_status_deferred.take() {
                deferred.complete();
            }
            self.apply_transport_result(ops, result);
        }
    }

    /// `ChangeDeviceState::start()` implementation.
    pub fn start(&mut self, ops: &mut dyn TransportOps) {
        if self.device_status.driver_ok() {
            let features = self.driver_feature;
            let mut queues = Vec::new();
            for (i, qd) in self.queues.iter_mut().enumerate() {
                if !qd.params.enable {
                    continue;
                }
                let initial_state = qd.saved_state.take();
                queues.push((
                    i,
                    qd.params,
                    qd.msix_vector,
                    qd.event.clone(),
                    initial_state,
                ));
            }
            let queues: Vec<_> = queues
                .into_iter()
                .map(|(i, params, msix_vector, event, initial_state)| {
                    let notify = ops.create_queue_interrupt(i, msix_vector);
                    (
                        i as u16,
                        QueueResources {
                            params,
                            notify,
                            event,
                            guest_memory: self.guest_memory.clone(),
                        },
                        initial_state,
                    )
                })
                .collect();

            let params = StartParams { queues, features };

            // Fire and forget — start() is sync, can't await.
            self.device_sender
                .send(DeviceCommand::Start(Rpc::detached(params)));
        }
    }

    /// `ChangeDeviceState::stop()` implementation.
    pub async fn stop(&mut self, ops: &mut dyn TransportOps) {
        if let Some(result) = self.state.drain().await {
            self.apply_transport_result(ops, result);
        }
        drop(self.pending_status_deferred.take());
        self.stalled_io.clear();

        let states = self
            .device_sender
            .call(DeviceCommand::Stop, ())
            .await
            .expect("device task is gone");
        for (i, state) in states.into_iter().enumerate() {
            self.queues[i].saved_state = state;
        }
    }

    /// `ChangeDeviceState::reset()` implementation.
    pub async fn reset(&mut self, ops: &mut dyn TransportOps) {
        let _ = self.state.drain().await;
        let _ = self.device_sender.call(DeviceCommand::Reset, ()).await;
        self.reset_status(ops);
    }

    /// Signal a queue notification by index.
    pub fn notify_queue(&self, queue_index: u32) {
        if let Some(qd) = self.queues.get(queue_index as usize) {
            qd.event.signal();
        }
    }

    /// Save the transport-agnostic portion of the common configuration.
    pub fn save_common(&self) -> Result<super::saved_state::state::CommonSavedState, SaveError> {
        if !self.supports_save_restore {
            return Err(SaveError::NotSupported);
        }
        Ok(super::saved_state::state::CommonSavedState {
            device_status: self.device_status.into(),
            driver_feature_banks: (0..2usize).map(|i| self.driver_feature.bank(i)).collect(),
            device_feature_select: self.device_feature_select,
            driver_feature_select: self.driver_feature_select,
            queue_select: self.queue_select,
            config_generation: self.config_generation,
        })
    }

    /// Save the transport-agnostic portion of a single queue.
    pub fn save_queue_common(&self, idx: usize) -> super::saved_state::state::CommonQueueState {
        let qd = &self.queues[idx];
        super::saved_state::state::CommonQueueState {
            size: qd.params.size,
            enable: qd.params.enable,
            desc_addr: qd.params.desc_addr,
            avail_addr: qd.params.avail_addr,
            used_addr: qd.params.used_addr,
            queue_state: qd.saved_state,
        }
    }

    /// Restore the transport-agnostic portion of the common configuration.
    ///
    /// Validates the saved state, then restores feature negotiation, queue
    /// parameters, device status, and doorbells. The caller must restore
    /// transport-specific fields (interrupt status, MSI-X vectors, etc.)
    /// afterward.
    pub fn restore_common(
        &mut self,
        ops: &mut dyn TransportOps,
        common: &super::saved_state::state::CommonSavedState,
        queue_states: impl Iterator<Item = (super::saved_state::state::CommonQueueState, u16)>,
        saved_queue_count: usize,
    ) -> Result<(), RestoreError> {
        if !self.supports_save_restore {
            return Err(RestoreError::SavedStateNotSupported);
        }

        let queue_items: Vec<_> = queue_states.collect();

        super::saved_state::validate_restore(
            common,
            &self.device_feature,
            queue_items
                .iter()
                .enumerate()
                .map(|(i, (q, _))| (i, q.size)),
            self.queues.len(),
            saved_queue_count,
            crate::MAX_QUEUE_SIZE,
        )?;

        // Restore feature negotiation.
        self.driver_feature = VirtioDeviceFeatures::new();
        for (i, &bank) in common.driver_feature_banks.iter().enumerate() {
            self.driver_feature.set_bank(i, bank);
        }
        self.device_feature_select = common.device_feature_select;
        self.driver_feature_select = common.driver_feature_select;
        self.queue_select = common.queue_select;
        self.config_generation = common.config_generation;

        // Restore per-queue parameters.
        for (i, (q, msix_vector)) in queue_items.into_iter().enumerate() {
            let qd = &mut self.queues[i];
            qd.params = QueueParams {
                size: q.size,
                enable: q.enable,
                desc_addr: q.desc_addr,
                avail_addr: q.avail_addr,
                used_addr: q.used_addr,
            };
            qd.msix_vector = msix_vector;
            qd.saved_state = q.queue_state;
        }

        self.device_status = VirtioDeviceStatus::from(common.device_status);

        // Verify ephemeral runtime state.
        assert!(!self.state.is_busy());
        self.poll_waker = None;

        // Reinstall doorbells.
        self.doorbells.clear();
        if self.device_status.driver_ok() {
            self.install_doorbells(ops);
        }

        Ok(())
    }
}
