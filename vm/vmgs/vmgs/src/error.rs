// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Error object for the VMGS crate
use crate::storage::StorageError;
use thiserror::Error;

/// VMGS errors.
#[derive(Debug, Error)]
pub enum Error {
    /// Error reading from disk
    #[error("Error reading from disk")]
    ReadDisk(#[source] StorageError),
    /// Error writing to disk
    #[error("Error writing to disk")]
    WriteDisk(#[source] StorageError),
    /// Error flushing the disk
    #[error("Error flushing the disk")]
    FlushDisk(#[source] StorageError),

    /// The requested file ID is not allocated
    #[error("{0} is not allocated")]
    FileInfoNotAllocated(vmgs_format::FileId),
    /// Cannot allocate 0 blocks
    #[error("Cannot allocate 0 blocks")]
    AllocateZero,
    /// Invalid data allocation offsets
    #[error("Invalid data allocation offsets")]
    AllocateOffset,
    /// Insufficient resources
    #[error("Insufficient resources")]
    InsufficientResources,
    /// Invalid file ID
    #[error("Invalid file ID")]
    FileId,
    /// Invalid data buffer length
    #[error("Invalid data buffer length")]
    WriteFileLength,
    /// Trying to allocate too many blocks
    #[error("Trying to allocate too many blocks")]
    WriteFileBlocks,
    /// Fatal storage initialization error
    #[error("Fatal storage initialization error: {0}")]
    Initialization(#[source] StorageError),
    /// Invalid VMGS file format
    #[error("Invalid VMGS file format: {0}")]
    InvalidFormat(String),
    /// Corrupt VMGS file format
    #[error("Corrupt VMGS file format: {0}")]
    CorruptFormat(String),
    /// The VMGS file has a non zero size but the contents are empty
    #[error("The VMGS file has a non zero size but the contents are empty")]
    EmptyFile,
    /// Cannot overwrite encrypted file with plaintext data
    #[error("Cannot overwrite encrypted file with plaintext data")]
    OverwriteEncrypted,
    /// File must be decrypted to perform this operation
    #[error("File must be decrypted to perform this operation")]
    NeedsUnlock,
    /// Failed to use the root key provided to decrypt VMGS metadata key
    #[error("Failed to use the root key provided to decrypt VMGS metadata key")]
    DecryptMetadataKey,
    /// VMGS file version does not support encryption
    #[error("VMGS file version does not support encryption")]
    EncryptionNotSupported,
    /// Cannot perform operation on unencrypted file
    #[error("Cannot perform operation on unencrypted file")]
    NotEncrypted,
    /// There is no space to add a new encryption key
    #[error("There is no space to add a new encryption key")]
    DatastoreKeysFull,
    /// Unable to determine inactive key for removal
    #[error("Unable to determine inactive key for removal")]
    NoActiveDatastoreKey,
    /// VMGS is v1 format
    #[error("VMGS is v1 format")]
    V1Format,
    /// Cannot overwrite file when moving
    #[error("Cannot overwrite file when moving")]
    OverwriteMove,
    /// Unexpected data length
    #[error("Unexpected {0} length: should be {1}, got {2}")]
    UnexpectedLength(&'static str, usize, usize),
    /// Invalid argument
    #[error("Invalid argument: {0}")]
    InvalidArgument(&'static str),

    /// OpenSSL error
    #[cfg(feature = "encryption_ossl")]
    #[error("OpenSSL error {1}: {0}")]
    OpenSSL(#[source] openssl::error::ErrorStack, &'static str),
    /// BCrypt error
    #[cfg(all(windows, feature = "encryption_win"))]
    #[error("BCrypt error {1}: {0}")]
    BCrypt(#[source] windows_result::Error, &'static str),
    /// Serde JSON error
    #[error("Serde JSON error: {0}")]
    Json(#[from] serde_json::Error),
}
