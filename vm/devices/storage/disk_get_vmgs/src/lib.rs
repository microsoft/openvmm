// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! A disk backend using the GET's VMGS block interface.
//!
//! This is coded as a general-purpose block device (I guess you could boot a VM
//! off of it), but it is likely only useful for using as the VMGS backing
//! device.

#![cfg(target_os = "linux")]
#![forbid(unsafe_code)]

use disk_backend::DiskError;
use disk_backend::DiskIo;
use guest_emulation_transport::GuestEmulationTransportClient;
use guestmem::MemoryRead;
use guestmem::MemoryWrite;
use inspect::Inspect;
use save_restore::SavedBlockStorageMetadata;
use scsi_buffers::RequestBuffers;
use std::io;
use thiserror::Error;

/// An implementation of [`DiskIo`] backed by the GET.
#[derive(Clone, Debug, Inspect)]
pub struct GetVmgsDisk {
    get: GuestEmulationTransportClient,
    sector_size: u32,
    sector_shift: u32,
    physical_sector_size: u32,
    sector_count: u64,
    max_transfer_sectors: u32,
    max_transfer_size_bytes: u32,
}

/// An error that can occur when creating a new [`GetVmgsDisk`].
#[derive(Debug, Error)]
pub enum NewGetVmgsDiskError {
    /// An IO error occurred while fetching the device info.
    #[error("GET VMGS IO error")]
    Io(#[source] guest_emulation_transport::error::VmgsIoError),
    /// The sector size is not a power of two.
    #[error("invalid sector size")]
    InvalidSectorSize,
    /// The physical sector size is not a power of two or is smaller than the sector size.
    #[error("invalid physical sector size")]
    InvalidPhysicalSectorSize,
    /// The sector count is too large.
    #[error("invalid sector count")]
    InvalidSectorCount,
    /// The disk ends with a partial physical sector.
    #[error("disk ends with a partial physical sector")]
    IncompletePhysicalSector,
    /// The maximum transfer size is smaller than the physical sector size.
    #[error("transfer size is smaller than the physical sector size")]
    InvalidMaxTransferSize,
}

impl GetVmgsDisk {
    /// Returns a new disk instance, communicating read and write IOs over the
    /// `get` transport.
    pub async fn new(get: GuestEmulationTransportClient) -> Result<Self, NewGetVmgsDiskError> {
        let response = get
            .vmgs_get_device_info()
            .await
            .map_err(NewGetVmgsDiskError::Io)?;
        Self::new_inner(
            get,
            response.bytes_per_logical_sector.into(),
            response.bytes_per_physical_sector.into(),
            response.capacity,
            response.maximum_transfer_size_bytes,
        )
    }

    /// Create a disk using metadata previously-fetched via [`Self::save_meta`].
    ///
    /// # Caution
    ///
    /// This method does not confirm that the provided `meta` is what would be
    /// provided by `get`. Callers MUST ensure that the provided `meta` matches
    /// the provided `get` instance.
    ///
    /// Failing to do so may result in data corruption/loss, though, notably: it
    /// will _not_ result in any memory-unsafety (hence why the function isn't
    /// marked `unsafe`).
    pub fn restore_with_meta(
        get: GuestEmulationTransportClient,
        meta: SavedBlockStorageMetadata,
    ) -> Result<Self, NewGetVmgsDiskError> {
        Self::new_inner(
            get,
            meta.sector_size,
            meta.physical_sector_size,
            meta.sector_count,
            meta.max_transfer_size_bytes,
        )
    }

    /// Save the metadata for this disk, for use in passing to
    /// [`Self::restore_with_meta`]
    pub fn save_meta(&self) -> SavedBlockStorageMetadata {
        SavedBlockStorageMetadata {
            capacity: self.sector_count * self.sector_size as u64,
            logical_sector_size: self.sector_size,
            sector_count: self.sector_count,
            sector_size: self.sector_size,
            physical_sector_size: self.physical_sector_size,
            max_transfer_size_bytes: self.max_transfer_size_bytes,
        }
    }

