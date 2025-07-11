// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Simple transfer protocol with support for seeks.
//!
//! The forward seeks beyond the size of the file might be platform dependendant.
//! Normally, the OS extends the file thus providing a way to truncate the file
//! to the desired size fast.
//!
//! Each transfer starts with the header packetthat provides the seek position,
//! size and the dierction. It might be followed by the data bytes sent in the
//! either direction. The sentinel value of `-1i128` for the header designates the
//! end of the file. Any transfer after that will return an error.

use bitfield_struct::bitfield;
use futures::AsyncReadExt;
use futures::AsyncWriteExt;
use open_enum::open_enum;
use std::io::Read;
use std::io::Seek;
use std::io::Write;
use zerocopy::FromBytes;
use zerocopy::Immutable;
use zerocopy::IntoBytes;

open_enum! {
    ///! Represents the position from which to seek in the medium.
    #[derive(IntoBytes, FromBytes, Immutable)]
    pub(crate) enum SeekPosition: u8 {
        ///! Seek relative to the current position.
        CURRENT = 0b00,
        ///! Seek relative to the start of the medium.
        START = 0b01,
        ///! Seek relative to the end of the medium.
        END = 0b10,
    }
}

impl SeekPosition {
    const fn into_bits(self) -> u8 {
        self.0
    }

    const fn from_bits(bits: u8) -> Self {
        Self(bits)
    }
}

open_enum! {
    ///! Represents the direction of the data transfer.
    #[derive(IntoBytes, FromBytes, Immutable)]
    pub(crate) enum HostFileOperation: u8 {
        ///! Write data to the medium.
        WRITE = 0,
        ///! Read data from the medium.
        READ = 1,
    }
}

impl HostFileOperation {
    const fn into_bits(self) -> u8 {
        self.0
    }

    const fn from_bits(bits: u8) -> Self {
        Self(bits)
    }
}

///! Represents the transport header.
#[derive(IntoBytes, FromBytes, Immutable)]
#[bitfield(u128)]
pub(crate) struct TransportHeader {
    #[bits(62)]
    pub seek_amount: i64,
    #[bits(2)]
    pub seek_pos: SeekPosition,
    pub data_size: u32,
    #[bits(1)]
    pub direction: HostFileOperation,
    #[bits(31)]
    _reserved1: u32,
}

impl TransportHeader {
    ///! Checks if the header represents the end of the file.
    pub fn is_eof(&self) -> bool {
        self.into_bits() == Self::eof().into_bits()
    }

    ///! Creates the end of file header.
    pub fn eof() -> Self {
        Self::from_bits(-1i128 as u128)
    }

    ///! Checks if the header requests flushing the medium.
    pub fn is_flush(&self) -> bool {
        self.into_bits() == Self::flush().into_bits()
    }

    ///! Creates a new transport header to request flushing the medium.
    pub fn flush() -> Self {
        Self::from_bits(0)
    }
}

