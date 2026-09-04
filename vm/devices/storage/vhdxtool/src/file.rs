// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Cross-platform positional file I/O for the VHDX metadata and payload paths.

use std::borrow::Borrow;
use std::fs;
use std::io;
use std::path::Path;
use std::sync::Arc;
use vhdx::AsyncFile;

#[cfg(unix)]
fn read_at(file: &fs::File, buf: &mut [u8], offset: u64) -> io::Result<usize> {
    std::os::unix::fs::FileExt::read_at(file, buf, offset)
}

#[cfg(windows)]
fn read_at(file: &fs::File, buf: &mut [u8], offset: u64) -> io::Result<usize> {
    std::os::windows::fs::FileExt::seek_read(file, buf, offset)
}

#[cfg(unix)]
fn write_at(file: &fs::File, buf: &[u8], offset: u64) -> io::Result<usize> {
    std::os::unix::fs::FileExt::write_at(file, buf, offset)
}

#[cfg(windows)]
fn write_at(file: &fs::File, buf: &[u8], offset: u64) -> io::Result<usize> {
    std::os::windows::fs::FileExt::seek_write(file, buf, offset)
}

fn read_exact_at(file: &fs::File, mut buf: &mut [u8], mut offset: u64) -> io::Result<()> {
    while !buf.is_empty() {
        let count = read_at(file, buf, offset)?;
        if count == 0 {
            return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "short read"));
        }
        offset += count as u64;
        buf = &mut buf[count..];
    }
    Ok(())
}

fn write_all_at(file: &fs::File, mut buf: &[u8], mut offset: u64) -> io::Result<()> {
    while !buf.is_empty() {
        let count = write_at(file, buf, offset)?;
        if count == 0 {
            return Err(io::Error::new(
                io::ErrorKind::WriteZero,
                "failed to write any bytes",
            ));
        }
        offset += count as u64;
        buf = &buf[count..];
    }
    Ok(())
}

#[derive(Clone)]
pub(crate) struct BlockingFile {
    file: Arc<fs::File>,
}

impl BlockingFile {
    pub(crate) fn open(path: &Path, read_only: bool) -> io::Result<Self> {
        let file = fs::OpenOptions::new()
            .read(true)
            .write(!read_only)
            .open(path)?;
        Ok(Self {
            file: Arc::new(file),
        })
    }

    pub(crate) fn create(path: &Path, force: bool) -> io::Result<Self> {
        let file = fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(true)
            .create_new(!force)
            .open(path)?;
        Ok(Self {
            file: Arc::new(file),
        })
    }
}

impl AsyncFile for BlockingFile {
    type Buffer = Vec<u8>;

    fn alloc_buffer(&self, len: usize) -> Self::Buffer {
        vec![0; len]
    }

    async fn read_into(&self, offset: u64, buf: Self::Buffer) -> io::Result<Self::Buffer> {
        let file = self.file.clone();
        blocking::unblock(move || {
            let mut buf = buf;
            read_exact_at(&file, &mut buf, offset)?;
            Ok(buf)
        })
        .await
    }

    async fn write_from(
        &self,
        offset: u64,
        buf: impl Borrow<Self::Buffer> + Send + 'static,
    ) -> io::Result<()> {
        let file = self.file.clone();
        blocking::unblock(move || write_all_at(&file, buf.borrow(), offset)).await
    }

    async fn flush(&self) -> io::Result<()> {
        let file = self.file.clone();
        blocking::unblock(move || file.sync_all()).await
    }

    async fn file_size(&self) -> io::Result<u64> {
        Ok(self.file.metadata()?.len())
    }

    async fn set_file_size(&self, size: u64) -> io::Result<()> {
        let file = self.file.clone();
        blocking::unblock(move || file.set_len(size)).await
    }
}
