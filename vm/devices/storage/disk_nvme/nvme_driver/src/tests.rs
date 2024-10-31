// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use crate::queue_pair::QueuePair;
use crate::NvmeDriver;
use chipset_device::mmio::ExternallyManagedMmioIntercepts;
use disk_ramdisk::RamDisk;
use guid::Guid;
use nvme::NvmeControllerCaps;
use nvme_spec::nvm::DsmRange;
use pal_async::async_test;
use pal_async::DefaultDriver;
use pci_core::msi::MsiInterruptSet;
use scsi_buffers::OwnedRequestBuffers;
use std::sync::Arc;
use test_with_tracing::test;
use user_driver::emulated::DeviceSharedMemory;
use user_driver::emulated::EmulatedDevice;
use user_driver::DeviceBacking;
use vmcore::vm_task::SingleDriverBackend;
use vmcore::vm_task::VmTaskDriverSource;

#[async_test]
async fn test_nvme_driver_direct_dma(driver: DefaultDriver) {
    test_nvme_driver(driver, true).await;
}

#[async_test]
async fn test_nvme_driver_bounce_buffer(driver: DefaultDriver) {
    test_nvme_driver(driver, false).await;
}

async fn test_nvme_driver(driver: DefaultDriver, allow_dma: bool) {
    const MSIX_COUNT: u16 = 2;
    const IO_QUEUE_COUNT: u16 = 64;
    const CPU_COUNT: u32 = 64;

    let base_len = 64 << 20;
    let payload_len = QueuePair::required_dma_size() * 4;
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
    let nvme = nvme::NvmeController::new(
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

    let mut driver = NvmeDriver::new(&driver_source, CPU_COUNT, device)
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

#[async_test]
async fn test_nvme_save_restore(driver: DefaultDriver) {
    const MSIX_COUNT: u16 = 2;
    const IO_QUEUE_COUNT: u16 = 64;
    const CPU_COUNT: u32 = 64;

    let driver_source = VmTaskDriverSource::new(SingleDriverBackend::new(driver));
    let payload_len = QueuePair::required_dma_size() * 4;
    let emu_mem = DeviceSharedMemory::new(64 * 1024 * 1024, payload_len);
    let mut msi_x = MsiInterruptSet::new();
    let nvme_ctrl = nvme::NvmeController::new(
        &driver_source,
        emu_mem.guest_memory().clone(),
        &mut msi_x,
        &mut ExternallyManagedMmioIntercepts,
        NvmeControllerCaps {
            msix_count: MSIX_COUNT,
            max_io_queues: IO_QUEUE_COUNT,
            subsystem_id: Guid::default(),
        },
    );

    // Add a namespace so Identify Namespace command will succeed later.
    nvme_ctrl
        .client()
        .add_namespace(1, Arc::new(RamDisk::new(1024 * 1024, false).unwrap()))
        .await
        .unwrap();
    let device = EmulatedDevice::new(nvme_ctrl, msi_x, emu_mem);

    let mut nvme_driver = NvmeDriver::new(&driver_source, CPU_COUNT, device)
        .await
        .unwrap();

    let _ns1 = nvme_driver.namespace(1).await.unwrap();
    let saved_state = nvme_driver.save().await.unwrap();
    // As of today we do not save namespace data to avoid possible conflict
    // when namespace has changed during servicing.
    // TODO: Review and re-enable in future.
    assert_eq!(saved_state.namespaces.len(), 0);

    // Create a second set of devices since the ownership has been moved.
    let new_emu_mem = DeviceSharedMemory::new(64 * 1024 * 1024, payload_len);
    let mut new_msi_x = MsiInterruptSet::new();
    let new_nvme_ctrl = nvme::NvmeController::new(
        &driver_source,
        new_emu_mem.guest_memory().clone(),
        &mut new_msi_x,
        &mut ExternallyManagedMmioIntercepts,
        NvmeControllerCaps {
            msix_count: MSIX_COUNT,
            max_io_queues: IO_QUEUE_COUNT,
            subsystem_id: Guid::default(),
        },
    );
    let new_device = EmulatedDevice::new(new_nvme_ctrl, new_msi_x, new_emu_mem);
    let allocator = Arc::new(new_device.host_allocator());
    let _new_nvme_driver = NvmeDriver::restore(
        &driver_source,
        CPU_COUNT,
        allocator,
        new_device,
        &saved_state,
    )
    .await
    .unwrap();
}
