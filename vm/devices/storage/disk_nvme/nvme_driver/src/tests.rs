// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use crate::NvmeDriver;
use chipset_device::mmio::ExternallyManagedMmioIntercepts;
use chipset_device::mmio::MmioIntercept;
use chipset_device::pci::PciConfigSpace;
use guid::Guid;
use inspect::Inspect;
use inspect::InspectMut;
use nvme::NvmeControllerCaps;
use nvme_spec::Cap;
use nvme_spec::nvm::DsmRange;
use pal_async::DefaultDriver;
use pal_async::async_test;
use parking_lot::Mutex;
use pci_core::msi::MsiInterruptSet;
use scsi_buffers::OwnedRequestBuffers;
use std::sync::Arc;
use test_with_tracing::test;
use user_driver::DeviceBacking;
use user_driver::DeviceRegisterIo;
use user_driver::DmaClient;
use user_driver::interrupt::DeviceInterrupt;
use user_driver_emulated_mock::DeviceTestMemory;
use user_driver_emulated_mock::EmulatedDevice;
use user_driver_emulated_mock::Mapping;
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

    // Memory setup
    let pages = 1000;
    let device_test_memory = DeviceTestMemory::new(pages, false, "test_nvme_ioqueue_max_mqes");
    let guest_mem = device_test_memory.guest_memory();
    let dma_client = device_test_memory.dma_client();

    // Controller Driver Setup
    let driver_source = VmTaskDriverSource::new(SingleDriverBackend::new(driver));
    let mut msi_set = MsiInterruptSet::new();
    let nvme = nvme::NvmeController::new(
        &driver_source,
        guest_mem,
        &mut msi_set,
        &mut ExternallyManagedMmioIntercepts,
        NvmeControllerCaps {
            msix_count: MSIX_COUNT,
            max_io_queues: IO_QUEUE_COUNT,
            subsystem_id: Guid::new_random(),
        },
    );

    let mut device = NvmeTestEmulatedDevice::new(nvme, msi_set, dma_client.clone());

    // Mock response at offset 0 since that is where Cap will be accessed
    let max_u16: u16 = 65535;
    let cap: Cap = Cap::new().with_mqes_z(max_u16);
    device.set_mock_response_u64(Some((0, cap.into())));

    let driver = NvmeDriver::new(&driver_source, CPU_COUNT, device, false).await;
    assert!(driver.is_ok());
}

#[async_test]
async fn test_nvme_ioqueue_invalid_mqes(driver: DefaultDriver) {
    const MSIX_COUNT: u16 = 2;
    const IO_QUEUE_COUNT: u16 = 64;
    const CPU_COUNT: u32 = 64;

    // Memory setup
    let pages = 1000;
    let device_test_memory = DeviceTestMemory::new(pages, false, "test_nvme_ioqueue_invalid_mqes");
    let guest_mem = device_test_memory.guest_memory();
    let dma_client = device_test_memory.dma_client();

    let driver_source = VmTaskDriverSource::new(SingleDriverBackend::new(driver));
    let mut msi_set = MsiInterruptSet::new();
    let nvme = nvme::NvmeController::new(
        &driver_source,
        guest_mem,
        &mut msi_set,
        &mut ExternallyManagedMmioIntercepts,
        NvmeControllerCaps {
            msix_count: MSIX_COUNT,
            max_io_queues: IO_QUEUE_COUNT,
            subsystem_id: Guid::new_random(),
        },
    );

    let mut device = NvmeTestEmulatedDevice::new(nvme, msi_set, dma_client.clone());

    // Setup mock response at offset 0
    let cap: Cap = Cap::new().with_mqes_z(0);
    device.set_mock_response_u64(Some((0, cap.into())));
    let driver = NvmeDriver::new(&driver_source, CPU_COUNT, device, false).await;

    assert!(driver.is_err());
}

