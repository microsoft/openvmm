// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use super::test_helpers::TestDisk;
use super::test_helpers::TestDiskOperation;
use super::test_helpers::TestDiskStorageState;
use super::test_helpers::TestDiskWriteBlocker;
use crate::SimpleScsiDisk;
use crate::scsi;
use disk_backend::Disk;
use guestmem::GuestMemory;
use pal_async::async_test;
use parking_lot::Mutex;
use scsi_buffers::OwnedRequestBuffers;
use scsi_core::AsyncScsiDisk;
use scsi_core::Request;
use scsidisk_resources::DiskParameters;
use scsidisk_resources::HostManagedZonedParameters;
use std::mem::size_of;
use std::sync::Arc;
use zerocopy::FromBytes;
use zerocopy::IntoBytes;

const SECTOR_SIZE: u32 = 512;
const PHYSICAL_SECTOR_SIZE: u32 = 4096;
const ZONE_SIZE: u64 = 4096;
const SECTOR_COUNT: u64 = 128;
const VISIBLE_SECTOR_COUNT: u64 = 112;
const LARGE_DISK_SIZE_BYTES: u64 = 40 * 1024 * 1024 * 1024 * 1024;
const LARGE_ZONE_SIZE_BYTES: u64 = 256 * 1024 * 1024;

fn new_zoned_disk() -> SimpleScsiDisk {
    new_zoned_disk_with(ZONE_SIZE, 2)
}

fn new_zoned_disk_with(zone_size_bytes: u64, max_open_zones: u32) -> SimpleScsiDisk {
    new_zoned_disk_with_state(zone_size_bytes, max_open_zones).0
}

fn zoned_parameters(zone_size_bytes: u64, max_open_zones: u32) -> DiskParameters {
    DiskParameters {
        host_managed_zoned: Some(HostManagedZonedParameters {
            zone_size_bytes,
            conventional_zone_count: 1,
            max_open_zones,
            unrestricted_read: true,
        }),
        ..Default::default()
    }
}

fn new_zoned_disk_with_state(
    zone_size_bytes: u64,
    max_open_zones: u32,
) -> (SimpleScsiDisk, Arc<Mutex<TestDiskStorageState>>) {
    new_zoned_disk_with_backing_sector_count(SECTOR_COUNT, zone_size_bytes, max_open_zones)
}

fn new_zoned_disk_with_backing_sector_count(
    backing_sector_count: u64,
    zone_size_bytes: u64,
    max_open_zones: u32,
) -> (SimpleScsiDisk, Arc<Mutex<TestDiskStorageState>>) {
    let (disk, state) = TestDisk::new(
        SECTOR_SIZE,
        PHYSICAL_SECTOR_SIZE,
        backing_sector_count,
        false,
        true,
    );
    (
        SimpleScsiDisk::new(
            Disk::new(disk).unwrap(),
            zoned_parameters(zone_size_bytes, max_open_zones),
        )
        .unwrap(),
        state,
    )
}

#[async_test]
async fn zoned_disk_exposes_only_complete_zones_before_metadata() {
    let backing_sector_count = SECTOR_COUNT + 2;
    let (disk, state) =
        new_zoned_disk_with_backing_sector_count(backing_sector_count, ZONE_SIZE, 2);

    let zoned = disk.zoned.as_ref().unwrap();
    assert_eq!(zoned.sector_count(), VISIBLE_SECTOR_COUNT);
    for index in 0..(VISIBLE_SECTOR_COUNT / (ZONE_SIZE / u64::from(SECTOR_SIZE))) as usize {
        assert_eq!(
            zone_descriptor(&disk, index).await.zone_length.get(),
            ZONE_SIZE / u64::from(SECTOR_SIZE)
        );
    }

    let memory = GuestMemory::allocate(4096);
    let range = OwnedRequestBuffers::linear(0, 4096, true);
    let result = disk
        .execute_scsi(
            &range.buffer(&memory),
            &zbc_out_request(scsi::SERVICE_ACTION_OPEN_ZONE, 8),
        )
        .await;
    assert_eq!(result.scsi_status, scsi::ScsiStatus::GOOD, "{result:?}");

    let state = state.lock();
    let physical_sector_count = u64::from(PHYSICAL_SECTOR_SIZE / SECTOR_SIZE);
    let metadata_end_lba = backing_sector_count / physical_sector_count * physical_sector_count;
    let metadata_start =
        ((metadata_end_lba - physical_sector_count * 2) * u64::from(SECTOR_SIZE)) as usize;
    assert_eq!(metadata_start % PHYSICAL_SECTOR_SIZE as usize, 0);
    assert!(
        state.storage[(VISIBLE_SECTOR_COUNT * u64::from(SECTOR_SIZE)) as usize..metadata_start]
            .iter()
            .all(|byte| *byte == 0)
    );
    assert!(
        state.storage[metadata_start..metadata_start + PHYSICAL_SECTOR_SIZE as usize]
            .iter()
            .any(|byte| *byte != 0)
    );
    assert!(
        state.storage[metadata_start + PHYSICAL_SECTOR_SIZE as usize
            ..metadata_start + (PHYSICAL_SECTOR_SIZE * 2) as usize]
            .iter()
            .all(|byte| *byte == 0)
    );
    assert!(
        state.storage[(metadata_end_lba * u64::from(SECTOR_SIZE)) as usize..]
            .iter()
            .all(|byte| *byte == 0)
    );
}

