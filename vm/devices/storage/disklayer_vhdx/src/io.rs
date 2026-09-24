// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! VHDX metadata adapter for shared host-file I/O.

use disk_file::host_file::OwnedFile;
use std::borrow::Borrow;
use std::fs;
use std::io;
use std::path::Path;
use vhdx::AsyncFile;

/// Owned-buffer file I/O used for VHDX metadata.
#[derive(Clone)]
pub struct MetadataFile {
    file: OwnedFile,
}

impl MetadataFile {
    /// Wraps an existing open file.
    pub fn new(file: fs::File) -> Self {
        Self {
            file: OwnedFile::new(file),
        }
    }

    /// Wraps the metadata interface of a shared host file.
    pub fn from_owned(file: OwnedFile) -> Self {
        Self { file }
    }

    /// Opens a file at the given path.
    pub fn open(path: &Path, read_only: bool) -> io::Result<Self> {
        let file = fs::OpenOptions::new()
            .read(true)
            .write(!read_only)
            .create(!read_only)
            .open(path)?;
        Ok(Self::new(file))
    }
}

impl AsyncFile for MetadataFile {
    type Buffer = Vec<u8>;

    fn alloc_buffer(&self, len: usize) -> Vec<u8> {
        vec![0; len]
    }

    async fn read_into(&self, offset: u64, buf: Vec<u8>) -> io::Result<Vec<u8>> {
        self.file.read_into(offset, buf).await
    }

    async fn write_from(
        &self,
        offset: u64,
        buf: impl Borrow<Vec<u8>> + Send + 'static,
    ) -> io::Result<()> {
        self.file.write_from(offset, buf).await
    }

    async fn flush(&self) -> io::Result<()> {
        self.file.flush().await
    }

    async fn file_size(&self) -> io::Result<u64> {
        self.file.file_size()
    }

    async fn set_file_size(&self, size: u64) -> io::Result<()> {
        self.file.set_file_size(size).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    #[pal_async::async_test]
    async fn round_trip_read_write() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.bin");
        let file = MetadataFile::open(&path, false).unwrap();
        file.set_file_size(4096).await.unwrap();

        let write_data = Arc::new(vec![0xAB; 512]);
        file.write_from(0, write_data.clone()).await.unwrap();
        file.write_from(1024, write_data.clone()).await.unwrap();

        assert_eq!(file.read_into(0, vec![0; 512]).await.unwrap(), *write_data);
        assert_eq!(
            file.read_into(1024, vec![0; 512]).await.unwrap(),
            *write_data
        );
        assert_eq!(
            file.read_into(512, vec![0; 512]).await.unwrap(),
            vec![0; 512]
        );
        assert_eq!(file.file_size().await.unwrap(), 4096);
    }

    #[pal_async::async_test]
    async fn opens_vhdx() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.vhdx");
        let file = MetadataFile::open(&path, false).unwrap();
        let mut params = vhdx::CreateParams {
            disk_size: 1024 * 1024,
            ..Default::default()
        };
        vhdx::create(&file, &mut params).await.unwrap();

        let file = MetadataFile::open(&path, false).unwrap();
        let vhdx = vhdx::VhdxFile::open(file).read_only().await.unwrap();
        assert_eq!(vhdx.disk_size(), 1024 * 1024);
        assert_eq!(vhdx.logical_sector_size(), 512);
    }
}
