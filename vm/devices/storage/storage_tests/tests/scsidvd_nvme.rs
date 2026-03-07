// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Tests using NVMe as the block backend for SimpleScsiDvd.

#![cfg(any(windows, target_os = "linux"))]

use chipset_device::mmio::ExternallyManagedMmioIntercepts;
use disk_backend::Disk;
use disk_nvme::NvmeDisk;
use guestmem::GuestMemory;
use guid::Guid;
use nvme::NvmeController;
use nvme::NvmeControllerCaps;
use nvme_driver::NvmeDriver;
use page_pool_alloc::PagePoolAllocator;
use pal_async::DefaultDriver;
use pal_async::async_test;
use pci_core::msi::MsiConnection;
use scsi_buffers::OwnedRequestBuffers;
use scsi_buffers::RequestBuffers;
use scsi_core::AsyncScsiDisk;
use scsi_core::Request;
use scsi_defs::ISO_SECTOR_SIZE;
use scsi_defs::ScsiOp;
use scsidisk::scsidvd::SimpleScsiDvd;
use test_with_tracing::test;
use user_driver_emulated_mock::DeviceTestMemory;
use user_driver_emulated_mock::EmulatedDevice;
use vmcore::vm_task::SingleDriverBackend;
use vmcore::vm_task::VmTaskDriverSource;
use zerocopy::IntoBytes;

struct ScsiDvdNvmeTest {
    scsi_dvd: SimpleScsiDvd,
    _nvme_driver: NvmeDriver<EmulatedDevice<NvmeController, PagePoolAllocator>>, // We need to store this to keep it from going out of scope
}

impl ScsiDvdNvmeTest {
    async fn new(
        driver: DefaultDriver,
        sector_size: u32,
        sector_count: u64,
        read_only: bool,
    ) -> Self {
        const MSIX_COUNT: u16 = 2;
        const IO_QUEUE_COUNT: u16 = 64;
        const CPU_COUNT: u32 = 64;

        let driver_source = VmTaskDriverSource::new(SingleDriverBackend::new(driver));
        // First 4MB for the device and second 4MB for the payload
        let pages = 1024; // 4MB
        let mem = DeviceTestMemory::new(pages * 2, false, "storage_tests_scsidvd_nvme");
        let guest_mem = mem.guest_memory();
        let dma_client = mem.dma_client();
        let payload_mem = mem.payload_mem();

        let msi_conn = MsiConnection::new();
        let nvme = NvmeController::new(
            &driver_source,
            guest_mem.clone(),
            msi_conn.target(),
            &mut ExternallyManagedMmioIntercepts,
            NvmeControllerCaps {
                msix_count: MSIX_COUNT,
                max_io_queues: IO_QUEUE_COUNT,
                subsystem_id: Guid::new_random(),
            },
        );

        let buf = make_repeat_data_buffer(sector_count as usize, sector_size as usize);
        payload_mem.write_at(0, &buf).unwrap();

        nvme.client()
            .add_namespace(
                1,
                disklayer_ram::ram_disk(sector_size as u64 * sector_count, read_only).unwrap(),
            )
            .await
            .unwrap();

        let device = EmulatedDevice::new(nvme, msi_conn, dma_client.clone());
        let mut nvme_driver = NvmeDriver::new(&driver_source, CPU_COUNT, device, false)
            .await
            .unwrap();
        let namespace = nvme_driver.namespace(1).await.unwrap();
        let buf_range = OwnedRequestBuffers::linear(0, 16384, true);
        for i in 0..(sector_count / 8) {
            namespace
                .write(
                    0,
                    i * 8,
                    8,
                    false,
                    &payload_mem,
                    buf_range.buffer(&payload_mem).range(),
                )
                .await
                .unwrap();
        }
        let disk = NvmeDisk::new(namespace);

        let scsi_dvd = SimpleScsiDvd::new(Some(Disk::new(disk).unwrap()));
        Self {
            scsi_dvd,
            _nvme_driver: nvme_driver,
        }
    }
}

fn make_repeat_data_buffer(sector_count: usize, sector_size: usize) -> Vec<u8> {
    let mut buf = vec![0u8; sector_count * sector_size];
    let mut temp = vec![0u8; sector_size];
    assert!(sector_size > 2);
    temp[sector_size / 2 - 1] = 2;
    temp[sector_size / 2] = 3;

    for i in (0..buf.len()).step_by(temp.len()) {
        let end_point = i + temp.len();
        buf[i..end_point].copy_from_slice(&temp);
    }

    buf
}

async fn check_execute_scsi(
    scsi_dvd: &mut SimpleScsiDvd,
    external_data: &RequestBuffers<'_>,
    request: &Request,
    should_succeed: bool,
) {
    let result = scsi_dvd.execute_scsi(external_data, request).await;
    assert_eq!(
        should_succeed,
        result.scsi_status == scsi_defs::ScsiStatus::GOOD,
        "execute_scsi: expected {:?}, result {:?}, request: {:?}",
        should_succeed,
        result,
        request
    );
}