///! Errors that can occur during host data operations.
#[derive(Debug, thiserror::Error)]
pub enum HostFileError {
    ///! End of file reached.
    #[error("end of file reached")]
    EndOfFile,
    ///! Invalid seek position specified in the header.
    #[error("invalid seek position specified in the header")]
    InvalidSeekPosition,
    ///! Invalid data size specified in the header.
    #[error("invalid data size specified in the header")]
    InvalidDataSize,
    ///! Write limit exceeded.
    #[error("write limit exceeded")]
    WriteLimitExceeded,
    ///! Invalid direction specified in the header.
    #[error("invalid direction specified in the header")]
    InvalidDirection,
    ///! An I/O error occurred during the operation.
    #[error("I/O error occurred during the operation")]
    IoError(#[source] std::io::Error),
}

///! Provides input for the host data operations.
pub(crate) enum HostData<'a> {
    ///! Data to be written to the medium.
    Write(&'a [u8]),
    ///! Buffer to read data into from the medium.
    Read(&'a mut [u8]),
}

///! Represents the write limit for the data storage.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WriteLimit {
    ///! No write limit.
    None,
    ///! A specific write limit in bytes.
    Limit(usize),
}

impl Into<usize> for WriteLimit {
    fn into(self) -> usize {
        match self {
            WriteLimit::None => usize::MAX,
            WriteLimit::Limit(limit) => limit,
        }
    }
}

impl Into<Option<usize>> for WriteLimit {
    fn into(self) -> Option<usize> {
        match self {
            WriteLimit::None => None,
            WriteLimit::Limit(limit) => Some(limit),
        }
    }
}

impl From<Option<usize>> for WriteLimit {
    fn from(limit: Option<usize>) -> Self {
        match limit {
            Some(size) => WriteLimit::Limit(size),
            None => WriteLimit::None,
        }
    }
}

///! A storage medium for the data operations.
pub struct HostFileStorage<M: Read + Write + Seek> {
    medium: M,
    write_limit: WriteLimit,
    bytes_read: usize,
    bytes_written: usize,
    eof: bool,
}

impl<M: Read + Write + Seek> HostFileStorage<M> {
    ///! Creates a new `HostFileStorage` with the given medium.
    pub fn new(medium: M, write_limit: WriteLimit) -> Self {
        Self {
            medium,
            write_limit,
            bytes_read: 0,
            bytes_written: 0,
            eof: false,
        }
    }

    ///! Returns the number of bytes read so far.
    pub fn bytes_read(&self) -> usize {
        self.bytes_read
    }

    ///! Returns the number of bytes written so far.
    pub fn bytes_written(&self) -> usize {
        self.bytes_written
    }

    ///! Performs a data transfer with the given header and data.
    ///! Returns the number of bytes transferred or an error if the operation failed.
    pub(crate) fn transfer(
        &mut self,
        header: TransportHeader,
        data: HostData<'_>,
    ) -> Result<usize, HostFileError> {
        if self.eof {
            tracing::debug!("Transfer requested after EOF reached");
            return Err(HostFileError::EndOfFile);
        }

        if header.is_eof() {
            tracing::debug!("End of file header received");
            self.eof = true;
            return Err(HostFileError::EndOfFile);
        } else if header.is_flush() {
            tracing::debug!("Flush header received");
            self.medium.flush().map_err(HostFileError::IoError)?;
            return Ok(0);
        }

        tracing::debug!("Transfer header {header:?}");

        if let WriteLimit::Limit(limit) = self.write_limit {
            if self.bytes_written + header.data_size() as usize > limit {
                tracing::debug!(
                    "Write limit exceeded: {} bytes written, limit is {} bytes",
                    self.bytes_written,
                    limit
                );
                return Err(HostFileError::WriteLimitExceeded);
            }
        }

        let seek_amount = header.seek_amount();
        if seek_amount != 0 {
            let seek_from = match header.seek_pos() {
                SeekPosition::CURRENT => std::io::SeekFrom::Current(seek_amount as i64),
                SeekPosition::START => std::io::SeekFrom::Start(seek_amount as u64),
                SeekPosition::END => std::io::SeekFrom::End(seek_amount as i64),
                _ => return Err(HostFileError::InvalidSeekPosition),
            };

            self.medium
                .seek(seek_from)
                .map_err(HostFileError::IoError)?;
        }

        let bytes_transferred = match header.direction() {
            HostFileOperation::WRITE => {
                if let HostData::Write(bytes_to_write) = data {
                    if bytes_to_write.len() != header.data_size() as usize {
                        return Err(HostFileError::InvalidDataSize);
                    }

                    self.medium
                        .write_all(bytes_to_write)
                        .map_err(HostFileError::IoError)?;
                    self.bytes_written += bytes_to_write.len();

                    bytes_to_write.len()
                } else {
                    return Err(HostFileError::InvalidDirection);
                }
            }
            HostFileOperation::READ => {
                if let HostData::Read(buffer) = data {
                    if buffer.len() != header.data_size() as usize {
                        return Err(HostFileError::InvalidDataSize);
                    }

                    let bytes_read = self.medium.read(buffer).map_err(HostFileError::IoError)?;
                    self.bytes_read += bytes_read;

                    bytes_read
                } else {
                    return Err(HostFileError::InvalidDirection);
                }
            }
            _ => return Err(HostFileError::InvalidDirection),
        };

        tracing::debug!(
            "Transfer completed: {} bytes transferred, totals: {} bytes written, {} bytes read",
            bytes_transferred,
            self.bytes_written,
            self.bytes_read
        );

        Ok(bytes_transferred)
    }

