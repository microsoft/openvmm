// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Error object for the VMGS crate
use crate::storage::StorageError;
use thiserror::Error;

/// VMGS errors.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum Error {
    /// Error reading from disk.
    #[error("read disk error")]
    ReadDisk(#[source] StorageError),
    /// Error writing to disk.
    #[error("write disk error")]
    WriteDisk(#[source] StorageError),
    /// Error flushing the disk.
    #[error("flush disk error")]
    FlushDisk(#[source] StorageError),

    /// Invalid file id or file header.
    #[error("invalid file id or file header")]
    FileInfo,
    /// No allocated bytes for file id being read.
    #[error("no allocated bytes for file id being read")]
    FileInfoNotAllocated,
    /// Cannot allocate 0 blocks.
    #[error("cannot allocate 0 blocks")]
    AllocateZero,
    /// Invalid data allocation offsets.
    #[error("invalid data allocation offsets")]
    AllocateOffset,
    /// Insufficient resources.
    #[error("insufficient resources")]
    InsufficientResources,
    /// Invalid file id.
    #[error("invalid file id")]
    FileId,
    /// Invalid data buffer length.
    #[error("invalid data buffer length")]
    WriteFileLength,
    /// Trying to allocate too many blocks.
    #[error("trying to allocate too many blocks")]
    WriteFileBlocks,
    /// Fatal storage initialization failures
    #[error("Fatal storage initialization error: {0}")]
    Initialization(#[source] StorageError),
    /// Invalid VMGS file format.
    #[error("VMGS_INVALID_FORMAT: {0}")]
    InvalidFormat(String),
    /// Corrupt VMGS file format.
    #[error("VMGS_CORRUPT_FORMAT: {0}")]
    CorruptFormat(String),
    /// Empty VMGS file.
    #[error("empty file")]
    EmptyFile,
    /// Cannot overwrite encrypted file with plaintext data.
    #[error("cannot overwrite encrypted file with plaintext data")]
    OverwriteEncrypted,
    /// Cannot read encrypted file - VMGS is locked.
    #[error("cannot read encrypted file - VMGS is locked")]
    ReadEncrypted,
    /// Tried to add a new encryption key, but there are already two keys.
    #[error("no space to add new encryption key")]
    DatastoreKeysFull,
    /// Datastore keys full, but there is no active key.
    #[error("unable to determine inactive key for removal")]
    NoActiveDatastoreKey,
    /// VMGS is v1 format
    #[error("VMGS is v1 format")]
    V1Format,
    /// Cannot overwrite file when moving
    #[error("cannot overwrite file when moving")]
    OverwriteMove,

    /// OpenSSL errors.
    #[cfg(feature = "encryption_ossl")]
    #[error("OpenSSL error {1}: {0}")]
    OpenSSL(#[source] openssl::error::ErrorStack, String),

    /// Other errors - TODO: REMOVE THIS
    #[error(transparent)]
    Other(#[from] anyhow::Error),
}
