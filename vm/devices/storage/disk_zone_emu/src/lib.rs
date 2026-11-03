// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Host-managed zoned block device emulation over an ordinary disk backend.

#![forbid(unsafe_code)]

/// Resource resolver for the zoned disk emulator.
pub mod resolver;

use disk_backend::Disk;
use disk_backend::DiskError;
use disk_backend::DiskIo;
use disk_backend::UnmapBehavior;
use disk_backend::zoned::ZoneCondition as ReportedZoneCondition;
use disk_backend::zoned::ZoneDescriptor;
use disk_backend::zoned::ZoneError;
use disk_backend::zoned::ZoneOperation;
use disk_backend::zoned::ZoneReport;
use disk_backend::zoned::ZoneReportFilter;
use disk_backend::zoned::ZoneType as ReportedZoneType;
use disk_backend::zoned::ZonedDisk as ZonedDiskCapability;
use disk_backend::zoned::ZonedDiskInfo;
use disk_backend_resources::ZoneEmulationParameters;
use futures::lock::Mutex as AsyncMutex;
use guestmem::GuestMemory;
use guestmem::MemoryWrite;
use inspect::Inspect;
use parking_lot::Mutex;
use scsi_buffers::OwnedRequestBuffers;
use scsi_buffers::RequestBuffers;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::AtomicU8;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering;
use thiserror::Error;

const METADATA_MAGIC: &[u8; 8] = b"OVSMR001";
const METADATA_VERSION: u32 = 1;
const METADATA_HEADER_SIZE: usize = 64;
const METADATA_RECORD_SIZE: usize = 16;
const METADATA_CHECKSUM_OFFSET: usize = 60;
const METADATA_FLAG_UNRESTRICTED_READ: u32 = 1;
const ZONE_INSPECT_LIMIT: usize = 256;
const ZONE_INSPECT_EDGE_COUNT: usize = 32;
/// Maximum practical zone count supported by the emulator.
pub const MAX_ZONE_COUNT: u64 = 262_144;

/// Invalid zoned disk emulator configuration.
#[derive(Debug, Error)]
pub enum InvalidZonedConfiguration {
    /// The zone size is invalid.
    #[error("zone size must be a nonzero multiple of the physical sector size")]
    InvalidZoneSize,
    /// The geometry does not contain any sequential-write-required zones.
    #[error("the disk must contain at least one sequential-write-required zone")]
    NoSequentialZones,
    /// The geometry contains more zones than the emulator can represent.
    #[error("the disk has too many zones to represent in REPORT ZONES")]
    TooManyZones,
    /// The backing disk cannot hold the requested geometry and metadata.
    #[error("the disk is too small to reserve zoned metadata")]
    NoMetadataSpace,
}

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
enum ZoneType {
    Conventional,
    SequentialWriteRequired,
}

impl ZoneType {
    fn reported(self) -> ReportedZoneType {
        match self {
            Self::Conventional => ReportedZoneType::Conventional,
            Self::SequentialWriteRequired => ReportedZoneType::SequentialWriteRequired,
        }
    }

    fn name(self) -> &'static str {
        match self {
            Self::Conventional => "conventional",
            Self::SequentialWriteRequired => "sequential_write_required",
        }
    }
}

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
enum ZoneCondition {
    NotWritePointer,
    Empty,
    ImplicitlyOpened,
    ExplicitlyOpened,
    Closed,
    ReadOnly,
    Full,
    Offline,
}

impl ZoneCondition {
    fn value(self) -> u8 {
        match self {
            Self::NotWritePointer => 0,
            Self::Empty => 1,
            Self::ImplicitlyOpened => 2,
            Self::ExplicitlyOpened => 3,
            Self::Closed => 4,
            Self::ReadOnly => 0xd,
            Self::Full => 0xe,
            Self::Offline => 0xf,
        }
    }

    fn is_open(self) -> bool {
        matches!(self, Self::ImplicitlyOpened | Self::ExplicitlyOpened)
    }

    fn name(self) -> &'static str {
        match self {
            Self::NotWritePointer => "not_write_pointer",
            Self::Empty => "empty",
            Self::ImplicitlyOpened => "implicitly_opened",
            Self::ExplicitlyOpened => "explicitly_opened",
            Self::Closed => "closed",
            Self::ReadOnly => "read_only",
            Self::Full => "full",
            Self::Offline => "offline",
        }
    }

    fn from_value(value: u8) -> Option<Self> {
        match value {
            0 => Some(Self::NotWritePointer),
            1 => Some(Self::Empty),
            2 => Some(Self::ImplicitlyOpened),
            3 => Some(Self::ExplicitlyOpened),
            4 => Some(Self::Closed),
            0xd => Some(Self::ReadOnly),
            0xe => Some(Self::Full),
            0xf => Some(Self::Offline),
            _ => None,
        }
    }

    fn reported(self) -> ReportedZoneCondition {
        match self {
            Self::NotWritePointer => ReportedZoneCondition::NotWritePointer,
            Self::Empty => ReportedZoneCondition::Empty,
            Self::ImplicitlyOpened => ReportedZoneCondition::ImplicitlyOpened,
            Self::ExplicitlyOpened => ReportedZoneCondition::ExplicitlyOpened,
            Self::Closed => ReportedZoneCondition::Closed,
            Self::ReadOnly => ReportedZoneCondition::ReadOnly,
            Self::Full => ReportedZoneCondition::Full,
            Self::Offline => ReportedZoneCondition::Offline,
        }
    }
}

#[derive(Copy, Clone, Debug)]
struct ZoneState {
    condition: ZoneCondition,
    // Kept valid internally for every sequential zone, including FULL zones
    // completed by FINISH ZONE. REPORT ZONES masks it when the condition says
    // the write pointer is invalid.
    write_pointer: u64,
}

struct Zone {
    start_lba: u64,
    length: u64,
    zone_type: ZoneType,
    io_lock: AsyncMutex<()>,
    state: Mutex<ZoneState>,
    io_operations: AtomicU64,
    outstanding_io: AtomicU64,
    lock_contention: AtomicU64,
}

struct ZoneIoGuard<'a> {
    _lock: futures::lock::MutexGuard<'a, ()>,
    _outstanding: OutstandingIoGuard<'a>,
}

struct OutstandingIoGuard<'a>(&'a AtomicU64);

impl Drop for OutstandingIoGuard<'_> {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::Relaxed);
    }
}

impl Zone {
    fn begin_io(&self) -> OutstandingIoGuard<'_> {
        self.io_operations.fetch_add(1, Ordering::Relaxed);
        self.outstanding_io.fetch_add(1, Ordering::Relaxed);
        OutstandingIoGuard(&self.outstanding_io)
    }

    async fn lock_io(&self) -> ZoneIoGuard<'_> {
        let outstanding = self.begin_io();
        let lock = match self.io_lock.try_lock() {
            Some(lock) => lock,
            None => {
                self.lock_contention.fetch_add(1, Ordering::Relaxed);
                self.io_lock.lock().await
            }
        };
        ZoneIoGuard {
            _lock: lock,
            _outstanding: outstanding,
        }
    }
}

/// A host-managed zoned disk emulator layered over an ordinary disk.
pub struct ZoneEmulationDisk {
    inner: Disk,
    parameters: ZoneEmulationParameters,
    sector_count: u64,
    zone_size_sectors: u64,
    physical_sector_count: u64,
    metadata_slot_sectors: u64,
    metadata_start_lba: u64,
    zones: Vec<Zone>,
    mutation_lock: AsyncMutex<()>,
    metadata: AsyncMutex<MetadataState>,
    open_zone_count: Mutex<u32>,
    telemetry: ZonedTelemetry,
}

#[derive(Copy, Clone, Debug)]
struct MetadataState {
    loaded: bool,
    generation: u64,
    active_slot: usize,
}

#[derive(Copy, Clone)]
struct ZoneMutation {
    index: usize,
    old_state: ZoneState,
}

enum DecodedMetadata {
    Empty,
    Invalid,
    Valid {
        generation: u64,
        states: Vec<ZoneState>,
    },
}

#[derive(Copy, Clone)]
enum TelemetryOperation {
    None,
    Read,
    Write,
    WriteSame,
    ReportZones,
    OpenZone,
    CloseZone,
    FinishZone,
    ResetWritePointer,
}

impl TelemetryOperation {
    fn name(self) -> &'static str {
        match self {
            Self::None => "none",
            Self::Read => "read",
            Self::Write => "write",
            Self::WriteSame => "write_same",
            Self::ReportZones => "report_zones",
            Self::OpenZone => "open_zone",
            Self::CloseZone => "close_zone",
            Self::FinishZone => "finish_zone",
            Self::ResetWritePointer => "reset_write_pointer",
        }
    }
}

