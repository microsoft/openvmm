// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use memory_range::MemoryRange;
use std::{
    collections::HashMap,
    sync::Arc
};
use user_driver::{memory::MemoryBlock, vfio::VfioDmaBuffer};
use parking_lot::Mutex;

pub struct GlobalDmaManager {
    inner: Arc<Mutex<GlobalDmaManagerInner>>,
}

pub struct GlobalDmaManagerInner {
    _physical_ranges: Vec<MemoryRange>,
    _bounce_buffers_manager: Vec<MemoryRange>,
    //clients: Mutex<Vec<Weak<DmaClient>>>,
    //client_thresholds: Mutex<Vec<(Weak<DmaClient>, usize)>>,
    dma_buffer_spawner: Box<dyn Fn(String) -> anyhow::Result<Arc<dyn VfioDmaBuffer>> + Send>,
    clients: HashMap<String, Arc<DmaClientImpl>>,
}

impl GlobalDmaManager {
    pub fn new(
        dma_buffer_spawner: Box<dyn Fn(String) -> anyhow::Result<Arc<dyn VfioDmaBuffer>> + Send>,
    ) -> Self {
        let inner = GlobalDmaManagerInner {
            _physical_ranges: Vec::new(),
            _bounce_buffers_manager: Vec::new(),
            dma_buffer_spawner,
            clients: HashMap::new(),
        };

        GlobalDmaManager {
            inner: Arc::new(Mutex::new(inner)),
        }
    }

    fn create_client_internal(
        inner: &Arc<Mutex<GlobalDmaManagerInner>>,
        pci_id: String,
        device_name: String,
    ) -> anyhow::Result<Arc<DmaClientImpl>> {
        let mut manager_inner = inner.lock();
        let allocator = {
            // Access the page_pool and call its allocator method directly
            (manager_inner.dma_buffer_spawner)(device_name)
                .map_err(|e| anyhow::anyhow!("Failed to get DMA buffer allocator: {:?}", e))?
        };

        let client = DmaClientImpl {
            dma_manager_inner: inner.clone(),
            dma_buffer_allocator: Some(allocator.clone()), // Set the allocator now
        };

        // Create an Arc<DmaClient>
        let arc_client = Arc::new(client);

        // Insert the client into the clients HashMap
        //let mut inner = inner.lock().expect("Failed to lock GlobalDmaManagerInner");
        manager_inner.clients.insert(pci_id, arc_client.clone());

        Ok(arc_client) // Return the `Arc<Mutex<DmaClient>>`
    }

    pub fn get_client(&self, pci_id: &str) -> Option<Arc<DmaClientImpl>> {
        let inner = self.inner.lock();
        inner.clients.get(pci_id).cloned()
    }

    pub fn get_client_spawner(&self) -> DmaClientSpawner {
        DmaClientSpawner {
            dma_manager_inner: self.inner.clone(),
        }
    }
}

pub struct DmaClientImpl {
    dma_manager_inner: Arc<Mutex<GlobalDmaManagerInner>>,
    dma_buffer_allocator: Option<Arc<dyn VfioDmaBuffer>>,
}

impl user_driver::DmaClient for DmaClientImpl {
    fn map_dma_ranges(&self, ranges: i32) -> anyhow::Result<Vec<i32>> {
        self.map_dma_ranges(ranges)
    }

    fn allocate_dma_buffer(&self, total_size: usize) -> anyhow::Result<MemoryBlock> {
        if self.dma_buffer_allocator.is_none() {
            return Err(anyhow::anyhow!("DMA buffer allocator is not set"));
        }

        let allocator = self.dma_buffer_allocator.as_ref().unwrap();

        allocator.create_dma_buffer(total_size)
    }

    fn attach_dma_buffer(&self, len: usize, base_pfn: u64) -> anyhow::Result<MemoryBlock> {
        let allocator = self.dma_buffer_allocator.as_ref().unwrap();
        allocator.restore_dma_buffer(len, base_pfn)
    }
}

impl DmaClientImpl {
    fn map_dma_ranges(&self, _ranges: i32) -> anyhow::Result<Vec<i32>> {
        Ok(Vec::new())
    }
}

#[derive(Clone)]
pub struct DmaClientSpawner {
    dma_manager_inner: Arc<Mutex<GlobalDmaManagerInner>>,
}

impl DmaClientSpawner {
    pub fn create_client(
        &self,
        pci_id: String,
        device_name: String,
    ) -> anyhow::Result<Arc<DmaClientImpl>> {
        GlobalDmaManager::create_client_internal(&self.dma_manager_inner, pci_id, device_name)
    }
}
