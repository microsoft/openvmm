// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Block device backed by a VHD-formatted file.

use crate::disk::BlockStorage;
use crate::disk::BlockStorageMetadata;
use async_trait::async_trait;
use guid::Guid;
use std::fs;
use std::io;
use std::io::Error;
use std::io::ErrorKind;
use std::io::Read;
use std::io::Seek;
use std::io::SeekFrom;
use std::io::Write;
use std::path::Path;
use vhd1_defs::VhdFooter;
use zerocopy::AsBytes;

const ONE_MEGA_BYTE: u64 = 1024 * 1024;
/// Default VMGS file size, 4 MB.
pub const VMGS_DEFAULT_FILE_SIZE: u64 = 4 * ONE_MEGA_BYTE;

/// Access mode for the file disk device.
#[derive(Copy, Clone)]
pub enum FileDiskFlag {
    /// Create a new readable/writable file with the specified length and overwrite_existing_file flag.
    Create {
        /// Size of file being created. If `None`, will default to [`VMGS_DEFAULT_FILE_SIZE`], 4 MB.
        file_size: Option<u64>,
        /// Force create VMGS file.
        force_create: bool,
    },
    /// Open file as read only.
    Read,
    /// Open file as read write.
    ReadWrite,
}

/// Block device backed by a VHD file.
#[derive(Debug)]
pub struct VhdFileDisk {
    file: fs::File,
    capacity_sectors: u64,
    capacity_bytes: u64,
    bytes_per_logical_sector: u32, // block_size size of block in the file, in bytes
    max_trans_size_bytes: u32,
}

/// VhdFileDisk sector size
pub const SECTOR_SIZE: u64 = 512;
const MAX_PAYLOAD_SIZE: u32 = 8192;

impl VhdFileDisk {
    /// Create a new VHD-formatted file-backed block device.
    ///
    /// Passing `FileDiskFlag::Create(0, _)` will create a file of length [`VMGS_DEFAULT_FILE_SIZE`].
    pub fn new(path: impl AsRef<Path>, flag: FileDiskFlag) -> io::Result<Self> {
        if let FileDiskFlag::Create {
            file_size: length,
            force_create,
        } = flag
        {
            let mut file = fs::OpenOptions::new()
                .read(true)
                .write(true)
                .create(true)
                .truncate(true)
                .create_new(!force_create)
                .open(path)?;

            let file_size = if let Some(size) = length {
                if size == 0 {
                    return Err(Error::new(ErrorKind::Unsupported, "file size cannot be 0"));
                } else if size % SECTOR_SIZE != 0 {
                    return Err(Error::new(
                        ErrorKind::Unsupported,
                        "file size must be multiple of 512",
                    ));
                }
                size
            } else {
                VMGS_DEFAULT_FILE_SIZE
            };

            file.set_len(file_size)?;
            file.seek(SeekFrom::End(0))?;
            file.write_all(VhdFooter::new_fixed(file_size, Guid::new_random()).as_bytes())?;

            Ok(VhdFileDisk {
                file,
                capacity_bytes: file_size,
                capacity_sectors: file_size / SECTOR_SIZE,
                bytes_per_logical_sector: SECTOR_SIZE as u32, // block_size size of block in the file, in bytes
                max_trans_size_bytes: MAX_PAYLOAD_SIZE,
            })
        } else {
            let read_only = matches!(flag, FileDiskFlag::Read);

            let file = fs::OpenOptions::new()
                .read(true)
                .write(!read_only)
                .open(path)?;

            let len = file.metadata()?.len();

            Ok(VhdFileDisk {
                file,
                capacity_bytes: len,
                capacity_sectors: len / SECTOR_SIZE,
                bytes_per_logical_sector: SECTOR_SIZE as u32, // block_size size of block in the file, in bytes
                max_trans_size_bytes: MAX_PAYLOAD_SIZE,
            })
        }
    }

    fn validate_read_write(&self, offset: u64, length: u64) -> io::Result<()> {
        if (offset % self.bytes_per_logical_sector as u64 != 0) || (offset > self.capacity_bytes) {
            return Err(invalid_data_err("Invalid offset"));
        }
        if length > self.capacity_bytes - offset {
            return Err(invalid_data_err("Invalid file length"));
        }
        Ok(())
    }

    /// Returns the length of the file
    pub fn len(&self) -> io::Result<u64> {
        Ok(self.file.metadata()?.len())
    }

    /// Returns a reference to the underlying file object
    pub fn get_file(&self) -> &fs::File {
        &self.file
    }
}