#[derive(Copy, Clone)]
struct LastCommand {
    sequence: u64,
    operation: TelemetryOperation,
    lba: u64,
    sector_count: u64,
    write_pointer: u64,
    result: &'static str,
}

struct ZonedTelemetry {
    sequence: AtomicU64,
    read_commands: AtomicU64,
    read_sectors: AtomicU64,
    write_commands: AtomicU64,
    write_sectors: AtomicU64,
    write_same_commands: AtomicU64,
    write_same_sectors: AtomicU64,
    conventional_read_segments: AtomicU64,
    sequential_read_segments: AtomicU64,
    conventional_write_commands: AtomicU64,
    sequential_write_commands: AtomicU64,
    successful_sequential_writes: AtomicU64,
    report_zones_commands: AtomicU64,
    open_zone_commands: AtomicU64,
    close_zone_commands: AtomicU64,
    implicit_zone_auto_closes: AtomicU64,
    finish_zone_commands: AtomicU64,
    reset_write_pointer_commands: AtomicU64,
    zero_fill_operations: AtomicU64,
    zero_filled_bytes: AtomicU64,
    disk_errors: AtomicU64,
    unaligned_write_errors: AtomicU64,
    write_boundary_errors: AtomicU64,
    read_boundary_errors: AtomicU64,
    read_invalid_data_errors: AtomicU64,
    out_of_range_errors: AtomicU64,
    insufficient_zone_resource_errors: AtomicU64,
    read_only_errors: AtomicU64,
    offline_errors: AtomicU64,
    metadata_loaded: AtomicBool,
    metadata_generation: AtomicU64,
    metadata_active_slot: AtomicU8,
    metadata_recovery_source: AtomicU8,
    metadata_load_attempts: AtomicU64,
    metadata_fresh_initializations: AtomicU64,
    metadata_recoveries: AtomicU64,
    metadata_corruptions: AtomicU64,
    metadata_writes: AtomicU64,
    metadata_flushes: AtomicU64,
    metadata_failures: AtomicU64,
    last_command: Mutex<LastCommand>,
    recent_writes: Mutex<Vec<LastCommand>>,
}

impl ZonedTelemetry {
    fn new() -> Self {
        Self {
            sequence: AtomicU64::new(0),
            read_commands: AtomicU64::new(0),
            read_sectors: AtomicU64::new(0),
            write_commands: AtomicU64::new(0),
            write_sectors: AtomicU64::new(0),
            write_same_commands: AtomicU64::new(0),
            write_same_sectors: AtomicU64::new(0),
            conventional_read_segments: AtomicU64::new(0),
            sequential_read_segments: AtomicU64::new(0),
            conventional_write_commands: AtomicU64::new(0),
            sequential_write_commands: AtomicU64::new(0),
            successful_sequential_writes: AtomicU64::new(0),
            report_zones_commands: AtomicU64::new(0),
            open_zone_commands: AtomicU64::new(0),
            close_zone_commands: AtomicU64::new(0),
            implicit_zone_auto_closes: AtomicU64::new(0),
            finish_zone_commands: AtomicU64::new(0),
            reset_write_pointer_commands: AtomicU64::new(0),
            zero_fill_operations: AtomicU64::new(0),
            zero_filled_bytes: AtomicU64::new(0),
            disk_errors: AtomicU64::new(0),
            unaligned_write_errors: AtomicU64::new(0),
            write_boundary_errors: AtomicU64::new(0),
            read_boundary_errors: AtomicU64::new(0),
            read_invalid_data_errors: AtomicU64::new(0),
            out_of_range_errors: AtomicU64::new(0),
            insufficient_zone_resource_errors: AtomicU64::new(0),
            read_only_errors: AtomicU64::new(0),
            offline_errors: AtomicU64::new(0),
            metadata_loaded: AtomicBool::new(false),
            metadata_generation: AtomicU64::new(0),
            metadata_active_slot: AtomicU8::new(0),
            metadata_recovery_source: AtomicU8::new(0),
            metadata_load_attempts: AtomicU64::new(0),
            metadata_fresh_initializations: AtomicU64::new(0),
            metadata_recoveries: AtomicU64::new(0),
            metadata_corruptions: AtomicU64::new(0),
            metadata_writes: AtomicU64::new(0),
            metadata_flushes: AtomicU64::new(0),
            metadata_failures: AtomicU64::new(0),
            last_command: Mutex::new(LastCommand {
                sequence: 0,
                operation: TelemetryOperation::None,
                lba: 0,
                sector_count: 0,
                write_pointer: 0,
                result: "none",
            }),
            recent_writes: Mutex::new(Vec::with_capacity(16)),
        }
    }
}

impl ZoneEmulationDisk {
    /// Creates a zoned disk emulator over `inner`.
    pub fn new(
        inner: Disk,
        parameters: ZoneEmulationParameters,
    ) -> Result<Self, InvalidZonedConfiguration> {
        let backing_sector_count = inner.sector_count();
        let sector_size = inner.sector_size();
        let physical_sector_size = inner.physical_sector_size();
        if parameters.zone_size_bytes == 0
            || !parameters
                .zone_size_bytes
                .is_multiple_of(u64::from(physical_sector_size))
        {
            return Err(InvalidZonedConfiguration::InvalidZoneSize);
        }

        let zone_size_sectors = parameters.zone_size_bytes / u64::from(sector_size);
        let physical_sector_count = u64::from(physical_sector_size / sector_size);
        let metadata_end_lba = backing_sector_count / physical_sector_count * physical_sector_count;
        let mut metadata_slot_sectors = physical_sector_count;
        let (sector_count, zone_count, metadata_start_lba) = loop {
            let metadata_sectors = metadata_slot_sectors
                .checked_mul(2)
                .ok_or(InvalidZonedConfiguration::NoMetadataSpace)?;
            let metadata_start_lba = metadata_end_lba
                .checked_sub(metadata_sectors)
                .ok_or(InvalidZonedConfiguration::NoMetadataSpace)?;
            let sector_count = metadata_start_lba / zone_size_sectors * zone_size_sectors;
            let zone_count = sector_count / zone_size_sectors;
            let metadata_bytes = METADATA_HEADER_SIZE
                .checked_add(
                    usize::try_from(zone_count)
                        .map_err(|_| InvalidZonedConfiguration::TooManyZones)?
                        .checked_mul(METADATA_RECORD_SIZE)
                        .ok_or(InvalidZonedConfiguration::TooManyZones)?,
                )
                .ok_or(InvalidZonedConfiguration::TooManyZones)?;
            let required_sectors = (metadata_bytes as u64).div_ceil(u64::from(sector_size));
            let required_slot_sectors =
                required_sectors.div_ceil(physical_sector_count) * physical_sector_count;
            if required_slot_sectors <= metadata_slot_sectors {
                break (sector_count, zone_count, metadata_start_lba);
            }
            metadata_slot_sectors = required_slot_sectors;
        };

        if zone_count > MAX_ZONE_COUNT {
            return Err(InvalidZonedConfiguration::TooManyZones);
        }
        if u64::from(parameters.conventional_zone_count) >= zone_count {
            return Err(InvalidZonedConfiguration::NoSequentialZones);
        }

        let mut zones = Vec::with_capacity(zone_count as usize);
        for index in 0..zone_count {
            let start_lba = index * zone_size_sectors;
            let zone_type = if index < u64::from(parameters.conventional_zone_count) {
                ZoneType::Conventional
            } else {
                ZoneType::SequentialWriteRequired
            };
            let condition = match zone_type {
                ZoneType::Conventional => ZoneCondition::NotWritePointer,
                ZoneType::SequentialWriteRequired => ZoneCondition::Empty,
            };
            zones.push(Zone {
                start_lba,
                length: zone_size_sectors,
                zone_type,
                io_lock: AsyncMutex::new(()),
                state: Mutex::new(ZoneState {
                    condition,
                    write_pointer: start_lba,
                }),
                io_operations: AtomicU64::new(0),
                outstanding_io: AtomicU64::new(0),
                lock_contention: AtomicU64::new(0),
            });
        }

        Ok(Self {
            inner,
            parameters,
            sector_count,
            zone_size_sectors,
            physical_sector_count,
            metadata_slot_sectors,
            metadata_start_lba,
            zones,
            mutation_lock: AsyncMutex::new(()),
            metadata: AsyncMutex::new(MetadataState {
                loaded: false,
                generation: 0,
                active_slot: 1,
            }),
            open_zone_count: Mutex::new(0),
            telemetry: ZonedTelemetry::new(),
        })
    }