#[test]
fn zoned_disk_rejects_zone_size_not_aligned_to_physical_sector() {
    let (disk, _) = TestDisk::new(
        SECTOR_SIZE,
        PHYSICAL_SECTOR_SIZE,
        SECTOR_COUNT,
        false,
        false,
    );
    assert!(
        SimpleScsiDisk::new(
            Disk::new(disk).unwrap(),
            zoned_parameters(u64::from(PHYSICAL_SECTOR_SIZE) + u64::from(SECTOR_SIZE), 2),
        )
        .is_err()
    );
}

#[test]
fn zoned_disk_rejects_impractical_zone_count() {
    let zone_size_sectors = ZONE_SIZE / u64::from(SECTOR_SIZE);
    let backing_sector_count = (crate::zoned::MAX_ZONE_COUNT + 1) * zone_size_sectors + 20_000;
    let (disk, _) = TestDisk::new(
        SECTOR_SIZE,
        PHYSICAL_SECTOR_SIZE,
        backing_sector_count,
        false,
        false,
    );
    assert!(
        SimpleScsiDisk::new(Disk::new(disk).unwrap(), zoned_parameters(ZONE_SIZE, 2),).is_err()
    );
}

#[async_test]
async fn zoned_disk_reports_40tb_capacity_without_backing_allocation() {
    let backing_sector_count = LARGE_DISK_SIZE_BYTES / u64::from(SECTOR_SIZE);
    let (disk, _) = TestDisk::new(
        SECTOR_SIZE,
        PHYSICAL_SECTOR_SIZE,
        backing_sector_count,
        false,
        false,
    );
    let disk = SimpleScsiDisk::new(
        Disk::new(disk).unwrap(),
        zoned_parameters(LARGE_ZONE_SIZE_BYTES, 32),
    )
    .unwrap();
    let visible_sector_count = disk.zoned.as_ref().unwrap().sector_count();
    let zone_size_sectors = LARGE_ZONE_SIZE_BYTES / u64::from(SECTOR_SIZE);

    assert!(visible_sector_count > u64::from(u32::MAX));
    assert_eq!(visible_sector_count % zone_size_sectors, 0);
    assert_eq!(
        visible_sector_count / zone_size_sectors,
        backing_sector_count / zone_size_sectors - 1
    );

    let bytes = execute_to_bytes(
        &disk,
        &read_capacity16_request(size_of::<scsi::ReadCapacity16Data>() as u32),
    )
    .await;
    let capacity = scsi::ReadCapacity16Data::read_from_prefix(&bytes)
        .unwrap()
        .0;
    assert_eq!(
        capacity.ex.logical_block_address.get(),
        visible_sector_count - 1
    );
    assert_eq!(capacity.ex.bytes_per_block.get(), SECTOR_SIZE);
}

fn reopen_zoned_disk(
    state: Arc<Mutex<TestDiskStorageState>>,
    zone_size_bytes: u64,
    max_open_zones: u32,
) -> SimpleScsiDisk {
    let disk = TestDisk {
        sector_size: SECTOR_SIZE,
        physical_sector_size: PHYSICAL_SECTOR_SIZE,
        read_only: false,
        state,
        write_blocker: None,
    };
    SimpleScsiDisk::new(
        Disk::new(disk).unwrap(),
        zoned_parameters(zone_size_bytes, max_open_zones),
    )
    .unwrap()
}

fn request_from_bytes(bytes: &[u8]) -> Request {
    let mut cdb = [0; 16];
    cdb[..bytes.len()].copy_from_slice(bytes);
    Request { cdb, srb_flags: 0 }
}

fn inquiry_request(vpd: bool, page_code: u8, allocation_length: u16) -> Request {
    let cdb = scsi::CdbInquiry {
        operation_code: scsi::ScsiOp::INQUIRY.0,
        flags: scsi::InquiryFlags::new().with_vpd(vpd),
        page_code,
        allocation_length: allocation_length.into(),
        control: 0,
    };
    request_from_bytes(cdb.as_bytes())
}

fn report_zones_request() -> Request {
    let cdb = scsi::ReportZones {
        operation_code: scsi::ScsiOp::ZBC_IN,
        service_action: scsi::SERVICE_ACTION_REPORT_ZONES,
        zone_start_lba: 0u64.into(),
        allocation_length: 4096u32.into(),
        flags: scsi::REPORT_ZONES_OPTION_ALL,
        control: 0,
    };
    request_from_bytes(cdb.as_bytes())
}

