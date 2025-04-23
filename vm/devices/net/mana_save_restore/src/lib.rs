// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Mana save/restore module

/// Module containing structures for saving and restoring MANA device state
pub mod save_restore {
    use mesh::payload::Protobuf;

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
}
