// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Types to save and restore the state of a MANA device.

use mesh::payload::Protobuf;

/// Mana saved state
#[derive(Debug, Protobuf, Clone)]
#[mesh(package = "mana_driver")]
pub struct ManaSavedState {
    /// The saved state of the MANA device driver
    #[mesh(1)]
    pub mana_device: ManaDeviceSavedState,

    /// Id of the device
    #[mesh(2)]
    pub pci_id: String,
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

    /// The id of the HWC activity
    #[mesh(9)]
    pub hwc_activity_id: u32,

    /// How many msix vectors are available
    #[mesh(10)]
    pub num_msix: u32,

    /// Minimum number of queues available
    #[mesh(11)]
    pub min_queue_avail: u32,
    /// Link status by vport index
    #[mesh(12)]
    pub link_toggle: Vec<(u32, bool)>,
}

/// The saved state of a completion queue or event queue for restoration
/// during servicing
#[derive(Clone, Protobuf, Debug)]
#[mesh(package = "mana_driver")]
pub struct CqEqSavedState {
    /// The doorbell state of the queue, which is how the device is notified
    #[mesh(1)]
    pub doorbell: DoorbellSavedState,

    /// The address of the doorbell register
    #[mesh(2)]
    pub doorbell_addr: u32,

    /// The id of the queue
    #[mesh(3)]
    pub id: u32,

    /// The index of the next entry in the queue
    #[mesh(4)]
    pub next: u32,

    /// The total size of the queue
    #[mesh(5)]
    pub size: u32,

    /// The bit shift value for the queue
    #[mesh(6)]
    pub shift: u32,
}

/// Saved state of a doorbell for restoration during servicing
#[derive(Clone, Protobuf, Debug)]
#[mesh(package = "mana_driver")]
pub struct DoorbellSavedState {
    /// The doorbell's id
    #[mesh(1)]
    pub doorbell_id: u64,

    /// The number of pages allocated for the doorbell
    #[mesh(2)]
    pub page_count: u32,
}

/// Saved state of a work queue for restoration during servicing
#[derive(Debug, Protobuf, Clone)]
#[mesh(package = "mana_driver")]
pub struct WqSavedState {
    /// The doorbell state of the queue, which is how the device is notified
    #[mesh(1)]
    pub doorbell: DoorbellSavedState,

    /// The address of the doorbell
    #[mesh(2)]
    pub doorbell_addr: u32,

    /// The id of the queue
    #[mesh(3)]
    pub id: u32,

    /// The head of the queue
    #[mesh(4)]
    pub head: u32,

    /// The tail of the queue
    #[mesh(5)]
    pub tail: u32,

    /// The bitmask for wrapping queue indices
    #[mesh(6)]
    pub mask: u32,
}

/// Saved state for a memory region used by the driver
/// to be restored by a DMA client during servicing
#[derive(Debug, Protobuf, Clone)]
#[mesh(package = "mana_driver")]
pub struct SavedMemoryState {
    /// The base page frame number of the memory region
    #[mesh(1)]
    pub base_pfn: u64,

    /// How long the memory region is
    #[mesh(2)]
    pub len: usize,
}