    fn record_result<T>(
        &self,
        operation: TelemetryOperation,
        lba: u64,
        sector_count: u64,
        result: &Result<T, DiskError>,
    ) {
        let sequence = self.telemetry.sequence.fetch_add(1, Ordering::Relaxed) + 1;
        let mut last = LastCommand {
            sequence,
            operation,
            lba,
            sector_count,
            write_pointer: 0,
            result: "success",
        };

        if let Err(error) = result {
            match error {
                DiskError::Zone(error) => {
                    last.result = "zone_error";
                    match error {
                        ZoneError::UnalignedWrite => &self.telemetry.unaligned_write_errors,
                        ZoneError::WriteBoundaryViolation => &self.telemetry.write_boundary_errors,
                        ZoneError::ReadBoundaryViolation => &self.telemetry.read_boundary_errors,
                        ZoneError::ReadInvalidData => &self.telemetry.read_invalid_data_errors,
                        ZoneError::LogicalBlockAddressOutOfRange => {
                            &self.telemetry.out_of_range_errors
                        }
                        ZoneError::InsufficientResources => {
                            &self.telemetry.insufficient_zone_resource_errors
                        }
                        ZoneError::ReadOnly => &self.telemetry.read_only_errors,
                        ZoneError::Offline => &self.telemetry.offline_errors,
                        ZoneError::InvalidOperation | ZoneError::InternalFailure => {
                            &self.telemetry.disk_errors
                        }
                    }
                    .fetch_add(1, Ordering::Relaxed);
                }
                _ => {
                    self.telemetry.disk_errors.fetch_add(1, Ordering::Relaxed);
                    last.result = "disk_error";
                }
            }
        }

        if matches!(
            operation,
            TelemetryOperation::Write | TelemetryOperation::WriteSame
        ) {
            if let Some(index) = self.zone_index(lba) {
                last.write_pointer = self.zones[index].state.lock().write_pointer;
            }
            let mut recent_writes = self.telemetry.recent_writes.lock();
            if recent_writes.len() == 16 {
                recent_writes.remove(0);
            }
            recent_writes.push(last);
        }
        *self.telemetry.last_command.lock() = last;
    }

    fn metadata_error() -> DiskError {
        ZoneError::InternalFailure.into()
    }

    fn metadata_slot_bytes(&self, sector_size: u32) -> Result<usize, DiskError> {
        usize::try_from(self.metadata_slot_sectors)
            .ok()
            .and_then(|sectors| sectors.checked_mul(sector_size as usize))
            .ok_or(DiskError::InvalidInput)
    }

    fn metadata_slot_lba(&self, slot: usize) -> u64 {
        self.metadata_start_lba + slot as u64 * self.metadata_slot_sectors
    }

    async fn read_metadata_slot(&self, slot: usize) -> Result<Vec<u8>, DiskError> {
        let slot_bytes = self.metadata_slot_bytes(self.inner.sector_size())?;
        let memory = GuestMemory::allocate(slot_bytes);
        let buffers = OwnedRequestBuffers::linear(0, slot_bytes, true);
        self.inner
            .read_vectored(&buffers.buffer(&memory), self.metadata_slot_lba(slot))
            .await?;
        let mut bytes = vec![0; slot_bytes];
        memory
            .read_at(0, &mut bytes)
            .map_err(|_| DiskError::InvalidInput)?;
        Ok(bytes)
    }

    fn decode_metadata(&self, bytes: &[u8]) -> DecodedMetadata {
        if bytes.iter().all(|byte| *byte == 0) {
            return DecodedMetadata::Empty;
        }
        if bytes.len() < METADATA_HEADER_SIZE
            || &bytes[0..8] != METADATA_MAGIC
            || u32::from_le_bytes(bytes[8..12].try_into().unwrap()) != METADATA_VERSION
            || u32::from_le_bytes(bytes[12..16].try_into().unwrap()) != METADATA_HEADER_SIZE as u32
            || u64::from_le_bytes(bytes[24..32].try_into().unwrap()) != self.sector_count
            || u64::from_le_bytes(bytes[32..40].try_into().unwrap()) != self.zone_size_sectors
            || u32::from_le_bytes(bytes[40..44].try_into().unwrap()) as usize != self.zones.len()
            || u32::from_le_bytes(bytes[44..48].try_into().unwrap())
                != self.parameters.conventional_zone_count
            || u32::from_le_bytes(bytes[48..52].try_into().unwrap())
                != self.parameters.max_open_zones
            || u32::from_le_bytes(bytes[52..56].try_into().unwrap())
                != if self.parameters.unrestricted_read {
                    METADATA_FLAG_UNRESTRICTED_READ
                } else {
                    0
                }
            || u32::from_le_bytes(bytes[56..60].try_into().unwrap()) as usize
                != self.zones.len() * METADATA_RECORD_SIZE
        {
            return DecodedMetadata::Invalid;
        }

        let expected_checksum =
            u32::from_le_bytes(bytes[METADATA_CHECKSUM_OFFSET..64].try_into().unwrap());
        let mut hasher = crc32fast::Hasher::new();
        hasher.update(&bytes[..METADATA_CHECKSUM_OFFSET]);
        hasher.update(&[0; 4]);
        hasher.update(&bytes[64..]);
        if hasher.finalize() != expected_checksum {
            return DecodedMetadata::Invalid;
        }

        let mut states = Vec::with_capacity(self.zones.len());
        let mut open_zone_count = 0u32;
        for (index, zone) in self.zones.iter().enumerate() {
            let offset = METADATA_HEADER_SIZE + index * METADATA_RECORD_SIZE;
            let record = &bytes[offset..offset + METADATA_RECORD_SIZE];
            let Some(condition) = ZoneCondition::from_value(record[0]) else {
                return DecodedMetadata::Invalid;
            };
            let write_pointer = u64::from_le_bytes(record[8..16].try_into().unwrap());
            if (zone.zone_type == ZoneType::Conventional
                && condition != ZoneCondition::NotWritePointer)
                || (zone.zone_type == ZoneType::SequentialWriteRequired
                    && condition == ZoneCondition::NotWritePointer)
                || write_pointer < zone.start_lba
                || write_pointer > zone.start_lba + zone.length
            {
                return DecodedMetadata::Invalid;
            }
            if condition.is_open() {
                open_zone_count += 1;
            }
            states.push(ZoneState {
                condition,
                write_pointer,
            });
        }
        if open_zone_count > self.parameters.max_open_zones {
            return DecodedMetadata::Invalid;
        }

        DecodedMetadata::Valid {
            generation: u64::from_le_bytes(bytes[16..24].try_into().unwrap()),
            states,
        }
    }

    fn apply_metadata_states(&self, states: &[ZoneState]) {
        for (zone, state) in self.zones.iter().zip(states) {
            *zone.state.lock() = *state;
        }
        self.refresh_open_zone_count();
    }

    fn refresh_open_zone_count(&self) {
        let open_zone_count = self
            .zones
            .iter()
            .filter(|zone| zone.state.lock().condition.is_open())
            .count() as u32;
        *self.open_zone_count.lock() = open_zone_count;
    }

    fn rollback_mutations(&self, mutations: &[ZoneMutation]) {
        for mutation in mutations.iter().rev() {
            *self.zones[mutation.index].state.lock() = mutation.old_state;
        }
        self.refresh_open_zone_count();
    }

    async fn make_data_durable(&self, fua: bool) -> Result<(), DiskError> {
        if fua && self.inner.is_fua_respected() {
            return Ok(());
        }
        self.inner.sync_cache().await
    }

