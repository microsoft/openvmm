// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Device-private state and restore validation for the shared transport core.
//!
//! `stop()` captures the device-private state next to the queue states, and
//! `save()` takes it. `restore_common()` validates the restored rings and the
//! private state before it changes any transport state, and
//! `restore_device_state()` then restores the private state into the stopped
//! device task and waits for the result.

use super::QueueData;
use super::VirtioTransportCore;
use crate::DynVirtioDevice;
use crate::device::saved_state::DeviceQueueState;
use crate::device::saved_state::DeviceStateValidator;
use crate::queue::QueueCoreCompleteWork;
use crate::queue::QueueCoreGetWork;
use crate::queue::QueueParams;
use crate::queue::QueueState;
use crate::spec::VirtioDeviceFeatures;
use crate::spec::VirtioDeviceStatus;
use crate::transport::saved_state::state::CommonQueueState;
use crate::transport::saved_state::state::CommonSavedState;
use crate::transport::task::DeviceCommand;
use crate::transport::task::restore::StopResult;
use guestmem::GuestMemory;
use mesh::rpc::Rpc;
use mesh::rpc::RpcSend;
use vmcore::save_restore::RestoreError;
use vmcore::save_restore::SaveError;
use vmcore::save_restore::SavedStateBlob;

/// Device-private state of the transport core.
pub(crate) struct TransportRestore {
    /// Validates restored private state; see VirtioDevice::device_state_validator.
    validator: DeviceStateValidator,
    /// Private state captured by the last `stop()`, taken by `save()`.
    captured: Option<Result<Option<SavedStateBlob>, SaveError>>,
}

impl TransportRestore {
    pub(super) fn new(device: &dyn DynVirtioDevice) -> Self {
        Self {
            validator: device.device_state_validator(),
            captured: None,
        }
    }

    /// Resets the state for a transport reset.
    pub(super) fn reset(&mut self) {
        self.captured = None;
    }

    /// Drops the private state captured by the last `stop()`, which a start
    /// makes stale.
    pub(super) fn discard_captured(&mut self) {
        self.captured = None;
    }

    /// Captures the device-private state of a stop result and returns the
    /// queue states to save. Queues that were not started keep their saved
    /// state.
    pub(super) fn capture_stop(
        &mut self,
        result: StopResult,
        queues: &[QueueData],
    ) -> Vec<Option<QueueState>> {
        self.captured = Some(result.device_state);
        result
            .queues
            .into_iter()
            .zip(queues)
            .map(|((was_started, state), qd)| if was_started { state } else { qd.saved_state })
            .collect()
    }
}

impl VirtioTransportCore {
    /// Restores device-private state into the stopped device task, after
    /// `restore_common()` has validated it, and waits for the result, so that
    /// a device that cannot restore, such as one that cannot reopen its host
    /// resources, fails the restore rather than a later start.
    pub async fn restore_device_state(
        &mut self,
        device_state: Option<SavedStateBlob>,
    ) -> Result<(), RestoreError> {
        self.device_sender
            .call_failable(DeviceCommand::Restore, device_state)
            .await
            .map_err(|error| RestoreError::Other(error.into()))
    }

    /// Take the device-private state captured when the queues stopped.
    pub fn take_device_state(&mut self) -> Result<Option<SavedStateBlob>, SaveError> {
        self.restore
            .captured
            .take()
            .ok_or_else(|| SaveError::Other(anyhow::anyhow!("device state was not captured")))?
    }

    /// Resets the device task when the guest resets a device that is not
    /// DRIVER_OK, which resets the private state that a restore gave it.
    pub(super) fn reset_inactive_device(&self) {
        if !self.device_status.driver_ok() {
            self.device_sender
                .send(DeviceCommand::Reset(Rpc::detached(())));
        }
    }

