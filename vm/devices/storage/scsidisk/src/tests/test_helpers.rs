// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! ScsiDisk test helpers.

use crate::atapi_scsi::AtapiScsiDisk;
use crate::scsi;
use crate::scsidvd::SimpleScsiDvd;
use crate::SimpleScsiDisk;
use disk_backend::Disk;
use disk_backend::DiskError;
use disk_backend::DiskIo;
use disk_prwrap::DiskWithReservations;
use guestmem::GuestMemory;
use guestmem::MemoryRead;
use guestmem::MemoryWrite;
use hvdef::HV_PAGE_SIZE;
use inspect::Inspect;
use parking_lot::Mutex;
use scsi::ScsiOp;
use scsi::ScsiStatus;
use scsi_buffers::RequestBuffers;
use scsi_core::AsyncScsiDisk;
use scsi_core::Request;
use scsi_core::ScsiResult;
use std::sync::Arc;
use zerocopy::IntoBytes;

#[derive(Debug)]
pub struct TestDiskStorageState {
    pub storage: Vec<u8>,
    pub is_fua_set: bool,
    pub sector_count: u64,
}

#[derive(Debug)]
pub struct TestDisk {
    pub sector_size: u32,
    pub physical_sector_size: u32,
    pub read_only: bool,
    pub state: Arc<Mutex<TestDiskStorageState>>,
}

impl Inspect for TestDisk {
    fn inspect(&self, req: inspect::Request<'_>) {
        req.respond();
    }
}

impl TestDisk {
    pub fn new(
        logical_sector_size: u32,
        physical_sector_size: u32,
        sector_count: u64,
        read_only: bool,
        storage: bool,
    ) -> (TestDisk, Arc<Mutex<TestDiskStorageState>>) {
        let mut buffer = Vec::new();
        if storage {
            buffer.resize(sector_count as usize * logical_sector_size as usize, 0);
        }
        let state = Arc::new(Mutex::new(TestDiskStorageState {
            storage: buffer,
            is_fua_set: false,
            sector_count,
        }));
        (
            TestDisk {
                sector_size: logical_sector_size,
                read_only,
                state: state.clone(),
                physical_sector_size,
            },
            state,
        )
    }
}

impl DiskIo for TestDisk {
    fn disk_type(&self) -> &str {
        "test"
    }

    fn sector_count(&self) -> u64 {
        self.state.lock().sector_count
    }

    fn sector_size(&self) -> u32 {
        self.sector_size
    }

    fn is_read_only(&self) -> bool {
        self.read_only
    }

    fn disk_id(&self) -> Option<[u8; 16]> {
        None
    }

    fn physical_sector_size(&self) -> u32 {
        self.physical_sector_size
    }

    fn is_fua_respected(&self) -> bool {
        false
    }

    async fn read_vectored(
        &self,
        buffers: &RequestBuffers<'_>,
        sector: u64,
    ) -> Result<(), DiskError> {
        let offset = sector as usize * self.sector_size() as usize;
        let end_point = offset + buffers.len();
        let mut state = self.state.lock();
        if state.storage.len() < end_point {
            return Err(DiskError::IllegalBlock);
        }
        buffers.writer().write(&state.storage[offset..end_point])?;
        state.is_fua_set = false;
        Ok(())
    }

    async fn write_vectored(
        &self,
        buffers: &RequestBuffers<'_>,
        sector: u64,
        fua: bool,
    ) -> Result<(), DiskError> {
        let offset = sector as usize * self.sector_size() as usize;
        let end_point = offset + buffers.len();
        let mut state = self.state.lock();
        if state.storage.len() < end_point {
            return Err(DiskError::IllegalBlock);
        }
        buffers
            .reader()
            .read(&mut state.storage[offset..end_point])?;
        state.is_fua_set = fua;
        Ok(())
    }

    async fn sync_cache(&self) -> Result<(), DiskError> {
        Ok(())
    }

    async fn unmap(
        &self,
        _sector: u64,
        _count: u64,
        _block_level_only: bool,
    ) -> Result<(), DiskError> {
        Ok(())
    }

    fn unmap_behavior(&self) -> disk_backend::UnmapBehavior {
        disk_backend::UnmapBehavior::Ignored
    }
}

pub fn new_scsi_disk(
    logical_sector_size: u32,
    physical_sector_size: u32,
    sector_count: u64,
    read_only: bool,
    storage: bool,
    pr: bool,
) -> (SimpleScsiDisk, Arc<Mutex<TestDiskStorageState>>) {
    let (disk, state) = TestDisk::new(
        logical_sector_size,
        physical_sector_size,
        sector_count,
        read_only,
        storage,
    );

    let simple_disk = if pr {
        Disk::new(DiskWithReservations::new(Disk::new(disk).unwrap())).unwrap()
    } else {
        Disk::new(disk).unwrap()
    };

    let scsi_disk = SimpleScsiDisk::new(simple_disk, Default::default());
    let sector_shift = logical_sector_size.trailing_zeros() as u8;
    let physical_extra_shift = physical_sector_size.trailing_zeros() as u8 - sector_shift;
    assert_eq!(scsi_disk.sector_size, logical_sector_size);
    assert_eq!(
        scsi_disk.get_and_update_sector_count(ScsiOp::READ),
        Ok(sector_count)
    );
    assert_eq!(scsi_disk.physical_extra_shift, physical_extra_shift);
    assert_eq!(scsi_disk.disk.is_read_only(), read_only);
    assert_eq!(scsi_disk.sector_shift, sector_shift);
    assert_eq!(
        scsi_disk.scsi_parameters.physical_sector_size,
        physical_sector_size
    );
    assert_eq!(state.lock().is_fua_set, false);
    (scsi_disk, state)
}