fn zbc_out_request(service_action: u8, zone_id: u64) -> Request {
    let cdb = scsi::ZbcOut {
        operation_code: scsi::ScsiOp::ZBC_OUT,
        service_action,
        zone_id: zone_id.into(),
        reserved: [0; 4],
        flags: 0,
        control: 0,
    };
    request_from_bytes(cdb.as_bytes())
}

fn read_capacity16_request(allocation_length: u32) -> Request {
    let cdb = scsi::ServiceActionIn16 {
        operation_code: scsi::ScsiOp::READ_CAPACITY16,
        service_action: scsi::SERVICE_ACTION_READ_CAPACITY16,
        logical_block: [0; 8],
        allocation_length: allocation_length.to_be_bytes(),
        flags: 0,
        control: 0,
    };
    request_from_bytes(cdb.as_bytes())
}

fn write16_request(start_lba: u64, sectors: u32) -> Request {
    let cdb = scsi::Cdb16 {
        operation_code: scsi::ScsiOp::WRITE16,
        flags: scsi::Cdb16Flags::new(),
        logical_block: start_lba.into(),
        transfer_blocks: sectors.into(),
        reserved2: 0,
        control: 0,
    };
    request_from_bytes(cdb.as_bytes())
}

fn read16_request(start_lba: u64, sectors: u32) -> Request {
    let cdb = scsi::Cdb16 {
        operation_code: scsi::ScsiOp::READ16,
        flags: scsi::Cdb16Flags::new(),
        logical_block: start_lba.into(),
        transfer_blocks: sectors.into(),
        reserved2: 0,
        control: 0,
    };
    request_from_bytes(cdb.as_bytes())
}

async fn execute_to_bytes(disk: &SimpleScsiDisk, request: &Request) -> Vec<u8> {
    let memory = GuestMemory::allocate(4096);
    let range = OwnedRequestBuffers::linear(0, 4096, true);
    let result = disk.execute_scsi(&range.buffer(&memory), request).await;
    assert_eq!(result.scsi_status, scsi::ScsiStatus::GOOD, "{result:?}");
    let mut bytes = vec![0; result.tx];
    memory.read_at(0, &mut bytes).unwrap();
    bytes
}

fn descriptor(bytes: &[u8], index: usize) -> scsi::ZoneDescriptor {
    let offset = size_of::<scsi::ReportZonesData>() + index * size_of::<scsi::ZoneDescriptor>();
    scsi::ZoneDescriptor::read_from_prefix(&bytes[offset..])
        .unwrap()
        .0
}

async fn zone_descriptor(disk: &SimpleScsiDisk, index: usize) -> scsi::ZoneDescriptor {
    descriptor(
        &execute_to_bytes(disk, &report_zones_request()).await,
        index,
    )
}

#[async_test]
async fn zoned_disk_reports_host_managed_identity_and_geometry() {
    let disk = new_zoned_disk();

    let inquiry = execute_to_bytes(&disk, &inquiry_request(false, 0, 104)).await;
    assert_eq!(inquiry[0] & 0x1f, scsi::HOST_MANAGED_ZONED_BLOCK_DEVICE);

    let supported = execute_to_bytes(
        &disk,
        &inquiry_request(true, scsi::VPD_SUPPORTED_PAGES, 255),
    )
    .await;
    assert!(supported[4..].contains(&scsi::VPD_ZONED_BLOCK_DEVICE_CHARACTERISTICS));

    let characteristics = execute_to_bytes(
        &disk,
        &inquiry_request(true, scsi::VPD_ZONED_BLOCK_DEVICE_CHARACTERISTICS, 64),
    )
    .await;
    assert_eq!(
        characteristics[1],
        scsi::VPD_ZONED_BLOCK_DEVICE_CHARACTERISTICS
    );
    assert_eq!(
        u16::from_be_bytes([characteristics[2], characteristics[3]]),
        60
    );
    assert_eq!(characteristics[4] & scsi::VPD_ZONED_URSWRZ, 1);
    assert_eq!(
        u32::from_be_bytes(characteristics[16..20].try_into().unwrap()),
        2
    );

    let zones = execute_to_bytes(&disk, &report_zones_request()).await;
    let header = scsi::ReportZonesData::read_from_prefix(&zones).unwrap().0;
    assert_eq!(header.zone_list_length.get(), 14 * 64);
    assert_eq!(header.max_lba.get(), VISIBLE_SECTOR_COUNT - 1);

    let conventional = descriptor(&zones, 0);
    assert_eq!(
        conventional.zone_type & scsi::ZONE_TYPE_MASK,
        scsi::ZONE_TYPE_CONVENTIONAL
    );
    assert_eq!(
        conventional.condition >> scsi::ZONE_CONDITION_SHIFT,
        scsi::ZONE_CONDITION_NOT_WRITE_POINTER
    );

    let sequential = descriptor(&zones, 1);
    assert_eq!(
        sequential.zone_type & scsi::ZONE_TYPE_MASK,
        scsi::ZONE_TYPE_SEQUENTIAL_WRITE_REQUIRED
    );
    assert_eq!(
        sequential.condition >> scsi::ZONE_CONDITION_SHIFT,
        scsi::ZONE_CONDITION_EMPTY
    );
    assert_eq!(sequential.zone_start_lba.get(), 8);
    assert_eq!(sequential.zone_length.get(), 8);
    assert_eq!(sequential.write_pointer_lba.get(), 8);
}