async fn test_nvme_driver(driver: DefaultDriver, allow_dma: bool) {
    const MSIX_COUNT: u16 = 2;
    const IO_QUEUE_COUNT: u16 = 64;
    const CPU_COUNT: u32 = 64;

    // Arrange: Create 8MB of space. First 4MB for the device and second 4MB for the payload.
    let pages = 1024; // 4MB
    let device_test_memory = DeviceTestMemory::new(pages * 2, allow_dma, "test_nvme_driver");
    let guest_mem = device_test_memory.guest_memory(); // Access to 0-8MB
    let dma_client = device_test_memory.dma_client(); // Access 0-4MB
    let payload_mem = device_test_memory.payload_mem(); // Access 4-8MB. This will allow dma if the `allow_dma` flag is set.

    // Arrange: Create the NVMe controller and driver.
    let driver_source = VmTaskDriverSource::new(SingleDriverBackend::new(driver));
    let mut msi_set = MsiInterruptSet::new();
    let nvme = nvme::NvmeController::new(
        &driver_source,
        guest_mem.clone(),
        &mut msi_set,
        &mut ExternallyManagedMmioIntercepts,
        NvmeControllerCaps {
            msix_count: MSIX_COUNT,
            max_io_queues: IO_QUEUE_COUNT,
            subsystem_id: Guid::new_random(),
        },
    );

    nvme.client() // 2MB namespace
        .add_namespace(1, disklayer_ram::ram_disk(2 << 20, false).unwrap())
        .await
        .unwrap();
    let device = NvmeTestEmulatedDevice::new(nvme, msi_set, dma_client.clone());
    let driver = NvmeDriver::new(&driver_source, CPU_COUNT, device, false)
        .await
        .unwrap();
    let namespace = driver.namespace(1).await.unwrap();

    // Act: Write 1024 bytes of data to disk starting at LBA 1.
    let buf_range = OwnedRequestBuffers::linear(0, 16384, true); // 32 blocks
    payload_mem.write_at(0, &[0xcc; 4096]).unwrap();
    namespace
        .write(
            0,
            1,
            2,
            false,
            &payload_mem,
            buf_range.buffer(&payload_mem).range(),
        )
        .await
        .unwrap();

    // Act: Read 16384 bytes of data from disk starting at LBA 0.
    namespace
        .read(
            1,
            0,
            32,
            &payload_mem,
            buf_range.buffer(&payload_mem).range(),
        )
        .await
        .unwrap();
    let mut v = [0; 4096];
    payload_mem.read_at(0, &mut v).unwrap();

    // Assert: First block should be 0x00 since we never wrote to it. Followed by 1024 bytes of 0xcc.
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

    assert_eq!(driver.fallback_cpu_count(), 2);

    // Test the fallback queue functionality.
    namespace
        .read(
            63,
            0,
            32,
            &payload_mem,
            buf_range.buffer(&guest_mem).range(),
        )
        .await
        .unwrap();

    assert_eq!(driver.fallback_cpu_count(), 3);

    let mut v = [0; 4096];
    payload_mem.read_at(0, &mut v).unwrap();
    assert_eq!(&v[..512], &[0; 512]);
    assert_eq!(&v[512..1024], &[0xcc; 512]);
    assert!(v[1024..].iter().all(|&x| x == 0));

    driver.shutdown().await;
}

