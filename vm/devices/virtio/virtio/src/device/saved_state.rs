// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Device-private saved state for [`VirtioDevice`](super::VirtioDevice)
//! implementations.
//!
//! The transport saves queue and feature-negotiation state itself. Devices with
//! additional state implement `save_device`, `restore_device`, and
//! `device_state_validator`, using the types and helpers in this module.

use crate::queue::QueueError;
use crate::queue::QueueParams;
use crate::queue::QueueState;
use crate::queue::new_queue;
use crate::spec::VirtioDeviceFeatures;
use guestmem::GuestMemory;
use vmcore::save_restore::RestoreError;
use vmcore::save_restore::SaveError;
use vmcore::save_restore::SavedStateBlob;

/// Restored queue metadata available to device-private state validation.
#[derive(Clone, Copy, Debug)]
pub struct DeviceQueueState {
    pub params: QueueParams,
    pub queue_state: Option<QueueState>,
}

/// Synchronous validation for opaque device-private saved state and its queue
/// dependencies.
pub type DeviceStateValidator = Box<
    dyn Fn(
            Option<&SavedStateBlob>,
            &VirtioDeviceFeatures,
            &[DeviceQueueState],
            &GuestMemory,
        ) -> Result<(), RestoreError>
        + Send
        + Sync,
>;

/// Inspect the readable length of the front restored descriptor without
/// advancing the queue.
pub fn restored_queue_front_readable_length(
    features: VirtioDeviceFeatures,
    params: QueueParams,
    mem: GuestMemory,
    initial_state: Option<QueueState>,
) -> Result<Option<u64>, QueueError> {
    let (mut queue, _) = new_queue(features, mem, params, initial_state)?;
    Ok(queue.try_peek_work()?.map(|work| {
        work.payload
            .iter()
            .filter(|payload| !payload.writeable)
            .map(|payload| u64::from(payload.length))
            .sum()
    }))
}

/// The default `VirtioDevice::save_device`: devices that support save/restore
/// have no private state, and other devices cannot be saved.
pub(super) fn default_save_device(
    supports_save_restore: bool,
) -> Result<Option<SavedStateBlob>, SaveError> {
    if supports_save_restore {
        Ok(None)
    } else {
        Err(SaveError::NotSupported)
    }
}

/// The default `VirtioDevice::restore_device`: devices that support
/// save/restore accept only an absent private state.
pub(super) fn default_restore_device(
    supports_save_restore: bool,
    state: Option<SavedStateBlob>,
) -> Result<(), RestoreError> {
    if !supports_save_restore {
        return Err(RestoreError::SavedStateNotSupported);
    }
    if state.is_some() {
        return Err(RestoreError::InvalidSavedState(anyhow::anyhow!(
            "device does not accept private saved state"
        )));
    }
    Ok(())
}

/// The default `VirtioDevice::device_state_validator`, which applies the same
/// rules as [`default_restore_device`].
pub(super) fn default_device_state_validator(supports_save_restore: bool) -> DeviceStateValidator {
    Box::new(move |state, _features, _queues, _guest_memory| {
        if !supports_save_restore {
            return Err(RestoreError::SavedStateNotSupported);
        }
        if state.is_some() {
            return Err(RestoreError::InvalidSavedState(anyhow::anyhow!(
                "device does not accept private saved state"
            )));
        }
        Ok(())
    })
}