    fn new_inner(
        get: GuestEmulationTransportClient,
        sector_size: u32,
        physical_sector_size: u32,
        sector_count: u64,
        max_transfer_size: u32,
    ) -> Result<Self, NewGetVmgsDiskError> {
        if !sector_size.is_power_of_two() {
            Err(NewGetVmgsDiskError::InvalidSectorSize)
        } else if !physical_sector_size.is_power_of_two() || physical_sector_size < sector_size {
            Err(NewGetVmgsDiskError::InvalidPhysicalSectorSize)
        } else if sector_count.checked_mul(sector_size as u64).is_none() {
            Err(NewGetVmgsDiskError::InvalidSectorCount)
        } else if !sector_count.is_multiple_of((physical_sector_size / sector_size) as u64) {
            Err(NewGetVmgsDiskError::IncompletePhysicalSector)
        } else if max_transfer_size < physical_sector_size {
            Err(NewGetVmgsDiskError::InvalidMaxTransferSize)
        } else {
            Ok(GetVmgsDisk {
                get,
                sector_size,
                sector_shift: sector_size.trailing_zeros(),
                physical_sector_size,
                sector_count,
                max_transfer_size_bytes: max_transfer_size,
                // Always transfer in multiples of the physical sector size, if possible.
                max_transfer_sectors: max_transfer_size / physical_sector_size
                    * physical_sector_size
                    / sector_size,
            })
        }
    }
}

impl DiskIo for GetVmgsDisk {
    fn disk_type(&self) -> &str {
        "vmgs-get"
    }

    fn sector_count(&self) -> u64 {
        self.sector_count
    }

