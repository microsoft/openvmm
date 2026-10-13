// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use guestmem::GuestMemory;
use std::cmp;
use std::io;
use std::io::Read;
use std::io::Write;
use virtio::VirtioQueueCallbackWork;
use virtio::queue::VirtioQueuePayload;

const READ_UNTIL_CHUNK_SIZE: usize = 4096;

/// An implementation of `Read` that allows reading data from a virtio payload that may use
/// multiple buffers.
pub struct VirtioPayloadReader<'payload, 'mem> {
    guest_memory: &'mem GuestMemory,
    payload: std::slice::Iter<'payload, VirtioQueuePayload>,
    current: Option<&'payload VirtioQueuePayload>,
    offset: usize,
    position: usize,
    len: usize,
}

impl<'payload, 'mem> VirtioPayloadReader<'payload, 'mem> {
    /// Create a new reader for the specified payload.
    pub fn new(guest_memory: &'mem GuestMemory, work: &'payload VirtioQueueCallbackWork) -> Self {
        let mut reader = Self {
            guest_memory,
            payload: work.payload.iter(),
            current: None,
            offset: 0,
            position: 0,
            len: work.get_payload_length(false) as usize,
        };

        reader.next_payload();
        reader
    }

    /// Skip ahead to the next readable payload.
    fn next_payload(&mut self) {
        // It would be nice to use filter instead when assigning to self.payload in the ctor, but
        // I can't figure out how to store the result of that (the type it returns is generic over
        // the callback type, and I don't want to use Box<dyn Iterator> if I can avoid it).
        self.current = self.payload.find(|p| !p.writeable);
    }

    /// Gets the current payload, or None if the end was reached.
    ///
    /// This function takes care of moving to the next payload buffer if the current offset matches
    /// the end.
    fn get_payload(&mut self) -> Option<&'payload VirtioQueuePayload> {
        let payload = self.current?;
        if self.offset == payload.length as usize {
            self.next_payload();
            self.offset = 0;
        }

        self.current
    }

    /// Gets the remaining length of the current payload buffer only.
    fn get_current_remaining_len(&mut self) -> usize {
        if let Some(payload) = self.get_payload() {
            (payload.length as usize).saturating_sub(self.offset)
        } else {
            0
        }
    }
}

impl Read for VirtioPayloadReader<'_, '_> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if let Some(payload) = self.get_payload() {
            // Determine how much space is left in the current buffer, and read at most that much.
            // A single call to read won't cross payload buffers, so to read more you must call
            // it repeatedly or e.g. use read_exact.
            let remaining = (payload.length as usize)
                .checked_sub(self.offset)
                .ok_or_else(|| io::Error::from(io::ErrorKind::InvalidData))?;
            let size = cmp::min(remaining, buf.len());
            let address = payload
                .address
                .checked_add(self.offset as u64)
                .ok_or_else(|| io::Error::from(io::ErrorKind::InvalidData))?;
            self.guest_memory
                .read_at(address, &mut buf[..size])
                .map_err(io::Error::other)?;
            self.offset += size;
            self.position += size;
            Ok(size)
        } else {
            Ok(0)
        }
    }
}

impl fuse::RequestReader for VirtioPayloadReader<'_, '_> {
    fn read_until(&mut self, byte: u8) -> lx::Result<Vec<u8>> {
        // Unlike with a simple slice, we can't just scan ahead for the desired byte. Instead,
        // repeatedly read some data and see if it contains the byte. It is expected that in most
        // cases, the string will be contained in the remainder of the current payload buffer,
        // in which case this loop only has one iteration.
        let mut buffer: Vec<u8> = Vec::new();
        let mut buffer_offset = 0;
        loop {
            // Bound each scan so a large descriptor with an early terminator does not require an
            // allocation for the entire descriptor.
            let len = self.get_current_remaining_len().min(READ_UNTIL_CHUNK_SIZE);
            if len == 0 {
                return Err(lx::Error::EINVAL);
            }

            let new_len = buffer.len().checked_add(len).ok_or(lx::Error::E2BIG)?;
            if new_len > crate::MAX_GUEST_BUFFER_SIZE {
                return Err(lx::Error::E2BIG);
            }
            buffer
                .try_reserve_exact(len)
                .map_err(|_| lx::Error::ENOMEM)?;
            buffer.resize(new_len, 0);
            let start_offset = self.offset;
            let start_position = self.position;
            if self.read(&mut buffer[buffer_offset..])? != len {
                return Err(lx::Error::EIO);
            }

            // Search for a matching byte in the portion of the buffer we just read.
            if let Some(length) = buffer[buffer_offset..].iter().position(|&c| c == byte) {
                // Return up to the matching byte.
                buffer.truncate(buffer_offset + length);

                // Rewind the offset to be just after the matching byte.
                self.offset = start_offset + length + 1;
                self.position = start_position + length + 1;
                return Ok(buffer);
            } else {
                buffer_offset += len;
            }
        }
    }

