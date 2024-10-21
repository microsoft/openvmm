// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! File-backed blobs.

use super::Blob;
use async_trait::async_trait;
use inspect::Inspect;
use std::fs::File;
use std::io;
use std::sync::Arc;

/// A blob backed by a local file.
#[derive(Debug, Inspect)]
pub struct FileBlob {
    file: Arc<File>,
    len: u64,
}

impl FileBlob {
    /// Returns a new file blob for `file`.
    pub fn new(file: File) -> io::Result<Self> {
        let len = file.metadata()?.len();
        Ok(Self {
            file: Arc::new(file),
            len,
        })
    }
}

#[async_trait]
impl Blob for FileBlob {
    async fn read(&self, buf: &mut [u8], offset: u64) -> io::Result<()> {
        let file = self.file.clone();
        let len = buf.len();
        let data = blocking::unblock(move || {
            let mut data = vec![0; len];
            let n = file.read_at(&mut data, offset)?;
            if n < data.len() {
                return Err(io::ErrorKind::UnexpectedEof.into());
            }
            io::Result::Ok(data)
        })
        .await?;
        buf.copy_from_slice(&data);
        Ok(())
    }

    fn len(&self) -> u64 {
        self.len
    }
}

/// A unified extension trait for [`std::fs::File`] for reading at a
/// given offset.
///
/// The semantics are slightly different between Windows and Unix--on Windows,
/// each operation updates the current file pointer, whereas on Unix it does
/// not.
trait ReadAt {
    fn read_at(&self, buf: &mut [u8], offset: u64) -> io::Result<usize>;
}

#[cfg(windows)]
impl ReadAt for File {
    fn read_at(&self, buf: &mut [u8], offset: u64) -> io::Result<usize> {
        std::os::windows::fs::FileExt::seek_read(self, buf, offset)
    }
}

#[cfg(unix)]
impl ReadAt for File {
    fn read_at(&self, buf: &mut [u8], offset: u64) -> io::Result<usize> {
        std::os::unix::fs::FileExt::read_at(self, buf, offset)
    }
}
