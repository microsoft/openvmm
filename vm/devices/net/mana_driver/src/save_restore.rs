// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Types to save and restore the state of a MANA device.

use mesh::payload::Protobuf;
use std::collections::HashMap;

#[derive(Debug, Protobuf, Clone)]
#[mesh(package = "mana_driver")]
pub struct ManaDeviceSavedState {
    #[mesh(1)]
    pub gdma: GdmaDriverSavedState,
}

#[derive(Protobuf, Clone, Debug)]
#[mesh(package = "mana_driver")]
pub struct GdmaDriverSavedState {
    #[mesh(1)]
    pub mem: SavedMemoryState,
    #[mesh(2)]
    pub eq: CqEqSavedState,
    #[mesh(3)]
    pub cq: CqEqSavedState,
    #[mesh(4)]
    pub rq: WqSavedState,
    #[mesh(5)]
    pub sq: WqSavedState,
    #[mesh(6)]
    pub db_id: u64,
    #[mesh(7)]
    pub gpa_mkey: u32,
    #[mesh(8)]
    pub pdid: u32,
    #[mesh(9)]
    pub hwc_subscribed: bool,
    #[mesh(10)]
    pub eq_armed: bool,
    #[mesh(11)]
    pub cq_armed: bool,
    #[mesh(12)]
    pub eq_id_msix: HashMap<u32, u32>,
    #[mesh(13)]
    pub hwc_activity_id: u32,
    #[mesh(14)]
    pub num_msix: u32,
    #[mesh(15)]
    pub min_queue_avail: u32,
    #[mesh(16)]
    pub interrupt_config: Vec<InterruptSavedState>,
}

#[derive(Debug, Protobuf, Clone)]
#[mesh(package = "mana_driver")]
pub struct SavedMemoryState {
    #[mesh(1)]
    pub base_pfn: u64,
    #[mesh(2)]
    pub len: usize,
}

#[derive(Clone, Protobuf, Debug)]
#[mesh(package = "mana_driver")]
pub struct CqEqSavedState {
    #[mesh(1)]
    pub doorbell: DoorbellSavedState,
    #[mesh(2)]
    pub doorbell_addr: u32,
    #[mesh(4)]
    pub mem: MemoryBlockSavedState,
    #[mesh(5)]
    pub id: u32,
    #[mesh(6)]
    pub next: u32,
    #[mesh(7)]
    pub size: u32,
    #[mesh(8)]
    pub shift: u32,
}

#[derive(Protobuf, Clone, Debug)]
#[mesh(package = "mana_driver")]
pub struct MemoryBlockSavedState {
    #[mesh(1)]
    pub base: u64,
    #[mesh(2)]
    pub len: usize,
    #[mesh(3)]
    pub pfns: Vec<u64>,
    #[mesh(4)]
    pub pfn_bias: u64,
}

#[derive(Debug, Protobuf, Clone)]
#[mesh(package = "mana_driver")]
pub struct WqSavedState {
    #[mesh(1)]
    pub doorbell: DoorbellSavedState,
    #[mesh(2)]
    pub doorbell_addr: u32,
    #[mesh(3)]
    pub mem: MemoryBlockSavedState,
    #[mesh(4)]
    pub id: u32,
    #[mesh(5)]
    pub head: u32,
    #[mesh(6)]
    pub tail: u32,
    #[mesh(7)]
    pub mask: u32,
}

#[derive(Clone, Protobuf, Debug)]
#[mesh(package = "mana_driver")]
pub struct DoorbellSavedState {
    #[mesh(1)]
    pub doorbell_id: u64,
    #[mesh(2)]
    pub page_count: u32,
}

#[derive(Protobuf, Clone, Debug)]
#[mesh(package = "mana_driver")]
pub struct InterruptSavedState {
    #[mesh(1)]
    pub msix_index: u32,
    #[mesh(2)]
    pub cpu: u32,
}
