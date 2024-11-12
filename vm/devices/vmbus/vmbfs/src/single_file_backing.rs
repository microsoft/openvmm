// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Implements a backing store for the vmbus file system that provides a single
//! file.

use crate::backing::FileError;
use crate::backing::FileInfo;
use crate::backing::VmbfsIo;
use inspect::InspectMut;
use std::fs::File;
use std::io::Read;
use std::io::Seek;
use thiserror::Error;

/// A backing store for the vmbus file system that provides a single file.
#[derive(InspectMut)]
pub struct VmbfsSingleFileBacking {
    path: String,
    file: File,
}

/// An error indicating that the input file name is invalid.
#[derive(Debug, Error)]
#[error("invalid path")]
pub struct InvalidFileName;

impl VmbfsSingleFileBacking {
    /// Returns a new instance that provides access to the given file via the
    /// given name.
    ///
    /// Fails if the input file name contains a '/' or a '\'.
    pub fn new(name: &str, file: File) -> Result<Self, InvalidFileName> {
        if name.contains('/') || name.contains('\\') {
            return Err(InvalidFileName);
        }
        let path = format!("/{name}");
        Ok(Self { path, file })
    }
}

impl VmbfsIo for VmbfsSingleFileBacking {
    fn file_info(&mut self, path: &str) -> Result<FileInfo, FileError> {
        if path != self.path {
            return Err(FileError::NotFound);
        }
        let metadata = self.file.metadata()?;
        Ok(FileInfo {
            directory: false,
            file_size: metadata.len(),
        })
    }

    fn read_file(&mut self, path: &str, offset: u64, buf: &mut [u8]) -> Result<(), FileError> {
        if path != self.path {
            return Err(FileError::NotFound);
        }
        self.file.seek(std::io::SeekFrom::Start(offset))?;
        self.file.read_exact(buf)?;
        Ok(())
    }
}
