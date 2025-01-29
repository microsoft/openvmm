// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use parking_lot::Mutex;
use std::sync::Arc;
use user_driver::{memory::MemoryBlock, vfio::VfioDmaBuffer};

pub struct GlobalDmaManager {
    inner: Arc<Mutex<GlobalDmaManagerInner>>,
}

pub struct GlobalDmaManagerInner {
    dma_buffer_spawner: Box<dyn Fn(String) -> anyhow::Result<Arc<dyn VfioDmaBuffer>> + Send>,
}

impl GlobalDmaManager {
    pub fn new(
        dma_buffer_spawner: Box<dyn Fn(String) -> anyhow::Result<Arc<dyn VfioDmaBuffer>> + Send>,
    ) -> Self {
        let inner = GlobalDmaManagerInner { dma_buffer_spawner };

        GlobalDmaManager {
            inner: Arc::new(Mutex::new(inner)),
        }
    }

    fn create_client_internal(
        inner: &Arc<Mutex<GlobalDmaManagerInner>>,
        device_name: String,
    ) -> anyhow::Result<Arc<DmaClientImpl>> {
        let manager_inner = inner.lock();
        let allocator = {
            // Access the page_pool and call its allocator method directly
            (manager_inner.dma_buffer_spawner)(device_name)
                .map_err(|e| anyhow::anyhow!("Failed to get DMA buffer allocator: {:?}", e))?
        };

        let client = DmaClientImpl {
            _dma_manager_inner: inner.clone(),
            dma_buffer_allocator: Some(allocator.clone()), // Set the allocator now
        };

        // Create an Arc<DmaClient>
        let arc_client = Arc::new(client);

        Ok(arc_client) // Return the `Arc<Mutex<DmaClient>>`
    }

    pub fn get_client_spawner(&self) -> DmaClientSpawner {
        DmaClientSpawner {
            dma_manager_inner: self.inner.clone(),
        }
    }
}

pub struct DmaClientImpl {
    /// This is added to support map/pin functionality in the future.
    _dma_manager_inner: Arc<Mutex<GlobalDmaManagerInner>>,
    dma_buffer_allocator: Option<Arc<dyn VfioDmaBuffer>>,
}

impl user_driver::DmaClient for DmaClientImpl {
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

#[derive(Clone)]
pub struct DmaClientSpawner {
    dma_manager_inner: Arc<Mutex<GlobalDmaManagerInner>>,
}

impl DmaClientSpawner {
    pub fn create_client(&self, device_name: String) -> anyhow::Result<Arc<DmaClientImpl>> {
        GlobalDmaManager::create_client_internal(&self.dma_manager_inner, device_name)
    }
}
