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
}
