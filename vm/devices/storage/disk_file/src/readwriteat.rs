// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Helpers for doing IO at a given offset.

use std::fs;
use std::io::Result;
use std::io::{Error, ErrorKind};

/// A unified extension trait for [`std::fs::File`] for reading/writing at a
/// given offset.
///
/// The semantics are slightly different between Windows and Unix--on Windows,
/// each operation updates the current file pointer, whereas on Unix it does
/// not.
pub trait ReadWriteAt {
    fn write_at(&self, buf: &[u8], offset: u64) -> Result<usize>;
    fn read_at(&self, buf: &mut [u8], offset: u64) -> Result<usize>;

    fn read_exact_at(&self, mut buf: &mut [u8], mut offset: u64) -> Result<()> {
        while !buf.is_empty() {
            let count = match self.read_at(buf, offset) {
                Ok(0) => return Err(Error::from(ErrorKind::UnexpectedEof)),
                Ok(count) => count,
                Err(err) if err.kind() == ErrorKind::Interrupted => continue,
                Err(err) => return Err(err),
            };
            if count > buf.len() {
                return Err(Error::from(ErrorKind::InvalidData));
            }
            offset = offset
                .checked_add(count as u64)
                .ok_or_else(|| Error::from(ErrorKind::InvalidInput))?;
            buf = &mut buf[count..];
        }
        Ok(())
    }

    fn write_all_at(&self, mut buf: &[u8], mut offset: u64) -> Result<()> {
        while !buf.is_empty() {
            let count = match self.write_at(buf, offset) {
                Ok(0) => return Err(Error::from(ErrorKind::WriteZero)),
                Ok(count) => count,
                Err(err) if err.kind() == ErrorKind::Interrupted => continue,
                Err(err) => return Err(err),
            };
            if count > buf.len() {
                return Err(Error::from(ErrorKind::InvalidData));
            }
            offset = offset
                .checked_add(count as u64)
                .ok_or_else(|| Error::from(ErrorKind::InvalidInput))?;
            buf = &buf[count..];
        }
        Ok(())
    }
}

#[cfg(windows)]
impl ReadWriteAt for fs::File {
    fn write_at(&self, buf: &[u8], offset: u64) -> Result<usize> {
        std::os::windows::fs::FileExt::seek_write(self, buf, offset)
    }

    fn read_at(&self, buf: &mut [u8], offset: u64) -> Result<usize> {
        std::os::windows::fs::FileExt::seek_read(self, buf, offset)
    }
}

#[cfg(unix)]
impl ReadWriteAt for fs::File {
    fn write_at(&self, buf: &[u8], offset: u64) -> Result<usize> {
        std::os::unix::fs::FileExt::write_at(self, buf, offset)
    }

    fn read_at(&self, buf: &mut [u8], offset: u64) -> Result<usize> {
        std::os::unix::fs::FileExt::read_at(self, buf, offset)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;
    use test_with_tracing::test;

    struct ShortIo {
        bytes: RefCell<Vec<u8>>,
        chunk: usize,
    }

    impl ReadWriteAt for ShortIo {
        fn read_at(&self, buf: &mut [u8], offset: u64) -> Result<usize> {
            let bytes = self.bytes.borrow();
            let offset = offset as usize;
            let count = buf
                .len()
                .min(self.chunk)
                .min(bytes.len().saturating_sub(offset));
            if count != 0 {
                buf[..count].copy_from_slice(&bytes[offset..offset + count]);
            }
            Ok(count)
        }

        fn write_at(&self, buf: &[u8], offset: u64) -> Result<usize> {
            let count = buf.len().min(self.chunk);
            let mut bytes = self.bytes.borrow_mut();
            let offset = offset as usize;
            let len = bytes.len().max(offset + count);
            bytes.resize(len, 0);
            bytes[offset..offset + count].copy_from_slice(&buf[..count]);
            Ok(count)
        }
    }

    #[test]
    fn exact_operations_complete_short_transfers() {
        let io = ShortIo {
            bytes: RefCell::new(Vec::new()),
            chunk: 2,
        };
        io.write_all_at(b"payload", 1).unwrap();
        let mut actual = [0; 7];
        io.read_exact_at(&mut actual, 1).unwrap();
        assert_eq!(&actual, b"payload");
    }

    #[test]
    fn incomplete_transfers_report_errors() {
        let io = ShortIo {
            bytes: RefCell::new(vec![1]),
            chunk: 2,
        };
        assert_eq!(
            io.read_exact_at(&mut [0; 2], 0).unwrap_err().kind(),
            ErrorKind::UnexpectedEof,
        );
        let io = ShortIo {
            bytes: RefCell::new(Vec::new()),
            chunk: 0,
        };
        assert_eq!(
            io.write_all_at(&[1], 0).unwrap_err().kind(),
            ErrorKind::WriteZero
        );
    }
}
