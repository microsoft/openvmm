// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use async_trait::async_trait;
use guest_emulation_transport::GuestEmulationTransportClient;
use std::io;
use vmgs::disk::BlockStorage;
use vmgs::disk::BlockStorageMetadata;

/// An implementation of [`vmgs::disk::BlockStorage`] backed by the GET.
#[derive(Clone, Debug)]
pub struct VmgsGet {
    get: GuestEmulationTransportClient,
    meta: BlockStorageMetadata,
}

impl VmgsGet {
    pub async fn new(
        get: GuestEmulationTransportClient,
    ) -> Result<Self, guest_emulation_transport::error::VmgsIoError> {
        let response = get.vmgs_get_device_info().await?;
        Ok(VmgsGet {
            get,
            meta: BlockStorageMetadata {
                capacity: response.capacity * response.bytes_per_logical_sector as u64,
                logical_sector_size: response.bytes_per_logical_sector.into(),
                sector_count: response.capacity,
                sector_size: response.bytes_per_logical_sector.into(),
                physical_sector_size: response.bytes_per_physical_sector.into(),
                max_trans_size_bytes: response.maximum_transfer_size_bytes,
            },
        })
    }

    /// Create a [`VmgsGet`] using metadata previously-fetched via
    /// [`GuestEmulationTransportClient::vmgs_get_device_info`].
    ///
    /// # Safety
    ///
    /// `new_with_meta` does NOT perform ANY validation on the provided `meta`,
    /// and will blindly assume that it matches the corresponding `get`
    /// instance!
    ///
    /// Callers MUST ensure that the provided `meta` matches the provided `get`
    /// instance.
    ///
    /// Failing to do so may result in data corruption/loss, though, notably: it
    /// will _not_ result in any memory-unsafety (hence why the function isn't
    /// marked `unsafe`).
    pub fn new_with_meta(get: GuestEmulationTransportClient, meta: BlockStorageMetadata) -> Self {
        VmgsGet { get, meta }
    }
}

#[async_trait]
impl BlockStorage for VmgsGet {
    async fn read_block(&mut self, byte_offset: u64, buf: &mut [u8]) -> io::Result<()> {
        let sector_size = self.meta.sector_size as usize;
        if byte_offset % sector_size as u64 != 0 {
            return Err(io::Error::new(
                io::ErrorKind::Other,
                "Must read from sector aligned byte offset",
            ));
        }
        self.get
            .vmgs_read(byte_offset / sector_size as u64, buf, sector_size)
            .await
            .map_err(|e| {
                tracing::error!(error = &e as &dyn std::error::Error, "error reading block");
                io::Error::new(io::ErrorKind::Other, e)
            })?;

        Ok(())
    }

    async fn write_block(&mut self, byte_offset: u64, buf: &[u8]) -> io::Result<()> {
        let sector_size = self.meta.sector_size as usize;
        if byte_offset % sector_size as u64 != 0 {
            return Err(io::Error::new(
                io::ErrorKind::Other,
                "Must write from sector aligned byte offset",
            ));
        }

        self.get
            .vmgs_write(byte_offset / sector_size as u64, buf, sector_size)
            .await
            .map_err(|e| {
                tracing::error!(error = &e as &dyn std::error::Error, "error writing block");
                io::Error::new(io::ErrorKind::Other, e)
            })
    }

    async fn flush(&mut self) -> io::Result<()> {
        self.get.vmgs_flush().await.map_err(|e| {
            tracing::error!(error = &e as &dyn std::error::Error, "error sending flush");
            io::Error::new(io::ErrorKind::Other, e)
        })
    }

    fn meta(&self) -> BlockStorageMetadata {
        self.meta
    }

    fn validate_transfer_size(&self, max_trans_size_bytes: u32) -> io::Result<()> {
        if max_trans_size_bytes == 0
            || max_trans_size_bytes as usize > guest_emulation_transport::api::MAX_TRANSFER_SIZE
        {
            return Err(io::Error::new(io::ErrorKind::Other, format!(
                "Maximum transfer size bytes {} is either 0 or greater than the maximum transfer size.",
                max_trans_size_bytes,
            )));
        }

        Ok(())
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use guest_emulation_transport::api::ProtocolVersion;
    use guest_emulation_transport::test_utilities::new_transport_pair;
    use guest_emulation_transport::test_utilities::TestGet;
    use pal_async::async_test;
    use pal_async::task::Task;
    use pal_async::DefaultDriver;
    use vmgs::FileId;
    use vmgs::Vmgs;
    use vmgs_broker::spawn_vmgs_broker;
    use vmgs_broker::VmgsClient;

    async fn spawn_vmgs(driver: &DefaultDriver) -> (VmgsClient, TestGet, Task<()>) {
        let get = new_transport_pair(driver, None, ProtocolVersion::NICKEL_REV2).await;
        let vmgs_get = VmgsGet::new(get.client.clone()).await.unwrap();
        let vmgs = Vmgs::format_new(Box::new(vmgs_get), None).await.unwrap();
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
        let get = new_transport_pair(&driver, None, ProtocolVersion::NICKEL_REV2).await;
        let vmgs_get = VmgsGet::new(get.client.clone()).await.unwrap();
        let mut vmgs = Vmgs::format_new(Box::new(vmgs_get), None).await.unwrap();
        let file_id = FileId::BIOS_NVRAM;
        let encryption_key = vec![1; 32];

        vmgs.add_new_encryption_key(&encryption_key, vmgs::EncryptionAlgorithm::AES_GCM)
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

        let vmgs_get = VmgsGet::new(get.client.clone()).await.unwrap();
        let mut vmgs = Vmgs::open(Box::new(vmgs_get), None).await.unwrap();

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