    async fn ensure_metadata_loaded(&self) -> Result<(), DiskError> {
        let mut metadata = self.metadata.lock().await;
        if metadata.loaded {
            return Ok(());
        }

        self.telemetry
            .metadata_load_attempts
            .fetch_add(1, Ordering::Relaxed);
        let first = self.read_metadata_slot(0).await.inspect_err(|_| {
            self.telemetry
                .metadata_failures
                .fetch_add(1, Ordering::Relaxed);
        })?;
        let second = self.read_metadata_slot(1).await.inspect_err(|_| {
            self.telemetry
                .metadata_failures
                .fetch_add(1, Ordering::Relaxed);
        })?;
        let decoded = [self.decode_metadata(&first), self.decode_metadata(&second)];
        let mut newest: Option<(usize, u64, Vec<ZoneState>)> = None;
        let mut all_empty = true;
        for (slot, decoded) in decoded.into_iter().enumerate() {
            match decoded {
                DecodedMetadata::Empty => {}
                DecodedMetadata::Invalid => all_empty = false,
                DecodedMetadata::Valid { generation, states } => {
                    all_empty = false;
                    if newest
                        .as_ref()
                        .is_none_or(|(_, current_generation, _)| generation > *current_generation)
                    {
                        newest = Some((slot, generation, states));
                    }
                }
            }
        }

        if let Some((slot, generation, states)) = newest {
            self.apply_metadata_states(&states);
            metadata.generation = generation;
            metadata.active_slot = slot;
            self.telemetry
                .metadata_recoveries
                .fetch_add(1, Ordering::Relaxed);
            self.telemetry
                .metadata_recovery_source
                .store(slot as u8 + 2, Ordering::Relaxed);
        } else if !all_empty {
            self.telemetry
                .metadata_corruptions
                .fetch_add(1, Ordering::Relaxed);
            self.telemetry
                .metadata_failures
                .fetch_add(1, Ordering::Relaxed);
            self.telemetry
                .metadata_recovery_source
                .store(4, Ordering::Relaxed);
            return Err(Self::metadata_error());
        } else {
            self.telemetry
                .metadata_fresh_initializations
                .fetch_add(1, Ordering::Relaxed);
            self.telemetry
                .metadata_recovery_source
                .store(1, Ordering::Relaxed);
        }
        metadata.loaded = true;
        self.telemetry
            .metadata_loaded
            .store(true, Ordering::Relaxed);
        self.telemetry
            .metadata_generation
            .store(metadata.generation, Ordering::Relaxed);
        self.telemetry
            .metadata_active_slot
            .store(metadata.active_slot as u8, Ordering::Relaxed);
        Ok(())
    }

    fn encode_metadata(&self, generation: u64, slot_bytes: usize) -> Vec<u8> {
        let mut bytes = vec![0; slot_bytes];
        bytes[0..8].copy_from_slice(METADATA_MAGIC);
        bytes[8..12].copy_from_slice(&METADATA_VERSION.to_le_bytes());
        bytes[12..16].copy_from_slice(&(METADATA_HEADER_SIZE as u32).to_le_bytes());
        bytes[16..24].copy_from_slice(&generation.to_le_bytes());
        bytes[24..32].copy_from_slice(&self.sector_count.to_le_bytes());
        bytes[32..40].copy_from_slice(&self.zone_size_sectors.to_le_bytes());
        bytes[40..44].copy_from_slice(&(self.zones.len() as u32).to_le_bytes());
        bytes[44..48].copy_from_slice(&self.parameters.conventional_zone_count.to_le_bytes());
        bytes[48..52].copy_from_slice(&self.parameters.max_open_zones.to_le_bytes());
        let flags = if self.parameters.unrestricted_read {
            METADATA_FLAG_UNRESTRICTED_READ
        } else {
            0
        };
        bytes[52..56].copy_from_slice(&flags.to_le_bytes());
        bytes[56..60]
            .copy_from_slice(&((self.zones.len() * METADATA_RECORD_SIZE) as u32).to_le_bytes());

        for (index, zone) in self.zones.iter().enumerate() {
            let state = *zone.state.lock();
            let offset = METADATA_HEADER_SIZE + index * METADATA_RECORD_SIZE;
            bytes[offset] = state.condition.value();
            bytes[offset + 8..offset + 16].copy_from_slice(&state.write_pointer.to_le_bytes());
        }

        let mut hasher = crc32fast::Hasher::new();
        hasher.update(&bytes[..METADATA_CHECKSUM_OFFSET]);
        hasher.update(&[0; 4]);
        hasher.update(&bytes[64..]);
        bytes[METADATA_CHECKSUM_OFFSET..64].copy_from_slice(&hasher.finalize().to_le_bytes());
        bytes
    }

    async fn persist_metadata(&self) -> Result<(), DiskError> {
        let mut metadata = self.metadata.lock().await;
        let generation = metadata.generation.wrapping_add(1);
        let slot = metadata.active_slot ^ 1;
        let slot_bytes = self.metadata_slot_bytes(self.inner.sector_size())?;
        let bytes = self.encode_metadata(generation, slot_bytes);
        let memory = GuestMemory::allocate(slot_bytes);
        memory.write_at(0, &bytes).map_err(|_| {
            self.telemetry
                .metadata_failures
                .fetch_add(1, Ordering::Relaxed);
            DiskError::InvalidInput
        })?;
        let buffers = OwnedRequestBuffers::linear(0, slot_bytes, true);
        self.inner
            .write_vectored(
                &buffers.buffer(&memory),
                self.metadata_slot_lba(slot),
                false,
            )
            .await
            .inspect_err(|_| {
                self.telemetry
                    .metadata_failures
                    .fetch_add(1, Ordering::Relaxed);
            })?;
        self.telemetry
            .metadata_writes
            .fetch_add(1, Ordering::Relaxed);
        self.inner.sync_cache().await.inspect_err(|_| {
            self.telemetry
                .metadata_failures
                .fetch_add(1, Ordering::Relaxed);
        })?;
        self.telemetry
            .metadata_flushes
            .fetch_add(1, Ordering::Relaxed);
        metadata.generation = generation;
        metadata.active_slot = slot;
        self.telemetry
            .metadata_generation
            .store(generation, Ordering::Relaxed);
        self.telemetry
            .metadata_active_slot
            .store(slot as u8, Ordering::Relaxed);
        Ok(())
    }

    fn zone_index(&self, lba: u64) -> Option<usize> {
        if lba >= self.sector_count {
            None
        } else {
            Some((lba / self.zone_size_sectors) as usize)
        }
    }

    fn zone_index_for_start(&self, lba: u64) -> Option<usize> {
        let index = self.zone_index(lba)?;
        (self.zones[index].start_lba == lba).then_some(index)
    }

    fn insufficient_zone_resources() -> DiskError {
        ZoneError::InsufficientResources.into()
    }

    fn zone_read_only() -> DiskError {
        ZoneError::ReadOnly.into()
    }

    fn zone_offline() -> DiskError {
        ZoneError::Offline.into()
    }

    fn reserve_open_zone(
        &self,
        target_index: usize,
        mutations: &mut Vec<ZoneMutation>,
    ) -> Result<(), DiskError> {
        let mut count = self.open_zone_count.lock();
        if *count < self.parameters.max_open_zones {
            *count += 1;
            return Ok(());
        }

        for (index, zone) in self.zones.iter().enumerate() {
            if index == target_index || zone.outstanding_io.load(Ordering::Acquire) != 0 {
                continue;
            }

            // Do not wait for a zone state lock while holding the open-zone count lock.
            // Other paths take these locks in the opposite order.
            let Some(mut state) = zone.state.try_lock() else {
                continue;
            };
            if zone.outstanding_io.load(Ordering::Acquire) == 0
                && state.condition == ZoneCondition::ImplicitlyOpened
            {
                mutations.push(ZoneMutation {
                    index,
                    old_state: *state,
                });
                state.condition = ZoneCondition::Closed;
                self.telemetry
                    .implicit_zone_auto_closes
                    .fetch_add(1, Ordering::Relaxed);
                return Ok(());
            }
        }

        Err(Self::insufficient_zone_resources())
    }

    fn release_open_zone(&self) {
        let mut count = self.open_zone_count.lock();
        assert!(*count > 0);
        *count -= 1;
    }

    fn validate_sequential_write(
        &self,
        zone: &Zone,
        state: ZoneState,
        start_lba: u64,
        sector_count: u64,
    ) -> Result<bool, DiskError> {
        let end_lba = start_lba
            .checked_add(sector_count)
            .ok_or(ZoneError::WriteBoundaryViolation)?;
        if end_lba > zone.start_lba + zone.length {
            return Err(ZoneError::WriteBoundaryViolation.into());
        }
        if start_lba != state.write_pointer || end_lba % self.physical_sector_count != 0 {
            return Err(ZoneError::UnalignedWrite.into());
        }
        match state.condition {
            ZoneCondition::ReadOnly => Err(Self::zone_read_only()),
            ZoneCondition::Offline => Err(Self::zone_offline()),
            ZoneCondition::Full => Err(ZoneError::UnalignedWrite.into()),
            ZoneCondition::Empty | ZoneCondition::Closed => Ok(true),
            ZoneCondition::ImplicitlyOpened | ZoneCondition::ExplicitlyOpened => Ok(false),
            ZoneCondition::NotWritePointer => unreachable!(),
        }
    }

