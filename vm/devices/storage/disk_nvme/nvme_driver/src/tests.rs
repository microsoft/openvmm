// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use crate::NvmeDriver;
use chipset_device::mmio::ExternallyManagedMmioIntercepts;
use chipset_device::mmio::MmioIntercept;
use chipset_device::pci::PciConfigSpace;
use guid::Guid;
use inspect::Inspect;
use inspect::InspectMut;
use nvme::NvmeController;
use nvme::NvmeControllerCaps;
use nvme_spec::nvm::DsmRange;
use nvme_spec::Cap;
use pal_async::async_test;
use pal_async::DefaultDriver;
use parking_lot::Mutex;
use pci_core::msi::MsiInterruptSet;
use scsi_buffers::OwnedRequestBuffers;
use std::sync::Arc;
use test_with_tracing::test;
use user_driver::emulated::DeviceSharedMemory;
use user_driver::emulated::EmulatedDevice;
use user_driver::emulated::EmulatedDmaAllocator;
use user_driver::emulated::Mapping;
use user_driver::interrupt::DeviceInterrupt;
use user_driver::memory::MemoryBlock;
use user_driver::memory::PAGE_SIZE;
use user_driver::DeviceBacking;
use user_driver::DeviceRegisterIo;
use user_driver::DmaClient;
use vmcore::vm_task::SingleDriverBackend;
use vmcore::vm_task::VmTaskDriverSource;
use zerocopy::IntoBytes;

#[async_test]
async fn test_nvme_driver_direct_dma(driver: DefaultDriver) {
    test_nvme_driver(driver, true).await;
}

#[async_test]
async fn test_nvme_driver_bounce_buffer(driver: DefaultDriver) {
    test_nvme_driver(driver, false).await;
}

#[async_test]
async fn test_nvme_save_restore(driver: DefaultDriver) {
    test_nvme_save_restore_inner(driver).await;
}

#[async_test]
async fn test_nvme_ioqueue_max_mqes(driver: DefaultDriver) {
    const MSIX_COUNT: u16 = 2;
    const IO_QUEUE_COUNT: u16 = 64;
    const CPU_COUNT: u32 = 64;

    let base_len = 64 << 20;
    let payload_len = 4 << 20;
    let mem = DeviceSharedMemory::new(base_len, payload_len);

    let driver_source = VmTaskDriverSource::new(SingleDriverBackend::new(driver));
    let mut msi_set = MsiInterruptSet::new();
    let nvme = NvmeController::new(
        &driver_source,
        mem.guest_memory().clone(),
        &mut msi_set,
        &mut ExternallyManagedMmioIntercepts,
        NvmeControllerCaps {
            msix_count: MSIX_COUNT,
            max_io_queues: IO_QUEUE_COUNT,
            subsystem_id: Guid::new_random(),
        },
    );

    let mut device = NvmeTestEmulatedDevice::new(nvme, msi_set, mem);
    // Setup mock response at offset 0
    let max_u16: u16 = 65535;
    let cap: Cap = Cap::new().with_mqes_z(max_u16);
    device.set_mock_response_u64(Some((0, cap.into())));
    let driver = NvmeDriver::new(&driver_source, CPU_COUNT, device).await;

    assert!(driver.is_ok());
}

#[async_test]
async fn test_nvme_ioqueue_invalid_mqes(driver: DefaultDriver) {
    const MSIX_COUNT: u16 = 2;
    const IO_QUEUE_COUNT: u16 = 64;
    const CPU_COUNT: u32 = 64;

    let base_len = 64 << 20;
    let payload_len = 4 << 20;
    let mem = DeviceSharedMemory::new(base_len, payload_len);

    let driver_source = VmTaskDriverSource::new(SingleDriverBackend::new(driver));
    let mut msi_set = MsiInterruptSet::new();
    let nvme = NvmeController::new(
        &driver_source,
        mem.guest_memory().clone(),
        &mut msi_set,
        &mut ExternallyManagedMmioIntercepts,
        NvmeControllerCaps {
            msix_count: MSIX_COUNT,
            max_io_queues: IO_QUEUE_COUNT,
            subsystem_id: Guid::new_random(),
        },
    );

    let mut device = NvmeTestEmulatedDevice::new(nvme, msi_set, mem);
    // Setup mock response at offset 0
    let cap: Cap = Cap::new().with_mqes_z(0);
    device.set_mock_response_u64(Some((0, cap.into())));
    let driver = NvmeDriver::new(&driver_source, CPU_COUNT, device).await;

    assert!(driver.is_err());
}