    fn read_count(&mut self, count: usize) -> lx::Result<Box<[u8]>> {
        if count > crate::MAX_GUEST_BUFFER_SIZE {
            return Err(lx::Error::E2BIG);
        }
        let mut buffer = crate::guest_buffer(count as u32)?;
        self.read_exact(&mut buffer)?;
        Ok(buffer.into_boxed_slice())
    }

    fn remaining_len(&self) -> usize {
        self.len.saturating_sub(self.position)
    }
}

/// An implementation of `Write` that allows writing data to virtio payload that may use multiple
/// buffers.
pub struct VirtioPayloadWriter<'payload, 'mem> {
    guest_memory: &'mem GuestMemory,
    payload: std::slice::Iter<'payload, VirtioQueuePayload>,
    current: Option<&'payload VirtioQueuePayload>,
    offset: usize,
}

impl<'payload, 'mem> VirtioPayloadWriter<'payload, 'mem> {
    /// Create a new writer for the specified work.
    pub fn new(guest_memory: &'mem GuestMemory, work: &'payload VirtioQueueCallbackWork) -> Self {
        let mut writer = Self {
            guest_memory,
            payload: work.payload.iter(),
            current: None,
            offset: 0,
        };

        writer.next_payload();
        writer
    }

    /// Skip ahead to the next writable payload buffer.
    fn next_payload(&mut self) {
        self.current = self.payload.find(|p| p.writeable);
    }

    /// Gets the current payload, or None if the end was reached.
    ///
    /// This function takes care of moving to the next payload buffer if the current offset matches
    /// the end.
    fn get_payload(&mut self) -> Option<&'payload VirtioQueuePayload> {
        let payload = self.current?;
        if self.offset == payload.length as usize {
            self.next_payload();
            self.offset = 0;
        }

        self.current
    }
}

