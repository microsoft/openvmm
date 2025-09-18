// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Tests of user-mode storvsc implementation with user-mode storvsp.

use guestmem::GuestMemory;
use pal_async::DefaultDriver;
use pal_async::async_test;
use pal_async::timer::PolledTimer;
use scsi_defs::ScsiOp;
use scsi_defs::ScsiStatus;
use std::sync::Arc;
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
        data_in: 1,
        ..FromZeros::new_zeroed()
    };

    scsi_req.payload[0..10].copy_from_slice(cdb.as_bytes());
    scsi_req
}

#[async_test]
async fn test_request_response(driver: DefaultDriver) {
    let (host, guest) = connected_async_channels(16 * 1024);

    let mut test_guest_mem = GuestMemory::allocate(0x4000);
    let controller = ScsiController::new(None);
    let disk = scsidisk::SimpleScsiDisk::new(
        disklayer_ram::ram_disk(0x4000, false).unwrap(),
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

    // Fill memory with pattern for write.
    let pattern = {
        let mut acc = 3u32;
        (0..0x4000)
            .map(|_| {
                acc = acc.wrapping_mul(7);
                acc as u8
            })
            .collect::<Vec<_>>()
    };
    test_guest_mem
        .inner_buf_mut()
        .unwrap()
        .copy_from_slice(&pattern);
    // Clear destination memory to make sure read works
    test_guest_mem.inner_buf_mut().unwrap()[..4096].fill(0u8);

    let storvsp = TestWorker::start(
        controller,
        driver.clone(),
        test_guest_mem.clone(),
        host,
        None,
    );

    let mut storvsc = TestStorvscWorker::new();
    storvsc.start(driver.clone(), guest);

    // Wait for negotiation or panic.
    let mut timer = PolledTimer::new(&driver);
    let negotiation_timeout_millis = 1000;
    storvsc
        .wait_for_negotiation(&mut timer, negotiation_timeout_millis)
        .await;

    // Send SCSI write request
    let write_response = storvsc
        .send_request(&generate_write_packet(0, 0, 0, 1, 4096), 4096, 4096)
        .await
        .unwrap();
    assert_eq!(write_response.scsi_status, ScsiStatus::GOOD);

    // Send SCSI read request
    let read_response = storvsc
        .send_request(&generate_read_packet(0, 0, 0, 1, 4096), 8192, 4096)
        .await
        .unwrap();
    assert_eq!(read_response.scsi_status, ScsiStatus::GOOD);
    let mut read_data = [0u8; 4096];
    test_guest_mem.read_at(8192, &mut read_data).unwrap();
    assert_eq!(&read_data, &pattern[4096..8192]);
    storvsc.teardown().await;
    storvsp.teardown_or_panic().await;
}