    fn begin_write(
        &self,
        zone_index: usize,
        start_lba: u64,
        sector_count: u64,
        mutations: &mut Vec<ZoneMutation>,
    ) -> Result<ZoneState, DiskError> {
        let zone = &self.zones[zone_index];
        let mut state = zone.state.lock();
        let old_state = *state;
        if self.validate_sequential_write(zone, old_state, start_lba, sector_count)? {
            self.reserve_open_zone(zone_index, mutations)?;
            mutations.push(ZoneMutation {
                index: zone_index,
                old_state,
            });
            state.condition = ZoneCondition::ImplicitlyOpened;
        } else {
            mutations.push(ZoneMutation {
                index: zone_index,
                old_state,
            });
        }
        Ok(old_state)
    }

    fn complete_write(&self, zone: &Zone, old_state: ZoneState, sector_count: u64) {
        let mut state = zone.state.lock();
        state.write_pointer = old_state.write_pointer + sector_count;
        if state.write_pointer == zone.start_lba + zone.length {
            if state.condition.is_open() {
                self.release_open_zone();
            }
            state.condition = ZoneCondition::Full;
        }
        self.telemetry
            .successful_sequential_writes
            .fetch_add(1, Ordering::Relaxed);
    }

    async fn write(
        &self,
        buffers: &RequestBuffers<'_>,
        start_lba: u64,
        sector_count: u64,
        fua: bool,
    ) -> Result<(), DiskError> {
        self.telemetry
            .write_commands
            .fetch_add(1, Ordering::Relaxed);
        self.telemetry
            .write_sectors
            .fetch_add(sector_count, Ordering::Relaxed);
        if let Some(index) = self.zone_index(start_lba) {
            match self.zones[index].zone_type {
                ZoneType::Conventional => {
                    self.telemetry
                        .conventional_write_commands
                        .fetch_add(1, Ordering::Relaxed);
                }
                ZoneType::SequentialWriteRequired => {
                    self.telemetry
                        .sequential_write_commands
                        .fetch_add(1, Ordering::Relaxed);
                }
            }
        }
        let result = self
            .write_inner(buffers, start_lba, sector_count, fua)
            .await;
        self.record_result(TelemetryOperation::Write, start_lba, sector_count, &result);
        result
    }

    async fn write_inner(
        &self,
        buffers: &RequestBuffers<'_>,
        start_lba: u64,
        sector_count: u64,
        fua: bool,
    ) -> Result<(), DiskError> {
        self.ensure_metadata_loaded().await?;
        let index = self
            .zone_index(start_lba)
            .ok_or(ZoneError::LogicalBlockAddressOutOfRange)?;
        let zone = &self.zones[index];
        if zone.zone_type == ZoneType::Conventional {
            let end_lba = start_lba
                .checked_add(sector_count)
                .ok_or(ZoneError::WriteBoundaryViolation)?;
            if end_lba > zone.start_lba + zone.length {
                return Err(ZoneError::WriteBoundaryViolation.into());
            }
            let _outstanding = zone.begin_io();
            return self.inner.write_vectored(buffers, start_lba, fua).await;
        }

        // Keep the data write, zone transition, and journal commit in one
        // transaction so a metadata snapshot cannot mix concurrent states.
        let _mutation_guard = self.mutation_lock.lock().await;
        let _guard = zone.lock_io().await;
        let mut mutations = Vec::with_capacity(2);
        let old_state = self.begin_write(index, start_lba, sector_count, &mut mutations)?;
        match self.inner.write_vectored(buffers, start_lba, fua).await {
            Ok(()) => {
                if let Err(err) = self.make_data_durable(fua).await {
                    self.rollback_mutations(&mutations);
                    return Err(err);
                }
                self.complete_write(zone, old_state, sector_count);
                if let Err(err) = self.persist_metadata().await {
                    self.rollback_mutations(&mutations);
                    return Err(err);
                }
                Ok(())
            }
            Err(err) => {
                self.rollback_mutations(&mutations);
                Err(err)
            }
        }
    }

    async fn write_same_inner(
        &self,
        buffer: &RequestBuffers<'_>,
        start_lba: u64,
        sector_count: u64,
        fua: bool,
    ) -> Result<(), DiskError> {
        self.telemetry
            .write_same_commands
            .fetch_add(1, Ordering::Relaxed);
        self.telemetry
            .write_same_sectors
            .fetch_add(sector_count, Ordering::Relaxed);
        let result = self
            .write_same_transaction(buffer, start_lba, sector_count, fua)
            .await;
        self.record_result(
            TelemetryOperation::WriteSame,
            start_lba,
            sector_count,
            &result,
        );
        result
    }

    async fn write_same_transaction(
        &self,
        buffer: &RequestBuffers<'_>,
        start_lba: u64,
        sector_count: u64,
        fua: bool,
    ) -> Result<(), DiskError> {
        self.ensure_metadata_loaded().await?;
        let index = self
            .zone_index(start_lba)
            .ok_or(ZoneError::LogicalBlockAddressOutOfRange)?;
        let zone = &self.zones[index];
        if zone.zone_type == ZoneType::Conventional {
            let end_lba = start_lba
                .checked_add(sector_count)
                .ok_or(ZoneError::WriteBoundaryViolation)?;
            if end_lba > zone.start_lba + zone.length {
                return Err(ZoneError::WriteBoundaryViolation.into());
            }
            let _outstanding = zone.begin_io();
            for lba in start_lba..end_lba {
                self.inner.write_vectored(buffer, lba, fua).await?;
            }
            return Ok(());
        }

        let _mutation_guard = self.mutation_lock.lock().await;
        let _guard = zone.lock_io().await;
        let mut mutations = Vec::with_capacity(2);
        let old_state = self.begin_write(index, start_lba, sector_count, &mut mutations)?;
        let end_lba = start_lba + sector_count;
        for lba in start_lba..end_lba {
            if let Err(err) = self.inner.write_vectored(buffer, lba, fua).await {
                self.rollback_mutations(&mutations);
                return Err(err);
            }
        }
        if let Err(err) = self.make_data_durable(fua).await {
            self.rollback_mutations(&mutations);
            return Err(err);
        }
        self.complete_write(zone, old_state, sector_count);
        if let Err(err) = self.persist_metadata().await {
            self.rollback_mutations(&mutations);
            return Err(err);
        }
        Ok(())
    }

    async fn read(
        &self,
        buffers: &RequestBuffers<'_>,
        start_lba: u64,
        sector_count: u64,
    ) -> Result<(), DiskError> {
        self.telemetry.read_commands.fetch_add(1, Ordering::Relaxed);
        self.telemetry
            .read_sectors
            .fetch_add(sector_count, Ordering::Relaxed);
        let result = self.read_inner(buffers, start_lba, sector_count).await;
        self.record_result(TelemetryOperation::Read, start_lba, sector_count, &result);
        result
    }

    async fn read_inner(
        &self,
        buffers: &RequestBuffers<'_>,
        start_lba: u64,
        sector_count: u64,
    ) -> Result<(), DiskError> {
        self.ensure_metadata_loaded().await?;
        let end_lba = start_lba
            .checked_add(sector_count)
            .ok_or(ZoneError::ReadBoundaryViolation)?;
        let mut lba = start_lba;
        let mut byte_offset = 0;
        let sector_size = self.inner.sector_size() as usize;

        while lba < end_lba {
            let index = self
                .zone_index(lba)
                .ok_or(ZoneError::LogicalBlockAddressOutOfRange)?;
            let zone = &self.zones[index];
            let zone_end = zone.start_lba + zone.length;
            let segment_end = end_lba.min(zone_end);
            let segment_sectors = segment_end - lba;
            let segment_bytes = segment_sectors as usize * sector_size;
            let segment = buffers.subrange(byte_offset, segment_bytes);

            if zone.zone_type == ZoneType::Conventional {
                self.telemetry
                    .conventional_read_segments
                    .fetch_add(1, Ordering::Relaxed);
                let _outstanding = zone.begin_io();
                self.inner.read_vectored(&segment, lba).await?;
            } else {
                self.telemetry
                    .sequential_read_segments
                    .fetch_add(1, Ordering::Relaxed);
                if !self.parameters.unrestricted_read && end_lba > zone_end {
                    return Err(ZoneError::ReadBoundaryViolation.into());
                }

                let _guard = zone.lock_io().await;
                let state = *zone.state.lock();
                if state.condition == ZoneCondition::Offline {
                    return Err(Self::zone_offline());
                }

                let written_end = state.write_pointer.clamp(lba, segment_end);
                if lba < written_end {
                    let written_sectors = written_end - lba;
                    let written_bytes = written_sectors as usize * sector_size;
                    self.inner
                        .read_vectored(&segment.subrange(0, written_bytes), lba)
                        .await?;
                }

                if written_end < segment_end {
                    if !self.parameters.unrestricted_read {
                        return Err(ZoneError::ReadInvalidData.into());
                    }
                    let zero_offset = (written_end - lba) as usize * sector_size;
                    let zero_len = (segment_end - written_end) as usize * sector_size;
                    self.telemetry
                        .zero_fill_operations
                        .fetch_add(1, Ordering::Relaxed);
                    self.telemetry
                        .zero_filled_bytes
                        .fetch_add(zero_len as u64, Ordering::Relaxed);
                    let zeroes = vec![0; zero_len];
                    segment
                        .subrange(zero_offset, zero_len)
                        .writer()
                        .write(&zeroes)?;
                }
            }

            lba = segment_end;
            byte_offset += segment_bytes;
        }

        Ok(())
    }

