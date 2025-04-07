// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Mana save/restore module

/// Module containing structures for saving and restoring Mana device state
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

        /// The memory region used by the queue
        #[mesh(4)]
        pub mem: MemoryBlockSavedState,

        /// The id of the queue
        #[mesh(5)]
        pub id: u32,

        /// The index of the next entry in the queue
        #[mesh(6)]
        pub next: u32,

        /// The total size of the queue
        #[mesh(7)]
        pub size: u32,

        /// The bit shift value for the queue
        #[mesh(8)]
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

    /// Saved state of a memory region allocated for queues
    #[derive(Protobuf, Clone, Debug)]
    #[mesh(package = "mana_driver")]
    pub struct MemoryBlockSavedState {
        /// Base address of the block in guest memory
        #[mesh(1)]
        pub base: u64,

        /// Length of the memory block
        #[mesh(2)]
        pub len: usize,

        /// The page frame numbers comprising the block
        #[mesh(3)]
        pub pfns: Vec<u64>,

        /// The page frame offset of the block
        #[mesh(4)]
        pub pfn_bias: u64,
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

        /// The memory region used by the queue
        #[mesh(3)]
        pub mem: MemoryBlockSavedState,

        /// The id of the queue
        #[mesh(4)]
        pub id: u32,

        /// The head of the queue
        #[mesh(5)]
        pub head: u32,

        /// The tail of the queue
        #[mesh(6)]
        pub tail: u32,

        /// The bitmask for wrapping queue indices
        #[mesh(7)]
        pub mask: u32,
    }

    /// Saved state for the memory region used by the driver
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

    /// Saved state of a ContiguousBufferManager to be restored after servicing
    #[derive(Debug, Protobuf, Clone)]
    #[mesh(package = "mana_driver")]
    pub struct ContiguousBufferManagerSavedState {
        /// Length of the buffer
        #[mesh(1)]
        pub len: u32,

        /// Head of the buffer
        #[mesh(2)]
        pub head: u32,

        /// Tail of the buffer
        #[mesh(3)]
        pub tail: u32,

        /// Memory state to be restored by a [`DmaClient`]
        #[mesh(4)]
        pub mem: MemoryBlockSavedState,

        /// Counter that keeps track of split headers
        #[mesh(5)]
        pub split_headers: u64,

        /// Counter that keeps track of failed allocations
        #[mesh(6)]
        pub failed_allocations: u64,
    }

    /// Saved state of a queue to be restored after servicing
    #[derive(Debug, Protobuf, Clone)]
    #[mesh(package = "mana_driver")]
    pub enum QueueSavedState {
        /// Variant specific to ManaQueues
        #[mesh(1)]
        ManaQueue(ManaQueueSavedState),
    }

    /// Saved state of a MANA queue to be restored after servicing
    #[derive(Debug, Protobuf, Clone)]
    #[mesh(package = "mana_driver")]
    pub struct ManaQueueSavedState {
        /// The saved state of the RX bounce buffer, if it exists
        #[mesh(1)]
        pub rx_bounce_buffer: Option<ContiguousBufferManagerSavedState>,

        /// The saved state of the TX bounce buffer
        #[mesh(2)]
        pub tx_bounce_buffer: ContiguousBufferManagerSavedState,

        /// The saved state of the EQ
        #[mesh(3)]
        pub eq: CqEqSavedState,

        /// Whether or not the EQ was armed when servicing occurred.
        #[mesh(4)]
        pub eq_armed: bool,

        /// Whether or not the TX CQ was armed when servicing occurred.
        #[mesh(5)]
        pub tx_cq_armed: bool,

        /// Whether or not the RX CQ was armed when servicing occurred.
        #[mesh(6)]
        pub rx_cq_armed: bool,

        /// The VPort offset to be included in TX packets.
        #[mesh(7)]
        pub vp_offset: u16,

        /// The memory key that refers to all GPA space.
        #[mesh(8)]
        pub mem_key: u32,

        /// Saved state of the TX worker queue to be restored after servicing.
        #[mesh(9)]
        pub tx_wq: WqSavedState,

        /// Saved state of the TX completion queue to be restored after servicing.
        #[mesh(10)]
        pub tx_cq: CqEqSavedState,

        /// Saved state of the RX worker queue to be restored after servicing.
        #[mesh(11)]
        pub rx_wq: WqSavedState,

        /// Saved state of the RX completion queue to be restored after servicing.
        #[mesh(12)]
        pub rx_cq: CqEqSavedState,

        /// Upper bound on how many packets can be in the RX queue.
        #[mesh(13)]
        pub rx_max: usize,

        /// Upper bound on how many packets can be in the TX queue.
        #[mesh(14)]
        pub tx_max: usize,

        /// Whether or not TX packet headers should be forced to be bounced.
        #[mesh(15)]
        pub force_tx_header_bounce: bool,
    }
}
