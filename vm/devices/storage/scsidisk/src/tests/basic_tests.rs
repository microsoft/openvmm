// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! ScsiDisk basic tests.

use super::test_helpers::check_execute_scsi_pass;
use super::test_helpers::check_guest_memory;
use super::test_helpers::make_cdb10_request;
use super::test_helpers::make_cdb16_request;
use super::test_helpers::make_guest_memory;
use super::test_helpers::make_repeat_data_buffer;
use super::test_helpers::new_atapi_disk;
use super::test_helpers::new_scsi_disk;
use super::test_helpers::new_scsi_dvd;
use crate::scsi;
use crate::SimpleScsiDisk;
use guestmem::GuestMemory;
use pal_async::async_test;
use scsi::AdditionalSenseCode;
use scsi::ScsiOp;
use scsi::ScsiStatus;
use scsi::SenseKey;
use scsi_buffers::OwnedRequestBuffers;
use scsi_core::save_restore::SavedSenseData;
use scsi_core::save_restore::ScsiDiskSavedState;
use scsi_core::save_restore::ScsiSavedState;
use scsi_core::AsyncScsiDisk;
use scsi_core::Request;
use scsi_core::ScsiSaveRestore;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use zerocopy::IntoBytes;

fn save_scsi_disk(scsi_disk: &SimpleScsiDisk) -> ScsiDiskSavedState {
    let saved_state = if let Some(ScsiSavedState::ScsiDisk(saved_state)) = scsi_disk.save().unwrap()
    {
        saved_state
    } else {
        panic!("saved_state cannot be none")
    };
    assert_eq!(
        scsi_disk.get_and_update_sector_count(ScsiOp::READ),
        Ok(saved_state.sector_count),
    );
    let sense = scsi_disk.sense_data.get();
    let sense_data = sense.map(|sense| SavedSenseData {
        sense_key: sense.header.sense_key.0,
        additional_sense_code: sense.additional_sense_code.0,
        additional_sense_code_qualifier: sense.additional_sense_code_qualifier,
    });
    assert_eq!(saved_state.sense_data, sense_data);

    saved_state
}

fn restore_scsi_disk(
    scsi_disk: &SimpleScsiDisk,
    saved_state: ScsiDiskSavedState,
    size_change: bool,
) {
    let sector_size = scsi_disk.sector_size;
    let sector_count = scsi_disk.last_sector_count.load(Ordering::Relaxed);
    let sector_shift = scsi_disk.sector_shift;
    let physical_extra_shift = scsi_disk.physical_extra_shift;

    if scsi_disk
        .restore(&ScsiSavedState::ScsiDisk(saved_state))
        .is_err()
    {
        panic!("restore scsi disk failed. saved_state {:?}", saved_state);
    }

    let sense = scsi_disk.sense_data.get();
    let sense_data = sense.map(|sense| SavedSenseData {
        sense_key: sense.header.sense_key.0,
        additional_sense_code: sense.additional_sense_code.0,
        additional_sense_code_qualifier: sense.additional_sense_code_qualifier,
    });
    assert_eq!(saved_state.sense_data, sense_data);
    assert_eq!(sector_size, scsi_disk.sector_size);
    let r = scsi_disk.get_and_update_sector_count(ScsiOp::READ);
    if size_change {
        r.unwrap_err();
    } else {
        assert_eq!(r, Ok(sector_count));
    }
    assert_eq!(sector_shift, scsi_disk.sector_shift);
    assert_eq!(physical_extra_shift, scsi_disk.physical_extra_shift);
}

async fn check_report_pending_unit_attention(scsi_disk: &SimpleScsiDisk, report: bool) {
    let external_data = OwnedRequestBuffers::new(&[0]);
    let guest_mem = GuestMemory::allocate(4096);
    let result = scsi_disk
        .execute_scsi(
            &external_data.buffer(&guest_mem),
            &Request {
                cdb: scsi::Cdb16 {
                    operation_code: ScsiOp::READ16,
                    flags: scsi::Cdb16Flags::new(),
                    logical_block: 0.into(),
                    transfer_blocks: 1.into(),
                    reserved2: 0,
                    control: 0,
                }
                .as_bytes()
                .try_into()
                .unwrap(),
                srb_flags: 0,
            },
        )
        .await;
    if report {
        assert_eq!(result.scsi_status, ScsiStatus::CHECK_CONDITION);
        assert_eq!(
            result.sense_data.unwrap().header.sense_key,
            SenseKey::UNIT_ATTENTION
        );
    } else {
        assert_eq!(result.scsi_status, ScsiStatus::GOOD);
    }
}