    fn matches_reporting_option(
        &self,
        filter: ZoneReportFilter,
        zone: &Zone,
        state: ZoneState,
    ) -> bool {
        match filter {
            ZoneReportFilter::All => true,
            ZoneReportFilter::Empty => state.condition == ZoneCondition::Empty,
            ZoneReportFilter::ImplicitlyOpened => {
                state.condition == ZoneCondition::ImplicitlyOpened
            }
            ZoneReportFilter::ExplicitlyOpened => {
                state.condition == ZoneCondition::ExplicitlyOpened
            }
            ZoneReportFilter::Closed => state.condition == ZoneCondition::Closed,
            ZoneReportFilter::Full => state.condition == ZoneCondition::Full,
            ZoneReportFilter::ReadOnly => state.condition == ZoneCondition::ReadOnly,
            ZoneReportFilter::Offline => state.condition == ZoneCondition::Offline,
            ZoneReportFilter::ResetWritePointerRecommended | ZoneReportFilter::NonSequential => {
                false
            }
            ZoneReportFilter::NotWritePointer => zone.zone_type == ZoneType::Conventional,
        }
    }

    async fn report_zones_inner(
        &self,
        start_lba: u64,
        filter: ZoneReportFilter,
        max_zones: usize,
    ) -> Result<ZoneReport, DiskError> {
        self.telemetry
            .report_zones_commands
            .fetch_add(1, Ordering::Relaxed);
        let result = self
            .report_zones_transaction(start_lba, filter, max_zones)
            .await;
        self.record_result(
            TelemetryOperation::ReportZones,
            start_lba,
            max_zones as u64,
            &result,
        );
        result
    }

    async fn report_zones_transaction(
        &self,
        start_lba: u64,
        filter: ZoneReportFilter,
        max_zones: usize,
    ) -> Result<ZoneReport, DiskError> {
        self.ensure_metadata_loaded().await?;
        let _mutation_guard = self.mutation_lock.lock().await;
        let start_index = self
            .zone_index(start_lba)
            .ok_or(ZoneError::LogicalBlockAddressOutOfRange)?;
        let mut total_zone_count = 0;
        let mut descriptors = Vec::with_capacity(max_zones.min(self.zones.len()));
        for zone in &self.zones[start_index..] {
            let state = *zone.state.lock();
            if !self.matches_reporting_option(filter, zone, state) {
                continue;
            }

            total_zone_count += 1;
            if descriptors.len() >= max_zones {
                continue;
            }

            let write_pointer_lba = match state.condition {
                ZoneCondition::NotWritePointer
                | ZoneCondition::ReadOnly
                | ZoneCondition::Full
                | ZoneCondition::Offline => None,
                _ => Some(state.write_pointer),
            };
            descriptors.push(ZoneDescriptor {
                zone_type: zone.zone_type.reported(),
                condition: state.condition.reported(),
                length: zone.length,
                start_lba: zone.start_lba,
                write_pointer_lba,
                reset_write_pointer_recommended: false,
                non_sequential: false,
            });
        }

        Ok(ZoneReport {
            zones: descriptors,
            total_zone_count,
        })
    }

    async fn zone_operation_inner(
        &self,
        operation: ZoneOperation,
        zone_id: Option<u64>,
    ) -> Result<(), DiskError> {
        let telemetry_operation = match operation {
            ZoneOperation::Close => TelemetryOperation::CloseZone,
            ZoneOperation::Finish => TelemetryOperation::FinishZone,
            ZoneOperation::Open => TelemetryOperation::OpenZone,
            ZoneOperation::ResetWritePointer => TelemetryOperation::ResetWritePointer,
        };
        match telemetry_operation {
            TelemetryOperation::OpenZone => {
                self.telemetry
                    .open_zone_commands
                    .fetch_add(1, Ordering::Relaxed);
            }
            TelemetryOperation::CloseZone => {
                self.telemetry
                    .close_zone_commands
                    .fetch_add(1, Ordering::Relaxed);
            }
            TelemetryOperation::FinishZone => {
                self.telemetry
                    .finish_zone_commands
                    .fetch_add(1, Ordering::Relaxed);
            }
            TelemetryOperation::ResetWritePointer => {
                self.telemetry
                    .reset_write_pointer_commands
                    .fetch_add(1, Ordering::Relaxed);
            }
            _ => {}
        }
        let result = self.zone_operation_transaction(operation, zone_id).await;
        self.record_result(telemetry_operation, zone_id.unwrap_or(0), 0, &result);
        result
    }

    async fn zone_operation_transaction(
        &self,
        operation: ZoneOperation,
        zone_id: Option<u64>,
    ) -> Result<(), DiskError> {
        self.ensure_metadata_loaded().await?;
        let _mutation_guard = self.mutation_lock.lock().await;
        let mut mutations = Vec::new();
        let mut changed = false;
        if zone_id.is_none() {
            for index in 0..self.zones.len() {
                let zone = &self.zones[index];
                if zone.zone_type == ZoneType::SequentialWriteRequired {
                    match self
                        .apply_action(index, operation, true, &mut mutations)
                        .await
                    {
                        Ok(zone_changed) => changed |= zone_changed,
                        Err(err) => {
                            self.rollback_mutations(&mutations);
                            return Err(err);
                        }
                    }
                }
            }
        } else {
            let index = self
                .zone_index_for_start(zone_id.unwrap())
                .ok_or(ZoneError::InvalidOperation)?;
            let zone = &self.zones[index];
            if zone.zone_type != ZoneType::SequentialWriteRequired {
                return Err(ZoneError::InvalidOperation.into());
            }
            changed = match self
                .apply_action(index, operation, false, &mut mutations)
                .await
            {
                Ok(changed) => changed,
                Err(err) => {
                    self.rollback_mutations(&mutations);
                    return Err(err);
                }
            };
        }
        if changed {
            if let Err(err) = self.persist_metadata().await {
                self.rollback_mutations(&mutations);
                return Err(err);
            }
        }

        Ok(())
    }

    async fn apply_action(
        &self,
        zone_index: usize,
        action: ZoneOperation,
        all: bool,
        mutations: &mut Vec<ZoneMutation>,
    ) -> Result<bool, DiskError> {
        let zone = &self.zones[zone_index];
        let _guard = zone.lock_io().await;
        let mut state = zone.state.lock();

        match state.condition {
            ZoneCondition::ReadOnly => return Err(Self::zone_read_only()),
            ZoneCondition::Offline => return Err(Self::zone_offline()),
            _ => {}
        }

        match action {
            ZoneOperation::Close => {
                if state.condition.is_open() {
                    mutations.push(ZoneMutation {
                        index: zone_index,
                        old_state: *state,
                    });
                    self.release_open_zone();
                    state.condition = ZoneCondition::Closed;
                    return Ok(true);
                }
            }
            ZoneOperation::Finish => {
                if all
                    && !matches!(
                        state.condition,
                        ZoneCondition::ImplicitlyOpened
                            | ZoneCondition::ExplicitlyOpened
                            | ZoneCondition::Closed
                    )
                {
                    return Ok(false);
                }
                if state.condition != ZoneCondition::Full {
                    mutations.push(ZoneMutation {
                        index: zone_index,
                        old_state: *state,
                    });
                    if state.condition.is_open() {
                        self.release_open_zone();
                    }
                    state.condition = ZoneCondition::Full;
                    return Ok(true);
                }
            }
            ZoneOperation::Open => {
                if all && state.condition != ZoneCondition::Closed {
                    return Ok(false);
                }
                match state.condition {
                    ZoneCondition::Empty | ZoneCondition::Closed => {
                        self.reserve_open_zone(zone_index, mutations)?;
                        mutations.push(ZoneMutation {
                            index: zone_index,
                            old_state: *state,
                        });
                        state.condition = ZoneCondition::ExplicitlyOpened;
                        return Ok(true);
                    }
                    ZoneCondition::ImplicitlyOpened => {
                        mutations.push(ZoneMutation {
                            index: zone_index,
                            old_state: *state,
                        });
                        state.condition = ZoneCondition::ExplicitlyOpened;
                        return Ok(true);
                    }
                    ZoneCondition::ExplicitlyOpened | ZoneCondition::Full => {}
                    _ => unreachable!(),
                }
            }
            ZoneOperation::ResetWritePointer => {
                if all
                    && !matches!(
                        state.condition,
                        ZoneCondition::ImplicitlyOpened
                            | ZoneCondition::ExplicitlyOpened
                            | ZoneCondition::Closed
                            | ZoneCondition::Full
                    )
                {
                    return Ok(false);
                }
                let changed = state.condition != ZoneCondition::Empty
                    || state.write_pointer != zone.start_lba;
                if !changed {
                    return Ok(false);
                }
                mutations.push(ZoneMutation {
                    index: zone_index,
                    old_state: *state,
                });
                if state.condition.is_open() {
                    self.release_open_zone();
                }
                state.condition = ZoneCondition::Empty;
                state.write_pointer = zone.start_lba;
                return Ok(true);
            }
        }

        Ok(false)
    }
}

