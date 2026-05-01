// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Error types for the VHDX parser.
//!
//! Separate error types are provided for each category of operation:
//!
//! - [`CreateError`] — file creation parameter validation
//! - [`OpenError`] — file open and format parsing
//! - [`VhdxIoError`] — runtime I/O (read, write, flush, trim, close)

use thiserror::Error;

use crate::log_task::LogTaskError;

/// The VHDX write pipeline has been poisoned by a previous fatal error.
///
/// Once set, all runtime I/O operations on the file fail permanently.
/// Produced by the log permits semaphore and LSN watermark when the
/// log or apply task encounters a fatal error.
#[derive(Debug, Clone, Error)]
#[error("VHDX pipeline failed: {0}")]
pub(crate) struct PipelineFailed(pub(crate) String);

/// Errors returned by VHDX file creation ([`create::create`](crate::create::create)).
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
///
/// Covers file identifier validation, header parsing, region table
/// validation, metadata parsing, BAT loading, and parent locator
/// parsing.
#[derive(Debug, Error)]
pub(crate) enum OpenErrorInner {
    /// An I/O error occurred while reading the VHDX file.
    #[error("I/O error")]
    Io(#[source] std::io::Error),

    /// The VHDX file is corrupt or has an invalid structure.
    #[error("VHDX file is corrupt")]
    Corrupt(#[from] CorruptionType),

    /// An open option (e.g. block alignment) is invalid.
    #[error("invalid parameter")]
    InvalidParameter(InvalidFormatReason),

    /// The write pipeline failed during writable open initialization.
    #[error("pipeline failed during open")]
    PipelineFailed(#[source] PipelineFailed),

    /// A metadata item could not be read through the page cache.
    #[error("failed to access metadata page cache")]
    MetadataCache(#[source] CacheError),
}

/// Errors returned when opening or parsing a VHDX file.
///
/// Covers file identifier validation, header parsing, region table
/// validation, metadata parsing, BAT loading, and parent locator
/// parsing.
#[derive(Debug, Error)]
#[error(transparent)]
pub struct OpenError(pub(crate) OpenErrorInner);

impl<T: Into<OpenErrorInner>> From<T> for OpenError {
    fn from(inner: T) -> Self {
        OpenError(inner.into())
    }
}

/// Errors returned by runtime VHDX I/O operations.
///
/// Covers read, write, flush, trim, and close. Use [`kind()`](Self::kind)
/// to classify the error.
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
#[non_exhaustive]
pub enum VhdxIoErrorKind {
    /// The file was opened read-only.
    ReadOnly,
    /// The I/O request had invalid parameters (e.g., unaligned).
    InvalidInput,
    /// The I/O request referenced a sector beyond the virtual disk.
    InvalidSector,
    /// Any other error (I/O failure, pipeline failure, etc.).
    Other,
}

/// Inner representation of [`VhdxIoError`].
#[derive(Debug, Error)]
pub(crate) enum VhdxIoErrorInner {
    #[error("failed to write header")]
    WriteHeader(#[source] std::io::Error),
    #[error("failed to flush")]
    Flush(#[source] std::io::Error),
    #[error("failed to commit cache")]
    CommitCache(#[source] CacheError),
    #[error("failed to read sector bitmap")]
    ReadSectorBitmap(#[source] CacheError),
    #[error("failed to zero block at file offset {file_offset:#x}")]
    ZeroBlock {
        #[source]
        err: std::io::Error,
        file_offset: u64,
    },
    #[error("failed to extend file to {target_file_size:#x}")]
    ExtendFile {
        #[source]
        err: std::io::Error,
        target_file_size: u64,
    },
    #[error("failed to truncate file to {target_file_size:#x}")]
    TruncateFile {
        #[source]
        err: std::io::Error,
        target_file_size: u64,
    },
    #[error("failed to access BAT page cache")]
    BatCache(#[source] CacheError),
    #[error("failed to access sector bitmap page cache")]
    SectorBitmapCache(#[source] CacheError),
    #[error("VHDX file is opened read-only")]
    ReadOnly,
    #[error("VHDX file failed")]
    Failed(#[source] PipelineFailed),
    #[error("I/O request is not aligned to logical sector size")]
    UnalignedIo,
    #[error("I/O request extends beyond end of virtual disk")]
    BeyondEndOfDisk,
    #[error("failed to close log task")]
    LogClose(#[source] LogTaskError),
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
///
/// Each variant corresponds to a distinct validation error detected
/// when processing VHDX parameters (e.g. during file creation).
#[derive(Debug, Clone, Error)]
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

    /// The disk size exceeds the maximum (64 TiB).
    #[error("disk size exceeds maximum (64 TiB)")]
    DiskSizeTooLarge,

    /// The block size is not a multiple of 1 MiB.
    #[error("block size must be a multiple of 1 MiB")]
    BlockSizeNotAligned,

    /// The block size exceeds the maximum (256 MiB).
    #[error("block size exceeds maximum (256 MiB)")]
    BlockSizeTooLarge,

    /// The block alignment is not a power of 2.
    #[error("block alignment must be a power of 2")]
    BlockAlignmentNotPowerOfTwo,

    /// The block size / logical sector size combination is invalid (chunk ratio is zero).
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
///
/// Each variant corresponds to a distinct corruption condition detected
/// during parsing or validation. Covers all corruption types from the
/// VHDX implementation.
#[derive(Debug, Clone, Error)]
pub(crate) enum CorruptionType {
    #[error("user metadata entry is marked as required")]
    MetadataUserRequired,
    #[error("BAT region is too small for the disk geometry")]
    BatTooSmall,
    #[error("no valid VHDX headers found")]
    NoValidHeaders,
    #[error("invalid log offset or length in header")]
    InvalidLogOffsetOrLength,
    #[error("log offset is not aligned")]
    InvalidLogOffset,
    #[error("log region extends beyond end of file")]
    LogBeyondEndOfFile,
    #[error("parent locator item is too small for its header")]
    LocatorTooSmallForHeader,
    #[error("parent locator item is too small for its entries")]
    LocatorTooSmallForEntries,
    #[error("parent locator entry key is invalid")]
    InvalidLocatorEntryKey,
    #[error("parent locator entry value is invalid")]
    InvalidLocatorEntryValue,
    #[error("metadata table has an invalid signature")]
    InvalidMetadataTableSignature,
    #[error("metadata table entry count too high")]
    MetadataTableEntryCountTooHigh,
    #[error("reserved metadata table field is nonzero")]
    ReservedMetadataTableFieldNonzero,
    #[error("duplicate metadata GUID")]
    MetadataDuplicateGuid,
    #[error("metadata entries have overlapping ranges")]
    MetadataOverlapping,
    #[error("user metadata entry count exceeded")]
    MetadataUserCountExceeded,
    #[error("file is empty")]
    EmptyFile,
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
    #[error("logical sector size marked as virtual disk metadata")]
    LogicalSectorSizeMarkedVirtual,
    #[error("invalid sector size")]
    InvalidSectorSize,
    #[error("logical sector size item has invalid size")]
    InvalidLogicalSectorSizeSize,
    #[error("disk size item marked as virtual disk metadata")]
    DiskMarkedVirtual,
    #[error("invalid virtual disk size")]
    InvalidDiskSize,
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
    #[error("bad log entry encountered during replay")]
    BadLogEntryOnReplay,
    #[error("no valid log entries found")]
    NoValidLogEntries,
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
    #[error("unknown required metadata item")]
    UnknownRequiredMetadata,
    #[error("file is marked as incomplete")]
    IncompleteFile,
    #[error("required metadata item is missing")]
    MissingRequiredMetadata,
    #[error("header has log GUID but log is missing")]
    MissingLogHasGuid,
    #[error("invalid metadata entry offset")]
    InvalidMetadataEntryOffset,
    #[error("metadata region is too large")]
    MetadataRegionTooLarge,
    #[error("metadata item is too large")]
    MetadataItemTooLarge,
    #[error("total metadata size per category exceeded")]
    TotalMetadataSizeExceeded,
    #[error("metadata entry has zero item GUID")]
    ZeroMetadataItemId,
    #[error("invalid file identifier signature")]
    InvalidFileIdentifier,
    #[error("invalid parent locator key-value count")]
    InvalidLocatorKeyValueCount,
    #[error("log is full")]
    LogFull,
    #[error("log replay required (log GUID is non-zero)")]
    LogReplayRequired,
    #[error("unsupported VHDX version")]
    UnsupportedVersion,
    #[error("unsupported VHDX log version")]
    UnsupportedLogVersion,
}