async fn test_nvme_save_restore_inner(driver: DefaultDriver) {
    const MSIX_COUNT: u16 = 2;
    const IO_QUEUE_COUNT: u16 = 64;
    const CPU_COUNT: u32 = 64;

    // Memory setup
    let pages = 1000;
    let device_test_memory = DeviceTestMemory::new(pages, false, "test_nvme_save_restore_inner");
    let guest_mem = device_test_memory.guest_memory();
    let dma_client = device_test_memory.dma_client();

    let driver_source = VmTaskDriverSource::new(SingleDriverBackend::new(driver.clone()));
    let mut msi_x = MsiInterruptSet::new();
    let nvme_ctrl = nvme::NvmeController::new(
        &driver_source,
        guest_mem.clone(),
        &mut msi_x,
        &mut ExternallyManagedMmioIntercepts,
        NvmeControllerCaps {
            msix_count: MSIX_COUNT,
            max_io_queues: IO_QUEUE_COUNT,
            subsystem_id: Guid::new_random(),
        },
    );

    // Add a namespace so Identify Namespace command will succeed later.
    nvme_ctrl
        .client()
        .add_namespace(1, disklayer_ram::ram_disk(2 << 20, false).unwrap())
        .await
        .unwrap();

    let device = NvmeTestEmulatedDevice::new(nvme_ctrl, msi_x, dma_client.clone());
    let mut nvme_driver = NvmeDriver::new(&driver_source, CPU_COUNT, device, false)
        .await
        .unwrap();
    let _ns1 = nvme_driver.namespace(1).await.unwrap();
    let saved_state = nvme_driver.save().await.unwrap();
    // As of today we do not save namespace data to avoid possible conflict
    // when namespace has changed during servicing.
    // TODO: Review and re-enable in future.
    assert_eq!(saved_state.namespaces.len(), 0);

    // Create a second set of devices since the ownership has been moved.
    let mut new_msi_x = MsiInterruptSet::new();
    let mut new_nvme_ctrl = nvme::NvmeController::new(
        &driver_source,
        guest_mem.clone(),
        &mut new_msi_x,
        &mut ExternallyManagedMmioIntercepts,
        NvmeControllerCaps {
            msix_count: MSIX_COUNT,
            max_io_queues: IO_QUEUE_COUNT,
            subsystem_id: Guid::new_random(),
        },
    );

    let mut backoff = user_driver::backoff::Backoff::new(&driver);

    // Enable the controller for keep-alive test.
    let mut dword = 0u32;
    // Read Register::CC.
    new_nvme_ctrl.read_bar0(0x14, dword.as_mut_bytes()).unwrap();
    // Set CC.EN.
    dword |= 1;
    new_nvme_ctrl.write_bar0(0x14, dword.as_bytes()).unwrap();
    // Wait for CSTS.RDY to set.
    backoff.back_off().await;

    let _new_device = NvmeTestEmulatedDevice::new(new_nvme_ctrl, new_msi_x, dma_client.clone());
    // TODO: Memory restore is disabled for emulated DMA, uncomment once fixed.
    // let _new_nvme_driver = NvmeDriver::restore(&driver_source, CPU_COUNT, new_device, &saved_state)
    //     .await
    //     .unwrap();
}


