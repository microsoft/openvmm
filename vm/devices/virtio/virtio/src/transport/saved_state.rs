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
        #[mesh(7)]
        pub interrupt_status: u32,
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
    #[error("queue count mismatch: saved {saved} vs device {device}")]
    QueueCountMismatch { saved: usize, device: usize },
}

/// Validate that saved driver features are a subset of the current device
/// features. Returns an error if the saved state contains feature bits
/// that the device does not advertise.
pub(crate) fn validate_driver_features(
    saved_banks: &[u32],
    device_features: &VirtioDeviceFeatures,
) -> Result<(), RestoreError> {
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
    Ok(())
}

/// Validate that the saved queue count matches the device's queue count.
pub(crate) fn validate_queue_count(saved: usize, device: usize) -> Result<(), RestoreError> {
    if saved != device {
        return Err(RestoreError::InvalidSavedState(
            VirtioRestoreError::QueueCountMismatch { saved, device }.into(),
        ));
    }
    Ok(())
}