#[async_test]
async fn zoned_disk_enforces_write_pointer_and_reset_masks_old_data() {
    let disk = new_zoned_disk();
    assert!(!disk.scsi_parameters.support_unmap);
    let memory = GuestMemory::allocate(4096);
    let range = OwnedRequestBuffers::linear(0, 4096, true);
    memory.write_at(0, &vec![0xa5; 4096]).unwrap();

    let result = disk
        .execute_scsi(&range.buffer(&memory), &write16_request(8, 8))
        .await;
    assert_eq!(result.scsi_status, scsi::ScsiStatus::GOOD, "{result:?}");

    let zones = execute_to_bytes(&disk, &report_zones_request()).await;
    let full = descriptor(&zones, 1);
    assert_eq!(
        full.condition >> scsi::ZONE_CONDITION_SHIFT,
        scsi::ZONE_CONDITION_FULL
    );
    assert_eq!(full.write_pointer_lba.get(), u64::MAX);

    let result = disk
        .execute_scsi(&range.buffer(&memory), &write16_request(8, 8))
        .await;
    assert_eq!(result.scsi_status, scsi::ScsiStatus::CHECK_CONDITION);
    let sense = result.sense_data.unwrap();
    assert_eq!(
        sense.additional_sense_code,
        scsi::AdditionalSenseCode::ILLEGAL_BLOCK
    );
    assert_eq!(
        sense.additional_sense_code_qualifier,
        scsi::SCSI_SENSEQ_UNALIGNED_WRITE
    );

    let result = disk
        .execute_scsi(
            &range.buffer(&memory),
            &zbc_out_request(scsi::SERVICE_ACTION_RESET_WRITE_POINTER, 8),
        )
        .await;
    assert_eq!(result.scsi_status, scsi::ScsiStatus::GOOD, "{result:?}");

    memory.write_at(0, &vec![0xff; 4096]).unwrap();
    let result = disk
        .execute_scsi(&range.buffer(&memory), &read16_request(8, 8))
        .await;
    assert_eq!(result.scsi_status, scsi::ScsiStatus::GOOD, "{result:?}");
    let mut bytes = vec![0xff; 4096];
    memory.read_at(0, &mut bytes).unwrap();
    assert!(bytes.iter().all(|value| *value == 0));
}

#[async_test]
async fn zoned_disk_read_inside_unwritten_zone_returns_zeroes() {
    let disk = new_zoned_disk();
    let memory = GuestMemory::allocate(2048);
    let range = OwnedRequestBuffers::linear(0, 2048, true);
    memory.write_at(0, &vec![0xff; 2048]).unwrap();

    let result = disk
        .execute_scsi(&range.buffer(&memory), &read16_request(12, 4))
        .await;
    assert_eq!(result.scsi_status, scsi::ScsiStatus::GOOD, "{result:?}");

    let mut bytes = vec![0xff; 2048];
    memory.read_at(0, &mut bytes).unwrap();
    assert!(bytes.iter().all(|value| *value == 0));
}

#[async_test]
async fn zoned_disk_open_close_and_reset_transitions() {
    let disk = new_zoned_disk();
    let memory = GuestMemory::allocate(4096);
    let range = OwnedRequestBuffers::linear(0, 4096, true);

    for (action, expected_condition) in [
        (
            scsi::SERVICE_ACTION_OPEN_ZONE,
            scsi::ZONE_CONDITION_EXPLICITLY_OPENED,
        ),
        (scsi::SERVICE_ACTION_CLOSE_ZONE, scsi::ZONE_CONDITION_CLOSED),
        (
            scsi::SERVICE_ACTION_RESET_WRITE_POINTER,
            scsi::ZONE_CONDITION_EMPTY,
        ),
    ] {
        let result = disk
            .execute_scsi(&range.buffer(&memory), &zbc_out_request(action, 8))
            .await;
        assert_eq!(result.scsi_status, scsi::ScsiStatus::GOOD, "{result:?}");
        assert_eq!(
            zone_descriptor(&disk, 1).await.condition >> scsi::ZONE_CONDITION_SHIFT,
            expected_condition
        );
    }

    let zone = zone_descriptor(&disk, 1).await;
    assert_eq!(
        zone.condition >> scsi::ZONE_CONDITION_SHIFT,
        scsi::ZONE_CONDITION_EMPTY
    );
    assert_eq!(zone.write_pointer_lba.get(), 8);
}