impl Write for VirtioPayloadWriter<'_, '_> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        if let Some(payload) = self.get_payload() {
            // Find the remaining size of the current payload buffer, and write at most that much.
            // This method never writes data spanning multiple buffers, so to write more you must
            // call it repeatedly or use write_all.
            let remaining = (payload.length as usize)
                .checked_sub(self.offset)
                .ok_or_else(|| io::Error::from(io::ErrorKind::InvalidData))?;
            let size = cmp::min(remaining, buf.len());
            let address = payload
                .address
                .checked_add(self.offset as u64)
                .ok_or_else(|| io::Error::from(io::ErrorKind::InvalidData))?;
            self.guest_memory
                .write_at(address, &buf[..size])
                .map_err(io::Error::other)?;
            self.offset += size;
            Ok(size)
        } else {
            Ok(0)
        }
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use fuse::FuseOperation;
    use fuse::Request;
    use fuse::protocol::FUSE_SETXATTR;
    use fuse::protocol::FUSE_WRITE;
    use fuse::protocol::fuse_in_header;
    use fuse::protocol::fuse_setxattr_in;
    use fuse::protocol::fuse_write_in;
    use std::mem::size_of;
    use test_with_tracing::test;
    use zerocopy::IntoBytes;

    fn test_reader<'payload, 'mem>(
        mem: &'mem GuestMemory,
        payload: &'payload [VirtioQueuePayload],
    ) -> VirtioPayloadReader<'payload, 'mem> {
        let mut reader = VirtioPayloadReader {
            guest_memory: mem,
            payload: payload.iter(),
            current: None,
            offset: 0,
            position: 0,
            len: payload
                .iter()
                .filter(|entry| !entry.writeable)
                .map(|entry| entry.length as usize)
                .sum(),
        };
        reader.next_payload();
        reader
    }

    fn parse_payload_request(
        opcode: u32,
        arg: &[u8],
        name: &[u8],
        payload_len: usize,
        materialize_payload: bool,
    ) -> Request {
        let prefix_len = size_of::<fuse_in_header>() + arg.len() + name.len();
        let request_len = prefix_len + payload_len;
        let header = fuse_in_header {
            len: request_len as u32,
            opcode,
            unique: 1,
            nodeid: 1,
            uid: 0,
            gid: 0,
            pid: 0,
            padding: 0,
        };
        let memory_len = if materialize_payload {
            request_len
        } else {
            prefix_len
        };
        let mem = GuestMemory::allocate(memory_len);
        mem.write_at(0, header.as_bytes()).unwrap();
        mem.write_at(size_of::<fuse_in_header>() as u64, arg)
            .unwrap();
        if !name.is_empty() {
            mem.write_at((size_of::<fuse_in_header>() + arg.len()) as u64, name)
                .unwrap();
        }
        if materialize_payload {
            mem.write_at(prefix_len as u64, &vec![0; payload_len])
                .unwrap();
        }

        let payload = [
            VirtioQueuePayload {
                writeable: false,
                address: 0,
                length: prefix_len as u32,
            },
            VirtioQueuePayload {
                writeable: false,
                address: prefix_len as u64,
                length: payload_len as u32,
            },
        ];
        Request::new(test_reader(&mem, &payload)).unwrap()
    }

    fn write_arg(size: usize) -> fuse_write_in {
        fuse_write_in {
            fh: 0,
            offset: 0,
            size: size as u32,
            write_flags: 0,
            lock_owner: 0,
            flags: 0,
            padding: 0,
        }
    }

    #[test]
    fn parsed_write_payload_is_bounded() {
        let arg = write_arg(crate::MAX_GUEST_BUFFER_SIZE);
        let request = parse_payload_request(
            FUSE_WRITE,
            arg.as_bytes(),
            &[],
            crate::MAX_GUEST_BUFFER_SIZE,
            true,
        );
        assert!(matches!(
            request.operation(),
            FuseOperation::Write { data, .. } if data.len() == crate::MAX_GUEST_BUFFER_SIZE
        ));

        let oversized = crate::MAX_GUEST_BUFFER_SIZE + 1;
        let arg = write_arg(oversized);
        let request = parse_payload_request(FUSE_WRITE, arg.as_bytes(), &[], oversized, false);
        assert!(matches!(
            request.operation(),
            FuseOperation::Error(err) if *err == lx::Error::E2BIG
        ));
    }

    #[test]
    fn parsed_xattr_payload_is_bounded() {
        let arg = fuse_setxattr_in {
            size: crate::MAX_GUEST_BUFFER_SIZE as u32,
            flags: 0,
        };
        let request = parse_payload_request(
            FUSE_SETXATTR,
            arg.as_bytes(),
            b"user.test\0",
            crate::MAX_GUEST_BUFFER_SIZE,
            true,
        );
        assert!(matches!(
            request.operation(),
            FuseOperation::SetXAttr { value, .. }
                if value.len() == crate::MAX_GUEST_BUFFER_SIZE
        ));

        let oversized = crate::MAX_GUEST_BUFFER_SIZE + 1;
        let arg = fuse_setxattr_in {
            size: oversized as u32,
            flags: 0,
        };
        let request = parse_payload_request(
            FUSE_SETXATTR,
            arg.as_bytes(),
            b"user.test\0",
            oversized,
            false,
        );
        assert!(matches!(
            request.operation(),
            FuseOperation::Error(err) if *err == lx::Error::E2BIG
        ));
    }
}
