// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Zoned block device support.

use crate::DiskError;
use inspect::Inspect;
use thiserror::Error;

/// Trait implemented by disks that expose zoned block device semantics.
#[async_trait::async_trait]
pub trait ZonedDisk: Sync {
    /// Returns immutable zoned device information.
    fn info(&self) -> ZonedDiskInfo;

    /// Reports zones beginning with the zone that contains `start_lba`.
    async fn report_zones(
        &self,
        start_lba: u64,
        filter: ZoneReportFilter,
        max_zones: usize,
    ) -> Result<ZoneReport, DiskError>;

    /// Applies a zone management operation.
    ///
    /// If `zone_id` is `None`, the operation applies to every eligible zone.
    async fn zone_operation(
        &self,
        operation: ZoneOperation,
        zone_id: Option<u64>,
    ) -> Result<(), DiskError>;
}

/// Immutable properties of a zoned block device.
#[derive(Debug, Copy, Clone, Inspect)]
pub struct ZonedDiskInfo {
    /// Whether reads may cross zone boundaries and access unwritten blocks.
    pub unrestricted_read: bool,
    /// Optimal number of sequential-write-preferred zones to keep open.
    pub optimal_open_sequential_write_preferred_zones: u32,
    /// Optimal number of sequential-write-preferred zones to write non-sequentially.
    pub optimal_nonsequential_sequential_write_preferred_zones: u32,
    /// Maximum number of sequential-write-required zones that may be open.
    pub max_open_sequential_write_required_zones: u32,
}

/// A protocol-neutral zone descriptor.
#[derive(Debug, Copy, Clone, Inspect)]
pub struct ZoneDescriptor {
    /// The zone type.
    pub zone_type: ZoneType,
    /// The current zone condition.
    pub condition: ZoneCondition,
    /// The zone length in logical sectors.
    pub length: u64,
    /// The first logical sector in the zone.
    pub start_lba: u64,
    /// The current write pointer, if valid for the current condition.
    pub write_pointer_lba: Option<u64>,
    /// Whether resetting the write pointer is recommended.
    pub reset_write_pointer_recommended: bool,
    /// Whether the zone contains non-sequentially written data.
    pub non_sequential: bool,
}

/// Results returned by [`ZonedDisk::report_zones`].
#[derive(Debug)]
pub struct ZoneReport {
    /// Descriptors that fit in the caller-provided limit.
    pub zones: Vec<ZoneDescriptor>,
    /// Total number of zones matching the request, including omitted descriptors.
    pub total_zone_count: usize,
}

/// The type of a zone.
#[derive(Debug, Copy, Clone, PartialEq, Eq, Inspect)]
pub enum ZoneType {
    /// A conventional randomly writable zone.
    Conventional,
    /// A zone that must be written sequentially.
    SequentialWriteRequired,
    /// A zone that prefers sequential writes but permits random writes.
    SequentialWritePreferred,
}

/// The current condition of a zone.
#[derive(Debug, Copy, Clone, PartialEq, Eq, Inspect)]
pub enum ZoneCondition {
    /// The zone does not have a write pointer.
    NotWritePointer,
    /// The zone is empty.
    Empty,
    /// The zone is implicitly open.
    ImplicitlyOpened,
    /// The zone is explicitly open.
    ExplicitlyOpened,
    /// The zone is closed.
    Closed,
    /// The zone is read only.
    ReadOnly,
    /// The zone is full.
    Full,
    /// The zone is offline.
    Offline,
}

/// Filter applied to a zone report.
#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub enum ZoneReportFilter {
    /// Report every zone.
    All,
    /// Report empty zones.
    Empty,
    /// Report implicitly open zones.
    ImplicitlyOpened,
    /// Report explicitly open zones.
    ExplicitlyOpened,
    /// Report closed zones.
    Closed,
    /// Report full zones.
    Full,
    /// Report read-only zones.
    ReadOnly,
    /// Report offline zones.
    Offline,
    /// Report zones for which resetting the write pointer is recommended.
    ResetWritePointerRecommended,
    /// Report zones containing non-sequentially written data.
    NonSequential,
    /// Report zones without a write pointer.
    NotWritePointer,
}

/// A zone management operation.
#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub enum ZoneOperation {
    /// Open a zone.
    Open,
    /// Close a zone.
    Close,
    /// Mark a zone full.
    Finish,
    /// Reset a zone write pointer.
    ResetWritePointer,
}

/// A zoned media constraint violation.
#[derive(Debug, Copy, Clone, PartialEq, Eq, Error)]
pub enum ZoneError {
    /// The logical block address is outside the exposed disk.
    #[error("logical block address out of range")]
    LogicalBlockAddressOutOfRange,
    /// A write crossed a zone boundary.
    #[error("write boundary violation")]
    WriteBoundaryViolation,
    /// A write did not begin at the write pointer or meet alignment requirements.
    #[error("unaligned write command")]
    UnalignedWrite,
    /// A read crossed a prohibited zone boundary.
    #[error("read boundary violation")]
    ReadBoundaryViolation,
    /// A read accessed unwritten data.
    #[error("attempt to read invalid data")]
    ReadInvalidData,
    /// The device cannot open another zone.
    #[error("insufficient zone resources")]
    InsufficientResources,
    /// The target zone is read only.
    #[error("zone is read only")]
    ReadOnly,
    /// The target zone is offline.
    #[error("zone is offline")]
    Offline,
    /// The target or operation is invalid for this zoned disk.
    #[error("invalid zone operation")]
    InvalidOperation,
    /// The zoned disk state could not be loaded or persisted safely.
    #[error("internal zoned disk failure")]
    InternalFailure,
}