    ///! Runs the data operations on the provided transport.
    pub fn run<T: Read + Write>(&mut self, mut transport: T) -> Result<(), HostFileError> {
        if self.eof {
            return Err(HostFileError::EndOfFile);
        }

        loop {
            let mut header = TransportHeader::eof();
            transport
                .read_exact(header.as_mut_bytes())
                .map_err(HostFileError::IoError)?;

            if header.is_eof() {
                self.eof = true;
                return Ok(());
            }

            let mut buf = vec![0; header.data_size() as usize];
            match header.direction() {
                HostFileOperation::READ => {
                    let bytes_read = self.transfer(header, HostData::Read(&mut buf))?;
                    transport
                        .write_all((bytes_read as u128).as_bytes())
                        .map_err(HostFileError::IoError)?;
                    transport
                        .write_all(&buf[..bytes_read])
                        .map_err(HostFileError::IoError)?;
                }
                HostFileOperation::WRITE => {
                    transport
                        .read_exact(&mut buf)
                        .map_err(HostFileError::IoError)?;
                    self.transfer(header, HostData::Write(&buf))?;
                }
                _ => return Err(HostFileError::InvalidDirection),
            }
        }
    }

    ///! Runs the data operations asynchronously on the provided transport.
    pub async fn run_async<T: AsyncReadExt + AsyncWriteExt + Unpin>(
        &mut self,
        mut transport: T,
    ) -> Result<(), HostFileError> {
        if self.eof {
            return Err(HostFileError::EndOfFile);
        }

        loop {
            let mut header = TransportHeader::eof();
            transport
                .read_exact(header.as_mut_bytes())
                .await
                .map_err(HostFileError::IoError)?;

            if header.is_eof() {
                self.eof = true;
                return Ok(());
            }

            let mut buf = vec![0; header.data_size() as usize];
            match header.direction() {
                HostFileOperation::READ => {
                    let bytes_read = self.transfer(header, HostData::Read(&mut buf))?;
                    transport
                        .write_all((bytes_read as u128).as_bytes())
                        .await
                        .map_err(HostFileError::IoError)?;
                    transport
                        .write_all(&buf[..bytes_read])
                        .await
                        .map_err(HostFileError::IoError)?;
                }
                HostFileOperation::WRITE => {
                    transport
                        .read_exact(&mut buf)
                        .await
                        .map_err(HostFileError::IoError)?;
                    self.transfer(header, HostData::Write(&buf))?;
                }
                _ => return Err(HostFileError::InvalidDirection),
            }
        }
    }
}

///! A wrapper around a transport that provides file-like access.
pub struct HostFileAccess<T: Read + Write> {
    transport: T,
}

impl<T: Read + Write> HostFileAccess<T> {
    ///! Creates a new `HostFileAccess` with the given transport.
    pub fn new(transport: T) -> Self {
        Self { transport }
    }
}

impl<T: Read + Write> Drop for HostFileAccess<T> {
    fn drop(&mut self) {
        let header = TransportHeader::eof();
        if let Err(e) = self.transport.write_all(header.as_bytes()) {
            tracing::error!("Failed to write EOF header: {}", e);
        }
    }
}

impl<T: Read + Write> Write for HostFileAccess<T> {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        tracing::debug!("Writing {} bytes to host file access", buf.len());

