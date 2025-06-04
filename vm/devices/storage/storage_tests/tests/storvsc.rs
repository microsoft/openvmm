// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Tests of user-mode storvsc implementation with user-mode storvsp.

use guestmem::GuestMemory;
use pal_async::DefaultDriver;
use pal_async::async_test;
use pal_async::timer::PolledTimer;
use scsi_defs::ScsiOp;
use std::sync::Arc;
use std::time;
use storvsc_driver::test_helpers::TestStorvscWorker;
use storvsp::ScsiController;
use storvsp::ScsiControllerDisk;
use storvsp::test_helpers::TestWorker;
use storvsp_resources::ScsiPath;
use test_with_tracing::test;
use vmbus_channel::connected_async_channels;
use zerocopy::FromZeros;
use zerocopy::IntoBytes;

// This function assumes the sector size is 512.
fn generate_write_packet(
    target_id: u8,
    path_id: u8,
    lun: u8,
    block: u32,
    byte_len: usize,
) -> storvsp_protocol::ScsiRequest {
    let cdb = scsi_defs::Cdb10 {
        operation_code: ScsiOp::WRITE,
        logical_block: block.into(),
        transfer_blocks: ((byte_len / 512) as u16).into(),
        ..FromZeros::new_zeroed()
    };

    let mut scsi_req = storvsp_protocol::ScsiRequest {
        target_id,
        path_id,
        lun,
        length: storvsp_protocol::SCSI_REQUEST_LEN_V2 as u16,
        cdb_length: size_of::<scsi_defs::Cdb10>() as u8,
        data_transfer_length: byte_len as u32,
        ..FromZeros::new_zeroed()
    };

    scsi_req.payload[0..10].copy_from_slice(cdb.as_bytes());
    scsi_req
}

// This function assumes the sector size is 512.
fn generate_read_packet(
    target_id: u8,
    path_id: u8,
    lun: u8,
    block: u32,
    byte_len: usize,
) -> storvsp_protocol::ScsiRequest {
    let cdb = scsi_defs::Cdb10 {
        operation_code: ScsiOp::READ,
        logical_block: block.into(),
        transfer_blocks: ((byte_len / 512) as u16).into(),
        ..FromZeros::new_zeroed()
    };

    let mut scsi_req = storvsp_protocol::ScsiRequest {
        target_id,
        path_id,
        lun,
        length: storvsp_protocol::SCSI_REQUEST_LEN_V2 as u16,
        cdb_length: size_of::<scsi_defs::Cdb10>() as u8,
        data_transfer_length: byte_len as u32,
        ..FromZeros::new_zeroed()
    };

    scsi_req.payload[0..10].copy_from_slice(cdb.as_bytes());
    scsi_req
}

#[async_test]
async fn test_request_response(driver: DefaultDriver) {
    let (host, guest) = connected_async_channels(16 * 1024);

    let test_guest_mem = GuestMemory::allocate(16384);
    let controller = ScsiController::new();
    let disk = scsidisk::SimpleScsiDisk::new(
        disklayer_ram::ram_disk(10 * 1024 * 1024, false).unwrap(),
        Default::default(),
    );
    controller
        .attach(
            ScsiPath {
                path: 0,
                target: 0,
                lun: 0,
            },
            ScsiControllerDisk::new(Arc::new(disk)),
        )
        .unwrap();

    let storvsp = TestWorker::start(
        controller,
        driver.clone(),
        test_guest_mem.clone(),
        host,
        None,
    );

    let mut storvsc = TestStorvscWorker::new();
    storvsc.start(driver.clone(), guest);

    let mut timer = PolledTimer::new(&driver);
    timer.sleep(time::Duration::from_secs(1)).await;

    // Send SCSI write request
    let write_buf = [7u8; 4096];
    test_guest_mem.write_at(4096, &write_buf).unwrap();
    storvsc
        .send_request(&generate_write_packet(0, 1, 2, 4096, 4096), 4096, 4096)
        .await
        .unwrap();

    // Send SCSI read request
    let write_buf = [7u8; 4096];
    test_guest_mem.write_at(4096, &write_buf).unwrap();
    storvsc
        .send_request(&generate_read_packet(0, 1, 2, 4096, 4096), 4096, 4096)
        .await
        .unwrap();

    storvsc.teardown().await;
    storvsp.teardown_ignore().await;
}