impl DiskIo for ZoneEmulationDisk {
    fn disk_type(&self) -> &str {
        "zone_emu"
    }

    fn sector_count(&self) -> u64 {
        self.sector_count
    }

    fn sector_size(&self) -> u32 {
        self.inner.sector_size()
    }

    fn disk_id(&self) -> Option<[u8; 16]> {
        self.inner.disk_id()
    }

    fn physical_sector_size(&self) -> u32 {
        self.inner.physical_sector_size()
    }

    fn is_fua_respected(&self) -> bool {
        self.inner.is_fua_respected()
    }

    fn is_read_only(&self) -> bool {
        self.inner.is_read_only()
    }

    fn unmap(
        &self,
        _sector: u64,
        _count: u64,
        _block_level_only: bool,
    ) -> impl Future<Output = Result<(), DiskError>> + Send {
        std::future::ready(Ok(()))
    }

    fn unmap_behavior(&self) -> UnmapBehavior {
        UnmapBehavior::Ignored
    }

    fn pr(&self) -> Option<&dyn disk_backend::pr::PersistentReservation> {
        self.inner.pr()
    }

    fn zoned(&self) -> Option<&dyn ZonedDiskCapability> {
        Some(self)
    }

    async fn read_vectored(
        &self,
        buffers: &RequestBuffers<'_>,
        sector: u64,
    ) -> Result<(), DiskError> {
        let sector_count = (buffers.len() as u64) / u64::from(self.sector_size());
        self.read(buffers, sector, sector_count).await
    }

    async fn write_vectored(
        &self,
        buffers: &RequestBuffers<'_>,
        sector: u64,
        fua: bool,
    ) -> Result<(), DiskError> {
        let sector_count = (buffers.len() as u64) / u64::from(self.sector_size());
        self.write(buffers, sector, sector_count, fua).await
    }

    fn write_same(
        &self,
        buffer: &RequestBuffers<'_>,
        sector: u64,
        count: u64,
        fua: bool,
    ) -> impl Future<Output = Result<(), DiskError>> + Send {
        self.write_same_inner(buffer, sector, count, fua)
    }

    fn sync_cache(&self) -> impl Future<Output = Result<(), DiskError>> + Send {
        self.inner.sync_cache()
    }
}

#[async_trait::async_trait]
impl ZonedDiskCapability for ZoneEmulationDisk {
    fn info(&self) -> ZonedDiskInfo {
        ZonedDiskInfo {
            unrestricted_read: self.parameters.unrestricted_read,
            optimal_open_sequential_write_preferred_zones: u32::MAX,
            optimal_nonsequential_sequential_write_preferred_zones: u32::MAX,
            max_open_sequential_write_required_zones: self.parameters.max_open_zones,
        }
    }

    async fn report_zones(
        &self,
        start_lba: u64,
        filter: ZoneReportFilter,
        max_zones: usize,
    ) -> Result<ZoneReport, DiskError> {
        self.report_zones_inner(start_lba, filter, max_zones).await
    }

    async fn zone_operation(
        &self,
        operation: ZoneOperation,
        zone_id: Option<u64>,
    ) -> Result<(), DiskError> {
        self.zone_operation_inner(operation, zone_id).await
    }
}

