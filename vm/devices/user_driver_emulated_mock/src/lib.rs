// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! This crate provides a collection of wrapper structs around things like devices and memory. Through the wrappers, it provides functionality to emulate devices such
//! as Nvme and Mana and gives some additional control over things like [`GuestMemory`] to make testing devices easier.
//! Everything in this crate is meant for TESTING PURPOSES ONLY and it should only ever be added as a dev-dependency (Few expceptions like using this for fuzzing)

mod guest_memory_access_wrapper;

use crate::guest_memory_access_wrapper::GuestMemoryAccessWrapper;

use anyhow::Context;
use chipset_device::mmio::MmioIntercept;
use chipset_device::pci::PciConfigSpace;
use guestmem::GuestMemory;
use inspect::Inspect;
use inspect::InspectMut;
use memory_range::MemoryRange;
use page_pool_alloc::PagePool;
use page_pool_alloc::PagePoolAllocator;
use page_pool_alloc::TestMapper;
use parking_lot::Mutex;
use pci_core::chipset_device_ext::PciChipsetDeviceExt;
use pci_core::msi::MsiControl;
use pci_core::msi::MsiInterruptSet;
use pci_core::msi::MsiInterruptTarget;
use std::sync::Arc;
use user_driver::DeviceBacking;
use user_driver::DeviceRegisterIo;
use user_driver::DmaClient;
use user_driver::interrupt::DeviceInterrupt;
use user_driver::interrupt::DeviceInterruptSource;
use user_driver::memory::PAGE_SIZE64;

/// A wrapper around any user_driver device T. It provides device emulation by providing access to the memory shared with the device and thus
/// allowing the user to control device behaviour to a certain extent. Can be used with devices such as the `NvmeController`
pub struct EmulatedDevice<T, U> {
    device: Arc<Mutex<T>>,
    controller: Arc<MsiController>,
    dma_client: Arc<U>,
    bar0_len: usize,
}

impl<T: InspectMut, U> Inspect for EmulatedDevice<T, U> {
    fn inspect(&self, req: inspect::Request<'_>) {
        self.device.lock().inspect_mut(req);
    }
}

struct MsiController {
    events: Arc<[DeviceInterruptSource]>,
}

impl MsiController {
    fn new(n: usize) -> Self {
        Self {
            events: (0..n).map(|_| DeviceInterruptSource::new()).collect(),
        }
    }
}

impl MsiInterruptTarget for MsiController {
    fn new_interrupt(&self) -> Box<dyn MsiControl> {
        let events = self.events.clone();
        Box::new(move |address, _data| {
            let index = address as usize;
            if let Some(event) = events.get(index) {
                tracing::debug!(index, "signaling interrupt");
                event.signal_uncached();
            } else {
                tracing::info!("interrupt ignored");
            }
        })
    }
}

impl<T: PciConfigSpace + MmioIntercept, U: DmaClient> Clone for EmulatedDevice<T, U> {
    fn clone(&self) -> Self {
        Self {
            device: self.device.clone(),
            controller: self.controller.clone(),
            dma_client: self.dma_client.clone(),
            bar0_len: self.bar0_len,
        }
    }
}

impl<T: PciConfigSpace + MmioIntercept, U: DmaClient> EmulatedDevice<T, U> {
    /// Creates a new emulated device, wrapping `device` of type T, using the provided MSI Interrupt Set. Dma_client should point to memory
    /// shared with the device.
    pub fn new(mut device: T, msi_set: MsiInterruptSet, dma_client: Arc<U>) -> Self {
        // Connect an interrupt controller.
        let controller = MsiController::new(msi_set.len());
        msi_set.connect(&controller);
        let controller = Arc::new(controller);

        let bars = device.probe_bar_masks();
        let bar0_len = !(bars[0] & !0xf) as usize + 1;

        // Enable BAR0 at 0, BAR4 at X.
        device.pci_cfg_write(0x20, 0).unwrap();
        device.pci_cfg_write(0x24, 0x1).unwrap();
        device
            .pci_cfg_write(
                0x4,
                pci_core::spec::cfg_space::Command::new()
                    .with_mmio_enabled(true)
                    .into_bits() as u32,
            )
            .unwrap();

        // Enable MSIX.
        for i in 0u64..64 {
            device
                .mmio_write((0x1 << 32) + i * 16, &i.to_ne_bytes())
                .unwrap();
            device
                .mmio_write((0x1 << 32) + i * 16 + 12, &0u32.to_ne_bytes())
                .unwrap();
        }
        device.pci_cfg_write(0x40, 0x80000000).unwrap();

        Self {
            device: Arc::new(Mutex::new(device)),
            controller,
            dma_client,
            bar0_len,
        }
    }
}