async fn test_nvme_driver(driver: DefaultDriver, allow_dma: bool) {
    const MSIX_COUNT: u16 = 2;
    const IO_QUEUE_COUNT: u16 = 64;
    const CPU_COUNT: u32 = 64;

    let base_len = 64 << 20;
    let payload_len = 4 << 20;
    let mem = DeviceSharedMemory::new(base_len, payload_len);
    let payload_mem = mem
        .guest_memory()
        .subrange(base_len as u64, payload_len as u64, false)
        .unwrap();
    let driver_dma_mem = if allow_dma {
        mem.guest_memory_for_driver_dma()
            .subrange(base_len as u64, payload_len as u64, false)
            .unwrap()
    } else {
        payload_mem.clone()
    };

    let buf_range = OwnedRequestBuffers::linear(0, 16384, true);

    let driver_source = VmTaskDriverSource::new(SingleDriverBackend::new(driver));
    let mut msi_set = MsiInterruptSet::new();
    let nvme = NvmeController::new(
        &driver_source,
        mem.guest_memory().clone(),
        &mut msi_set,
        &mut ExternallyManagedMmioIntercepts,
        NvmeControllerCaps {
            msix_count: MSIX_COUNT,
            max_io_queues: IO_QUEUE_COUNT,
            subsystem_id: Guid::new_random(),
        },
    );
    nvme.client()
        .add_namespace(1, disklayer_ram::ram_disk(2 << 20, false).unwrap())
        .await
        .unwrap();

    let device = EmulatedDevice::new(nvme, msi_set, mem);

    let driver = NvmeDriver::new(&driver_source, CPU_COUNT, device)
        .await
        .unwrap();

    let namespace = driver.namespace(1).await.unwrap();

    payload_mem.write_at(0, &[0xcc; 8192]).unwrap();
    namespace
        .write(
            0,
            1,
            2,
            false,
            &driver_dma_mem,
            buf_range.buffer(&payload_mem).range(),
        )
        .await
        .unwrap();

    namespace
        .read(
            1,
            0,
            32,
            &driver_dma_mem,
            buf_range.buffer(&payload_mem).range(),
        )
        .await
        .unwrap();
    let mut v = [0; 4096];
    payload_mem.read_at(0, &mut v).unwrap();
    assert_eq!(&v[..512], &[0; 512]);
    assert_eq!(&v[512..1536], &[0xcc; 1024]);
    assert!(v[1536..].iter().all(|&x| x == 0));

    namespace
        .deallocate(
            0,
            &[
                DsmRange {
                    context_attributes: 0,
                    starting_lba: 1000,
                    lba_count: 2000,
                },
                DsmRange {
                    context_attributes: 0,
                    starting_lba: 2,
                    lba_count: 2,
                },
            ],
        )
        .await
        .unwrap();

    assert_eq!(driver.fallback_cpu_count(), 0);

    // Test the fallback queue functionality.
    namespace
        .read(
            63,
            0,
            32,
            &driver_dma_mem,
            buf_range.buffer(&payload_mem).range(),
        )
        .await
        .unwrap();

    assert_eq!(driver.fallback_cpu_count(), 1);

    let mut v = [0; 4096];
    payload_mem.read_at(0, &mut v).unwrap();
    assert_eq!(&v[..512], &[0; 512]);
    assert_eq!(&v[512..1024], &[0xcc; 512]);
    assert!(v[1024..].iter().all(|&x| x == 0));

    driver.shutdown().await;
}

