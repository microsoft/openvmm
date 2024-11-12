use std::sync::{Arc, Mutex, Weak};
use memory_range::MemoryRange;
use once_cell::sync::OnceCell;

pub use dma_client::{DmaClient, DmaInterface, DmaTransaction, DmaTransactionHandler};

pub enum DmaError {
    InitializationFailed,
    MapFailed,
    UnmapFailed,
    PinFailed,
    BounceBufferFailed,
}

static GLOBAL_DMA_MANAGER: OnceCell<Arc<GlobalDmaManager>> = OnceCell::new();

/// Global DMA Manager to handle resources and manage clients
pub struct GlobalDmaManager {
    physical_ranges: Vec<MemoryRange>,
    bounce_buffers: Vec<MemoryRange>,
    clients: Mutex<Vec<Weak<DmaClient>>>,
    client_thresholds: Mutex<Vec<(Weak<DmaClient>, usize)>>,
}

impl GlobalDmaManager {
    /// Initializes the global DMA manager with physical ranges and bounce buffers
    pub fn initialize(
        physical_ranges: Vec<MemoryRange>,
        bounce_buffers: Vec<MemoryRange>,
    ) -> Result<(), DmaError> {
        let manager = Arc::new(Self {
            physical_ranges,
            bounce_buffers,
            clients: Mutex::new(Vec::new()),
            client_thresholds: Mutex::new(Vec::new()),
        });

        GLOBAL_DMA_MANAGER.set(manager).map_err(|_| DmaError::InitializationFailed)
    }

    /// Accesses the singleton instance of the global manager
    pub fn get_instance() -> Arc<GlobalDmaManager> {
        GLOBAL_DMA_MANAGER
            .get()
            .expect("GlobalDmaManager has not been initialized")
            .clone()
    }

    /// Creates a new `DmaClient` and registers it with the global manager, along with its threshold
    pub fn create_client(&self, pinning_threshold: usize) -> Arc<DmaClient> {
        let client = Arc::new(DmaClient::new(Arc::downgrade(&self.get_instance())));
        self.register_client(&client, pinning_threshold);
        client
    }

    /// Adds a new client to the list and stores its pinning threshold
    fn register_client(&self, client: &Arc<DmaClient>, threshold: usize) {
        let mut clients = self.clients.lock().unwrap();
        clients.push(Arc::downgrade(client));

        let mut thresholds = self.client_thresholds.lock().unwrap();
        thresholds.push((Arc::downgrade(client), threshold));
    }

    /// Retrieves the pinning threshold for a given client
    pub fn get_client_threshold(&self, client: &Arc<DmaClient>) -> Option<usize> {
        let thresholds = self.client_thresholds.lock().unwrap();
        thresholds.iter().find_map(|(weak_client, threshold)| {
            weak_client
                .upgrade()
                .filter(|c| Arc::ptr_eq(c, client))
                .map(|_| *threshold)
        })
    }

    /// Checks if the given memory range is already pinned
    pub fn is_pinned(&self, range: &MemoryRange) -> bool {
        false // Placeholder
    }

    /// Allocates a bounce buffer if available, otherwise returns an error
    pub fn allocate_bounce_buffer(&self, size: usize) -> Result<usize, DmaError> {
        Err(DmaError::BounceBufferFailed) // Placeholder
    }
}