    fn sector_size(&self) -> u32 {
        self.sector_size
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

    fn is_read_only(&self) -> bool {
        false
    }

    async fn read_vectored(
        &self,
        buffers: &RequestBuffers<'_>,
        mut sector: u64,
    ) -> Result<(), DiskError> {
        let mut writer = buffers.writer();
        let mut remaining_sectors = buffers.len() >> self.sector_shift;
        if sector + remaining_sectors as u64 > self.sector_count {
            return Err(DiskError::IllegalBlock);
        }
        while remaining_sectors != 0 {
            let this_sector_count = remaining_sectors.min(self.max_transfer_sectors as usize);
            let data = self
                .get
                .vmgs_read(sector, this_sector_count as u32, self.sector_size)
                .await
                .map_err(|err| DiskError::Io(io::Error::other(err)))?;

            writer.write(&data)?;
            sector += this_sector_count as u64;
            remaining_sectors -= this_sector_count;
        }
        Ok(())
    }

    async fn write_vectored(
        &self,
        buffers: &RequestBuffers<'_>,
        mut sector: u64,
        _fua: bool,
    ) -> Result<(), DiskError> {
        let mut reader = buffers.reader();
        let mut remaining_sector_count = buffers.len() >> self.sector_shift;
        if sector + remaining_sector_count as u64 > self.sector_count {
            return Err(DiskError::IllegalBlock);
        }
        while remaining_sector_count != 0 {
            let this_sector_count = remaining_sector_count.min(self.max_transfer_sectors as usize);
            let data = reader.read_n(this_sector_count << self.sector_shift)?;
            self.get
                .vmgs_write(sector, data, self.sector_size)
                .await
                .map_err(|err| DiskError::Io(io::Error::other(err)))?;

            remaining_sector_count -= this_sector_count;
            sector += this_sector_count as u64;
        }
        Ok(())
    }

    /// Issues an asynchronous flush operation to the disk.
    async fn sync_cache(&self) -> Result<(), DiskError> {
        self.get
            .vmgs_flush()
            .await
            .map_err(|err| DiskError::Io(io::Error::other(err)))
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

/// Save/restore structure definitions.
pub mod save_restore {
    use mesh::payload::Protobuf;

    /// Metadata for a saved block storage device.
    #[derive(Protobuf, Clone)]
    #[mesh(package = "vmgs")]
    pub struct SavedBlockStorageMetadata {
        /// The byte capacity. Redundant with sector_count * sector_size.
        #[mesh(1)]
        pub capacity: u64,
        /// The logical sector size. Identical to sector_size.
        #[mesh(2)]
        pub logical_sector_size: u32,
        /// The number of sectors.
        #[mesh(3)]
        pub sector_count: u64,
        /// The sector size in bytes.
        #[mesh(4)]
        pub sector_size: u32,
        /// The physical sector size in bytes.
        #[mesh(5)]
        pub physical_sector_size: u32,
        /// The maximum transfer size in bytes.
        #[mesh(6)]
        pub max_transfer_size_bytes: u32,
    }
}

// TODO: remove the VMGS specific tests and just test the `DiskIo` interfaces.
#[cfg(test)]
mod tests {
    use super::*;
    use disk_backend::Disk;
    use guest_emulation_transport::api::ProtocolVersion;
    use guest_emulation_transport::test_utilities::TestGet;
    use guest_emulation_transport::test_utilities::new_transport_pair;
    use pal_async::DefaultDriver;
    use pal_async::async_test;
    use pal_async::task::Task;
    use vmgs::FileId;
    use vmgs::Vmgs;
    use vmgs_broker::VmgsClient;
    use vmgs_broker::spawn_vmgs_broker;

    async fn spawn_vmgs(driver: &DefaultDriver) -> (VmgsClient, TestGet, Task<()>) {
        let get = new_transport_pair(driver, None, ProtocolVersion::NICKEL_REV2, None, None).await;
        let vmgs_get = GetVmgsDisk::new(get.client.clone()).await.unwrap();
        let vmgs = Vmgs::format_new(Disk::new(vmgs_get).unwrap(), None)
            .await
            .unwrap();
        let (vmgs, task) = spawn_vmgs_broker(driver, vmgs);
        (vmgs, get, task)
    }

    #[async_test]
    async fn basic_read_write(driver: DefaultDriver) {
        let (vmgs, _get, _task) = spawn_vmgs(&driver).await;
        let file_id = FileId::BIOS_NVRAM;

        // write
        let buf = b"hello world".to_vec();
        vmgs.write_file(file_id, buf.clone()).await.unwrap();

        // read
        let info = vmgs.get_file_info(file_id).await.unwrap();
        assert_eq!(info.valid_bytes as usize, buf.len());
        let read_buf = vmgs.read_file(file_id).await.unwrap();

        assert_eq!(buf, read_buf);
    }

    #[async_test]
    async fn multiple_read_write(driver: DefaultDriver) {
        let (vmgs, _get, _task) = spawn_vmgs(&driver).await;
        let file_id_1 = FileId::BIOS_NVRAM;
        let file_id_2 = FileId::TPM_PPI;
        let buf_1 = b"Data data data".to_vec();
        let buf_2 = b"password".to_vec();
        let buf_3 = b"other data data".to_vec();

        vmgs.write_file(file_id_1, buf_1.clone()).await.unwrap();
        let info = vmgs.get_file_info(file_id_1).await.unwrap();
        assert_eq!(info.valid_bytes as usize, buf_1.len());
        let read_buf_1 = vmgs.read_file(file_id_1).await.unwrap();
        assert_eq!(buf_1, read_buf_1);

        vmgs.write_file(file_id_2, buf_2.clone()).await.unwrap();
        let info = vmgs.get_file_info(file_id_2).await.unwrap();
        assert_eq!(info.valid_bytes as usize, buf_2.len());
        let read_buf_2 = vmgs.read_file(file_id_2).await.unwrap();
        assert_eq!(buf_2, read_buf_2);

        vmgs.write_file(file_id_1, buf_3.clone()).await.unwrap();
        let info = vmgs.get_file_info(file_id_1).await.unwrap();
        assert_eq!(info.valid_bytes as usize, buf_3.len());
        let read_buf_3 = vmgs.read_file(file_id_1).await.unwrap();
        assert_eq!(buf_3, read_buf_3);

        vmgs.write_file(file_id_1, buf_1.clone()).await.unwrap();
        let info = vmgs.get_file_info(file_id_1).await.unwrap();
        assert_eq!(info.valid_bytes as usize, buf_1.len());
        let read_buf_1 = vmgs.read_file(file_id_1).await.unwrap();
        assert_eq!(buf_1, read_buf_1);

        vmgs.write_file(file_id_2, buf_2.clone()).await.unwrap();
        let info = vmgs.get_file_info(file_id_2).await.unwrap();
        assert_eq!(info.valid_bytes as usize, buf_2.len());
        let read_buf_2 = vmgs.read_file(file_id_2).await.unwrap();
        assert_eq!(buf_2, read_buf_2);

        vmgs.write_file(file_id_1, buf_3.clone()).await.unwrap();
        let info = vmgs.get_file_info(file_id_1).await.unwrap();
        assert_eq!(info.valid_bytes as usize, buf_3.len());
        let read_buf_3 = vmgs.read_file(file_id_1).await.unwrap();
        assert_eq!(buf_3, read_buf_3);
    }

    #[async_test]
    async fn test_empty_write(driver: DefaultDriver) {
        let (vmgs, _get, _task) = spawn_vmgs(&driver).await;
        let file_id = FileId::BIOS_NVRAM;

        let buf: Vec<u8> = Vec::new();
        vmgs.write_file(file_id, buf.clone()).await.unwrap();

        // read
        let info = vmgs.get_file_info(file_id).await.unwrap();
        assert_eq!(info.valid_bytes as usize, 0);
        let read_buf = vmgs.read_file(file_id).await.unwrap();

        assert_eq!(buf, read_buf);
        assert_eq!(read_buf.len(), 0);
    }

    #[async_test]
    async fn test_read_write_large(driver: DefaultDriver) {
        let (vmgs, _get, _task) = spawn_vmgs(&driver).await;
        let file_id = FileId::BIOS_NVRAM;

        // write
        let buf: Vec<u8> = (0..).map(|x| x as u8).take(1024 * 4 * 4 + 1).collect();
        vmgs.write_file(file_id, buf.clone()).await.unwrap();

        // read
        let info = vmgs.get_file_info(file_id).await.unwrap();
        assert_eq!(info.valid_bytes as usize, buf.len());
        let read_buf = vmgs.read_file(file_id).await.unwrap();

        assert_eq!(buf, read_buf);
    }

    #[async_test]
    async fn test_read_write_encryption(driver: DefaultDriver) {
        let get = new_transport_pair(&driver, None, ProtocolVersion::NICKEL_REV2, None, None).await;
        let vmgs_get = GetVmgsDisk::new(get.client.clone()).await.unwrap();
        let mut vmgs = Vmgs::format_new(Disk::new(vmgs_get).unwrap(), None)
            .await
            .unwrap();
        let file_id = FileId::BIOS_NVRAM;
        let encryption_key = vec![1; 32];

        vmgs.update_encryption_key(&encryption_key, vmgs::EncryptionAlgorithm::AES_GCM)
            .await
            .unwrap();

        // write
        let buf: Vec<u8> = (0..).map(|x| x as u8).take(1024 * 4 * 4 + 1).collect();
        vmgs.write_file_encrypted(file_id, &buf).await.unwrap();

        // read
        let info = vmgs.get_file_info(file_id).unwrap();
        assert_eq!(info.valid_bytes as usize, buf.len());
        let read_buf = vmgs.read_file(file_id).await.unwrap();

        assert_eq!(buf, read_buf);

        drop(vmgs);

        let vmgs_get = GetVmgsDisk::new(get.client.clone()).await.unwrap();
        let mut vmgs = Vmgs::open(Disk::new(vmgs_get).unwrap(), None)
            .await
            .unwrap();

        let read_buf = vmgs.read_file(file_id).await.unwrap();

        assert_ne!(buf, read_buf);

        vmgs.unlock_with_encryption_key(&encryption_key)
            .await
            .unwrap();

        let (vmgs, _task) = spawn_vmgs_broker(&driver, vmgs);

        let info = vmgs.get_file_info(file_id).await.unwrap();
        assert_eq!(info.valid_bytes as usize, buf.len());
        let read_buf = vmgs.read_file(file_id).await.unwrap();

        assert_eq!(buf, read_buf);
    }
}