impl Inspect for ZoneEmulationDisk {
    fn inspect(&self, req: inspect::Request<'_>) {
        let mut empty = 0u32;
        let mut open = 0u32;
        let mut closed = 0u32;
        let mut full = 0u32;
        let mut interesting_zone_indices =
            Vec::with_capacity(ZONE_INSPECT_LIMIT - ZONE_INSPECT_EDGE_COUNT * 2);
        for (index, zone) in self.zones.iter().enumerate() {
            let condition = zone.state.lock().condition;
            match condition {
                ZoneCondition::Empty => empty += 1,
                ZoneCondition::ImplicitlyOpened | ZoneCondition::ExplicitlyOpened => open += 1,
                ZoneCondition::Closed => closed += 1,
                ZoneCondition::Full => full += 1,
                _ => {}
            }
            if interesting_zone_indices.capacity() > interesting_zone_indices.len()
                && (condition != ZoneCondition::Empty
                    && condition != ZoneCondition::NotWritePointer
                    || zone.io_operations.load(Ordering::Relaxed) != 0
                    || zone.outstanding_io.load(Ordering::Relaxed) != 0
                    || zone.lock_contention.load(Ordering::Relaxed) != 0)
            {
                interesting_zone_indices.push(index);
            }
        }
        let inspected_zone_indices =
            bounded_zone_indices(self.zones.len(), interesting_zone_indices);

        let mut resp = req.respond();
        resp.field("sector_count", self.sector_count)
            .field("zone_size_sectors", self.zone_size_sectors)
            .field("zone_count", self.zones.len())
            .field(
                "conventional_zone_count",
                self.parameters.conventional_zone_count,
            )
            .field("max_open_zones", self.parameters.max_open_zones)
            .field("unrestricted_read", self.parameters.unrestricted_read)
            .field("metadata_start_lba", self.metadata_start_lba)
            .field(
                "hidden_padding_sectors",
                self.metadata_start_lba - self.sector_count,
            )
            .field("open_zone_count", *self.open_zone_count.lock())
            .field("observed_open_zone_count", open)
            .field("empty_zone_count", empty)
            .field("closed_zone_count", closed)
            .field("full_zone_count", full)
            .field("inspected_zone_count", inspected_zone_indices.len())
            .field(
                "omitted_zone_count",
                self.zones.len() - inspected_zone_indices.len(),
            );

        resp.child("telemetry", |req| {
            let last = *self.telemetry.last_command.lock();
            let recent_writes = self.telemetry.recent_writes.lock().clone();
            let mut telemetry_resp = req.respond();
            telemetry_resp
                .field(
                    "read_commands",
                    self.telemetry.read_commands.load(Ordering::Relaxed),
                )
                .field(
                    "read_sectors",
                    self.telemetry.read_sectors.load(Ordering::Relaxed),
                )
                .field(
                    "write_commands",
                    self.telemetry.write_commands.load(Ordering::Relaxed),
                )
                .field(
                    "write_sectors",
                    self.telemetry.write_sectors.load(Ordering::Relaxed),
                )
                .field(
                    "write_same_commands",
                    self.telemetry.write_same_commands.load(Ordering::Relaxed),
                )
                .field(
                    "write_same_sectors",
                    self.telemetry.write_same_sectors.load(Ordering::Relaxed),
                )
                .field(
                    "conventional_read_segments",
                    self.telemetry
                        .conventional_read_segments
                        .load(Ordering::Relaxed),
                )
                .field(
                    "sequential_read_segments",
                    self.telemetry
                        .sequential_read_segments
                        .load(Ordering::Relaxed),
                )
                .field(
                    "conventional_write_commands",
                    self.telemetry
                        .conventional_write_commands
                        .load(Ordering::Relaxed),
                )
                .field(
                    "sequential_write_commands",
                    self.telemetry
                        .sequential_write_commands
                        .load(Ordering::Relaxed),
                )
                .field(
                    "successful_sequential_writes",
                    self.telemetry
                        .successful_sequential_writes
                        .load(Ordering::Relaxed),
                )
                .field(
                    "report_zones_commands",
                    self.telemetry.report_zones_commands.load(Ordering::Relaxed),
                )
                .field(
                    "open_zone_commands",
                    self.telemetry.open_zone_commands.load(Ordering::Relaxed),
                )
                .field(
                    "close_zone_commands",
                    self.telemetry.close_zone_commands.load(Ordering::Relaxed),
                )
                .field(
                    "implicit_zone_auto_closes",
                    self.telemetry
                        .implicit_zone_auto_closes
                        .load(Ordering::Relaxed),
                )
                .field(
                    "finish_zone_commands",
                    self.telemetry.finish_zone_commands.load(Ordering::Relaxed),
                )
                .field(
                    "reset_write_pointer_commands",
                    self.telemetry
                        .reset_write_pointer_commands
                        .load(Ordering::Relaxed),
                )
                .field(
                    "zero_fill_operations",
                    self.telemetry.zero_fill_operations.load(Ordering::Relaxed),
                )
                .field(
                    "zero_filled_bytes",
                    self.telemetry.zero_filled_bytes.load(Ordering::Relaxed),
                )
                .field(
                    "disk_errors",
                    self.telemetry.disk_errors.load(Ordering::Relaxed),
                )
                .field(
                    "unaligned_write_errors",
                    self.telemetry
                        .unaligned_write_errors
                        .load(Ordering::Relaxed),
                )
                .field(
                    "write_boundary_errors",
                    self.telemetry.write_boundary_errors.load(Ordering::Relaxed),
                )
                .field(
                    "read_boundary_errors",
                    self.telemetry.read_boundary_errors.load(Ordering::Relaxed),
                )
                .field(
                    "read_invalid_data_errors",
                    self.telemetry
                        .read_invalid_data_errors
                        .load(Ordering::Relaxed),
                )
                .field(
                    "out_of_range_errors",
                    self.telemetry.out_of_range_errors.load(Ordering::Relaxed),
                )
                .field(
                    "insufficient_zone_resource_errors",
                    self.telemetry
                        .insufficient_zone_resource_errors
                        .load(Ordering::Relaxed),
                )
                .field(
                    "read_only_errors",
                    self.telemetry.read_only_errors.load(Ordering::Relaxed),
                )
                .field(
                    "offline_errors",
                    self.telemetry.offline_errors.load(Ordering::Relaxed),
                );
            telemetry_resp.child("last_command", |req| {
                req.respond()
                    .field("sequence", last.sequence)
                    .field("operation", last.operation.name())
                    .field("lba", last.lba)
                    .field("sector_count", last.sector_count)
                    .field("write_pointer", last.write_pointer)
                    .field("result", last.result);
            });
            telemetry_resp.child("recent_writes", |req| {
                let mut writes_resp = req.respond();
                for write in recent_writes {
                    writes_resp.child(&write.sequence.to_string(), |req| {
                        req.respond()
                            .field("operation", write.operation.name())
                            .field("lba", write.lba)
                            .field("sector_count", write.sector_count)
                            .field("write_pointer", write.write_pointer)
                            .field("result", write.result);
                    });
                }
            });
        });

        resp.child("metadata", |req| {
            let recovery_source = match self
                .telemetry
                .metadata_recovery_source
                .load(Ordering::Relaxed)
            {
                1 => "fresh",
                2 => "slot_0",
                3 => "slot_1",
                4 => "corrupt",
                _ => "not_loaded",
            };
            req.respond()
                .field(
                    "loaded",
                    self.telemetry.metadata_loaded.load(Ordering::Relaxed),
                )
                .field(
                    "generation",
                    self.telemetry.metadata_generation.load(Ordering::Relaxed),
                )
                .field(
                    "active_slot",
                    self.telemetry.metadata_active_slot.load(Ordering::Relaxed),
                )
                .field("recovery_source", recovery_source)
                .field(
                    "load_attempts",
                    self.telemetry
                        .metadata_load_attempts
                        .load(Ordering::Relaxed),
                )
                .field(
                    "fresh_initializations",
                    self.telemetry
                        .metadata_fresh_initializations
                        .load(Ordering::Relaxed),
                )
                .field(
                    "recoveries",
                    self.telemetry.metadata_recoveries.load(Ordering::Relaxed),
                )
                .field(
                    "corruptions",
                    self.telemetry.metadata_corruptions.load(Ordering::Relaxed),
                )
                .field(
                    "writes",
                    self.telemetry.metadata_writes.load(Ordering::Relaxed),
                )
                .field(
                    "flushes",
                    self.telemetry.metadata_flushes.load(Ordering::Relaxed),
                )
                .field(
                    "failures",
                    self.telemetry.metadata_failures.load(Ordering::Relaxed),
                );
        });

        for index in inspected_zone_indices {
            let zone = &self.zones[index];
            resp.child(&format!("zones/{index}"), |req| {
                let state = *zone.state.lock();
                req.respond()
                    .field("start_lba", zone.start_lba)
                    .field("length", zone.length)
                    .field("type", zone.zone_type.name())
                    .field("condition", state.condition.name())
                    .field("write_pointer", state.write_pointer)
                    .field("io_operations", zone.io_operations.load(Ordering::Relaxed))
                    .field(
                        "outstanding_io",
                        zone.outstanding_io.load(Ordering::Relaxed),
                    )
                    .field(
                        "lock_contention",
                        zone.lock_contention.load(Ordering::Relaxed),
                    );
            });
        }
    }
}

fn bounded_zone_indices(
    zone_count: usize,
    interesting_zone_indices: impl IntoIterator<Item = usize>,
) -> Vec<usize> {
    if zone_count <= ZONE_INSPECT_LIMIT {
        return (0..zone_count).collect();
    }

    let mut indices = Vec::with_capacity(ZONE_INSPECT_LIMIT);
    indices.extend(0..ZONE_INSPECT_EDGE_COUNT);
    for index in interesting_zone_indices {
        if index >= ZONE_INSPECT_EDGE_COUNT
            && index < zone_count - ZONE_INSPECT_EDGE_COUNT
            && !indices.contains(&index)
        {
            indices.push(index);
        }
    }
    indices.extend(zone_count - ZONE_INSPECT_EDGE_COUNT..zone_count);
    indices.sort_unstable();
    indices.dedup();
    indices
}

#[cfg(test)]
mod inspect_tests {
    use super::*;
    use pal_async::async_test;

    #[test]
    fn bounded_zone_indices_preserves_small_disks() {
        assert_eq!(
            bounded_zone_indices(31, std::iter::empty()),
            (0..31).collect::<Vec<_>>()
        );
    }

    #[test]
    fn bounded_zone_indices_limits_large_disks_and_keeps_activity() {
        let indices = bounded_zone_indices(163_840, [40, 1000, 1000, 163_800]);

        assert_eq!(indices.len(), 67);
        assert_eq!(
            &indices[..ZONE_INSPECT_EDGE_COUNT],
            &(0..32).collect::<Vec<_>>()
        );
        assert!(indices.contains(&40));
        assert!(indices.contains(&1000));
        assert!(indices.contains(&163_800));
        assert_eq!(
            &indices[indices.len() - ZONE_INSPECT_EDGE_COUNT..],
            &(163_808..163_840).collect::<Vec<_>>()
        );
    }

    #[async_test]
    async fn exposes_protocol_neutral_zoned_capability() {
        let inner = disklayer_ram::ram_disk(128 * 512, false).unwrap();
        let emulated = ZoneEmulationDisk::new(
            inner,
            ZoneEmulationParameters {
                zone_size_bytes: 4096,
                conventional_zone_count: 1,
                max_open_zones: 2,
                unrestricted_read: true,
            },
        )
        .unwrap();
        let disk = Disk::new(emulated).unwrap();
        let zoned = disk.zoned().unwrap();

        let report = zoned
            .report_zones(0, ZoneReportFilter::All, usize::MAX)
            .await
            .unwrap();
        assert_eq!(report.total_zone_count, 15);
        assert_eq!(report.zones[0].zone_type, ReportedZoneType::Conventional);
        assert_eq!(
            report.zones[1].zone_type,
            ReportedZoneType::SequentialWriteRequired
        );
        assert_eq!(report.zones[1].write_pointer_lba, Some(8));

        let memory = GuestMemory::allocate(4096);
        memory.write_at(0, &[0xa5; 4096]).unwrap();
        let buffers = OwnedRequestBuffers::linear(0, 4096, true);
        disk.write_vectored(&buffers.buffer(&memory), 8, false)
            .await
            .unwrap();

        let report = zoned
            .report_zones(8, ZoneReportFilter::All, 1)
            .await
            .unwrap();
        assert_eq!(report.zones[0].condition, ReportedZoneCondition::Full);
        assert_eq!(report.zones[0].write_pointer_lba, None);

        let error = disk
            .write_vectored(&buffers.buffer(&memory), 8, false)
            .await
            .unwrap_err();
        assert!(matches!(error, DiskError::Zone(ZoneError::UnalignedWrite)));
    }
}
