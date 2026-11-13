// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Saved state types and validation helpers shared across virtio transports.
//!
//! The structs here represent the transport-agnostic portion of virtio common
//! configuration: device status, feature negotiation, queue parameters, and
//! per-queue progress (avail/used indices). Transport-specific state (e.g.
//! MSI-X vectors for PCI) is stored separately by each transport.

pub mod state {
    use crate::queue::QueueState;
    use mesh::payload::Protobuf;
    use vmcore::save_restore::SavedStateBlob;

    /// Transport-agnostic per-queue saved state.
    #[derive(Protobuf)]
    #[mesh(package = "virtio.queue")]
    pub struct CommonQueueState {
        #[mesh(1)]
        pub size: u16,
        #[mesh(2)]
        pub enable: bool,
        #[mesh(3)]
        pub desc_addr: u64,
        #[mesh(4)]
        pub avail_addr: u64,
        #[mesh(5)]
        pub used_addr: u64,
        #[mesh(6)]
        pub queue_state: Option<QueueState>,
    }

    /// Transport-agnostic saved state for the virtio common configuration.
    ///
    /// Per-queue state is not included here because transports may extend it
    /// with transport-specific fields (e.g. MSI-X vectors for PCI). Each
    /// transport stores its own `Vec` of queue state.
    #[derive(Protobuf)]
    #[mesh(package = "virtio.transport")]
    pub struct CommonSavedState {
        #[mesh(1)]
        pub device_status: u8,
        #[mesh(2)]
        pub driver_feature_banks: Vec<u32>,
        #[mesh(3)]
        pub driver_feature_select: u32,
        #[mesh(4)]
        pub device_feature_select: u32,
        #[mesh(5)]
        pub queue_select: u32,
        #[mesh(6)]
        pub config_generation: u32,
        #[mesh(7)]
        pub device_state: Option<SavedStateBlob>,
    }
}

use crate::queue::QueueError;
use crate::spec::VirtioDeviceFeatures;
use crate::spec::VirtioDeviceStatus;
use vmcore::save_restore::RestoreError;

#[derive(Debug, thiserror::Error)]
pub(crate) enum VirtioRestoreError {
    #[error("driver feature bank {bank}: saved {saved:#x} has bits not in device {device:#x}")]
    IncompatibleFeatures {
        bank: usize,
        saved: u32,
        device: u32,
    },
    #[error("saved state has {saved} feature banks, device only has {device}")]
    TooManyFeatureBanks { saved: usize, device: usize },
    #[error("queue count mismatch: saved {saved} vs device {device}")]
    QueueCountMismatch { saved: usize, device: usize },
    #[error("queue {index}: saved size {size} exceeds max {max}")]
    QueueSizeTooLarge { index: usize, size: u16, max: u16 },
    #[error("queue {index} has invalid saved state")]
    InvalidQueueState {
        index: usize,
        #[source]
        source: QueueError,
    },
}

