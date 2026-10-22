// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use super::ScsiError;
use disk_backend::Disk;
use futures::lock::Mutex as AsyncMutex;
use guestmem::GuestMemory;
use guestmem::MemoryWrite;
use inspect::Inspect;
use parking_lot::Mutex;
use scsi_buffers::OwnedRequestBuffers;
use scsi_buffers::RequestBuffers;
use scsi_defs as scsi;
use scsidisk_resources::HostManagedZonedParameters;
use std::mem::size_of;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::AtomicU8;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering;
use thiserror::Error;
use zerocopy::FromBytes;
use zerocopy::FromZeros;
use zerocopy::IntoBytes;

const METADATA_MAGIC: &[u8; 8] = b"OVSMR001";
const METADATA_VERSION: u32 = 1;
const METADATA_HEADER_SIZE: usize = 64;
const METADATA_RECORD_SIZE: usize = 16;
const METADATA_CHECKSUM_OFFSET: usize = 60;
const METADATA_FLAG_UNRESTRICTED_READ: u32 = 1;
const ZONE_INSPECT_LIMIT: usize = 256;
const ZONE_INSPECT_EDGE_COUNT: usize = 32;
pub(crate) const MAX_ZONE_COUNT: u64 = 262_144;

#[derive(Debug, Error)]
pub enum InvalidZonedConfiguration {
    #[error("zone size must be a nonzero multiple of the physical sector size")]
    InvalidZoneSize,
    #[error("the disk must contain at least one sequential-write-required zone")]
    NoSequentialZones,
    #[error("the disk has too many zones to represent in REPORT ZONES")]
    TooManyZones,
    #[error("the disk is too small to reserve zoned metadata")]
    NoMetadataSpace,
}

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
enum ZoneType {
    Conventional,
    SequentialWriteRequired,
}

