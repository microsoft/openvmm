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

    /// Saved state for queue resources
    #[derive(Debug, Protobuf, Clone)]
    #[mesh(package = "mana_driver")]
    pub struct QueueResourcesSavedState {
        #[mesh(1)]
        pub _eq: BnicEqSavedState,
        #[mesh(2)]
        pub rxq: BnicWqSavedState,
        #[mesh(3)]
        pub _txq: BnicWqSavedState,
    }

    #[derive(Protobuf, Clone, Debug)]
    #[mesh(package = "mana_driver")]
    pub struct BnicEqSavedState {
        #[mesh(1)]
        pub memory: SavedMemoryState,
        #[mesh(2)]
        pub queue: CqEqSavedState,
        #[mesh(3)]
        pub doorbell: DoorbellSavedState,
    }

    #[derive(Protobuf, Clone, Debug)]
    #[mesh(package = "mana_driver")]
    pub struct BnicWqSavedState {
        #[mesh(1)]
        pub memory: SavedMemoryState,
        #[mesh(2)]
        pub queue: WqSavedState,
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

    #[derive(Debug, Protobuf, Clone)]
    #[mesh(package = "mana_driver")]
    pub struct ContiguousBufferManagerSavedState {
        #[mesh(1)]
        pub len: u32,
        #[mesh(2)]
        pub head: u32,
        #[mesh(3)]
        pub tail: u32,
        #[mesh(4)]
        pub mem: MemoryBlockSavedState,
        #[mesh(5)]
        pub split_headers: u64,
        #[mesh(6)]
        pub failed_allocations: u64,
    }

    #[derive(Debug, Protobuf, Clone)]
    #[mesh(package = "mana_driver")]
    pub enum QueueSavedState {
        #[mesh(1)]
        ManaQueue(ManaQueueSavedState),
    }

    #[derive(Debug, Protobuf, Clone)]
    #[mesh(package = "mana_driver")]
    pub struct ManaQueueSavedState {
        #[mesh(1)]
        pub rx_bounce_buffer: Option<ContiguousBufferManagerSavedState>,
        #[mesh(2)]
        pub tx_bounce_buffer: ContiguousBufferManagerSavedState,

        // vport: Weak<Vport<T>>,
        #[mesh(3)]
        pub eq: CqEqSavedState,
        #[mesh(4)]
        pub eq_armed: bool,
        // interrupt: DeviceInterrupt,
        #[mesh(5)]
        pub tx_cq_armed: bool,
        #[mesh(6)]
        pub rx_cq_armed: bool,

        #[mesh(7)]
        pub vp_offset: u16,
        #[mesh(8)]
        pub mem_key: u32,

        #[mesh(9)]
        pub tx_wq: WqSavedState,
        #[mesh(10)]
        pub tx_cq: CqEqSavedState,

        #[mesh(11)]
        pub rx_wq: WqSavedState,
        #[mesh(12)]
        pub rx_cq: CqEqSavedState,

        // avail_rx: VecDeque<RxId>,
        // posted_rx: VecDeque<PostedRx>,
        #[mesh(13)]
        pub rx_max: usize,

        // posted_tx: VecDeque<PostedTx>,
        // dropped_tx: VecDeque<TxId>,
        #[mesh(14)]
        pub tx_max: usize,

        #[mesh(15)]
        pub force_tx_header_bounce: bool,
        // stats: QueueStats,
    }
}