#[async_test]
async fn zoned_disk_rejects_cross_zone_and_misordered_writes() {
    let disk = new_zoned_disk();
    let memory = GuestMemory::allocate(4096);
    let range = OwnedRequestBuffers::linear(0, 4096, true);
    memory.write_at(0, &vec![0xa5; 4096]).unwrap();

    let result = disk
        .execute_scsi(&range.buffer(&memory), &write16_request(12, 8))
        .await;
    assert_eq!(result.scsi_status, scsi::ScsiStatus::CHECK_CONDITION);
    let sense = result.sense_data.unwrap();
    assert_eq!(
        sense.additional_sense_code_qualifier,
        scsi::SCSI_SENSEQ_WRITE_BOUNDARY_VIOLATION
    );

    let result = disk
        .execute_scsi(&range.buffer(&memory), &write16_request(12, 4))
        .await;
    assert_eq!(result.scsi_status, scsi::ScsiStatus::CHECK_CONDITION);
    let sense = result.sense_data.unwrap();
    assert_eq!(
        sense.additional_sense_code_qualifier,
        scsi::SCSI_SENSEQ_UNALIGNED_WRITE
    );

    let zone = zone_descriptor(&disk, 1).await;
    assert_eq!(
        zone.condition >> scsi::ZONE_CONDITION_SHIFT,
        scsi::ZONE_CONDITION_EMPTY
    );
    assert_eq!(zone.write_pointer_lba.get(), 8);
}

#[async_test]
async fn zoned_disk_enforces_open_zone_limit_and_releases_closed_zone() {
    let disk = new_zoned_disk();
    let memory = GuestMemory::allocate(4096);
    let range = OwnedRequestBuffers::linear(0, 4096, true);

    for zone_id in [8, 16] {
        let result = disk
            .execute_scsi(
                &range.buffer(&memory),
                &zbc_out_request(scsi::SERVICE_ACTION_OPEN_ZONE, zone_id),
            )
            .await;
        assert_eq!(result.scsi_status, scsi::ScsiStatus::GOOD, "{result:?}");
    }

    let result = disk
        .execute_scsi(
            &range.buffer(&memory),
            &zbc_out_request(scsi::SERVICE_ACTION_OPEN_ZONE, 24),
        )
        .await;
    assert_eq!(result.scsi_status, scsi::ScsiStatus::CHECK_CONDITION);
    let sense = result.sense_data.unwrap();
    assert_eq!(
        sense.additional_sense_code,
        scsi::AdditionalSenseCode::RESOURCE_FAILURE
    );
    assert_eq!(
        sense.additional_sense_code_qualifier,
        scsi::SCSI_SENSEQ_INSUFFICIENT_ZONE_RESOURCES
    );

    let result = disk
        .execute_scsi(
            &range.buffer(&memory),
            &zbc_out_request(scsi::SERVICE_ACTION_CLOSE_ZONE, 8),
        )
        .await;
    assert_eq!(result.scsi_status, scsi::ScsiStatus::GOOD, "{result:?}");

    let result = disk
        .execute_scsi(
            &range.buffer(&memory),
            &zbc_out_request(scsi::SERVICE_ACTION_OPEN_ZONE, 24),
        )
        .await;
    assert_eq!(result.scsi_status, scsi::ScsiStatus::GOOD, "{result:?}");
}

#[async_test]
async fn zoned_disk_implicitly_closes_zone_at_open_limit() {
    let disk = new_zoned_disk_with(8192, 2);
    let memory = GuestMemory::allocate(4096);
    let range = OwnedRequestBuffers::linear(0, 4096, true);
    memory.write_at(0, &vec![0xa5; 4096]).unwrap();

    for zone_id in [16, 32] {
        let result = disk
            .execute_scsi(&range.buffer(&memory), &write16_request(zone_id, 8))
            .await;
        assert_eq!(result.scsi_status, scsi::ScsiStatus::GOOD, "{result:?}");
    }

    let result = disk
        .execute_scsi(&range.buffer(&memory), &write16_request(48, 8))
        .await;
    assert_eq!(result.scsi_status, scsi::ScsiStatus::GOOD, "{result:?}");

    let closed = zone_descriptor(&disk, 1).await;
    assert_eq!(
        closed.condition >> scsi::ZONE_CONDITION_SHIFT,
        scsi::ZONE_CONDITION_CLOSED
    );
    assert_eq!(closed.write_pointer_lba.get(), 24);
    for index in [2, 3] {
        assert_eq!(
            zone_descriptor(&disk, index).await.condition >> scsi::ZONE_CONDITION_SHIFT,
            scsi::ZONE_CONDITION_IMPLICITLY_OPENED
        );
    }

    let result = disk
        .execute_scsi(&range.buffer(&memory), &write16_request(24, 8))
        .await;
    assert_eq!(result.scsi_status, scsi::ScsiStatus::GOOD, "{result:?}");
    assert_eq!(
        zone_descriptor(&disk, 1).await.condition >> scsi::ZONE_CONDITION_SHIFT,
        scsi::ZONE_CONDITION_FULL
    );
    assert_eq!(
        zone_descriptor(&disk, 2).await.condition >> scsi::ZONE_CONDITION_SHIFT,
        scsi::ZONE_CONDITION_CLOSED
    );
}

