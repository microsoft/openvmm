//use std::sync::{Mutex, Weak};

use memory_range::MemoryRange;
use std::{collections::HashMap, sync::{Arc, Mutex}};
use page_pool_alloc::PagePool;
use user_driver::vfio::VfioDmaBuffer;
use user_driver::lockmem::LockedMemorySpawner;

#[derive(Clone)]
pub struct GlobalDmaManager {
    physical_ranges: Vec<MemoryRange>,
    bounce_buffers_manager: Vec<MemoryRange>,
    //clients: Mutex<Vec<Weak<DmaClient>>>,
    //client_thresholds: Mutex<Vec<(Weak<DmaClient>, usize)>>,

    page_pool: Option<PagePool>,
    clients: HashMap<String, Arc<DmaClient>>,
}

impl GlobalDmaManager {
    pub fn new(page_pool: Option<PagePool>) -> Self {
        GlobalDmaManager {
            physical_ranges: Vec::new(),
            bounce_buffers_manager: Vec::new(),
            //clients: Mutex::new(Vec::new()),
            //client_thresholds: Mutex::new(Vec::new()),
            page_pool,
            clients: HashMap::new(),
        }
    }

    pub fn create_client(&mut self, pci_id: String) -> Arc<DmaClient> {
        let client = DmaClient {
            dma_manager: Arc::new(Mutex::new(self.clone())), // Ensure `self` implements `Clone`.
        };
        let arc_client = Arc::new(client);
        self.clients.insert(pci_id, arc_client.clone()); // Store the cloned `Arc` in `clients`.
        arc_client // Return the `Arc<DmaClient>`.
    }

    pub fn get_client(&self, pci_id: &str) -> Option<Arc<DmaClient>> {
        self.clients.get(pci_id).cloned()
    }

    pub fn get_dma_buffer_allocator(
        &self,
        device_name: String,
    ) -> anyhow::Result<Arc<dyn VfioDmaBuffer>> {
        self.page_pool
            .as_ref()
            .map(|p : &PagePool| -> anyhow::Result<Arc<dyn VfioDmaBuffer>> {
                p.allocator(device_name)
                    .map(|alloc| Arc::new(alloc) as Arc<dyn VfioDmaBuffer>)
            })
            .unwrap_or(Ok(Arc::new(LockedMemorySpawner)))
    }
}

#[derive(Clone)]
pub struct DmaClient {
    dma_manager: Arc<Mutex<GlobalDmaManager>>,
}

impl user_driver::DmaClient for DmaClient {
    fn map_dma_ranges(
        &self,
        ranges: i32,
    ) -> anyhow::Result<Vec<i32>> {
        self.map_dma_ranges(ranges)
    }

    fn get_dma_buffer_allocator(
        &self,
        device_name: String,
    ) -> anyhow::Result<Arc<dyn VfioDmaBuffer>> {
        let manager = self.dma_manager.lock().unwrap();
        manager.get_dma_buffer_allocator(device_name)
    }

}

impl DmaClient {
    fn map_dma_ranges(
        &self,
        ranges: i32,
    ) -> anyhow::Result<Vec<i32>>
    {
        Ok(Vec::new())
    }

    pub fn get_dma_buffer_allocator(
        &self,
        device_name: String,
    ) -> anyhow::Result<Arc<dyn VfioDmaBuffer>> {
        let manager = self.dma_manager.lock().unwrap();
        manager.get_dma_buffer_allocator(device_name)
    }
}