impl ZoneType {
    fn value(self) -> u8 {
        match self {
            Self::Conventional => scsi::ZONE_TYPE_CONVENTIONAL,
            Self::SequentialWriteRequired => scsi::ZONE_TYPE_SEQUENTIAL_WRITE_REQUIRED,
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
            Self::NotWritePointer => scsi::ZONE_CONDITION_NOT_WRITE_POINTER,
            Self::Empty => scsi::ZONE_CONDITION_EMPTY,
            Self::ImplicitlyOpened => scsi::ZONE_CONDITION_IMPLICITLY_OPENED,
            Self::ExplicitlyOpened => scsi::ZONE_CONDITION_EXPLICITLY_OPENED,
            Self::Closed => scsi::ZONE_CONDITION_CLOSED,
            Self::ReadOnly => scsi::ZONE_CONDITION_READ_ONLY,
            Self::Full => scsi::ZONE_CONDITION_FULL,
            Self::Offline => scsi::ZONE_CONDITION_OFFLINE,
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
            scsi::ZONE_CONDITION_NOT_WRITE_POINTER => Some(Self::NotWritePointer),
            scsi::ZONE_CONDITION_EMPTY => Some(Self::Empty),
            scsi::ZONE_CONDITION_IMPLICITLY_OPENED => Some(Self::ImplicitlyOpened),
            scsi::ZONE_CONDITION_EXPLICITLY_OPENED => Some(Self::ExplicitlyOpened),
            scsi::ZONE_CONDITION_CLOSED => Some(Self::Closed),
            scsi::ZONE_CONDITION_READ_ONLY => Some(Self::ReadOnly),
            scsi::ZONE_CONDITION_FULL => Some(Self::Full),
            scsi::ZONE_CONDITION_OFFLINE => Some(Self::Offline),
            _ => None,
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

pub struct ZonedDisk {
    parameters: HostManagedZonedParameters,
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
    ZbcOut,
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
            Self::ZbcOut => "zbc_out",
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
    sense_key: u8,
    additional_sense_code: u8,
    additional_sense_code_qualifier: u8,
}

#[derive(Copy, Clone)]
struct LastScsiCompletion {
    sequence: u64,
    opcode: u8,
    result: &'static str,
    scsi_status: u8,
    srb_status: u8,
    transferred_bytes: u64,
    sense_key: u8,
    additional_sense_code: u8,
    additional_sense_code_qualifier: u8,
}

struct ZonedTelemetry {
    sequence: AtomicU64,
    scsi_commands: [AtomicU64; 256],
    scsi_check_conditions: AtomicU64,
    scsi_illegal_requests: AtomicU64,
    scsi_srb_errors: AtomicU64,
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
    check_conditions: AtomicU64,
    illegal_requests: AtomicU64,
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
    last_scsi_completion: Mutex<LastScsiCompletion>,
}

impl ZonedTelemetry {
    fn new() -> Self {
        Self {
            sequence: AtomicU64::new(0),
            scsi_commands: [const { AtomicU64::new(0) }; 256],
            scsi_check_conditions: AtomicU64::new(0),
            scsi_illegal_requests: AtomicU64::new(0),
            scsi_srb_errors: AtomicU64::new(0),
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
            check_conditions: AtomicU64::new(0),
            illegal_requests: AtomicU64::new(0),
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
                sense_key: 0,
                additional_sense_code: 0,
                additional_sense_code_qualifier: 0,
            }),
            recent_writes: Mutex::new(Vec::with_capacity(16)),
            last_scsi_completion: Mutex::new(LastScsiCompletion {
                sequence: 0,
                opcode: 0,
                result: "none",
                scsi_status: 0,
                srb_status: 0,
                transferred_bytes: 0,
                sense_key: 0,
                additional_sense_code: 0,
                additional_sense_code_qualifier: 0,
            }),
        }
    }
}

#[derive(Copy, Clone)]
enum ZoneAction {
    Close,
    Finish,
    Open,
    ResetWritePointer,
}

impl ZonedDisk {
    pub fn new(
        backing_sector_count: u64,
        sector_size: u32,
        physical_sector_size: u32,
        parameters: HostManagedZonedParameters,
    ) -> Result<Self, InvalidZonedConfiguration> {
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

    pub fn sector_count(&self) -> u64 {
        self.sector_count
    }

    pub fn parameters(&self) -> &HostManagedZonedParameters {
        &self.parameters
    }

    fn record_result<T>(
        &self,
        operation: TelemetryOperation,
        lba: u64,
        sector_count: u64,
        result: &Result<T, ScsiError>,
    ) {
        let sequence = self.telemetry.sequence.fetch_add(1, Ordering::Relaxed) + 1;
        let mut last = LastCommand {
            sequence,
            operation,
            lba,
            sector_count,
            write_pointer: 0,
            result: "success",
            sense_key: 0,
            additional_sense_code: 0,
            additional_sense_code_qualifier: 0,
        };

        if let Err(error) = result {
            match error {
                ScsiError::CheckCondition(key, asc, ascq) => {
                    self.telemetry
                        .check_conditions
                        .fetch_add(1, Ordering::Relaxed);
                    last.result = "check_condition";
                    last.sense_key = key.0;
                    last.additional_sense_code = asc.0;
                    last.additional_sense_code_qualifier = *ascq;
                    if asc.0 == scsi::AdditionalSenseCode::ILLEGAL_BLOCK.0 {
                        match *ascq {
                            scsi::SCSI_SENSEQ_UNALIGNED_WRITE => {
                                self.telemetry
                                    .unaligned_write_errors
                                    .fetch_add(1, Ordering::Relaxed);
                            }
                            scsi::SCSI_SENSEQ_WRITE_BOUNDARY_VIOLATION => {
                                self.telemetry
                                    .write_boundary_errors
                                    .fetch_add(1, Ordering::Relaxed);
                            }
                            scsi::SCSI_SENSEQ_READ_BOUNDARY_VIOLATION => {
                                self.telemetry
                                    .read_boundary_errors
                                    .fetch_add(1, Ordering::Relaxed);
                            }
                            scsi::SCSI_SENSEQ_READ_INVALID_DATA => {
                                self.telemetry
                                    .read_invalid_data_errors
                                    .fetch_add(1, Ordering::Relaxed);
                            }
                            scsi::SCSI_SENSEQ_LOGICAL_ADDRESS_OUT_OF_RANGE => {
                                self.telemetry
                                    .out_of_range_errors
                                    .fetch_add(1, Ordering::Relaxed);
                            }
                            _ => {}
                        }
                    } else if asc.0 == scsi::AdditionalSenseCode::RESOURCE_FAILURE.0
                        && *ascq == scsi::SCSI_SENSEQ_INSUFFICIENT_ZONE_RESOURCES
                    {
                        self.telemetry
                            .insufficient_zone_resource_errors
                            .fetch_add(1, Ordering::Relaxed);
                    } else if asc.0 == scsi::AdditionalSenseCode::WRITE_PROTECT.0
                        && *ascq == scsi::SCSI_SENSEQ_ZONE_IS_READ_ONLY
                    {
                        self.telemetry
                            .read_only_errors
                            .fetch_add(1, Ordering::Relaxed);
                    } else if asc.0 == scsi::AdditionalSenseCode::COMMAND_SEQUENCE_ERROR.0
                        && *ascq == scsi::SCSI_SENSEQ_ZONE_IS_OFFLINE
                    {
                        self.telemetry
                            .offline_errors
                            .fetch_add(1, Ordering::Relaxed);
                    }
                }
                ScsiError::IllegalRequest(asc) => {
                    self.telemetry
                        .illegal_requests
                        .fetch_add(1, Ordering::Relaxed);
                    last.result = "illegal_request";
                    last.sense_key = scsi::SenseKey::ILLEGAL_REQUEST.0;
                    last.additional_sense_code = asc.0;
                }
                ScsiError::Disk(_) => {
                    self.telemetry.disk_errors.fetch_add(1, Ordering::Relaxed);
                    last.result = "disk_error";
                }
                ScsiError::MemoryAccess(_) => last.result = "memory_access_error",
                ScsiError::DataOverrun => last.result = "data_overrun",
                ScsiError::SrbError => last.result = "srb_error",
                ScsiError::WriteProtected => last.result = "write_protected",
                ScsiError::UnitAttention => last.result = "unit_attention",
                ScsiError::UnsupportedModePageCode(_, _) => last.result = "unsupported_mode_page",
                ScsiError::UnsupportedVpdPageCode(_) => last.result = "unsupported_vpd_page",
                ScsiError::UnsupportedServiceAction(_) => {
                    last.result = "unsupported_service_action"
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

    pub(super) fn record_scsi_completion(
        &self,
        opcode: scsi::ScsiOp,
        result: &scsi_core::ScsiResult,
    ) {
        self.telemetry.scsi_commands[opcode.0 as usize].fetch_add(1, Ordering::Relaxed);
        let sequence = self.telemetry.sequence.fetch_add(1, Ordering::Relaxed) + 1;
        let mut last = LastScsiCompletion {
            sequence,
            opcode: opcode.0,
            result: if result.scsi_status == scsi::ScsiStatus::GOOD {
                "good"
            } else if result.scsi_status == scsi::ScsiStatus::CHECK_CONDITION {
                "check_condition"
            } else {
                "non_good"
            },
            scsi_status: result.scsi_status.0,
            srb_status: result.srb_status.0,
            transferred_bytes: result.tx as u64,
            sense_key: 0,
            additional_sense_code: 0,
            additional_sense_code_qualifier: 0,
        };
        if result.scsi_status == scsi::ScsiStatus::CHECK_CONDITION {
            self.telemetry
                .scsi_check_conditions
                .fetch_add(1, Ordering::Relaxed);
            if let Some(sense) = result.sense_data {
                last.sense_key = sense.header.sense_key.0;
                last.additional_sense_code = sense.additional_sense_code.0;
                last.additional_sense_code_qualifier = sense.additional_sense_code_qualifier;
                if sense.header.sense_key == scsi::SenseKey::ILLEGAL_REQUEST {
                    self.telemetry
                        .scsi_illegal_requests
                        .fetch_add(1, Ordering::Relaxed);
                }
            }
        }
        if result.srb_status == scsi::srb::SrbStatus::ERROR {
            self.telemetry
                .scsi_srb_errors
                .fetch_add(1, Ordering::Relaxed);
        }
        *self.telemetry.last_scsi_completion.lock() = last;
    }

    fn metadata_error() -> ScsiError {
        ScsiError::CheckCondition(
            scsi::SenseKey::HARDWARE_ERROR,
            scsi::AdditionalSenseCode::INTERNAL_TARGET_FAILURE,
            0,
        )
    }

    fn metadata_slot_bytes(&self, sector_size: u32) -> Result<usize, ScsiError> {
        usize::try_from(self.metadata_slot_sectors)
            .ok()
            .and_then(|sectors| sectors.checked_mul(sector_size as usize))
            .ok_or(ScsiError::SrbError)
    }

    fn metadata_slot_lba(&self, slot: usize) -> u64 {
        self.metadata_start_lba + slot as u64 * self.metadata_slot_sectors
    }

    async fn read_metadata_slot(&self, disk: &Disk, slot: usize) -> Result<Vec<u8>, ScsiError> {
        let slot_bytes = self.metadata_slot_bytes(disk.sector_size())?;
        let memory = GuestMemory::allocate(slot_bytes);
        let buffers = OwnedRequestBuffers::linear(0, slot_bytes, true);
        disk.read_vectored(&buffers.buffer(&memory), self.metadata_slot_lba(slot))
            .await
            .map_err(ScsiError::Disk)?;
        let mut bytes = vec![0; slot_bytes];
        memory
            .read_at(0, &mut bytes)
            .map_err(|_| ScsiError::SrbError)?;
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

    async fn make_data_durable(&self, disk: &Disk, fua: bool) -> Result<(), ScsiError> {
        if fua && disk.is_fua_respected() {
            return Ok(());
        }
        disk.sync_cache().await.map_err(ScsiError::Disk)
    }

    async fn ensure_metadata_loaded(&self, disk: &Disk) -> Result<(), ScsiError> {
        let mut metadata = self.metadata.lock().await;
        if metadata.loaded {
            return Ok(());
        }

        self.telemetry
            .metadata_load_attempts
            .fetch_add(1, Ordering::Relaxed);
        let first = self.read_metadata_slot(disk, 0).await.inspect_err(|_| {
            self.telemetry
                .metadata_failures
                .fetch_add(1, Ordering::Relaxed);
        })?;
        let second = self.read_metadata_slot(disk, 1).await.inspect_err(|_| {
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

    async fn persist_metadata(&self, disk: &Disk) -> Result<(), ScsiError> {
        let mut metadata = self.metadata.lock().await;
        let generation = metadata.generation.wrapping_add(1);
        let slot = metadata.active_slot ^ 1;
        let slot_bytes = self.metadata_slot_bytes(disk.sector_size())?;
        let bytes = self.encode_metadata(generation, slot_bytes);
        let memory = GuestMemory::allocate(slot_bytes);
        memory.write_at(0, &bytes).map_err(|_| {
            self.telemetry
                .metadata_failures
                .fetch_add(1, Ordering::Relaxed);
            ScsiError::SrbError
        })?;
        let buffers = OwnedRequestBuffers::linear(0, slot_bytes, true);
        disk.write_vectored(
            &buffers.buffer(&memory),
            self.metadata_slot_lba(slot),
            false,
        )
        .await
        .map_err(|error| {
            self.telemetry
                .metadata_failures
                .fetch_add(1, Ordering::Relaxed);
            ScsiError::Disk(error)
        })?;
        self.telemetry
            .metadata_writes
            .fetch_add(1, Ordering::Relaxed);
        disk.sync_cache().await.map_err(|error| {
            self.telemetry
                .metadata_failures
                .fetch_add(1, Ordering::Relaxed);
            ScsiError::Disk(error)
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

    fn insufficient_zone_resources() -> ScsiError {
        ScsiError::CheckCondition(
            scsi::SenseKey::DATA_PROTECT,
            scsi::AdditionalSenseCode::RESOURCE_FAILURE,
            scsi::SCSI_SENSEQ_INSUFFICIENT_ZONE_RESOURCES,
        )
    }

    fn zone_read_only() -> ScsiError {
        ScsiError::CheckCondition(
            scsi::SenseKey::DATA_PROTECT,
            scsi::AdditionalSenseCode::WRITE_PROTECT,
            scsi::SCSI_SENSEQ_ZONE_IS_READ_ONLY,
        )
    }

    fn zone_offline(sense_key: scsi::SenseKey) -> ScsiError {
        ScsiError::CheckCondition(
            sense_key,
            scsi::AdditionalSenseCode::COMMAND_SEQUENCE_ERROR,
            scsi::SCSI_SENSEQ_ZONE_IS_OFFLINE,
        )
    }

    fn invalid_lba(qualifier: u8) -> ScsiError {
        ScsiError::CheckCondition(
            scsi::SenseKey::ILLEGAL_REQUEST,
            scsi::AdditionalSenseCode::ILLEGAL_BLOCK,
            qualifier,
        )
    }

    fn reserve_open_zone(
        &self,
        target_index: usize,
        mutations: &mut Vec<ZoneMutation>,
    ) -> Result<(), ScsiError> {
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
    ) -> Result<bool, ScsiError> {
        let end_lba = start_lba
            .checked_add(sector_count)
            .ok_or_else(|| Self::invalid_lba(scsi::SCSI_SENSEQ_WRITE_BOUNDARY_VIOLATION))?;
        if end_lba > zone.start_lba + zone.length {
            return Err(Self::invalid_lba(
                scsi::SCSI_SENSEQ_WRITE_BOUNDARY_VIOLATION,
            ));
        }
        if start_lba != state.write_pointer || end_lba % self.physical_sector_count != 0 {
            return Err(Self::invalid_lba(scsi::SCSI_SENSEQ_UNALIGNED_WRITE));
        }
        match state.condition {
            ZoneCondition::ReadOnly => Err(Self::zone_read_only()),
            ZoneCondition::Offline => Err(Self::zone_offline(scsi::SenseKey::MEDIUM_ERROR)),
            ZoneCondition::Full => Err(Self::invalid_lba(scsi::SCSI_SENSEQ_UNALIGNED_WRITE)),
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
    ) -> Result<ZoneState, ScsiError> {
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

    pub async fn write(
        &self,
        disk: &Disk,
        buffers: &RequestBuffers<'_>,
        start_lba: u64,
        sector_count: u64,
        fua: bool,
    ) -> Result<(), ScsiError> {
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
            .write_inner(disk, buffers, start_lba, sector_count, fua)
            .await;
        self.record_result(TelemetryOperation::Write, start_lba, sector_count, &result);
        result
    }

    async fn write_inner(
        &self,
        disk: &Disk,
        buffers: &RequestBuffers<'_>,
        start_lba: u64,
        sector_count: u64,
        fua: bool,
    ) -> Result<(), ScsiError> {
        self.ensure_metadata_loaded(disk).await?;
        let index = self
            .zone_index(start_lba)
            .ok_or_else(|| Self::invalid_lba(scsi::SCSI_SENSEQ_LOGICAL_ADDRESS_OUT_OF_RANGE))?;
        let zone = &self.zones[index];
        if zone.zone_type == ZoneType::Conventional {
            let end_lba = start_lba
                .checked_add(sector_count)
                .ok_or_else(|| Self::invalid_lba(scsi::SCSI_SENSEQ_WRITE_BOUNDARY_VIOLATION))?;
            if end_lba > zone.start_lba + zone.length {
                return Err(Self::invalid_lba(
                    scsi::SCSI_SENSEQ_WRITE_BOUNDARY_VIOLATION,
                ));
            }
            let _outstanding = zone.begin_io();
            return disk
                .write_vectored(buffers, start_lba, fua)
                .await
                .map_err(ScsiError::Disk);
        }

        // Keep the data write, zone transition, and journal commit in one
        // transaction so a metadata snapshot cannot mix concurrent states.
        let _mutation_guard = self.mutation_lock.lock().await;
        let _guard = zone.lock_io().await;
        let mut mutations = Vec::with_capacity(2);
        let old_state = self.begin_write(index, start_lba, sector_count, &mut mutations)?;
        match disk.write_vectored(buffers, start_lba, fua).await {
            Ok(()) => {
                if let Err(err) = self.make_data_durable(disk, fua).await {
                    self.rollback_mutations(&mutations);
                    return Err(err);
                }
                self.complete_write(zone, old_state, sector_count);
                if let Err(err) = self.persist_metadata(disk).await {
                    self.rollback_mutations(&mutations);
                    return Err(err);
                }
                Ok(())
            }
            Err(err) => {
                self.rollback_mutations(&mutations);
                Err(ScsiError::Disk(err))
            }
        }
    }

    pub async fn write_same(
        &self,
        disk: &Disk,
        buffer: &RequestBuffers<'_>,
        start_lba: u64,
        sector_count: u64,
        fua: bool,
    ) -> Result<(), ScsiError> {
        self.telemetry
            .write_same_commands
            .fetch_add(1, Ordering::Relaxed);
        self.telemetry
            .write_same_sectors
            .fetch_add(sector_count, Ordering::Relaxed);
        let result = self
            .write_same_inner(disk, buffer, start_lba, sector_count, fua)
            .await;
        self.record_result(
            TelemetryOperation::WriteSame,
            start_lba,
            sector_count,
            &result,
        );
        result
    }

    async fn write_same_inner(
        &self,
        disk: &Disk,
        buffer: &RequestBuffers<'_>,
        start_lba: u64,
        sector_count: u64,
        fua: bool,
    ) -> Result<(), ScsiError> {
        self.ensure_metadata_loaded(disk).await?;
        let index = self
            .zone_index(start_lba)
            .ok_or_else(|| Self::invalid_lba(scsi::SCSI_SENSEQ_LOGICAL_ADDRESS_OUT_OF_RANGE))?;
        let zone = &self.zones[index];
        if zone.zone_type == ZoneType::Conventional {
            let end_lba = start_lba
                .checked_add(sector_count)
                .ok_or_else(|| Self::invalid_lba(scsi::SCSI_SENSEQ_WRITE_BOUNDARY_VIOLATION))?;
            if end_lba > zone.start_lba + zone.length {
                return Err(Self::invalid_lba(
                    scsi::SCSI_SENSEQ_WRITE_BOUNDARY_VIOLATION,
                ));
            }
            let _outstanding = zone.begin_io();
            for lba in start_lba..end_lba {
                disk.write_vectored(buffer, lba, fua)
                    .await
                    .map_err(ScsiError::Disk)?;
            }
            return Ok(());
        }

        let _mutation_guard = self.mutation_lock.lock().await;
        let _guard = zone.lock_io().await;
        let mut mutations = Vec::with_capacity(2);
        let old_state = self.begin_write(index, start_lba, sector_count, &mut mutations)?;
        let end_lba = start_lba + sector_count;
        for lba in start_lba..end_lba {
            if let Err(err) = disk.write_vectored(buffer, lba, fua).await {
                self.rollback_mutations(&mutations);
                return Err(ScsiError::Disk(err));
            }
        }
        if let Err(err) = self.make_data_durable(disk, fua).await {
            self.rollback_mutations(&mutations);
            return Err(err);
        }
        self.complete_write(zone, old_state, sector_count);
        if let Err(err) = self.persist_metadata(disk).await {
            self.rollback_mutations(&mutations);
            return Err(err);
        }
        Ok(())
    }

    pub async fn read(
        &self,
        disk: &Disk,
        buffers: &RequestBuffers<'_>,
        start_lba: u64,
        sector_count: u64,
    ) -> Result<(), ScsiError> {
        self.telemetry.read_commands.fetch_add(1, Ordering::Relaxed);
        self.telemetry
            .read_sectors
            .fetch_add(sector_count, Ordering::Relaxed);
        let result = self
            .read_inner(disk, buffers, start_lba, sector_count)
            .await;
        self.record_result(TelemetryOperation::Read, start_lba, sector_count, &result);
        result
    }

    async fn read_inner(
        &self,
        disk: &Disk,
        buffers: &RequestBuffers<'_>,
        start_lba: u64,
        sector_count: u64,
    ) -> Result<(), ScsiError> {
        self.ensure_metadata_loaded(disk).await?;
        let end_lba = start_lba
            .checked_add(sector_count)
            .ok_or_else(|| Self::invalid_lba(scsi::SCSI_SENSEQ_READ_BOUNDARY_VIOLATION))?;
        let mut lba = start_lba;
        let mut byte_offset = 0;
        let sector_size = disk.sector_size() as usize;

        while lba < end_lba {
            let index = self
                .zone_index(lba)
                .ok_or_else(|| Self::invalid_lba(scsi::SCSI_SENSEQ_LOGICAL_ADDRESS_OUT_OF_RANGE))?;
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
                disk.read_vectored(&segment, lba)
                    .await
                    .map_err(ScsiError::Disk)?;
            } else {
                self.telemetry
                    .sequential_read_segments
                    .fetch_add(1, Ordering::Relaxed);
                if !self.parameters.unrestricted_read && end_lba > zone_end {
                    return Err(Self::invalid_lba(scsi::SCSI_SENSEQ_READ_BOUNDARY_VIOLATION));
                }

                let _guard = zone.lock_io().await;
                let state = *zone.state.lock();
                if state.condition == ZoneCondition::Offline {
                    return Err(Self::zone_offline(scsi::SenseKey::MEDIUM_ERROR));
                }

                let written_end = state.write_pointer.clamp(lba, segment_end);
                if lba < written_end {
                    let written_sectors = written_end - lba;
                    let written_bytes = written_sectors as usize * sector_size;
                    disk.read_vectored(&segment.subrange(0, written_bytes), lba)
                        .await
                        .map_err(ScsiError::Disk)?;
                }

                if written_end < segment_end {
                    if !self.parameters.unrestricted_read {
                        return Err(Self::invalid_lba(scsi::SCSI_SENSEQ_READ_INVALID_DATA));
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
                        .write(&zeroes)
                        .map_err(ScsiError::MemoryAccess)?;
                }
            }

            lba = segment_end;
            byte_offset += segment_bytes;
        }

        Ok(())
    }

    fn matches_reporting_option(
        &self,
        option: u8,
        zone: &Zone,
        state: ZoneState,
    ) -> Result<bool, ScsiError> {
        Ok(match option {
            scsi::REPORT_ZONES_OPTION_ALL => true,
            scsi::REPORT_ZONES_OPTION_EMPTY => state.condition == ZoneCondition::Empty,
            scsi::REPORT_ZONES_OPTION_IMPLICITLY_OPENED => {
                state.condition == ZoneCondition::ImplicitlyOpened
            }
            scsi::REPORT_ZONES_OPTION_EXPLICITLY_OPENED => {
                state.condition == ZoneCondition::ExplicitlyOpened
            }
            scsi::REPORT_ZONES_OPTION_CLOSED => state.condition == ZoneCondition::Closed,
            scsi::REPORT_ZONES_OPTION_FULL => state.condition == ZoneCondition::Full,
            scsi::REPORT_ZONES_OPTION_READ_ONLY => state.condition == ZoneCondition::ReadOnly,
            scsi::REPORT_ZONES_OPTION_OFFLINE => state.condition == ZoneCondition::Offline,
            scsi::REPORT_ZONES_OPTION_RWP_RECOMMENDED
            | scsi::REPORT_ZONES_OPTION_NON_SEQUENTIAL => false,
            scsi::REPORT_ZONES_OPTION_NOT_WRITE_POINTER => zone.zone_type == ZoneType::Conventional,
            _ => {
                return Err(ScsiError::IllegalRequest(
                    scsi::AdditionalSenseCode::INVALID_CDB,
                ));
            }
        })
    }

    pub async fn report_zones(
        &self,
        disk: &Disk,
        external_data: &RequestBuffers<'_>,
        request: &scsi_core::Request,
    ) -> Result<usize, ScsiError> {
        self.telemetry
            .report_zones_commands
            .fetch_add(1, Ordering::Relaxed);
        let cdb = scsi::ReportZones::read_from_prefix(&request.cdb)
            .ok()
            .map(|(cdb, _)| cdb);
        let start_lba = cdb.map_or(0, |cdb| cdb.zone_start_lba.get());
        let allocation_length = cdb.map_or(0, |cdb| u64::from(cdb.allocation_length.get()));
        let result = self.report_zones_inner(disk, external_data, request).await;
        self.record_result(
            TelemetryOperation::ReportZones,
            start_lba,
            allocation_length,
            &result,
        );
        result
    }

    async fn report_zones_inner(
        &self,
        disk: &Disk,
        external_data: &RequestBuffers<'_>,
        request: &scsi_core::Request,
    ) -> Result<usize, ScsiError> {
        self.ensure_metadata_loaded(disk).await?;
        let _mutation_guard = self.mutation_lock.lock().await;
        let cdb = scsi::ReportZones::read_from_prefix(&request.cdb)
            .map_err(|_| ScsiError::IllegalRequest(scsi::AdditionalSenseCode::INVALID_CDB))?
            .0;
        if cdb.service_action & 0x1f != scsi::SERVICE_ACTION_REPORT_ZONES {
            return Err(ScsiError::IllegalRequest(
                scsi::AdditionalSenseCode::INVALID_CDB,
            ));
        }

        let allocation_length = cdb.allocation_length.get() as usize;
        if external_data.len() < allocation_length
            || allocation_length < size_of::<scsi::ReportZonesData>()
        {
            return Err(ScsiError::DataOverrun);
        }

        let start_lba = cdb.zone_start_lba.get();
        let start_index = self
            .zone_index(start_lba)
            .ok_or_else(|| Self::invalid_lba(scsi::SCSI_SENSEQ_LOGICAL_ADDRESS_OUT_OF_RANGE))?;
        let option = cdb.flags & scsi::REPORT_ZONES_OPTION_MASK;
        let partial = cdb.flags & scsi::REPORT_ZONES_PARTIAL != 0;

        let max_descriptors = (allocation_length - size_of::<scsi::ReportZonesData>())
            / size_of::<scsi::ZoneDescriptor>();
        let mut selected_count = 0;
        let mut descriptors = Vec::with_capacity(max_descriptors.min(self.zones.len()));
        for zone in &self.zones[start_index..] {
            let state = *zone.state.lock();
            if !self.matches_reporting_option(option, zone, state)? {
                continue;
            }

            selected_count += 1;
            if descriptors.len() >= max_descriptors {
                continue;
            }

            let write_pointer_lba = match state.condition {
                ZoneCondition::NotWritePointer
                | ZoneCondition::ReadOnly
                | ZoneCondition::Full
                | ZoneCondition::Offline => u64::MAX,
                _ => state.write_pointer,
            };
            descriptors.push(scsi::ZoneDescriptor {
                zone_type: zone.zone_type.value(),
                condition: state.condition.value() << scsi::ZONE_CONDITION_SHIFT,
                zone_length: zone.length.into(),
                zone_start_lba: zone.start_lba.into(),
                write_pointer_lba: write_pointer_lba.into(),
                ..FromZeros::new_zeroed()
            });
        }

        let returned_count = descriptors.len();
        let listed_count = if partial {
            returned_count
        } else {
            selected_count
        };

        let header = scsi::ReportZonesData {
            zone_list_length: u32::try_from(listed_count * size_of::<scsi::ZoneDescriptor>())
                .map_err(|_| ScsiError::SrbError)?
                .into(),
            same: scsi::ZONES_TYPE_AND_LENGTH_MAY_DIFFER,
            max_lba: (self.sector_count - 1).into(),
            ..FromZeros::new_zeroed()
        };

        let mut writer = external_data.writer();
        writer
            .write(header.as_bytes())
            .map_err(ScsiError::MemoryAccess)?;

        for descriptor in descriptors {
            writer
                .write(descriptor.as_bytes())
                .map_err(ScsiError::MemoryAccess)?;
        }

        Ok(size_of::<scsi::ReportZonesData>() + returned_count * size_of::<scsi::ZoneDescriptor>())
    }

    pub async fn zbc_out(
        &self,
        disk: &Disk,
        request: &scsi_core::Request,
    ) -> Result<usize, ScsiError> {
        let cdb = scsi::ZbcOut::read_from_prefix(&request.cdb)
            .ok()
            .map(|(cdb, _)| cdb);
        let (operation, zone_id) = if let Some(cdb) = cdb {
            let operation = match cdb.service_action & 0x1f {
                scsi::SERVICE_ACTION_CLOSE_ZONE => TelemetryOperation::CloseZone,
                scsi::SERVICE_ACTION_FINISH_ZONE => TelemetryOperation::FinishZone,
                scsi::SERVICE_ACTION_OPEN_ZONE => TelemetryOperation::OpenZone,
                scsi::SERVICE_ACTION_RESET_WRITE_POINTER => TelemetryOperation::ResetWritePointer,
                _ => TelemetryOperation::ZbcOut,
            };
            (operation, cdb.zone_id.get())
        } else {
            (TelemetryOperation::ZbcOut, 0)
        };
        match operation {
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
        let result = self.zbc_out_inner(disk, request).await;
        self.record_result(operation, zone_id, 0, &result);
        result
    }

    async fn zbc_out_inner(
        &self,
        disk: &Disk,
        request: &scsi_core::Request,
    ) -> Result<usize, ScsiError> {
        self.ensure_metadata_loaded(disk).await?;
        let cdb = scsi::ZbcOut::read_from_prefix(&request.cdb)
            .map_err(|_| ScsiError::IllegalRequest(scsi::AdditionalSenseCode::INVALID_CDB))?
            .0;
        let action = match cdb.service_action & 0x1f {
            scsi::SERVICE_ACTION_CLOSE_ZONE => ZoneAction::Close,
            scsi::SERVICE_ACTION_FINISH_ZONE => ZoneAction::Finish,
            scsi::SERVICE_ACTION_OPEN_ZONE => ZoneAction::Open,
            scsi::SERVICE_ACTION_RESET_WRITE_POINTER => ZoneAction::ResetWritePointer,
            _ => {
                return Err(ScsiError::IllegalRequest(
                    scsi::AdditionalSenseCode::INVALID_CDB,
                ));
            }
        };

        if cdb.flags & !scsi::ZBC_OUT_ALL != 0 {
            return Err(ScsiError::IllegalRequest(
                scsi::AdditionalSenseCode::INVALID_CDB,
            ));
        }

        let _mutation_guard = self.mutation_lock.lock().await;
        let mut mutations = Vec::new();
        let mut changed = false;
        if cdb.flags & scsi::ZBC_OUT_ALL != 0 {
            for index in 0..self.zones.len() {
                let zone = &self.zones[index];
                if zone.zone_type == ZoneType::SequentialWriteRequired {
                    match self.apply_action(index, action, true, &mut mutations).await {
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
                .zone_index_for_start(cdb.zone_id.get())
                .ok_or_else(|| ScsiError::IllegalRequest(scsi::AdditionalSenseCode::INVALID_CDB))?;
            let zone = &self.zones[index];
            if zone.zone_type != ZoneType::SequentialWriteRequired {
                return Err(ScsiError::IllegalRequest(
                    scsi::AdditionalSenseCode::INVALID_CDB,
                ));
            }
            changed = match self
                .apply_action(index, action, false, &mut mutations)
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
            if let Err(err) = self.persist_metadata(disk).await {
                self.rollback_mutations(&mutations);
                return Err(err);
            }
        }

        Ok(0)
    }

    async fn apply_action(
        &self,
        zone_index: usize,
        action: ZoneAction,
        all: bool,
        mutations: &mut Vec<ZoneMutation>,
    ) -> Result<bool, ScsiError> {
        let zone = &self.zones[zone_index];
        let _guard = zone.lock_io().await;
        let mut state = zone.state.lock();

        match state.condition {
            ZoneCondition::ReadOnly => return Err(Self::zone_read_only()),
            ZoneCondition::Offline => {
                return Err(Self::zone_offline(scsi::SenseKey::DATA_PROTECT));
            }
            _ => {}
        }

        match action {
            ZoneAction::Close => {
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
            ZoneAction::Finish => {
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
            ZoneAction::Open => {
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
            ZoneAction::ResetWritePointer => {
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

impl Inspect for ZonedDisk {
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
            let last_scsi = *self.telemetry.last_scsi_completion.lock();
            let mut telemetry_resp = req.respond();
            telemetry_resp
                .field(
                    "scsi_check_conditions",
                    self.telemetry.scsi_check_conditions.load(Ordering::Relaxed),
                )
                .field(
                    "scsi_illegal_requests",
                    self.telemetry.scsi_illegal_requests.load(Ordering::Relaxed),
                )
                .field(
                    "scsi_srb_errors",
                    self.telemetry.scsi_srb_errors.load(Ordering::Relaxed),
                )
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
                    "check_conditions",
                    self.telemetry.check_conditions.load(Ordering::Relaxed),
                )
                .field(
                    "illegal_requests",
                    self.telemetry.illegal_requests.load(Ordering::Relaxed),
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
                    .field("result", last.result)
                    .field("sense_key", last.sense_key)
                    .field("additional_sense_code", last.additional_sense_code)
                    .field(
                        "additional_sense_code_qualifier",
                        last.additional_sense_code_qualifier,
                    );
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
                            .field("result", write.result)
                            .field("sense_key", write.sense_key)
                            .field("additional_sense_code", write.additional_sense_code)
                            .field(
                                "additional_sense_code_qualifier",
                                write.additional_sense_code_qualifier,
                            );
                    });
                }
            });
            telemetry_resp.child("last_scsi_completion", |req| {
                req.respond()
                    .field("sequence", last_scsi.sequence)
                    .field("opcode", format!("0x{:02x}", last_scsi.opcode))
                    .field("result", last_scsi.result)
                    .field("scsi_status", last_scsi.scsi_status)
                    .field("srb_status", last_scsi.srb_status)
                    .field("transferred_bytes", last_scsi.transferred_bytes)
                    .field("sense_key", last_scsi.sense_key)
                    .field("additional_sense_code", last_scsi.additional_sense_code)
                    .field(
                        "additional_sense_code_qualifier",
                        last_scsi.additional_sense_code_qualifier,
                    );
            });
            for (opcode, count) in self.telemetry.scsi_commands.iter().enumerate() {
                let count = count.load(Ordering::Relaxed);
                if count != 0 {
                    telemetry_resp.field(&format!("scsi_opcode_0x{opcode:02x}"), count);
                }
            }
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
}