#[async_test]
async fn zoned_disk_closes_implicit_zone_before_explicit_open() {
    let disk = new_zoned_disk_with(8192, 2);
    let memory = GuestMemory::allocate(4096);
    let range = OwnedRequestBuffers::linear(0, 4096, true);
    memory.write_at(0, &vec![0xa5; 4096]).unwrap();

    let result = disk
        .execute_scsi(
            &range.buffer(&memory),
            &zbc_out_request(scsi::SERVICE_ACTION_OPEN_ZONE, 16),
        )
        .await;
    assert_eq!(result.scsi_status, scsi::ScsiStatus::GOOD, "{result:?}");
    let result = disk
        .execute_scsi(&range.buffer(&memory), &write16_request(32, 8))
        .await;
    assert_eq!(result.scsi_status, scsi::ScsiStatus::GOOD, "{result:?}");

    let result = disk
        .execute_scsi(
            &range.buffer(&memory),
            &zbc_out_request(scsi::SERVICE_ACTION_OPEN_ZONE, 48),
        )
        .await;
    assert_eq!(result.scsi_status, scsi::ScsiStatus::GOOD, "{result:?}");
    assert_eq!(
        zone_descriptor(&disk, 2).await.condition >> scsi::ZONE_CONDITION_SHIFT,
        scsi::ZONE_CONDITION_CLOSED
    );

    let result = disk
        .execute_scsi(
            &range.buffer(&memory),
            &zbc_out_request(scsi::SERVICE_ACTION_OPEN_ZONE, 64),
        )
        .await;
    assert_eq!(result.scsi_status, scsi::ScsiStatus::CHECK_CONDITION);
    let sense = result.sense_data.unwrap();
    assert_eq!(sense.header.sense_key, scsi::SenseKey::DATA_PROTECT);
    assert_eq!(
        sense.additional_sense_code,
        scsi::AdditionalSenseCode::RESOURCE_FAILURE
    );
    assert_eq!(
        sense.additional_sense_code_qualifier,
        scsi::SCSI_SENSEQ_INSUFFICIENT_ZONE_RESOURCES
    );
}

#[async_test]
async fn zoned_disk_finish_preserves_written_data_and_masks_unwritten_tail() {
    let disk = new_zoned_disk_with(8192, 1);
    let memory = GuestMemory::allocate(8192);
    let write_range = OwnedRequestBuffers::linear(0, 4096, true);
    memory.write_at(0, &vec![0xa5; 4096]).unwrap();

    let result = disk
        .execute_scsi(&write_range.buffer(&memory), &write16_request(16, 8))
        .await;
    assert_eq!(result.scsi_status, scsi::ScsiStatus::GOOD, "{result:?}");

    let result = disk
        .execute_scsi(
            &write_range.buffer(&memory),
            &zbc_out_request(scsi::SERVICE_ACTION_FINISH_ZONE, 16),
        )
        .await;
    assert_eq!(result.scsi_status, scsi::ScsiStatus::GOOD, "{result:?}");

    let zone = zone_descriptor(&disk, 1).await;
    assert_eq!(
        zone.condition >> scsi::ZONE_CONDITION_SHIFT,
        scsi::ZONE_CONDITION_FULL
    );
    assert_eq!(zone.write_pointer_lba.get(), u64::MAX);

    memory.write_at(0, &vec![0xff; 8192]).unwrap();
    let read_range = OwnedRequestBuffers::linear(0, 8192, true);
    let result = disk
        .execute_scsi(&read_range.buffer(&memory), &read16_request(16, 16))
        .await;
    assert_eq!(result.scsi_status, scsi::ScsiStatus::GOOD, "{result:?}");

    let mut bytes = vec![0; 8192];
    memory.read_at(0, &mut bytes).unwrap();
    assert!(bytes[..4096].iter().all(|value| *value == 0xa5));
    assert!(bytes[4096..].iter().all(|value| *value == 0));
}

