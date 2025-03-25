// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Types to save and restore the state of a MANA device.

use mana_save_restore::save_restore::CqEqSavedState;
use mana_save_restore::save_restore::QueueSavedState;
use mana_save_restore::save_restore::SavedMemoryState;
use mana_save_restore::save_restore::WqSavedState;
use mesh::payload::Protobuf;
use net_backend::save_restore::EndpointSavedState;
use std::collections::HashMap;

/// Mana saved state
#[derive(Debug, Protobuf, Clone)]
#[mesh(package = "mana_driver")]
pub struct ManaSavedState {
    /// The saved state of the MANA device driver
    #[mesh(1)]
    pub mana_device: ManaDeviceSavedState,

    /// The saved state of the MANA endpoints
    #[mesh(2)]
    pub endpoints: Vec<EndpointSavedState>,

    /// Saved queue state
    #[mesh(3)]
    pub queues: Vec<QueueSavedState>,
}

/// Mana device saved state
#[derive(Debug, Protobuf, Clone)]
#[mesh(package = "mana_driver")]
pub struct ManaDeviceSavedState {
    /// Saved state for restoration of the GDMA driver
    #[mesh(1)]
    pub gdma: GdmaDriverSavedState,
}

/// Top level saved state for the GDMA driver's saved state
#[derive(Protobuf, Clone, Debug)]
#[mesh(package = "mana_driver")]
pub struct GdmaDriverSavedState {
    /// Memory to be restored by a DMA client
    #[mesh(1)]
    pub mem: SavedMemoryState,

    /// EQ to be restored
    #[mesh(2)]
    pub eq: CqEqSavedState,

    /// CQ to be restored
    #[mesh(3)]
    pub cq: CqEqSavedState,

    /// RQ to be restored
    #[mesh(4)]
    pub rq: WqSavedState,

    /// SQ to be restored
    #[mesh(5)]
    pub sq: WqSavedState,

    /// Doorbell id
    #[mesh(6)]
    pub db_id: u64,

    /// Guest physical address memory key
    #[mesh(7)]
    pub gpa_mkey: u32,

    /// Protection domain id
    #[mesh(8)]
    pub pdid: u32,

    /// Whether the driver is subscribed to hwc
    #[mesh(9)]
    pub hwc_subscribed: bool,

    /// Whether the eq is armed or not
    #[mesh(10)]
    pub eq_armed: bool,

    /// Whether the cq is armed or not
    #[mesh(11)]
    pub cq_armed: bool,

    /// Event queue id to msix mapping
    #[mesh(12)]
    pub eq_id_msix: HashMap<u32, u32>,

    /// The id of the hwc activity
    #[mesh(13)]
    pub hwc_activity_id: u32,

    /// How many msix vectors are available
    #[mesh(14)]
    pub num_msix: u32,

    /// Minimum number of queues available
    #[mesh(15)]
    pub min_queue_avail: u32,

    /// Saved interrupts for restoration
    #[mesh(16)]
    pub interrupt_config: Vec<InterruptSavedState>,
}

/// Saved state of an interrupt for restoration during servicing
#[derive(Protobuf, Clone, Debug)]
#[mesh(package = "mana_driver")]
pub struct InterruptSavedState {
    /// The index in the msix table for this interrupt
    #[mesh(1)]
    pub msix_index: u32,

    /// Which CPU this interrupt is assigned to
    #[mesh(2)]
    pub cpu: u32,
}
