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
    }
}

use crate::spec::VirtioDeviceFeatures;
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
}

/// Validate that saved driver features are a subset of the current device
/// features. Returns an error if the saved state has more banks than the
/// device or contains feature bits that the device does not advertise.
///
/// Also validates that the queue count matches and all queue sizes are
/// within bounds.
pub(crate) fn validate_restore(
    common: &state::CommonSavedState,
    device_features: &VirtioDeviceFeatures,
    queue_sizes: impl Iterator<Item = (usize, u16)>,
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

    // Validate queue sizes.
    for (i, size) in queue_sizes {
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
    }

    Ok(())
}
