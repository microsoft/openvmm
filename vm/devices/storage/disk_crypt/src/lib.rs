// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! A disk device wrapper that provides confidentiality (but not authentication)
//! via encryption.

#![forbid(unsafe_code)]

pub mod resolver;

use block_crypto::XtsAes256;
use disk_backend::Disk;
use disk_backend::DiskError;
use disk_backend::DiskIo;
use disk_backend::UnmapBehavior;
use guestmem::GuestMemory;
use guestmem::MemoryRead;
use guestmem::MemoryWrite;
use inspect::Inspect;
use scsi_buffers::OwnedRequestBuffers;
use scsi_buffers::RequestBuffers;
use thiserror::Error;

/// An encrypted disk.
#[derive(Inspect)]
pub struct CryptDisk {
    inner: Disk,
    #[inspect(skip)]
    cipher: XtsAes256,
}

/// An error that occurred while creating a new encrypted disk.
#[derive(Debug, Error)]
pub enum NewDiskError {
    /// An error occurred during cryptographic operations.
    #[error("crypto error")]
    Crypto(#[source] block_crypto::Error),
    /// The key size is invalid.
    #[error("invalid key size for cipher")]
    InvalidKeySize,
}

impl CryptDisk {
    /// Creates a new encrypted disk device wrapping `inner`, using the provided
    /// cipher and key.
    pub fn new(
        cipher: disk_crypt_resources::Cipher,
        key: &[u8],
        inner: Disk,
    ) -> Result<Self, NewDiskError> {
        match cipher {
            disk_crypt_resources::Cipher::XtsAes256 => {}
        }
        let cipher = XtsAes256::new(
            key.try_into().map_err(|_| NewDiskError::InvalidKeySize)?,
            inner.sector_size(),
        )
        .map_err(NewDiskError::Crypto)?;
        Ok(Self { inner, cipher })
    }
}

impl DiskIo for CryptDisk {
    fn disk_type(&self) -> &str {
        "crypt"
    }

    fn sector_count(&self) -> u64 {
        self.inner.sector_count()
    }

    fn sector_size(&self) -> u32 {
        self.inner.sector_size()
    }

    fn disk_id(&self) -> Option<[u8; 16]> {
        self.inner.disk_id()
    }

    fn physical_sector_size(&self) -> u32 {
        self.inner.physical_sector_size()
    }

    fn is_fua_respected(&self) -> bool {
        self.inner.is_fua_respected()
    }

    fn is_read_only(&self) -> bool {
        self.inner.is_read_only()
    }

    /// Optionally returns a trait object to issue persistent reservation
    /// requests.
    fn pr(&self) -> Option<&dyn disk_backend::pr::PersistentReservation> {
        self.inner.pr()
    }

    async fn read_vectored(
        &self,
        buffers: &RequestBuffers<'_>,
        sector: u64,
    ) -> Result<(), DiskError> {
        // Read the encrypted data into the guest buffer. There is no harm
        // in letting the guest transiently see the encrypted data.
        self.inner.read_vectored(buffers, sector).await?;

        // Decrypt the data a sector at a time.
        let mut ctx = self.cipher.decrypt().map_err(crypto_error)?;
        let mut buf = vec![0; self.sector_size() as usize];
        let mut reader = buffers.reader();
        let mut writer = buffers.writer();
        for i in 0..buffers.len() >> self.inner.sector_shift() {
            reader.read(&mut buf)?;
            ctx.cipher((sector + i as u64).into(), &mut buf)
                .map_err(crypto_error)?;
            writer.write(&buf)?;
        }
        Ok(())
    }

    async fn write_vectored(
        &self,
        buffers: &RequestBuffers<'_>,
        sector: u64,
        fua: bool,
    ) -> Result<(), DiskError> {
        // Allocate a buffer to stage the encrypted data, since we cannot
        // modify the guest buffer or rely on it being stable.
        //
        // TODO: use a pool with a maximum size, or consider using memory
        // from the global bounce buffer (which could be pre-pinned to avoid
        // extra copies).
        let mut mem = GuestMemory::allocate(buffers.len());
        let buf = mem.inner_buf_mut().unwrap();
        let staged = OwnedRequestBuffers::linear(0, buffers.len(), true);

        // Encrypt the data a sector at a time.
        let mut ctx = self.cipher.encrypt().map_err(crypto_error)?;
        let mut reader = buffers.reader();
        let sector_size = self.inner.sector_size() as usize;
        let mut offset = 0;
        let mut tweak = sector;
        while offset < buffers.len() {
            let this_buf = &mut buf[offset..][..sector_size];
            reader.read(this_buf)?;
            ctx.cipher(tweak.into(), this_buf).map_err(crypto_error)?;
            offset += sector_size;
            tweak += 1;
        }

        // Write the encrypted data.
        self.inner
            .write_vectored(&staged.buffer(&mem), sector, fua)
            .await?;
        Ok(())
    }

    async fn sync_cache(&self) -> Result<(), DiskError> {
        self.inner.sync_cache().await
    }

    /// Waits for the disk sector size to be different than the specified value.
    async fn wait_resize(&self, sector_count: u64) -> u64 {
        self.inner.wait_resize(sector_count).await
    }

    fn unmap(
        &self,
        sector: u64,
        count: u64,
        block_level_only: bool,
    ) -> impl Future<Output = Result<(), DiskError>> + Send {
        self.inner.unmap(sector, count, block_level_only)
    }

    fn unmap_behavior(&self) -> UnmapBehavior {
        match self.inner.unmap_behavior() {
            // Even if the inner disk zeroes on unmap, the decrypted view of
            // those zeroes will be random data.
            UnmapBehavior::Unspecified | UnmapBehavior::Zeroes => UnmapBehavior::Unspecified,
            UnmapBehavior::Ignored => UnmapBehavior::Ignored,
        }
    }

    fn optimal_unmap_sectors(&self) -> u32 {
        self.inner.optimal_unmap_sectors()
    }
}

fn crypto_error(err: block_crypto::Error) -> DiskError {
    DiskError::Io(std::io::Error::other(err))
}

#[cfg(test)]
mod tests {
    use crate::CryptDisk;
    use disk_backend::Disk;
    use guestmem::GuestMemory;
    use pal_async::async_test;
    use scsi_buffers::OwnedRequestBuffers;

    #[async_test]
    async fn test_basic_read_write() {
        let key = [[0u8; 32], [1; 32]];
        let disk = CryptDisk::new(
            disk_crypt_resources::Cipher::XtsAes256,
            key.as_flattened(),
            disklayer_ram::ram_disk(0x200000, false).unwrap(),
        )
        .unwrap();
        let disk = Disk::new(disk).unwrap();
        let buffers = OwnedRequestBuffers::linear(0, 0x10000, true);
        let mut mem = GuestMemory::allocate(0x10000);
        let pattern = {
            let mut acc = 3u32;
            (0..0x10000)
                .map(|_| {
                    acc = acc.wrapping_mul(7);
                    acc as u8
                })
                .collect::<Vec<_>>()
        };
        mem.inner_buf_mut().unwrap().copy_from_slice(&pattern);
        disk.write_vectored(&buffers.buffer(&mem), 10, false)
            .await
            .unwrap();
        mem.inner_buf_mut().unwrap().fill(0);
        disk.read_vectored(&buffers.buffer(&mem), 10).await.unwrap();
        assert_eq!(mem.inner_buf_mut().unwrap(), &pattern);
    }
}