/// A memory mapping for an [`EmulatedDevice`].
#[derive(Inspect)]
pub struct Mapping<T> {
    #[inspect(skip)]
    device: Arc<Mutex<T>>,
    addr: u64,
    len: usize,
}

impl<T: 'static + Send + InspectMut + MmioIntercept, U: 'static + Send + DmaClient> DeviceBacking
    for EmulatedDevice<T, U>
{
    type Registers = Mapping<T>;

    fn id(&self) -> &str {
        "emulated"
    }

    fn map_bar(&mut self, n: u8) -> anyhow::Result<Self::Registers> {
        if n != 0 {
            anyhow::bail!("invalid bar {n}");
        }
        Ok(Mapping {
            device: self.device.clone(),
            addr: (n as u64) << 32,
            len: self.bar0_len,
        })
    }

    fn dma_client(&self) -> Arc<dyn DmaClient> {
        self.dma_client.clone()
    }

    fn dma_client_for(&self, _pool: user_driver::DmaPool) -> anyhow::Result<Arc<dyn DmaClient>> {
        // In the emulated device, we only have one dma client.
        Ok(self.dma_client.clone())
    }

    fn max_interrupt_count(&self) -> u32 {
        self.controller.events.len() as u32
    }

    fn map_interrupt(&mut self, msix: u32, _cpu: u32) -> anyhow::Result<DeviceInterrupt> {
        Ok(self
            .controller
            .events
            .get(msix as usize)
            .with_context(|| format!("invalid msix index {msix}"))?
            .new_target())
    }
}

impl<T: MmioIntercept + Send> DeviceRegisterIo for Mapping<T> {
    fn len(&self) -> usize {
        self.len
    }

    fn read_u32(&self, offset: usize) -> u32 {
        let mut n = [0; 4];
        self.device
            .lock()
            .mmio_read(self.addr + offset as u64, &mut n)
            .unwrap();
        u32::from_ne_bytes(n)
    }

    fn read_u64(&self, offset: usize) -> u64 {
        let mut n = [0; 8];
        self.device
            .lock()
            .mmio_read(self.addr + offset as u64, &mut n)
            .unwrap();
        u64::from_ne_bytes(n)
    }

    fn write_u32(&self, offset: usize, data: u32) {
        self.device
            .lock()
            .mmio_write(self.addr + offset as u64, &data.to_ne_bytes())
            .unwrap();
    }

    fn write_u64(&self, offset: usize, data: u64) {
        self.device
            .lock()
            .mmio_write(self.addr + offset as u64, &data.to_ne_bytes())
            .unwrap();
    }
}

/// A wrapper around the [`TestMapper`] that generates both [`GuestMemory`] and [`PagePoolAllocator`] backed
/// by the same underlying memory. Meant to provide shared memory for testing devices.
pub struct DeviceTestMemory {
    guest_mem: GuestMemory,
    payload_mem: GuestMemory,
    _pool: PagePool,
    allocator: Arc<PagePoolAllocator>,
}

impl DeviceTestMemory {
    /// Creates test memory that leverages the [`TestMapper`] as the backing. It creates 3 accessors for the underlying memory:
    /// guest_memory [`GuestMemory`] - Has access to the entire range.
    /// payload_memory [`GuestMemory`] - Has access to the second half of the range.
    /// dma_client [`PagePoolAllocator`] - Has access to the first half of the range.
    /// If the `allow_dma` switch is enabled, both guest_memory and payload_memory will report a base_iova of 0.
    pub fn new(num_pages: u64, allow_dma: bool, pool_name: &str) -> Self {
        let test_mapper = TestMapper::new(num_pages).unwrap();
        let sparse_mmap = test_mapper.sparse_mapping();
        let guest_mem = GuestMemoryAccessWrapper::create_test_guest_memory(sparse_mmap, allow_dma);
        let pool = PagePool::new(
            &[MemoryRange::from_4k_gpn_range(0..num_pages / 2)],
            test_mapper,
        )
        .unwrap();

        // Save page pool so that it is not dropped.
        let allocator = pool.allocator(pool_name.into()).unwrap();
        let range_half = num_pages / 2 * PAGE_SIZE64;
        Self {
            guest_mem: guest_mem.clone(),
            payload_mem: guest_mem.subrange(range_half, range_half, false).unwrap(),
            _pool: pool,
            allocator: Arc::new(allocator),
        }
    }

