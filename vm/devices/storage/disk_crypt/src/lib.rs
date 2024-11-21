// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! A disk device wrapper that provides confidentiality (but not authentication)
//! via encryption.

#![warn(missing_docs)]

pub mod resolver;

use block_crypto::AesXts256;
use disk_backend::AsyncDisk;
use disk_backend::DiskError;
use disk_backend::SimpleDisk;
use disk_backend::ASYNC_DISK_STACK_SIZE;
use guestmem::GuestMemory;
use guestmem::MemoryRead;
use guestmem::MemoryWrite;
use inspect::Inspect;
use scsi_buffers::OwnedRequestBuffers;
use scsi_buffers::RequestBuffers;
use stackfuture::StackFuture;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use thiserror::Error;

/// An encrypted disk.
#[derive(Inspect)]
pub struct CryptDisk {
    inner: Arc<dyn SimpleDisk>,
    #[inspect(skip)]
    cipher: AesXts256,
    sector_shift: u32,
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
        inner: Arc<dyn SimpleDisk>,
    ) -> Result<Self, NewDiskError> {
        match cipher {
            disk_crypt_resources::Cipher::AesXts256 => {}
        }
        let cipher = AesXts256::new(
            key.try_into().map_err(|_| NewDiskError::InvalidKeySize)?,
            inner.sector_size(),
        )
        .map_err(NewDiskError::Crypto)?;
        let sector_shift = inner.sector_size().trailing_zeros();
        Ok(Self {
            inner,
            cipher,
            sector_shift,
        })
    }
}

impl SimpleDisk for CryptDisk {
    fn disk_type(&self) -> &str {
        "crypt"
    }

    fn sector_count(&self) -> u64 {
        self.inner.sector_count()
    }

    fn sector_size(&self) -> u32 {
        1 << self.sector_shift
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

    /// Optionally returns a trait object to issue unmap (trim/discard)
    /// requests.
    fn unmap(&self) -> Option<&dyn disk_backend::Unmap> {
        self.inner.unmap()
    }

    /// Optionally returns a trait object to issue get LBA status requests.
    fn lba_status(&self) -> Option<&dyn disk_backend::GetLbaStatus> {
        self.inner.lba_status()
    }

    /// Optionally returns a trait object to issue persistent reservation
    /// requests.
    fn pr(&self) -> Option<&dyn disk_backend::pr::PersistentReservation> {
        self.inner.pr()
    }
}

fn crypto_error(err: block_crypto::Error) -> DiskError {
    DiskError::Io(std::io::Error::new(std::io::ErrorKind::Other, err))
}

impl AsyncDisk for CryptDisk {
    fn read_vectored<'a>(
        &'a self,
        buffers: &'a RequestBuffers<'a>,
        sector: u64,
    ) -> StackFuture<'a, Result<(), DiskError>, { ASYNC_DISK_STACK_SIZE }> {
        StackFuture::from_or_box(async move {
            // Read the encrypted data into the guest buffer. There is no harm
            // in letting the guest transiently see the encrypted data.
            self.inner.read_vectored(buffers, sector).await?;

            // Decrypt the data a sector at a time.
            let mut ctx = self.cipher.decrypt().map_err(crypto_error)?;
            let mut buf = vec![0; self.sector_size() as usize];
            let mut reader = buffers.reader();
            let mut writer = buffers.writer();
            for i in 0..buffers.len() >> self.sector_shift {
                reader.read(&mut buf)?;
                ctx.cipher((sector + i as u64).into(), &mut buf)
                    .map_err(crypto_error)?;
                writer.write(&buf)?;
            }
            Ok(())
        })
    }

    fn write_vectored<'a>(
        &'a self,
        buffers: &'a RequestBuffers<'a>,
        sector: u64,
        fua: bool,
    ) -> StackFuture<'a, Result<(), DiskError>, { ASYNC_DISK_STACK_SIZE }> {
        StackFuture::from_or_box(async move {
            // Allocate a buffer to stage the encrypted data, since we cannot
            // modify the guest buffer or rely on it being stable.
            //
            // TODO: use a pool with a maximum size, or consider using memory
            // from the global bounce buffer (which could be pre-pinned to avoid
            // extra copies).
            let mem = GuestMemory::allocate(buffers.len());
            let staged = OwnedRequestBuffers::linear(0, buffers.len(), true);
            let staged = staged.buffer(&mem);

            // Encrypt the data a sector at a time.
            let mut ctx = self.cipher.encrypt().map_err(crypto_error)?;
            let mut reader = buffers.reader();
            let mut writer = staged.writer();
            let mut buf = vec![0; self.sector_size() as usize];
            for i in 0..buffers.len() >> self.sector_shift {
                reader.read(&mut buf)?;
                ctx.cipher((sector + i as u64).into(), &mut buf)
                    .map_err(crypto_error)?;
                writer.write(&buf)?;
            }

            // Write the encrypted data.
            self.inner.write_vectored(&staged, sector, fua).await?;
            Ok(())
        })
    }

    fn sync_cache(&self) -> StackFuture<'_, Result<(), DiskError>, { ASYNC_DISK_STACK_SIZE }> {
        self.inner.sync_cache()
    }

    /// Waits for the disk sector size to be different than the specified value.
    fn wait_resize<'a>(
        &'a self,
        sector_count: u64,
    ) -> Pin<Box<dyn 'a + Send + Future<Output = u64>>> {
        self.inner.wait_resize(sector_count)
    }
}