#[async_test]
async fn zoned_disk_recovers_zone_state_and_data_after_reopen() {
    let (disk, state) = new_zoned_disk_with_state(ZONE_SIZE, 2);
    let memory = GuestMemory::allocate(4096);
    let range = OwnedRequestBuffers::linear(0, 4096, true);
    memory.write_at(0, &vec![0xa5; 4096]).unwrap();

    let result = disk
        .execute_scsi(&range.buffer(&memory), &write16_request(8, 8))
        .await;
    assert_eq!(result.scsi_status, scsi::ScsiStatus::GOOD, "{result:?}");
    assert_eq!(state.lock().sync_count, 2);
    drop(disk);

    let reopened = reopen_zoned_disk(state, ZONE_SIZE, 2);
    let zone = zone_descriptor(&reopened, 1).await;
    assert_eq!(
        zone.condition >> scsi::ZONE_CONDITION_SHIFT,
        scsi::ZONE_CONDITION_FULL
    );
    assert_eq!(zone.write_pointer_lba.get(), u64::MAX);

    memory.write_at(0, &vec![0; 4096]).unwrap();
    let result = reopened
        .execute_scsi(&range.buffer(&memory), &read16_request(8, 8))
        .await;
    assert_eq!(result.scsi_status, scsi::ScsiStatus::GOOD, "{result:?}");
    let mut bytes = vec![0; 4096];
    memory.read_at(0, &mut bytes).unwrap();
    assert!(bytes.iter().all(|value| *value == 0xa5));
}

#[async_test]
async fn zoned_disk_flushes_data_before_committing_metadata() {
    let (disk, state) = new_zoned_disk_with_state(ZONE_SIZE, 2);
    let memory = GuestMemory::allocate(4096);
    let range = OwnedRequestBuffers::linear(0, 4096, true);
    memory.write_at(0, &vec![0xa5; 4096]).unwrap();

    let result = disk
        .execute_scsi(&range.buffer(&memory), &write16_request(8, 8))
        .await;
    assert_eq!(result.scsi_status, scsi::ScsiStatus::GOOD, "{result:?}");

    assert_eq!(
        state.lock().operations,
        vec![
            TestDiskOperation::Write {
                sector: 8,
                fua: false,
            },
            TestDiskOperation::Sync,
            TestDiskOperation::Write {
                sector: VISIBLE_SECTOR_COUNT,
                fua: false,
            },
            TestDiskOperation::Sync,
        ]
    );
}

#[async_test]
async fn zoned_disk_retries_management_action_after_metadata_write_failure() {
    let (disk, state) = new_zoned_disk_with_state(ZONE_SIZE, 2);
    let memory = GuestMemory::allocate(4096);
    let range = OwnedRequestBuffers::linear(0, 4096, true);
    memory.write_at(0, &vec![0xa5; 4096]).unwrap();

    let result = disk
        .execute_scsi(&range.buffer(&memory), &write16_request(8, 8))
        .await;
    assert_eq!(result.scsi_status, scsi::ScsiStatus::GOOD, "{result:?}");

    state.lock().fail_next_write_once = true;

    let reset = zbc_out_request(scsi::SERVICE_ACTION_RESET_WRITE_POINTER, 8);
    let result = disk.execute_scsi(&range.buffer(&memory), &reset).await;
    let failure_state = state.lock();
    assert_eq!(
        result.scsi_status,
        scsi::ScsiStatus::CHECK_CONDITION,
        "fail_next_write_once={}, operations={:?}",
        failure_state.fail_next_write_once,
        failure_state.operations
    );
    drop(failure_state);
    assert_eq!(
        zone_descriptor(&disk, 1).await.condition >> scsi::ZONE_CONDITION_SHIFT,
        scsi::ZONE_CONDITION_FULL
    );

    let result = disk.execute_scsi(&range.buffer(&memory), &reset).await;
    assert_eq!(result.scsi_status, scsi::ScsiStatus::GOOD, "{result:?}");
    drop(disk);

    let reopened = reopen_zoned_disk(state, ZONE_SIZE, 2);
    let zone = zone_descriptor(&reopened, 1).await;
    assert_eq!(
        zone.condition >> scsi::ZONE_CONDITION_SHIFT,
        scsi::ZONE_CONDITION_EMPTY
    );
    assert_eq!(zone.write_pointer_lba.get(), 8);
}