/// Validate that saved driver features are a subset of the current device
/// features. Returns an error if the saved state has more banks than the
/// device or contains feature bits that the device does not advertise.
///
/// Also validates queue count, size bounds, and supplied active-queue progress.
/// Missing progress retains the existing legacy snapshot behavior.
pub(crate) fn validate_restore<'a>(
    common: &state::CommonSavedState,
    device_features: &VirtioDeviceFeatures,
    queues: impl Iterator<Item = (usize, &'a state::CommonQueueState)>,
    device_queue_count: usize,
    saved_queue_count: usize,
    max_queue_size: u16,
) -> Result<(), RestoreError> {
    // Validate feature banks.
    let saved_banks = &common.driver_feature_banks;
    if saved_banks.len() > 2 {
        return Err(RestoreError::InvalidSavedState(
            VirtioRestoreError::TooManyFeatureBanks {
                saved: saved_banks.len(),
                device: 2,
            }
            .into(),
        ));
    }
    let mut driver_features = VirtioDeviceFeatures::new();
    for (i, &bank) in saved_banks.iter().enumerate() {
        let device_bank = device_features.bank(i);
        if bank & !device_bank != 0 {
            return Err(RestoreError::InvalidSavedState(
                VirtioRestoreError::IncompatibleFeatures {
                    bank: i,
                    saved: bank,
                    device: device_bank,
                }
                .into(),
            ));
        }
        driver_features.set_bank(i, bank);
    }

    // Validate queue count.
    if saved_queue_count != device_queue_count {
        return Err(RestoreError::InvalidSavedState(
            VirtioRestoreError::QueueCountMismatch {
                saved: saved_queue_count,
                device: device_queue_count,
            }
            .into(),
        ));
    }

    for (i, queue) in queues {
        let size = queue.size;
        if size > max_queue_size {
            return Err(RestoreError::InvalidSavedState(
                VirtioRestoreError::QueueSizeTooLarge {
                    index: i,
                    size,
                    max: max_queue_size,
                }
                .into(),
            ));
        }
        if VirtioDeviceStatus::from(common.device_status).driver_ok()
            && queue.enable
            && let Some(state) = queue.queue_state
        {
            state
                .in_flight(size, driver_features.ring_packed())
                .map_err(|source| {
                    RestoreError::InvalidSavedState(
                        VirtioRestoreError::InvalidQueueState { index: i, source }.into(),
                    )
                })?;
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::queue::QueueState;
    use test_with_tracing::test;

    fn validate_progress(
        packed: bool,
        size: u16,
        queue_state: Option<QueueState>,
        driver_ok: bool,
        enable: bool,
    ) -> Result<(), RestoreError> {
        let features = VirtioDeviceFeatures::new().with_ring_packed(packed);
        let common = state::CommonSavedState {
            device_status: VirtioDeviceStatus::new().with_driver_ok(driver_ok).into(),
            driver_feature_banks: vec![features.bank(0), features.bank(1)],
            device_feature_select: 0,
            driver_feature_select: 0,
            queue_select: 0,
            config_generation: 0,
            device_state: None,
        };
        let queue = state::CommonQueueState {
            size,
            enable,
            desc_addr: 0,
            avail_addr: 0x1000,
            used_addr: 0x2000,
            queue_state,
        };
        validate_restore(
            &common,
            &features,
            std::iter::once((0, &queue)),
            1,
            1,
            crate::MAX_QUEUE_SIZE,
        )
    }

    #[test]
    fn preserves_legacy_missing_queue_progress() {
        for driver_ok in [false, true] {
            for enabled in [false, true] {
                assert!(validate_progress(false, 4, None, driver_ok, enabled).is_ok());
            }
        }
        assert!(validate_progress(false, 0, None, false, false).is_ok());
    }

    #[test]
    fn validates_split_and_packed_saved_progress() {
        for (packed, size, avail_index, used_index, valid) in [
            (false, 8, 0, 0, true),
            (false, 8, 4, 1, true),
            (false, 8, 8, 0, true),
            (false, 8, 1, u16::MAX, true),
            (false, 8, 9, 0, false),
            (false, 8, 1, 2, false),
            (false, 0, 0, 0, false),
            (false, 3, 0, 0, false),
            (true, 3, 0x8002, 0x8000, true),
            (true, 3, 0, 0x8002, true),
            (true, 3, 0, 0x8000, true),
            (true, 3, 3, 0, false),
            (true, 3, 0, 3, false),
            (true, 8, 1, 6, false),
            (true, 0, 0, 0, false),
            (false, crate::MAX_QUEUE_SIZE, 0, 0, true),
            (true, crate::MAX_QUEUE_SIZE, 0, 0, true),
            (true, crate::MAX_QUEUE_SIZE + 1, 0, 0, false),
        ] {
            assert_eq!(
                validate_progress(
                    packed,
                    size,
                    Some(QueueState {
                        avail_index,
                        used_index,
                    }),
                    true,
                    true,
                )
                .is_ok(),
                valid,
                "packed={packed}, size={size}, avail={avail_index}, used={used_index}",
            );
        }
    }
}