async fn test_nvme_save_restore_inner(driver: DefaultDriver) {
    // ===== SETUP =====
    const CPU_COUNT: u32 = 64;
    let base_len = 64 * 1024 * 1024;
    let driver_source = VmTaskDriverSource::new(SingleDriverBackend::new(driver.clone()));

    // ===== FIRST DRIVER INIT, SAVE, & TEARDOWN =====
    let (shared_mem_original, msi_x, nvme_ctrl) = create_driver_resources(base_len, 0, &driver_source);

    // Add a namespace so Identify Namespace command will succeed later.
    nvme_ctrl
        .client()
        .add_namespace(1, disklayer_ram::ram_disk(2 << 20, false).unwrap())
        .await
        .unwrap();

    let saved_state = {
        let device = EmulatedDevice::new(nvme_ctrl, msi_x, shared_mem_original.clone());
        let mut nvme_driver = NvmeDriver::new(&driver_source, CPU_COUNT, device)
            .await
            .unwrap();
        let _ns1 = nvme_driver.namespace(1).await.unwrap();
        let saved = nvme_driver.save().await.unwrap();

        nvme_driver.shutdown().await;
        saved
    };
    
    // As of today we do not save namespace data to avoid possible conflict when namespace has changed during servicing.
    assert_eq!(saved_state.namespaces.len(), 0);

    // ===== SECOND DRIVER RESOURCES AND KEEPALIVE SETUP =======
    let (shared_mem_copy, new_msi_x, mut new_nvme_ctrl) = create_driver_resources(base_len, 0, &driver_source);

    // Read Register::CC -> set CC.EN -> wait CSTS.RDY
    let mut dword = 0u32;
    new_nvme_ctrl.read_bar0(0x14, dword.as_mut_bytes()).unwrap();
    dword |= 1;
    new_nvme_ctrl.write_bar0(0x14, dword.as_bytes()).unwrap();
    user_driver::backoff::Backoff::new(&driver).back_off().await;

    // ===== COPY MEMORY =====
    let host_allocator_original = EmulatedDmaAllocator::new(shared_mem_original.clone());
    let mem_original= DmaClient::attach_dma_buffer(&host_allocator_original, base_len, 0).unwrap();
    copy_mem_block(&mem_original, shared_mem_copy.clone());

    // ====== SECOND DRIVER RESTORE & VERIFY =====
    let new_device = EmulatedDevice::new(new_nvme_ctrl, new_msi_x, shared_mem_copy);
    let mut new_nvme_driver = NvmeDriver::restore(&driver_source, CPU_COUNT, new_device, &saved_state)
        .await
        .unwrap();

    // Verify restore functions will panic if verification failed.
    new_nvme_driver.verify_restore(&saved_state, mem_original).await;
}

/// Creates required resources for the driver
fn create_driver_resources(base_len: usize, payload_len: usize, driver_source: &VmTaskDriverSource) -> (DeviceSharedMemory, MsiInterruptSet, NvmeController) {
    const MSIX_COUNT: u16 = 2;
    const IO_QUEUE_COUNT: u16 = 64;

    let mem = DeviceSharedMemory::new(base_len, 0);
    let mut msi_x = MsiInterruptSet::new();
    let nvme_ctrl = NvmeController::new(
        &driver_source,
        mem.guest_memory().clone(),
        &mut msi_x,
        &mut ExternallyManagedMmioIntercepts,
        NvmeControllerCaps {
            msix_count: MSIX_COUNT,
            max_io_queues: IO_QUEUE_COUNT,
            subsystem_id: Guid::new_random(),
        },
    );

    (mem, msi_x, nvme_ctrl)
}


/// Copies contents of mem_original to shared_mem_copy starting at pfn 0. Caller makes sure that mem_original.len() <= shared_mem_copy.capacity()
fn copy_mem_block(mem_original: &MemoryBlock, shared_mem_copy: DeviceSharedMemory) {
    let host_allocator_copy = EmulatedDmaAllocator::new(shared_mem_copy);
    let mem_copy= DmaClient::attach_dma_buffer(&host_allocator_copy, mem_original.len(), 0).unwrap();

    let mut data_transfer_buffer: [u8; PAGE_SIZE] = [0; PAGE_SIZE];

    let total_pages = mem_original.len()/PAGE_SIZE;
    for  pfn in 0..total_pages {
        mem_original.read_at(pfn * PAGE_SIZE, &mut data_transfer_buffer);
        mem_copy.write_at(pfn * PAGE_SIZE, &data_transfer_buffer);
    }
}

