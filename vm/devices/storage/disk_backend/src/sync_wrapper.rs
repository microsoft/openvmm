// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! A wrapper around [`Disk`] that adapts the trait for use with
//! synchronous [`std::io`] traits (such as `Read`, `Write`, `Seek`, etc...).
//!
//! NOTE: this is _not_ code that should see wide use across the OpenVMM
//! codebase! It was written to support a very-specific use-case: leveraging
//! existing, synchronous, Rust/C library code that reformats/repartitions
//! drives.
//!
//! The fact that this adapter exists should be considered a implementation
//! wart, and it would be great if we could swap out any dependant code with
//! native-async implementations at some point in the future.

use crate::Disk;
use futures::executor::block_on;
use guestmem::GuestMemory;
use scsi_buffers::OwnedRequestBuffers;
use std::io;
use std::io::Read;
use std::io::Seek;
use std::io::SeekFrom;
use std::io::Write;

/// Wrapper around [`Disk`] that implements the synchronous [`std::io`]
/// traits (such as `Read`, `Write`, `Seek`, etc...) using [`block_on`].
pub struct BlockingDisk {
    /// Inner disk instance for base operations.
    inner: Disk,
    /// The current position in the disk.
    pos: u64,
    /// Buffer for temporary data storage during read/write operations.
    buffer: Vec<u8>,
    /// A flag to indicate whether the buffer has been modified (true) or not (false).
    buffer_dirty: bool,
}

impl BlockingDisk {
    /// Create a new blocking disk wrapping `inner`.
    pub fn new(inner: Disk) -> Self {
        let sector_size = inner.sector_size();
        BlockingDisk {
            inner,
            pos: 0,
            buffer: vec![0; sector_size as usize],
            buffer_dirty: false,
        }
    }

    /// Fetches data from the disk into the buffer, flushing the buffer if it is dirty.
    async fn fetch(&mut self) -> io::Result<()> {
        if self.buffer_dirty {
            block_on(self.flush())?;
        }
        let guest_mem = GuestMemory::allocate(self.inner.sector_size() as usize);
        let read_buffers = OwnedRequestBuffers::linear(0, self.inner.sector_size() as usize, true);
        let binding = read_buffers.buffer(&guest_mem);
        let result = self
            .inner
            .read_vectored(&binding, self.pos / self.inner.sector_size() as u64)
            .await;
        guest_mem
            .read_at(0, &mut self.buffer)
            .map_err(|e| io::Error::other(format!("Fetch error: {}", e)))?;
        result.map_err(|e| io::Error::other(format!("Fetch error: {}", e)))
    }

    /// Writes the buffer to the disk if it is dirty.
    async fn flush(&mut self) -> io::Result<()> {
        if self.buffer_dirty {
            let guest_mem = GuestMemory::allocate(self.inner.sector_size() as usize);
            guest_mem.write_at(0, &self.buffer).unwrap();
            let write_buffers =
                OwnedRequestBuffers::linear(0, self.inner.sector_size() as usize, true);
            let binding = write_buffers.buffer(&guest_mem);
            let future = self.inner.write_vectored(
                &binding,
                self.pos / self.inner.sector_size() as u64,
                true,
            );
            let result = future.await;
            self.buffer_dirty = false;
            result.map_err(|e| io::Error::other(format!("Fetch error: {}", e)))
        } else {
            Ok(())
        }
    }