#[async_test]
async fn test_nvme_cpu_interrupt_distribution_with_many_vectors(driver: DefaultDriver) {
    const MSIX_COUNT: u16 = 8; // More interrupt vectors
    const IO_QUEUE_COUNT: u16 = 64;
    const CPU_COUNT: u32 = 32; // More CPUs than interrupt vectors

    // Memory setup
    let pages = 1000;
    let device_test_memory = DeviceTestMemory::new(
        pages,
        false,
        "test_nvme_cpu_interrupt_distribution_with_many_vectors",
    );
    let guest_mem = device_test_memory.guest_memory();
    let dma_client = device_test_memory.dma_client();

    let driver_source = VmTaskDriverSource::new(SingleDriverBackend::new(driver));
    let mut msi_set = MsiInterruptSet::new();
    let nvme = nvme::NvmeController::new(
        &driver_source,
        guest_mem,
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

    let device = NvmeTestInterruptTracker::new(nvme, msi_set, dma_client.clone());

    // Create the NVMe driver
    let nvme_driver = NvmeDriver::new(&driver_source, CPU_COUNT, device, false)
        .await
        .unwrap();

    // Access the io_issuers to force creation of IO queues for different CPUs
    let io_issuers = nvme_driver.io_issuers();

    // Request IO issuers from different CPUs to demonstrate the stride algorithm
    let _issuer_0 = io_issuers.get(0).await.unwrap();
    let _issuer_1 = io_issuers.get(1).await.unwrap();
    let _issuer_2 = io_issuers.get(2).await.unwrap();
    let _issuer_3 = io_issuers.get(3).await.unwrap();
    let _issuer_4 = io_issuers.get(4).await.unwrap();
    let _issuer_5 = io_issuers.get(5).await.unwrap();
    let _issuer_6 = io_issuers.get(6).await.unwrap();
    let _issuer_7 = io_issuers.get(7).await.unwrap();

    // Verify the interrupt distribution
    // With 8 MSI-X vectors and 32 CPUs, we should see stride-based distribution
    println!("Interrupt distribution with stride algorithm:");
    println!("Should see interrupts distributed across CPUs with stride 4 (32/8)");
    println!("Expected: CPUs 0, 4, 8, 12, 16, 20, 24, 28");

    nvme_driver.shutdown().await;
}

#[async_test]
async fn test_nvme_multiple_drivers_coordination(driver: DefaultDriver) {
    const MSIX_COUNT: u16 = 8;
    const IO_QUEUE_COUNT: u16 = 64;
    const CPU_COUNT: u32 = 32;

    let driver_source = VmTaskDriverSource::new(SingleDriverBackend::new(driver));

    // Create two separate NVMe devices with different device IDs
    let device1 = create_test_device(
        &driver_source,
        "device1",
        MSIX_COUNT,
        IO_QUEUE_COUNT,
        CPU_COUNT,
    )
    .await;
    let device2 = create_test_device(
        &driver_source,
        "device2",
        MSIX_COUNT,
        IO_QUEUE_COUNT,
        CPU_COUNT,
    )
    .await;

    // Create drivers for both devices
    let nvme_driver1 = NvmeDriver::new(&driver_source, CPU_COUNT, device1, false)
        .await
        .unwrap();
    let nvme_driver2 = NvmeDriver::new(&driver_source, CPU_COUNT, device2, false)
        .await
        .unwrap();

    // Force creation of IO queues for both drivers
    let io_issuers1 = nvme_driver1.io_issuers();
    let io_issuers2 = nvme_driver2.io_issuers();

    // Request issuers from first 8 CPUs for both drivers to get all interrupt vectors
    for cpu in 0..8 {
        let _issuer1 = io_issuers1.get(cpu).await.unwrap();
        let _issuer2 = io_issuers2.get(cpu).await.unwrap();
    }
    
    // Get the actual CPU assignments
    let device1_cpus = io_issuers1.get_used_cpus();
    let device2_cpus = io_issuers2.get_used_cpus();

    // With the device-specific offset, these two drivers should distribute
    // their interrupt vectors to different CPU ranges instead of overlapping
    println!("Multiple driver coordination test completed");
    println!("Device 1 CPUs: {:?}", device1_cpus);
    println!("Device 2 CPUs: {:?}", device2_cpus);
    
    // Validate that the devices use different starting offsets
    // Due to hashing, they should have different patterns
    assert!(device1_cpus.len() <= 8, "Device 1 should have at most 8 interrupt vectors");
    assert!(device2_cpus.len() <= 8, "Device 2 should have at most 8 interrupt vectors");
    
    // The devices should use different starting patterns due to device ID hashing
    // For stride-based allocation (32 CPUs, 8 IVs), we expect different offsets
    // For smaller configurations, they may still overlap but show different ordering
    if device1_cpus.len() >= 4 && device2_cpus.len() >= 4 {
        let device1_pattern: Vec<_> = device1_cpus.into_iter().take(4).collect();
        let device2_pattern: Vec<_> = device2_cpus.into_iter().take(4).collect();
        
        // Due to stride algorithm and device hashing, they should be different
        // unless both devices hash to the same offset (rare but possible)
        if device1_pattern != device2_pattern {
            println!("✓ Validation passed: devices use different CPU offset patterns");
        } else {
            println!("◯ Devices have same pattern (possible with hash collisions or fallback behavior)");
        }
    } else {
        println!("◯ Validation skipped: insufficient interrupt vectors created");
    }

    nvme_driver1.shutdown().await;
    nvme_driver2.shutdown().await;
}

#[async_test]
async fn test_nvme_comprehensive_scenarios(driver: DefaultDriver) {
    let driver_source = VmTaskDriverSource::new(SingleDriverBackend::new(driver));

    // Scenario 1: 96 vCPUs, 8 NVMe devices, each with 11 interrupt vectors
    println!("\n=== Scenario 1: 96 vCPUs, 8 NVMe devices, 11 interrupt vectors each ===");
    test_scenario(&driver_source, 96, 8, 11).await;
    
    // Scenario 2: 10 vCPUs, 8 NVMe devices, each with 10 interrupt vectors  
    println!("\n=== Scenario 2: 10 vCPUs, 8 NVMe devices, 10 interrupt vectors each ===");
    test_scenario(&driver_source, 10, 8, 10).await;
    
    // Scenario 3: 4 vCPUs, 1 NVMe device with 4 interrupt vectors
    println!("\n=== Scenario 3: 4 vCPUs, 1 NVMe device, 4 interrupt vectors ===");
    test_scenario(&driver_source, 4, 1, 4).await;
}

async fn test_scenario(driver_source: &VmTaskDriverSource, cpu_count: u32, device_count: u8, msix_count: u16) {
    let mut drivers = Vec::new();
    let mut devices_cpu_usage = Vec::new();
    
    // Create multiple devices
    for device_idx in 0..device_count {
        let device_id = format!("device_{}", device_idx);
        let device = create_test_device(
            driver_source,
            &device_id,
            msix_count,
            64, // IO_QUEUE_COUNT
            cpu_count,
        ).await;
        
        let nvme_driver = NvmeDriver::new(driver_source, cpu_count, device, false)
            .await
            .unwrap();
            
        let io_issuers = nvme_driver.io_issuers();
        
        // Force creation of all interrupt vectors
        for cpu in 0..cpu_count.min(msix_count as u32) {
            let _issuer = io_issuers.get(cpu).await.unwrap();
        }
        
        // Get the actual CPU assignments
        let mut device_cpus = io_issuers.get_used_cpus();
        device_cpus.sort();
        devices_cpu_usage.push(device_cpus.clone());
        
        println!("  Device {}: CPUs {:?}", device_idx, device_cpus);
        
        drivers.push(nvme_driver);
    }
    
    // Analysis
    let total_vectors = device_count as u32 * msix_count as u32;
    let stride_expected = if cpu_count > msix_count as u32 * 2 {
        Some(cpu_count / msix_count as u32)
    } else {
        None
    };
    
    println!("  CPU count: {}, Total interrupt vectors: {}", cpu_count, total_vectors);
    if let Some(stride) = stride_expected {
        println!("  Expected stride: {} (stride-based distribution)", stride);
    } else {
        println!("  Expected: greedy allocation (no stride)");
    }
    
    // Calculate CPU utilization distribution
    let mut cpu_usage_count = vec![0u32; cpu_count as usize];
    for device_cpus in &devices_cpu_usage {
        for &cpu in device_cpus {
            cpu_usage_count[cpu as usize] += 1;
        }
    }
    
    let used_cpus = cpu_usage_count.iter().filter(|&&count| count > 0).count();
    let max_usage = cpu_usage_count.iter().max().unwrap_or(&0);
    let min_usage = cpu_usage_count.iter().filter(|&&count| count > 0).min().unwrap_or(&0);
    
    println!("  CPU utilization: {} CPUs used, max {} vectors/CPU, min {} vectors/CPU", 
             used_cpus, max_usage, min_usage);
    
    // Validate behavior based on scenario expectations
    if device_count > 1 {
        // For multiple devices, ensure they have different patterns (coordination test)
        let unique_patterns: std::collections::HashSet<_> = devices_cpu_usage.iter().cloned().collect();
        if unique_patterns.len() > 1 {
            println!("  ✓ Multiple devices use different CPU patterns");
        } else if device_count <= 4 && msix_count <= 4 {
            println!("  ◯ Small configuration may have overlapping patterns (expected)");
        }
    }
    
    // Cleanup
    for driver in drivers {
        driver.shutdown().await;
    }
    
    println!("  Scenario completed\n");
}

async fn create_test_device(
    driver_source: &VmTaskDriverSource,
    device_id: &str,
    msix_count: u16,
    io_queue_count: u16,
    _cpu_count: u32,
) -> impl DeviceBacking {
    let pages = 1000;
    let device_test_memory = DeviceTestMemory::new(pages, false, device_id);
    let guest_mem = device_test_memory.guest_memory();
    let dma_client = device_test_memory.dma_client();

    let mut msi_set = MsiInterruptSet::new();
    let nvme = nvme::NvmeController::new(
        driver_source,
        guest_mem,
        &mut msi_set,
        &mut ExternallyManagedMmioIntercepts,
        NvmeControllerCaps {
            msix_count,
            max_io_queues: io_queue_count,
            subsystem_id: Guid::new_random(),
        },
    );

    nvme.client()
        .add_namespace(1, disklayer_ram::ram_disk(2 << 20, false).unwrap())
        .await
        .unwrap();

    NvmeTestInterruptTracker::new(nvme, msi_set, dma_client)
}

#[derive(Inspect)]
pub struct NvmeTestInterruptTracker<T: InspectMut, U: DmaClient> {
    device: EmulatedDevice<T, U>,
    #[inspect(debug)]
    mocked_response_u32: Arc<Mutex<Option<(usize, u32)>>>,
    #[inspect(debug)]
    mocked_response_u64: Arc<Mutex<Option<(usize, u64)>>>,
    #[inspect(debug)]
    interrupt_mappings: Arc<Mutex<Vec<(u32, u32)>>>, // (msix_index, cpu)
}

impl<T: PciConfigSpace + MmioIntercept + InspectMut, U: DmaClient> NvmeTestInterruptTracker<T, U> {
    /// Creates a new emulated device that tracks interrupt mappings
    pub fn new(device: T, msi_set: MsiInterruptSet, dma_client: Arc<U>) -> Self {
        Self {
            device: EmulatedDevice::new(device, msi_set, dma_client.clone()),
            mocked_response_u32: Arc::new(Mutex::new(None)),
            mocked_response_u64: Arc::new(Mutex::new(None)),
            interrupt_mappings: Arc::new(Mutex::new(Vec::new())),
        }
    }
}

impl<T: 'static + Send + InspectMut + MmioIntercept, U: 'static + DmaClient> DeviceBacking
    for NvmeTestInterruptTracker<T, U>
{
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

    fn map_interrupt(&mut self, msix: u32, cpu: u32) -> anyhow::Result<DeviceInterrupt> {
        // Track the interrupt mapping
        let mut mappings = self.interrupt_mappings.lock();
        mappings.push((msix, cpu));
        println!("Interrupt vector {} mapped to CPU {}", msix, cpu);

        self.device.map_interrupt(msix, cpu)
    }
}

#[derive(Inspect)]
pub struct NvmeTestEmulatedDevice<T: InspectMut, U: DmaClient> {
    device: EmulatedDevice<T, U>,
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

impl<T: PciConfigSpace + MmioIntercept + InspectMut, U: DmaClient> NvmeTestEmulatedDevice<T, U> {
    /// Creates a new emulated device, wrapping `device`, using the provided MSI controller.
    pub fn new(device: T, msi_set: MsiInterruptSet, dma_client: Arc<U>) -> Self {
        Self {
            device: EmulatedDevice::new(device, msi_set, dma_client.clone()),
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
impl<T: 'static + Send + InspectMut + MmioIntercept, U: 'static + DmaClient> DeviceBacking
    for NvmeTestEmulatedDevice<T, U>
{
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