#[async_test]
async fn zoned_disk_serializes_zone_transitions_with_metadata_snapshot() {
    let (mut backing, state) =
        TestDisk::new(SECTOR_SIZE, PHYSICAL_SECTOR_SIZE, SECTOR_COUNT, false, true);
    let (entered_send, entered_recv) = mesh::oneshot();
    let (release_send, release_recv) = mesh::oneshot();
    backing.write_blocker = Some(Arc::new(TestDiskWriteBlocker {
        sector: VISIBLE_SECTOR_COUNT,
        entered: Mutex::new(Some(entered_send)),
        release: Mutex::new(Some(release_recv)),
    }));
    let disk = SimpleScsiDisk::new(Disk::new(backing).unwrap(), zoned_parameters(8192, 1)).unwrap();

    let memory = GuestMemory::allocate(4096);
    memory.write_at(0, &vec![0xa5; 4096]).unwrap();
    let first_range = OwnedRequestBuffers::linear(0, 4096, true);
    let second_range = OwnedRequestBuffers::linear(0, 4096, true);
    let first_buffer = first_range.buffer(&memory);
    let second_buffer = second_range.buffer(&memory);
    let first_request = write16_request(16, 8);
    let second_request = write16_request(32, 8);
    let mut first = std::pin::pin!(disk.execute_scsi(&first_buffer, &first_request));
    assert!(futures::poll!(first.as_mut()).is_pending());
    entered_recv.await.unwrap();

    let operations_before_second = state.lock().operations.clone();
    let mut second = std::pin::pin!(disk.execute_scsi(&second_buffer, &second_request));
    assert!(futures::poll!(second.as_mut()).is_pending());
    assert_eq!(state.lock().operations, operations_before_second);

    release_send.send(());
    let first_result = first.await;
    assert_eq!(
        first_result.scsi_status,
        scsi::ScsiStatus::GOOD,
        "{first_result:?}"
    );
    let second_result = second.await;
    assert_eq!(
        second_result.scsi_status,
        scsi::ScsiStatus::GOOD,
        "{second_result:?}"
    );

    let reopened = reopen_zoned_disk(state, 8192, 1);
    assert_eq!(
        zone_descriptor(&reopened, 1).await.condition >> scsi::ZONE_CONDITION_SHIFT,
        scsi::ZONE_CONDITION_CLOSED
    );
    assert_eq!(
        zone_descriptor(&reopened, 2).await.condition >> scsi::ZONE_CONDITION_SHIFT,
        scsi::ZONE_CONDITION_IMPLICITLY_OPENED
    );
}

#[async_test]
async fn zoned_disk_recovers_previous_slot_when_newest_metadata_is_corrupt() {
    let (disk, state) = new_zoned_disk_with_state(ZONE_SIZE, 2);
    let memory = GuestMemory::allocate(4096);
    let range = OwnedRequestBuffers::linear(0, 4096, true);

    let result = disk
        .execute_scsi(
            &range.buffer(&memory),
            &zbc_out_request(scsi::SERVICE_ACTION_OPEN_ZONE, 8),
        )
        .await;
    assert_eq!(result.scsi_status, scsi::ScsiStatus::GOOD, "{result:?}");
    let result = disk
        .execute_scsi(
            &range.buffer(&memory),
            &zbc_out_request(scsi::SERVICE_ACTION_CLOSE_ZONE, 8),
        )
        .await;
    assert_eq!(result.scsi_status, scsi::ScsiStatus::GOOD, "{result:?}");
    assert_eq!(state.lock().sync_count, 2);
    drop(disk);

    let metadata_slot_sectors = u64::from(PHYSICAL_SECTOR_SIZE / SECTOR_SIZE);
    let newest_slot_offset =
        ((VISIBLE_SECTOR_COUNT + metadata_slot_sectors) * u64::from(SECTOR_SIZE)) as usize;
    state.lock().storage[newest_slot_offset] ^= 0xff;

    let reopened = reopen_zoned_disk(state, ZONE_SIZE, 2);
    let zone = zone_descriptor(&reopened, 1).await;
    assert_eq!(
        zone.condition >> scsi::ZONE_CONDITION_SHIFT,
        scsi::ZONE_CONDITION_EXPLICITLY_OPENED
    );
}

#[async_test]
async fn zoned_disk_rejects_backing_with_only_corrupt_metadata() {
    let (disk, state) = new_zoned_disk_with_state(ZONE_SIZE, 2);
    let memory = GuestMemory::allocate(4096);
    let range = OwnedRequestBuffers::linear(0, 4096, true);

    let result = disk
        .execute_scsi(
            &range.buffer(&memory),
            &zbc_out_request(scsi::SERVICE_ACTION_OPEN_ZONE, 8),
        )
        .await;
    assert_eq!(result.scsi_status, scsi::ScsiStatus::GOOD, "{result:?}");
    drop(disk);

    let metadata_offset = (VISIBLE_SECTOR_COUNT * u64::from(SECTOR_SIZE)) as usize;
    state.lock().storage[metadata_offset] ^= 0xff;

    let reopened = reopen_zoned_disk(state, ZONE_SIZE, 2);
    let result = reopened
        .execute_scsi(&range.buffer(&memory), &report_zones_request())
        .await;
    assert_eq!(result.scsi_status, scsi::ScsiStatus::CHECK_CONDITION);
    let sense = result.sense_data.unwrap();
    assert_eq!(sense.header.sense_key, scsi::SenseKey::HARDWARE_ERROR);
    assert_eq!(
        sense.additional_sense_code,
        scsi::AdditionalSenseCode::INTERNAL_TARGET_FAILURE
    );
}
