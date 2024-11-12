// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Defines the trait for the backing store of the vmbus file system.

use crate::protocol;
use inspect::InspectMut;

/// The backing store for the vmbus file system.
pub trait VmbfsIo: Send + InspectMut {
    /// Returns information about a file or directory.
    fn file_info(&mut self, path: &str) -> Result<FileInfo, FileError>;
    /// Reads the contents of a file.
    fn read_file(&mut self, path: &str, offset: u64, buf: &mut [u8]) -> Result<(), FileError>;
}

/// Information about a file or directory.
pub struct FileInfo {
    /// Whether the path is a directory.
    pub directory: bool,
    /// The size of the file in bytes.
    pub file_size: u64,
}

/// An error that can occur when interacting with the file system.
pub enum FileError {
    /// The file was not found.
    NotFound,
    /// The read operation reached the end of the file.
    EndOfFile,
    /// An I/O error occurred.
    Error(std::io::Error),
}

impl FileError {
    pub(crate) fn to_protocol(&self) -> protocol::Status {
        match self {
            FileError::NotFound => protocol::Status::NOT_FOUND,
            FileError::EndOfFile => protocol::Status::END_OF_FILE,
            FileError::Error(_) => protocol::Status::ERROR,
        }
    }
}

impl From<std::io::Error> for FileError {
    fn from(err: std::io::Error) -> Self {
        match err.kind() {
            std::io::ErrorKind::NotFound => FileError::NotFound,
            std::io::ErrorKind::UnexpectedEof => FileError::EndOfFile,
            _ => FileError::Error(err),
        }
    }
}