#[derive(Inspect)]
pub struct NvmeTestEmulatedDevice<T: InspectMut> {
    device: EmulatedDevice<T>,
    #[inspect(debug)]
    mocked_response_u32: Arc<Mutex<Option<(usize, u32)>>>,
    #[inspect(debug)]
    mocked_response_u64: Arc<Mutex<Option<(usize, u64)>>>,
}

#[derive(Inspect)]
pub struct NvmeTestMapping<T> {
    mapping: Mapping<T>,
    #[inspect(debug)]
    mocked_response_u32: Arc<Mutex<Option<(usize, u32)>>>,
    #[inspect(debug)]
    mocked_response_u64: Arc<Mutex<Option<(usize, u64)>>>,
}

impl<T: PciConfigSpace + MmioIntercept + InspectMut> NvmeTestEmulatedDevice<T> {
    /// Creates a new emulated device, wrapping `device`, using the provided MSI controller.
    pub fn new(device: T, msi_set: MsiInterruptSet, shared_mem: DeviceSharedMemory) -> Self {
        Self {
            device: EmulatedDevice::new(device, msi_set, shared_mem),
            mocked_response_u32: Arc::new(Mutex::new(None)),
            mocked_response_u64: Arc::new(Mutex::new(None)),
        }
    }

    // TODO: set_mock_response_u32 is intentionally not implemented to avoid dead code.
    pub fn set_mock_response_u64(&mut self, mapping: Option<(usize, u64)>) {
        let mut mock_response = self.mocked_response_u64.lock();
        *mock_response = mapping;
    }
}

/// Implementation of DeviceBacking trait for NvmeTestEmulatedDevice
impl<T: 'static + Send + InspectMut + MmioIntercept> DeviceBacking for NvmeTestEmulatedDevice<T> {
    type Registers = NvmeTestMapping<T>;

    fn id(&self) -> &str {
        self.device.id()
    }

    fn map_bar(&mut self, n: u8) -> anyhow::Result<Self::Registers> {
        Ok(NvmeTestMapping {
            mapping: self.device.map_bar(n).unwrap(),
            mocked_response_u32: Arc::clone(&self.mocked_response_u32),
            mocked_response_u64: Arc::clone(&self.mocked_response_u64),
        })
    }

    fn dma_client(&self) -> Arc<dyn DmaClient> {
        self.device.dma_client()
    }

    fn max_interrupt_count(&self) -> u32 {
        self.device.max_interrupt_count()
    }

    fn map_interrupt(&mut self, msix: u32, _cpu: u32) -> anyhow::Result<DeviceInterrupt> {
        self.device.map_interrupt(msix, _cpu)
    }
}

impl<T: MmioIntercept + Send> DeviceRegisterIo for NvmeTestMapping<T> {
    fn len(&self) -> usize {
        self.mapping.len()
    }

    fn read_u32(&self, offset: usize) -> u32 {
        let mock_response = self.mocked_response_u32.lock();

        // Intercept reads to the mocked offset address
        if let Some((mock_offset, mock_data)) = *mock_response {
            if mock_offset == offset {
                return mock_data;
            }
        }

        self.mapping.read_u32(offset)
    }

    fn read_u64(&self, offset: usize) -> u64 {
        let mock_response = self.mocked_response_u64.lock();

        // Intercept reads to the mocked offset address
        if let Some((mock_offset, mock_data)) = *mock_response {
            if mock_offset == offset {
                return mock_data;
            }
        }

        self.mapping.read_u64(offset)
    }

    fn write_u32(&self, offset: usize, data: u32) {
        self.mapping.write_u32(offset, data);
    }

    fn write_u64(&self, offset: usize, data: u64) {
        self.mapping.write_u64(offset, data);
    }
}