    /// Reads data from the disk into the provided buffer.
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        // If the buffer size is a multiple of sector size and the buffer is not dirty
        // use the read_full_sector method
        if buf.len().is_multiple_of(self.inner.sector_size() as usize) && !self.buffer_dirty {
            return self.read_full_sector(buf);
        }
        // Buffer size is not multiple of sector size
        let mut total_bytes_read = 0;
        let mut remaining = buf.len();
        if self.buffer_dirty {
            block_on(self.flush())?;
        }
        while remaining > 0 {
            block_on(self.fetch())?;
            let offset = (self.pos % self.inner.sector_size() as u64) as usize;
            let bytes_to_copy =
                std::cmp::min(remaining, self.inner.sector_size() as usize - offset);
            buf[total_bytes_read..total_bytes_read + bytes_to_copy]
                .copy_from_slice(&self.buffer[offset..offset + bytes_to_copy]);
            self.pos += bytes_to_copy as u64;
            total_bytes_read += bytes_to_copy;
            remaining -= bytes_to_copy;
            if remaining > 0 && offset + bytes_to_copy == self.inner.sector_size() as usize {
                // Reached the end of a sector, fetch the next sector on the next read
                block_on(self.fetch())?;
            }
        }
        Ok(total_bytes_read)
    }

    /// Writes data from the provided buffer to the disk.
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        // If the buffer size is a multiple of sector size and the buffer is not dirty
        // use the write_full_sector method
        if buf.len().is_multiple_of(self.inner.sector_size() as usize) && !self.buffer_dirty {
            return self.write_full_sector(buf);
        }
        // Buffer size is not multiple of sector size
        let mut total_bytes_written = 0;
        let mut remaining = buf.len();
        while remaining > 0 {
            let offset = (self.pos % self.inner.sector_size() as u64) as usize;
            let bytes_to_copy =
                std::cmp::min(remaining, self.inner.sector_size() as usize - offset);
            if self.buffer_dirty {
                block_on(self.flush())?;
            } else if bytes_to_copy < self.inner.sector_size() as usize {
                // Fetch the current sector if we are not writing a full sector
                block_on(self.fetch())?;
            }
            self.buffer[offset..offset + bytes_to_copy]
                .copy_from_slice(&buf[total_bytes_written..total_bytes_written + bytes_to_copy]);
            self.buffer_dirty = true;
            // Reached the end of a sector, flush the buffer
            if offset + bytes_to_copy == self.inner.sector_size() as usize {
                block_on(self.flush())?;
            }
            self.pos += bytes_to_copy as u64;
            total_bytes_written += bytes_to_copy;
            remaining -= bytes_to_copy;
        }
        Ok(total_bytes_written)
    }

    /// Adjusts the current position in the disk.
    fn seek(&mut self, pos: SeekFrom) -> io::Result<u64> {
        if self.buffer_dirty {
            block_on(self.flush())?;
        }
        let new_pos = match pos {
            SeekFrom::Start(offset) => offset,
            SeekFrom::Current(offset) => self.pos.wrapping_add(offset as u64),
            SeekFrom::End(offset) => {
                let end =
                    self.inner.sector_count() as i64 * self.inner.sector_size() as i64 + offset;
                end.try_into().unwrap()
            }
        };
        self.pos = new_pos;
        Ok(new_pos)
    }

    /// Reads a full sector from the disk into the provided buffer.
    fn read_full_sector(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        assert_eq!(
            buf.len() % self.inner.sector_size() as usize,
            0,
            "Buffer size must be a multiple of sector size"
        );
        let guest_mem = GuestMemory::allocate(buf.len());
        let read_buffers = OwnedRequestBuffers::linear(0, buf.len(), true);
        let binding = read_buffers.buffer(&guest_mem);
        let future = self
            .inner
            .read_vectored(&binding, self.pos / self.inner.sector_size() as u64);
        block_on(future).map_err(|e| io::Error::other(format!("Read error: {}", e)))?;

        // Copy the data read from guest memory to the input buffer
        guest_mem
            .read_at(0, buf)
            .map_err(|e| io::Error::other(format!("Fetch error: {}", e)))?;
        // Update the position based on the bytes read
        self.pos += buf.len() as u64;
        Ok(buf.len())
    }

    /// Writes a full sector from the provided buffer to the disk.
    fn write_full_sector(&mut self, buf: &[u8]) -> io::Result<usize> {
        assert_eq!(
            buf.len() % self.inner.sector_size() as usize,
            0,
            "Buffer size must be a multiple of sector size"
        );
        let guest_mem = GuestMemory::allocate(buf.len());
        guest_mem.write_at(0, buf).unwrap();
        let write_buffers = OwnedRequestBuffers::linear(0, buf.len(), true);
        let binding = write_buffers.buffer(&guest_mem);
        let future =
            self.inner
                .write_vectored(&binding, self.pos / self.inner.sector_size() as u64, true);
        block_on(future).map_err(|e| io::Error::other(format!("Write error: {}", e)))?;
        // Update the position based on the bytes written
        self.pos += buf.len() as u64;
        Ok(buf.len())
    }
}

impl Read for BlockingDisk {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        self.read(buf)
    }
}

impl Write for BlockingDisk {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.write(buf)
    }

    fn flush(&mut self) -> io::Result<()> {
        block_on(self.flush())
    }
}

impl Seek for BlockingDisk {
    fn seek(&mut self, pos: SeekFrom) -> io::Result<u64> {
        self.seek(pos)
    }
}