fn invalid_data_err(msg: &str) -> Error {
    Error::new(ErrorKind::InvalidData, msg.to_string())
}

fn round_up_count(count: usize, pow2: u32) -> u64 {
    (count as u64 + pow2 as u64 - 1) & !(pow2 as u64 - 1)
}

#[async_trait]
impl BlockStorage for VhdFileDisk {
    /// Reads bytes from the device at the specified offset. BlockStorage::Read()
    async fn read_block(&mut self, byte_offset: u64, buf: &mut [u8]) -> io::Result<()> {
        self.validate_read_write(byte_offset, buf.len() as u64)?;
        let mut buf_aligned = Vec::new();
        if buf.len() % self.bytes_per_logical_sector as usize != 0 {
            let sector_aligned_size = round_up_count(buf.len(), self.bytes_per_logical_sector);
            buf_aligned = vec![0_u8; sector_aligned_size as usize];
            let buf_len = buf.len();
            buf_aligned[0..buf_len].clone_from_slice(buf);
        }
        self.file.seek(SeekFrom::Start(byte_offset))?;
        if !buf_aligned.is_empty() {
            // NOTE: Temporarily allowing a blocking read, as there isn't a
            // great alternative atm. File I/O should be a leaf operation, so
            // there's no way to cause a deadlock. Will likely refactor once
            // storvsp has a better async story implemented, tracking with #33900303
            let bytes_read = self.file.read(&mut buf_aligned[..])?;
            if bytes_read != buf_aligned.len() {
                return Err(invalid_data_err("not enough room in buf"));
            }
            buf.clone_from_slice(&buf_aligned[0..buf.len()]);
        } else {
            // NOTE: Temporarily allowing a blocking read, as there isn't a
            // great alternative atm. File I/O should be a leaf operation, so
            // there's no way to cause a deadlock. Will likely refactor once
            // storvsp has a better async story implemented, tracking with #33900303
            let bytes_read = self.file.read(buf)?;
            if bytes_read != buf.len() {
                return Err(invalid_data_err("not enough room in buf"));
            }
        }
        Ok(())
    }

    /// Writes bytes to the device at the specified offset.
    async fn write_block(&mut self, byte_offset: u64, buf: &[u8]) -> io::Result<()> {
        self.validate_read_write(byte_offset, buf.len() as u64)?;
        let mut buf_aligned = Vec::new();
        if buf.len() % self.bytes_per_logical_sector as usize != 0 {
            let sector_aligned_size = round_up_count(buf.len(), self.bytes_per_logical_sector);
            buf_aligned = vec![0_u8; sector_aligned_size as usize];
            let buf_len = buf.len();
            buf_aligned[0..buf_len].clone_from_slice(buf);
        }
        self.file.seek(SeekFrom::Start(byte_offset))?;
        if !buf_aligned.is_empty() {
            // NOTE: Temporarily allowing a blocking read, as there isn't a
            // great alternative atm. File I/O should be a leaf operation, so
            // there's no way to cause a deadlock. Will likely refactor once
            // storvsp has a better async story implemented, tracking with #33900303
            let bytes_written = self.file.write(&buf_aligned[..])?;
            if bytes_written != buf_aligned.len() {
                return Err(invalid_data_err("invalid buffer size"));
            }
        } else {
            // NOTE: Temporarily allowing a blocking read, as there isn't a
            // great alternative atm. File I/O should be a leaf operation, so
            // there's no way to cause a deadlock. Will likely refactor once
            // storvsp has a better async story implemented, tracking with #33900303
            let bytes_written = self.file.write(buf)?;
            if bytes_written != buf.len() {
                return Err(invalid_data_err("invalid buffer size"));
            }
        }
        Ok(())
    }

    async fn flush(&mut self) -> io::Result<()> {
        // NOTE: Temporarily allowing a blocking read, as there isn't a
        // great alternative atm. File I/O should be a leaf operation, so
        // there's no way to cause a deadlock. Will likely refactor once
        // storvsp has a better async story implemented, tracking with #33900303
        self.file.sync_all()
    }

    fn meta(&self) -> BlockStorageMetadata {
        BlockStorageMetadata {
            capacity: self.capacity_bytes,
            logical_sector_size: self.bytes_per_logical_sector,
            sector_count: self.capacity_sectors,
            sector_size: self.bytes_per_logical_sector,
            physical_sector_size: 512,
            max_trans_size_bytes: self.max_trans_size_bytes,
        }
    }
}