async fn write_same(
    logical_sector_size: u32,
    physical_sector_size: u32,
    sector_count: u64,
    read_only: bool,
    fua: bool,
) {
    println!(
        "write_same test - read_only: {:?} fua: {:?}",
        read_only, fua
    );
    let (scsi_disk, state) = new_scsi_disk(
        logical_sector_size,
        physical_sector_size,
        sector_count,
        read_only,
        true,
        false,
    );
    let sector_size = scsi_disk
        .get_and_update_sector_count(ScsiOp::WRITE_SAME16)
        .unwrap() as usize;

    let data = make_repeat_data_buffer(1, sector_size);
    let guest_mem = make_guest_memory(&data);
    let external_data = OwnedRequestBuffers::linear(0, data.len(), true);
    println!("validate guest_mem and data ...");
    check_guest_memory(&guest_mem, 0, &data);

    let request = make_cdb16_request(ScsiOp::WRITE_SAME16, fua, 0, 4);
    println!("write same guest_mem to disk...");
    check_execute_scsi_pass(&scsi_disk, &external_data.buffer(&guest_mem), &request).await;
    assert_eq!(state.lock().is_fua_set, fua);

    let guest_mem2 = GuestMemory::allocate(4096);
    let request = make_cdb16_request(ScsiOp::READ16, fua, 0, 4);
    println!("read disk to guest_mem2 ...");
    check_execute_scsi_pass(&scsi_disk, &external_data.buffer(&guest_mem2), &request).await;
    assert_eq!(state.lock().is_fua_set, false);

    println!("validate guest_mem2 ...");
    check_guest_memory(&guest_mem2, 0, &data[..sector_size * 4].to_vec());
}

fn resize(new_sector_count: Option<u64>) {
    let (disk, state) = new_scsi_disk(512, 4096, 512, false, false, false);

    let old_sector_count = disk.get_and_update_sector_count(ScsiOp::READ).unwrap();
    if let Some(new_sector_count) = new_sector_count {
        state.lock().sector_count = new_sector_count;
    }
    let r = disk.get_and_update_sector_count(ScsiOp::READ);
    if let Some(new_sector_count) = new_sector_count {
        assert_eq!(r.unwrap_err(), new_sector_count);
    } else {
        assert_eq!(r.unwrap(), old_sector_count);
    }
}

#[test]
fn validate_new_scsi_disk() {
    let _disk = new_scsi_disk(512, 4096, 1024, false, false, false);
    let _disk = new_scsi_disk(512, 512, 1024, true, false, false);
    let _disk = new_scsi_disk(512, 4096, 1024, false, false, true);
    let _disk = new_scsi_disk(512, 512, 1024, true, false, true);
}

#[test]
fn validate_save_restore_scsi_disk_no_change() {
    let (scsi_disk, _state) = new_scsi_disk(512, 4096, 1024, false, false, false);
    let saved_state = save_scsi_disk(&scsi_disk);
    restore_scsi_disk(&scsi_disk, saved_state, false);
}

#[test]
fn validate_save_restore_scsi_disk_with_change() {
    let (disk, state) = new_scsi_disk(512, 4096, 1024, false, false, false);
    let saved_state = save_scsi_disk(&disk);
    restore_scsi_disk(&disk, saved_state, false);
    state.lock().sector_count = saved_state.sector_count + 1;
    restore_scsi_disk(&disk, saved_state, true);
}

#[test]
fn validate_save_restore_scsi_disk_with_sense_data() {
    let (disk, _state) = new_scsi_disk(512, 4096, 1024, false, false, false);
    let mut saved_state = save_scsi_disk(&disk);
    saved_state.sense_data = Some(SavedSenseData {
        sense_key: SenseKey::UNIT_ATTENTION.0,
        additional_sense_code: AdditionalSenseCode::OPERATING_CONDITIONS_CHANGED.0,
        additional_sense_code_qualifier: scsi::SCSI_SENSEQ_OPERATING_DEFINITION_CHANGED,
    });
    restore_scsi_disk(&disk, saved_state, false);
}

#[async_test]
async fn validate_report_pending_unit_attention() {
    let (disk, state) = new_scsi_disk(512, 4096, 1024, false, true, false);
    check_report_pending_unit_attention(&disk, false).await;
    state.lock().sector_count = 4096;
    check_report_pending_unit_attention(&disk, true).await;
    check_report_pending_unit_attention(&disk, false).await;
}

#[async_test]
async fn validate_async_write_same() {
    write_same(512, 4096, 512, false, false).await;
    write_same(512, 4096, 512, false, true).await;
}

#[test]
fn validate_resize() {
    resize(Some(1024));
    resize(Some(4096));
    resize(None);
}

#[async_test]
async fn validate_atapi_disk_read() {
    let sector_size = 512_usize;
    let (disk, state) = new_scsi_dvd(sector_size.try_into().unwrap(), 4096, 1024, true);
    let atapi_disk = new_atapi_disk(Arc::new(disk));
    let data = make_repeat_data_buffer(1, sector_size);
    let external_data = OwnedRequestBuffers::linear(0, data.len(), true);
    let guest_mem = GuestMemory::allocate(sector_size * 4);
    let request = make_cdb10_request(ScsiOp::READ, false, 0, 2);
    println!("read disk to guest_mem ...");
    check_execute_scsi_pass(&atapi_disk, &external_data.buffer(&guest_mem), &request).await;
    println!("validate guest_mem ...");
    let state = state.lock();
    check_guest_memory(&guest_mem, 0, &state.storage[..sector_size * 4].to_vec());
}