    /// Returns [`GuestMemory`] accessor to the underlying memory. Reports base_iova as 0 if `allow_dma` switch is enabled.
    pub fn guest_memory(&self) -> GuestMemory {
        self.guest_mem.clone()
    }

    /// Returns [`GuestMemory`] accessor to the second half of underlying memory. Reports base_iova as 0 if `allow_dma` switch is enabled.
    pub fn payload_mem(&self) -> GuestMemory {
        self.payload_mem.clone()
    }

    /// Returns [`PagePoolAllocator`] with access to the first half of the underlying memory.
    pub fn dma_client(&self) -> Arc<PagePoolAllocator> {
        self.allocator.clone()
    }
}

/// Callbacks for the [`DeviceTestDmaClient`]. Tests supply these to customize the behaviour of the dma client.
pub trait DeviceTestDmaClientCallbacks: Sync + Send {
    /// Called when the DMA client needs to allocate a new DMA buffer.
    fn allocate_dma_buffer(
        &self,
        allocator: &PagePoolAllocator,
        total_size: usize,
    ) -> anyhow::Result<user_driver::memory::MemoryBlock>;

    /// Called when the DMA client needs to attach pending buffers.
    fn attach_pending_buffers(
        &self,
        inner: &PagePoolAllocator,
    ) -> anyhow::Result<Vec<user_driver::memory::MemoryBlock>>;
}

/// A DMA client that uses a [`PagePoolAllocator`] as the backing. It can be customized through the use of
/// [`DeviceTestDmaClientCallbacks`] to modify its behaviour for testing purposes.
///
/// # Example
/// ```rust
/// use std::sync::Arc;
/// use user_driver::DmaClient;
/// use user_driver_emulated_mock::DeviceTestDmaClient;
/// use page_pool_alloc::PagePoolAllocator;
///
/// struct MyCallbacks;
/// impl user_driver_emulated_mock::DeviceTestDmaClientCallbacks for MyCallbacks {
///     fn allocate_dma_buffer(
///         &self,
///         allocator: &page_pool_alloc::PagePoolAllocator,
///         total_size: usize,
///     ) -> anyhow::Result<user_driver::memory::MemoryBlock> {
///         // Custom test logic here, for example:
///         anyhow::bail!("allocation failed for testing");
///     }
///
///     fn attach_pending_buffers(
///         &self,
///         allocator: &page_pool_alloc::PagePoolAllocator,
///     ) -> anyhow::Result<Vec<user_driver::memory::MemoryBlock>> {
///         // Custom test logic here, for example:
///         anyhow::bail!("attachment failed for testing");
///     }
/// }
///
/// // Use the above in a test ...
/// fn test_dma_client() {
///     let pages = 1000;
///     let device_test_memory = user_driver_emulated_mock::DeviceTestMemory::new(
///         pages,
///         true,
///         "test_dma_client",
///     );
///     let page_pool_allocator = device_test_memory.dma_client();
///     let dma_client = DeviceTestDmaClient::new(page_pool_allocator, MyCallbacks);
///
///     // Use dma_client in tests...
///     assert!(dma_client.allocate_dma_buffer(4096).is_err());
/// }
/// ```
#[derive(Inspect)]
#[inspect(transparent)]
pub struct DeviceTestDmaClient<C>
where
    C: DeviceTestDmaClientCallbacks,
{
    inner: Arc<PagePoolAllocator>,
    #[inspect(skip)]
    callbacks: C,
}

impl<C: DeviceTestDmaClientCallbacks> DeviceTestDmaClient<C> {
    /// Creates a new [`DeviceTestDmaClient`] with the given inner allocator.
    pub fn new(inner: Arc<PagePoolAllocator>, callbacks: C) -> Self {
        Self { inner, callbacks }
    }
}

impl<C: DeviceTestDmaClientCallbacks> DmaClient for DeviceTestDmaClient<C> {
    fn allocate_dma_buffer(
        &self,
        total_size: usize,
    ) -> anyhow::Result<user_driver::memory::MemoryBlock> {
        self.callbacks.allocate_dma_buffer(&self.inner, total_size)
    }

    fn attach_pending_buffers(&self) -> anyhow::Result<Vec<user_driver::memory::MemoryBlock>> {
        self.callbacks.attach_pending_buffers(&self.inner)
    }
}