pub fn new_scsi_dvd(
    logical_sector_size: u32,
    physical_sector_size: u32,
    sector_count: u64,
    storage: bool,
) -> (SimpleScsiDvd, Arc<Mutex<TestDiskStorageState>>) {
    let (disk, state) = TestDisk::new(
        logical_sector_size,
        physical_sector_size,
        sector_count,
        true,
        storage,
    );
    let scsi_dvd = SimpleScsiDvd::new(Some(Disk::new(disk).unwrap()));
    (scsi_dvd, state)
}

pub fn new_atapi_disk(scsi_disk: Arc<SimpleScsiDvd>) -> AtapiScsiDisk {
    AtapiScsiDisk::new(scsi_disk)
}

pub fn make_guest_memory(data: &[u8]) -> GuestMemory {
    let mem = GuestMemory::allocate(data.len());
    mem.write_at(0, data).unwrap();
    mem
}

pub fn make_repeat_data_buffer(len: usize, sector_size: usize) -> Vec<u8> {
    let mut buf = vec![0u8; len * HV_PAGE_SIZE as usize];
    let mut temp = vec![0u8; sector_size];
    getrandom::getrandom(&mut temp).unwrap();

    for i in (0..buf.len()).step_by(temp.len()) {
        let end_point = std::cmp::min(i + temp.len(), buf.len());
        buf[i..end_point].copy_from_slice(&temp[..end_point - i]);
    }

    buf
}

pub async fn check_execute_scsi_pass(
    scsi_disk: &dyn AsyncScsiDisk,
    external_data: &RequestBuffers<'_>,
    request: &Request,
) {
    let result = scsi_disk.execute_scsi(external_data, request).await;
    if result.scsi_status != ScsiStatus::GOOD {
        panic!(
            "execute_scsi failed! request: {:?} result: {:?}",
            request, result
        );
    }
}

pub async fn check_execute_scsi_pass_with_tx(
    scsi_disk: &dyn AsyncScsiDisk,
    external_data: &RequestBuffers<'_>,
    request: &Request,
    tx: usize,
) {
    let result = scsi_disk.execute_scsi(external_data, request).await;
    if result.scsi_status != ScsiStatus::GOOD {
        panic!(
            "execute_scsi failed! request: {:?} result: {:?}",
            request, result
        );
    }
    assert_eq!(result.tx, tx);
}

pub async fn check_execute_scsi_failed_with_result(
    scsi_disk: &dyn AsyncScsiDisk,
    external_data: &RequestBuffers<'_>,
    request: &Request,
    result: &ScsiResult,
) {
    let actual = scsi_disk.execute_scsi(external_data, request).await;
    assert_eq!(result.scsi_status, actual.scsi_status);
    assert_eq!(result.srb_status, actual.srb_status);
    assert_eq!(result.tx, actual.tx);
    if let Some(sense_data) = result.sense_data {
        assert!(actual.sense_data.is_some());
        let actual_sense_data = actual.sense_data.unwrap();
        assert!(sense_data.as_bytes()[..].eq(actual_sense_data.as_bytes()));
    } else {
        assert!(actual.sense_data.is_none());
    }
}

pub fn make_cdb16_request(
    operation_code: ScsiOp,
    fua: bool,
    start_lba: u64,
    lba_count: u32,
) -> Request {
    let cdb = scsi::Cdb16 {
        operation_code,
        flags: scsi::Cdb16Flags::new().with_fua(fua),
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

pub fn make_cdb10_request(
    operation_code: ScsiOp,
    fua: bool,
    start_lba: u32,
    lba_count: u16,
) -> Request {
    let cdb = scsi::Cdb10 {
        operation_code,
        flags: scsi::CdbFlags::new().with_fua(fua),
        logical_block: start_lba.into(),
        transfer_blocks: lba_count.into(),
        reserved2: 0,
        control: 0,
    };
    let mut data = [0u8; 16];
    data[..cdb.as_bytes().len()].copy_from_slice(cdb.as_bytes());
    Request {
        cdb: data,
        srb_flags: 0,
    }
}

pub fn check_guest_memory(guest_mem: &GuestMemory, start_lba: u64, buff: &Vec<u8>) {
    let mut b = vec![0u8; buff.len()];
    if guest_mem.read_at(start_lba, &mut b).is_err() {
        panic!("guest_mem read error");
    };
    if !buff[..].eq(&b[..]) {
        panic!("expect {:?} actual {:?}", buff, b);
    }
}