fn make_cdb16_request(operation_code: ScsiOp, start_lba: u64, lba_count: u32) -> Request {
    let cdb = scsi_defs::Cdb16 {
        operation_code,
        flags: scsi_defs::Cdb16Flags::new(),
        logical_block: start_lba.into(),
        transfer_blocks: lba_count.into(),
        reserved2: 0,
        control: 0,
    };
    let mut data = [0u8; 16];
    data[..].copy_from_slice(cdb.as_bytes());
    Request {
        cdb: data,
        srb_flags: 0,
    }
}

fn check_guest_memory(
    guest_mem: &GuestMemory,
    start_lba: u64,
    buff: &[u8],
    sector_size: usize,
) -> bool {
    let mut b = vec![0u8; buff.len()];
    guest_mem
        .read_at(start_lba, &mut b)
        .expect("guest_mem read error");
    buff[..].eq(&b[..]) && (b[sector_size / 2 - 1] == 2) && (b[sector_size / 2] == 3)
}

#[async_test]
async fn validate_new_scsi_dvd_nvme_512(driver: DefaultDriver) {
    // Read-only is set to false because the underlying NVMe controller only supports read-write access
    // because it only implements NVMe spec v1.2, while "namespace write protection" was only added in v1.4b.
    // This parameter should be changed to true if a later spec supporting this is implemented.
    ScsiDvdNvmeTest::new(driver, 512, 2048, false).await;
}

#[async_test]
async fn validate_new_scsi_dvd_nvme_4096(driver: DefaultDriver) {
    // Read-only is set to false because the underlying NVMe controller only supports read-write access
    // because it only implements NVMe spec v1.2, while "namespace write protection" was only added in v1.4b.
    // This parameter should be changed to true if a later spec supporting this is implemented.
    ScsiDvdNvmeTest::new(driver, 4096, 256, false).await;
}

#[async_test]
async fn validate_read16_nvme_512(driver: DefaultDriver) {
    let sector_size = 512;
    let sector_count = 2048;
    // Read-only is set to false because the underlying NVMe controller only supports read-write access
    // because it only implements NVMe spec v1.2, while "namespace write protection" was only added in v1.4b.
    // This parameter should be changed to true if a later spec supporting this is implemented.
    let mut test = ScsiDvdNvmeTest::new(driver, sector_size, sector_count, false).await;

    let dvd_sector_size = ISO_SECTOR_SIZE as u64;
    let dvd_sector_count = sector_count * sector_size as u64 / dvd_sector_size;
    let external_data =
        OwnedRequestBuffers::linear(0, (dvd_sector_size * dvd_sector_count) as usize, true);
    let guest_mem = GuestMemory::allocate(4096);
    let start_lba = 0;
    let lba_count = 2;
    let request = make_cdb16_request(ScsiOp::READ16, start_lba, lba_count);

    tracing::info!("read disk to guest_mem2 ...");
    check_execute_scsi(
        &mut test.scsi_dvd,
        &external_data.buffer(&guest_mem),
        &request,
        true,
    )
    .await;

    tracing::info!("validate guest_mem2 ...");
    let data = make_repeat_data_buffer(sector_count as usize, sector_size as usize);
    assert_eq!(
        check_guest_memory(
            &guest_mem,
            0,
            &data[..(ISO_SECTOR_SIZE * lba_count) as usize],
            sector_size as usize
        ),
        true
    );
}

#[async_test]
async fn validate_read16_nvme_4096(driver: DefaultDriver) {
    let sector_size = 4096;
    let sector_count = 256;
    // Read-only is set to false because the underlying NVMe controller only supports read-write access
    // because it only implements NVMe spec v1.2, while "namespace write protection" was only added in v1.4b.
    // This parameter should be changed to true if a later spec supporting this is implemented.
    let mut test = ScsiDvdNvmeTest::new(driver, sector_size, sector_count, false).await;

    let dvd_sector_size = ISO_SECTOR_SIZE as u64;
    let dvd_sector_count = sector_count * sector_size as u64 / dvd_sector_size;
    let external_data =
        OwnedRequestBuffers::linear(0, (dvd_sector_size * dvd_sector_count) as usize, true);
    let guest_mem = GuestMemory::allocate(4096);
    let start_lba = 0;
    let lba_count = 2;
    let request = make_cdb16_request(ScsiOp::READ16, start_lba, lba_count);

    tracing::info!("read disk to guest_mem2 ...");
    check_execute_scsi(
        &mut test.scsi_dvd,
        &external_data.buffer(&guest_mem),
        &request,
        true,
    )
    .await;

    tracing::info!("validate guest_mem2 ...");
    let data = make_repeat_data_buffer(sector_count as usize, sector_size as usize);
    assert_eq!(
        check_guest_memory(
            &guest_mem,
            0,
            &data[..(ISO_SECTOR_SIZE * lba_count) as usize],
            sector_size as usize
        ),
        true
    );
}
