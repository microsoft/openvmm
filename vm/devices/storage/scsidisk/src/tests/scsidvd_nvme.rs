// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

#![cfg(target_os = "linux")]

use crate::scsidvd::SimpleScsiDvd;
use chipset_device::mmio::ExternallyManagedMmioIntercepts;
use disk_backend::Disk;
use disk_nvme::NvmeDisk;
use guestmem::GuestMemory;
use guid::Guid;
use nvme::NvmeController;
use nvme::NvmeControllerCaps;
use nvme_driver::NvmeDriver;
use pal_async::async_test;
use pal_async::DefaultDriver;
use pci_core::msi::MsiInterruptSet;
use scsi_buffers::OwnedRequestBuffers;
use scsi_buffers::RequestBuffers;
use scsi_core::AsyncScsiDisk;
use scsi_core::Request;
use scsi_defs::ScsiOp;
use scsi_defs::ISO_SECTOR_SIZE;
use user_driver::emulated::DeviceSharedMemory;
use user_driver::emulated::EmulatedDevice;
use vmcore::vm_task::SingleDriverBackend;
use vmcore::vm_task::VmTaskDriverSource;
use zerocopy::IntoBytes;

struct ScsiDvdNvmeTest {
    scsi_dvd: SimpleScsiDvd,
    _nvme_driver: NvmeDriver<EmulatedDevice<NvmeController>>, // We need to store this to keep it from going out of scope
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
        let base_len = 64 << 20;
        let payload_len = 4 << 30;
        let mem = DeviceSharedMemory::new(base_len, payload_len);
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

        let payload_mem = mem
            .guest_memory()
            .subrange(base_len as u64, payload_len as u64, false)
            .unwrap();
        let driver_dma_mem = payload_mem.clone();
        let mut buf = vec![0u8; sector_count as usize * sector_size as usize];
        let mut temp = vec![0u8; sector_size as usize];
        assert!(sector_size > 2);
        temp[sector_size as usize / 2 - 1] = 2;
        temp[sector_size as usize / 2] = 3;

        for i in (0..buf.len()).step_by(temp.len()) {
            let end_point = i + temp.len();
            buf[i..end_point].copy_from_slice(&temp);
        }
        payload_mem.write_at(0, &buf).unwrap();

        nvme.client()
            .add_namespace(
                1,
                disklayer_ram::ram_disk(sector_size as u64 * sector_count, read_only).unwrap(),
            )
            .await
            .unwrap();

        let device = EmulatedDevice::new(nvme, msi_set, mem);
        let nvme_driver = NvmeDriver::new(&driver_source, CPU_COUNT, device)
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
                    &driver_dma_mem,
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
    pass: bool,
) {
    let result = scsi_dvd.execute_scsi(external_data, request).await;
    match pass {
        true if result.scsi_status != scsi_defs::ScsiStatus::GOOD => {
            panic!(
                "execute_scsi failed! request: {:?} result: {:?}",
                request, result
            );
        }
        false if result.scsi_status == scsi_defs::ScsiStatus::GOOD => {
            panic!(
                "execute_scsi passed! request: {:?} result: {:?}",
                request, result
            );
        }
        _ => (),
    }
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
    if guest_mem.read_at(start_lba, &mut b).is_err() {
        panic!("guest_mem read error");
    };
    buff[..].eq(&b[..]) && (b[sector_size / 2 - 1] == 2) && (b[sector_size / 2] == 3)
}

#[async_test]
async fn validate_new_scsi_dvd_nvme(driver: DefaultDriver) {
    ScsiDvdNvmeTest::new(driver, 512, 2048, false).await; // TODO: NVMe driver does not support read only at the moment
}

#[async_test]
async fn validate_read16_nvme(driver: DefaultDriver) {
    let sector_size = 512;
    let sector_count = 2048;
    let mut test = ScsiDvdNvmeTest::new(driver, sector_size, sector_count, false).await; // TODO: NVMe driver does not support read only at the moment

    let dvd_sector_size = ISO_SECTOR_SIZE as u64;
    let dvd_sector_count = sector_count * sector_size as u64 / dvd_sector_size;
    let external_data =
        OwnedRequestBuffers::linear(0, (dvd_sector_size * dvd_sector_count) as usize, true);
    let guest_mem = GuestMemory::allocate(4096);
    let start_lba = 0;
    let lba_count = 2;
    let request = make_cdb16_request(ScsiOp::READ16, start_lba, lba_count);

    println!("read disk to guest_mem2 ...");
    check_execute_scsi(
        &mut test.scsi_dvd,
        &external_data.buffer(&guest_mem),
        &request,
        true,
    )
    .await;

    println!("validate guest_mem2 ...");
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
