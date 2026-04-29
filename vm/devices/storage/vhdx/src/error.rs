// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Error types for the VHDX parser.
//!
//! Separate error types are provided for each category of operation:
//!
//! - [`CreateError`] for file creation parameter validation.
//! - [`OpenError`] for file open and format parsing.
//! - [`VhdxIoError`] for runtime I/O once the read/write path is added.

#![allow(dead_code)]

use crate::log_task::LogTaskError;
use thiserror::Error;

/// The VHDX write pipeline has been poisoned by a previous fatal error.
///
/// Once set, all runtime I/O operations on the file fail permanently.
/// Produced by the log permits semaphore and LSN watermark when the
/// log or apply task encounters a fatal error.
#[derive(Debug, Clone, Error)]
#[error("VHDX pipeline failed: {0}")]
pub(crate) struct PipelineFailed(pub(crate) String);

/// Errors returned by VHDX file creation.
#[derive(Debug, Error)]
pub enum CreateError {
    /// An I/O error occurred while writing the VHDX file.
    #[error("write error")]
    Write(#[source] std::io::Error),

    /// A parameter validation error.
    #[error("invalid format parameters")]
    InvalidFormat(#[source] InvalidFormatReason),
}

/// Errors returned when opening or parsing a VHDX file.
#[derive(Debug, Error)]
#[error(transparent)]
pub struct OpenError(pub(crate) OpenErrorInner);

impl<T: Into<OpenErrorInner>> From<T> for OpenError {
    fn from(inner: T) -> Self {
        OpenError(inner.into())
    }
}

/// Inner representation of [`OpenError`].
#[derive(Debug, Error)]
pub(crate) enum OpenErrorInner {
    /// An I/O error occurred while reading the VHDX file.
    #[error("I/O error")]
    Io(#[from] std::io::Error),

    /// The VHDX file is corrupt or has an invalid structure.
    #[error("VHDX file is corrupt")]
    Corrupt(#[from] CorruptionType),

    /// An open option is invalid.
    #[error("invalid parameter")]
    InvalidParameter(#[from] InvalidFormatReason),

    /// The write pipeline failed during writable open initialization.
    #[error("pipeline failed during open")]
    PipelineFailed(#[source] PipelineFailed),

    /// A metadata item could not be read through the page cache.
    #[error("failed to access metadata page cache")]
    MetadataCache(#[source] CacheError),
}

/// Errors returned by runtime VHDX I/O operations.
#[derive(Debug, Error)]
#[error(transparent)]
pub struct VhdxIoError(#[from] pub(crate) VhdxIoErrorInner);

impl VhdxIoError {
    /// Classify this error into a [`VhdxIoErrorKind`].
    pub fn kind(&self) -> VhdxIoErrorKind {
        match self.0 {
            VhdxIoErrorInner::ReadOnly => VhdxIoErrorKind::ReadOnly,
            VhdxIoErrorInner::UnalignedIo => VhdxIoErrorKind::InvalidInput,
            VhdxIoErrorInner::BeyondEndOfDisk => VhdxIoErrorKind::InvalidSector,
            _ => VhdxIoErrorKind::Other,
        }
    }
}

/// Classification of [`VhdxIoError`] for programmatic handling.
#[derive(Debug, Copy, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum VhdxIoErrorKind {
    /// The file was opened read-only.
    ReadOnly,
    /// The I/O request had invalid parameters, such as an unaligned offset.
    InvalidInput,
    /// The I/O request referenced a sector beyond the virtual disk.
    InvalidSector,
    /// Any other error.
    Other,
}

/// Inner representation of [`VhdxIoError`].
#[derive(Debug, Error)]
pub(crate) enum VhdxIoErrorInner {
    /// An I/O error occurred.
    #[error("I/O error")]
    Io(#[from] std::io::Error),

    /// Failed to write or flush a VHDX header update.
    #[error("failed to write header")]
    WriteHeader(#[source] std::io::Error),

    /// Failed to commit cached metadata pages to the log pipeline.
    #[error("failed to commit cache")]
    CommitCache(#[source] CacheError),

    /// The write pipeline failed permanently.
    #[error("VHDX file failed")]
    Failed(#[source] PipelineFailed),

    /// The file was opened read-only.
    #[error("VHDX file is opened read-only")]
    ReadOnly,

    /// The log task failed during graceful shutdown.
    #[error("failed to close log task")]
    LogClose(#[source] LogTaskError),

    /// The request is not aligned to the logical sector size.
    #[error("I/O request is not aligned to logical sector size")]
    UnalignedIo,

    /// The request extends beyond the virtual disk size.
    #[error("I/O request extends beyond end of virtual disk")]
    BeyondEndOfDisk,
}

/// Errors from the page cache write path.
///
/// Produced by [`PageCache::acquire_write`](crate::cache::PageCache::acquire_write)
/// when a page cannot be acquired for writing.
#[derive(Debug, Error)]
pub(crate) enum CacheError {
    /// An I/O error occurred while loading the page from disk.
    #[error("read error at file offset {file_offset:#x}")]
    Read {
        #[source]
        err: std::io::Error,
        file_offset: u64,
    },

    /// The write pipeline has been poisoned by a previous fatal error.
    #[error("pipeline failed")]
    PipelineFailed(#[source] PipelineFailed),
}

/// Specific reasons a VHDX creation or parameter validation may fail.
#[derive(Debug, Clone, Error, PartialEq, Eq)]
pub enum InvalidFormatReason {
    /// The logical sector size is not 512 or 4096.
    #[error("logical sector size must be 512 or 4096")]
    InvalidLogicalSectorSize,

    /// The physical sector size is not 512 or 4096.
    #[error("physical sector size must be 512 or 4096")]
    InvalidPhysicalSectorSize,

    /// The disk size is zero.
    #[error("disk size must be > 0")]
    DiskSizeZero,

    /// The disk size is not a multiple of the logical sector size.
    #[error("disk size must be a multiple of logical sector size")]
    DiskSizeNotAligned,

    /// The disk size exceeds the maximum.
    #[error("disk size exceeds maximum (64 TiB)")]
    DiskSizeTooLarge,

    /// The block size is not a multiple of 1 MiB.
    #[error("block size must be a multiple of 1 MiB")]
    BlockSizeNotAligned,

    /// The block size exceeds the maximum.
    #[error("block size exceeds maximum (256 MiB)")]
    BlockSizeTooLarge,

    /// The block alignment is not a power of two.
    #[error("block alignment must be a power of 2")]
    BlockAlignmentNotPowerOfTwo,

    /// The block size / logical sector size combination is invalid.
    #[error("invalid block size / logical sector size combination")]
    InvalidChunkRatio,

    /// The computed BAT entry count exceeds the absolute maximum.
    #[error("BAT entry count exceeds absolute maximum")]
    BatEntryCountTooLarge,

    /// The computed BAT size exceeds the maximum.
    #[error("BAT size exceeds maximum")]
    BatSizeTooLarge,
}

/// Specific reasons a VHDX file may be considered corrupt.
#[derive(Debug, Clone, Error, PartialEq, Eq)]
pub(crate) enum CorruptionType {
    #[error("invalid file identifier signature")]
    InvalidFileIdentifier,
    #[error("no valid VHDX headers found")]
    NoValidHeaders,
    #[error("header sequence number overflowed u64")]
    HeaderSequenceOverflow,
    #[error("invalid log offset or length in header")]
    InvalidLogOffsetOrLength,
    #[error("log offset is not aligned")]
    InvalidLogOffset,
    #[error("log region extends beyond end of file")]
    LogBeyondEndOfFile,
    #[error("unsupported VHDX version")]
    UnsupportedVersion,
    #[error("unsupported VHDX log version")]
    UnsupportedLogVersion,
    #[error("both region tables are corrupt")]
    RegionTablesBothCorrupt,
    #[error("invalid entry count in region table")]
    InvalidEntryCountInRegionTable,
    #[error("reserved region table field is nonzero")]
    ReservedRegionTableFieldNonzero,
    #[error("duplicate region table entry")]
    DuplicateRegionEntry,
    #[error("invalid offset or length in region table entry")]
    OffsetOrLengthInRegionTable,
    #[error("unknown required region")]
    UnknownRequiredRegion,
    #[error("BAT or metadata region is missing")]
    MissingBatOrMetadataRegion,
    #[error("metadata table has an invalid signature")]
    InvalidMetadataTableSignature,
    #[error("metadata table entry count too high")]
    MetadataTableEntryCountTooHigh,
    #[error("reserved metadata table field is nonzero")]
    ReservedMetadataTableFieldNonzero,
    #[error("metadata entry has zero item GUID")]
    ZeroMetadataItemId,
    #[error("duplicate metadata GUID")]
    MetadataDuplicateGuid,
    #[error("metadata entries have overlapping ranges")]
    MetadataOverlapping,
    #[error("metadata entry has invalid offset")]
    InvalidMetadataEntryOffset,
    #[error("metadata region is too large")]
    MetadataRegionTooLarge,
    #[error("metadata item is too large")]
    MetadataItemTooLarge,
    #[error("total metadata size per category exceeded")]
    TotalMetadataSizeExceeded,
    #[error("user metadata entry is marked as required")]
    MetadataUserRequired,
    #[error("user metadata entry count exceeded")]
    MetadataUserCountExceeded,
    #[error("unknown required metadata item")]
    UnknownRequiredMetadata,
    #[error("required metadata item is missing")]
    MissingRequiredMetadata,
    #[error("file is marked as incomplete")]
    IncompleteFile,
    #[error("file parameters item has invalid size")]
    InvalidFileParameterSize,
    #[error("reserved file parameters field is nonzero")]
    ReservedFileParametersFieldNonzero,
    #[error("file parameters marked as virtual disk metadata")]
    FileParametersMarkedVirtual,
    #[error("invalid block size")]
    InvalidBlockSize,
    #[error("invalid logical sector size")]
    InvalidLogicalSectorSize,
    #[error("logical sector size item has invalid size")]
    InvalidLogicalSectorSizeSize,
    #[error("logical sector size marked as virtual disk metadata")]
    LogicalSectorSizeMarkedVirtual,
    #[error("invalid sector size")]
    InvalidSectorSize,
    #[error("disk size item marked as virtual disk metadata")]
    DiskMarkedVirtual,
    #[error("invalid virtual disk size")]
    InvalidDiskSize,
    #[error("BAT region is too small for the disk geometry")]
    BatTooSmall,
    #[error("BAT entry references range beyond end of file")]
    RangeBeyondEof,
    #[error("BAT entries reference overlapping file ranges")]
    RangeCollision,
    #[error("invalid block state in BAT entry")]
    InvalidBlockState,
    #[error("reserved BAT entry field is nonzero")]
    ReservedBatEntryFieldNonzero,
    #[error("partially present block has no sector bitmap")]
    PartiallyPresentWithoutSectorBitmap,
    #[error("trimmed range collides with allocated range")]
    TrimmedRangeCollision,
    #[error("parent locator item is too small for its header")]
    LocatorTooSmallForHeader,
    #[error("parent locator item is too small for its entries")]
    LocatorTooSmallForEntries,
    #[error("invalid parent locator key-value count")]
    InvalidLocatorKeyValueCount,
    #[error("parent locator entry key is invalid")]
    InvalidLocatorEntryKey,
    #[error("parent locator entry value is invalid")]
    InvalidLocatorEntryValue,
    #[error("bad log entry encountered during replay")]
    BadLogEntryOnReplay,
    #[error("no valid log entries found")]
    NoValidLogEntries,
    #[error("header has log GUID but log is missing")]
    MissingLogHasGuid,
    #[error("log replay required (log GUID is non-zero)")]
    LogReplayRequired,
    #[error("log is full")]
    LogFull,
    #[error("file is empty")]
    EmptyFile,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn create_error_preserves_invalid_format_source() {
        let err = CreateError::InvalidFormat(InvalidFormatReason::DiskSizeZero);
        assert_eq!(err.to_string(), "invalid format parameters");
        assert_eq!(
            std::error::Error::source(&err).unwrap().to_string(),
            "disk size must be > 0"
        );
    }

    #[test]
    fn open_error_converts_from_corruption() {
        let err = OpenError::from(OpenErrorInner::from(CorruptionType::InvalidFileIdentifier));
        assert_eq!(err.to_string(), "VHDX file is corrupt");
        assert_eq!(
            std::error::Error::source(&err).unwrap().to_string(),
            "invalid file identifier signature"
        );
    }

    #[test]
    fn vhdx_io_error_reports_kind() {
        let err = VhdxIoError::from(VhdxIoErrorInner::UnalignedIo);
        assert_eq!(err.kind(), VhdxIoErrorKind::InvalidInput);
    }
}