    /// Validates restored state before `restore_common()` applies it.
    ///
    /// This is stricter than `saved_state::validate_restore()`, which
    /// `restore_common()` runs afterwards: there must be exactly two feature
    /// banks, queue sizes are bounded by each queue's device size, enabled
    /// rings must be valid in guest memory, and the device must accept its
    /// private state.
    pub(super) fn validate_restored_state(
        &self,
        common: &CommonSavedState,
        device_state: &Option<SavedStateBlob>,
        queue_items: &[(CommonQueueState, u16)],
        saved_queue_count: usize,
    ) -> Result<(), RestoreError> {
        let saved_banks = &common.driver_feature_banks;
        if saved_banks.len() != 2 {
            return Err(RestoreError::InvalidSavedState(
                RestoreValidationError::FeatureBankCountMismatch {
                    saved: saved_banks.len(),
                    expected: 2,
                }
                .into(),
            ));
        }
        // Check the feature banks and the queue count. The queue sizes are
        // checked below, against each queue's device size.
        crate::transport::saved_state::validate_restore(
            common,
            &self.device_feature,
            std::iter::empty(),
            self.queues.len(),
            saved_queue_count,
            crate::MAX_QUEUE_SIZE,
        )?;
        for (i, (q, _)) in queue_items.iter().enumerate() {
            let max = self.queues[i].initial_size;
            if q.size > max {
                return Err(RestoreError::InvalidSavedState(
                    RestoreValidationError::QueueSizeTooLarge {
                        index: i,
                        size: q.size,
                        max,
                    }
                    .into(),
                ));
            }
        }

        let mut restored_features = VirtioDeviceFeatures::new();
        for (index, &bank) in common.driver_feature_banks.iter().enumerate() {
            restored_features.set_bank(index, bank);
        }
        let mut ring_ranges = Vec::new();
        for (index, (queue, _)) in queue_items.iter().enumerate() {
            if !queue.enable {
                continue;
            }
            if VirtioDeviceStatus::from(common.device_status).driver_ok()
                && queue.queue_state.is_none()
            {
                return Err(RestoreError::InvalidSavedState(anyhow::anyhow!(
                    "queue {index}: enabled DRIVER_OK queue is missing progress state"
                )));
            }
            validate_restored_queue(
                index,
                queue,
                restored_features,
                &self.guest_memory,
                &mut ring_ranges,
            )?;
        }
        let device_queues = queue_items
            .iter()
            .map(|(queue, _)| DeviceQueueState {
                params: QueueParams {
                    size: queue.size,
                    enable: queue.enable,
                    desc_addr: queue.desc_addr,
                    avail_addr: queue.avail_addr,
                    used_addr: queue.used_addr,
                },
                queue_state: queue.queue_state,
            })
            .collect::<Vec<_>>();
        (self.restore.validator)(
            device_state.as_ref(),
            &restored_features,
            &device_queues,
            &self.guest_memory,
        )
    }
}

#[derive(Debug, thiserror::Error)]
enum RestoreValidationError {
    #[error("saved state has {saved} feature banks, expected {expected}")]
    FeatureBankCountMismatch { saved: usize, expected: usize },
    #[error("queue {index}: saved size {size} exceeds device maximum {max}")]
    QueueSizeTooLarge { index: usize, size: u16, max: u16 },
}

/// Validates the layout and progress of an enabled restored queue. Its rings
/// must not overlap previous_ranges, to which they are added.
pub(crate) fn validate_restored_queue(
    index: usize,
    queue: &CommonQueueState,
    features: VirtioDeviceFeatures,
    guest_memory: &GuestMemory,
    previous_ranges: &mut Vec<(usize, &'static str, std::ops::Range<u64>)>,
) -> Result<(), RestoreError> {
    let invalid = |message: String| {
        RestoreError::InvalidSavedState(anyhow::anyhow!("queue {index}: {message}"))
    };
    if queue.size == 0 || (!features.ring_packed() && !queue.size.is_power_of_two()) {
        return Err(invalid(format!(
            "invalid {} ring size {}",
            if features.ring_packed() {
                "packed"
            } else {
                "split"
            },
            queue.size
        )));
    }

    let descriptor_length = u64::from(queue.size)
        .checked_mul(16)
        .ok_or_else(|| invalid("descriptor length overflow".to_owned()))?;
    let (available_length, used_length) = if features.ring_packed() {
        (4, 4)
    } else {
        (
            u64::from(queue.size)
                .checked_mul(2)
                .and_then(|length| length.checked_add(6))
                .ok_or_else(|| invalid("available ring length overflow".to_owned()))?,
            u64::from(queue.size)
                .checked_mul(8)
                .and_then(|length| length.checked_add(6))
                .ok_or_else(|| invalid("used ring length overflow".to_owned()))?,
        )
    };
    let specifications = [
        ("descriptor", queue.desc_addr, descriptor_length, 16),
        ("available", queue.avail_addr, available_length, 2),
        ("used", queue.used_addr, used_length, 4),
    ];
    let mut queue_ranges = Vec::new();
    for (name, address, length, alignment) in specifications {
        if address % alignment != 0 {
            return Err(invalid(format!(
                "{name} ring address {address:#x} is not {alignment}-byte aligned"
            )));
        }
        let end = address
            .checked_add(length)
            .ok_or_else(|| invalid(format!("{name} ring range overflows")))?;
        guest_memory
            .subrange(address, length, true)
            .map_err(|error| invalid(format!("{name} ring is outside guest RAM: {error}")))?;
        let range = address..end;
        for (other_index, other_name, other_range) in
            previous_ranges.iter().chain(queue_ranges.iter())
        {
            if range.start < other_range.end && other_range.start < range.end {
                return Err(invalid(format!(
                    "{name} ring overlaps queue {other_index} {other_name} ring"
                )));
            }
        }
        queue_ranges.push((index, name, range));
    }

    let params = QueueParams {
        size: queue.size,
        enable: true,
        desc_addr: queue.desc_addr,
        avail_addr: queue.avail_addr,
        used_addr: queue.used_addr,
    };
    QueueCoreGetWork::new(features, guest_memory.clone(), params, queue.queue_state)
        .map_err(|error| invalid(format!("invalid available progress: {error}")))?;
    QueueCoreCompleteWork::new(features, guest_memory.clone(), params, queue.queue_state)
        .map_err(|error| invalid(format!("invalid used progress: {error}")))?;
    previous_ranges.extend(queue_ranges);
    Ok(())
}