        let header = TransportHeader::new()
            .with_seek_amount(0)
            .with_seek_pos(SeekPosition::START)
            .with_data_size(buf.len() as u32)
            .with_direction(HostFileOperation::WRITE);

        self.transport
            .write_all(header.as_bytes())
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e))?;
        self.transport
            .write_all(buf)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e))?;
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        tracing::debug!("Flushing host file access");

        let header = TransportHeader::flush();
        self.transport
            .write_all(header.as_bytes())
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e))?;
        Ok(())
    }
}

impl<T: Read + Write> Read for HostFileAccess<T> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        tracing::debug!("Reading {} bytes from host file access", buf.len());

        let header = TransportHeader::new()
            .with_seek_amount(0)
            .with_seek_pos(SeekPosition::START)
            .with_data_size(buf.len() as u32)
            .with_direction(HostFileOperation::READ);

        self.transport
            .write_all(header.as_bytes())
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e))?;

        let mut size_header = [0; 16];
        self.transport
            .read_exact(&mut size_header)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e))?;
        let size = u128::from_le_bytes(size_header) as usize;

        tracing::debug!("{} bytes available", size);

        if size == 0 {
            tracing::debug!("End of file reached");
            return Ok(0);
        }
        if size > buf.len() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "Buffer too small for read operation",
            ));
        }

        self.transport
            .read_exact(buf[..size].as_mut())
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e))?;
        Ok(size)
    }
}

impl<T: Read + Write> Seek for HostFileAccess<T> {
    fn seek(&mut self, pos: std::io::SeekFrom) -> std::io::Result<u64> {
        tracing::debug!("Seeking in host file access to {:?}", pos);

        let seek_amount = match pos {
            std::io::SeekFrom::Start(offset) => offset as i64,
            std::io::SeekFrom::End(offset) => offset as i64,
            std::io::SeekFrom::Current(offset) => offset as i64,
        };

        let header = TransportHeader::new()
            .with_seek_amount(seek_amount)
            .with_seek_pos(SeekPosition::START)
            .with_data_size(0)
            .with_direction(HostFileOperation::READ);

        self.transport
            .write_all(header.as_bytes())
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e))?;
        Ok(seek_amount as u64)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    #[test]
    fn test_host_data_storage() {
        // Write data to the storage
        let data = b"Hello, world!";
        let cursor = Cursor::new(Vec::new());
        let mut storage = HostFileStorage::new(cursor, WriteLimit::None);
        let header = TransportHeader::new()
            .with_seek_amount(0)
            .with_seek_pos(SeekPosition::START)
            .with_data_size(data.len() as u32)
            .with_direction(HostFileOperation::WRITE);
        let result = storage.transfer(header, HostData::Write(data));
        assert!(result.is_ok());
        assert_eq!(storage.bytes_written(), data.len());
        assert_eq!(storage.bytes_read(), 0);
        assert!(!storage.eof);

        // Read data from the storage
        let mut buffer = vec![0; data.len()];
        let header = TransportHeader::new()
            .with_seek_amount(0)
            .with_seek_pos(SeekPosition::START)
            .with_data_size(data.len() as u32)
            .with_direction(HostFileOperation::READ);
        let result = storage.transfer(header, HostData::Read(&mut buffer));
        assert!(result.is_ok());
        assert_eq!(buffer, data);
        assert_eq!(storage.bytes_written(), data.len());
        assert_eq!(storage.bytes_read(), data.len());
        assert!(!storage.eof);

        // Test end of file
        let eof_header = TransportHeader::eof();
        let result = storage.transfer(eof_header, HostData::Write(&[]));
        assert!(matches!(result, Err(HostFileError::EndOfFile)));
    }
}
