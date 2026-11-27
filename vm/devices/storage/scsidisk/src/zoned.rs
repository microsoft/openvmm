// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use super::ScsiError;
use disk_backend::Disk;
use disk_backend::zoned::ZoneCondition;
use disk_backend::zoned::ZoneError;
use disk_backend::zoned::ZoneOperation;
use disk_backend::zoned::ZoneReportFilter;
use disk_backend::zoned::ZoneType;
use guestmem::MemoryWrite;
use inspect::Inspect;
use parking_lot::Mutex;
use scsi_buffers::RequestBuffers;
use scsi_defs as scsi;
use std::mem::size_of;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering;
use zerocopy::FromBytes;
use zerocopy::FromZeros;
use zerocopy::IntoBytes;

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

pub struct ScsiTelemetry {
    sequence: AtomicU64,
    commands: [AtomicU64; 256],
    check_conditions: AtomicU64,
    illegal_requests: AtomicU64,
    srb_errors: AtomicU64,
    last_completion: Mutex<LastScsiCompletion>,
}

impl ScsiTelemetry {
    pub fn new() -> Self {
        Self {
            sequence: AtomicU64::new(0),
            commands: [const { AtomicU64::new(0) }; 256],
            check_conditions: AtomicU64::new(0),
            illegal_requests: AtomicU64::new(0),
            srb_errors: AtomicU64::new(0),
            last_completion: Mutex::new(LastScsiCompletion {
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

    pub fn record(&self, opcode: scsi::ScsiOp, result: &scsi_core::ScsiResult) {
        self.commands[opcode.0 as usize].fetch_add(1, Ordering::Relaxed);
        let sequence = self.sequence.fetch_add(1, Ordering::Relaxed) + 1;
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
            self.check_conditions.fetch_add(1, Ordering::Relaxed);
            if let Some(sense) = result.sense_data {
                last.sense_key = sense.header.sense_key.0;
                last.additional_sense_code = sense.additional_sense_code.0;
                last.additional_sense_code_qualifier = sense.additional_sense_code_qualifier;
                if sense.header.sense_key == scsi::SenseKey::ILLEGAL_REQUEST {
                    self.illegal_requests.fetch_add(1, Ordering::Relaxed);
                }
            }
        }
        if result.srb_status == scsi::srb::SrbStatus::ERROR {
            self.srb_errors.fetch_add(1, Ordering::Relaxed);
        }
        *self.last_completion.lock() = last;
    }
}

impl Inspect for ScsiTelemetry {
    fn inspect(&self, req: inspect::Request<'_>) {
        let last = *self.last_completion.lock();
        let mut resp = req.respond();
        resp.field(
            "check_conditions",
            self.check_conditions.load(Ordering::Relaxed),
        )
        .field(
            "illegal_requests",
            self.illegal_requests.load(Ordering::Relaxed),
        )
        .field("srb_errors", self.srb_errors.load(Ordering::Relaxed));
        resp.child("last_completion", |req| {
            req.respond()
                .field("sequence", last.sequence)
                .field("opcode", format!("0x{:02x}", last.opcode))
                .field("result", last.result)
                .field("scsi_status", last.scsi_status)
                .field("srb_status", last.srb_status)
                .field("transferred_bytes", last.transferred_bytes)
                .field("sense_key", last.sense_key)
                .field("additional_sense_code", last.additional_sense_code)
                .field(
                    "additional_sense_code_qualifier",
                    last.additional_sense_code_qualifier,
                );
        });
        for (opcode, count) in self.commands.iter().enumerate() {
            let count = count.load(Ordering::Relaxed);
            if count != 0 {
                resp.field(&format!("opcode_0x{opcode:02x}"), count);
            }
        }
    }
}

fn report_filter(option: u8) -> Result<ZoneReportFilter, ScsiError> {
    Ok(match option {
        scsi::REPORT_ZONES_OPTION_ALL => ZoneReportFilter::All,
        scsi::REPORT_ZONES_OPTION_EMPTY => ZoneReportFilter::Empty,
        scsi::REPORT_ZONES_OPTION_IMPLICITLY_OPENED => ZoneReportFilter::ImplicitlyOpened,
        scsi::REPORT_ZONES_OPTION_EXPLICITLY_OPENED => ZoneReportFilter::ExplicitlyOpened,
        scsi::REPORT_ZONES_OPTION_CLOSED => ZoneReportFilter::Closed,
        scsi::REPORT_ZONES_OPTION_FULL => ZoneReportFilter::Full,
        scsi::REPORT_ZONES_OPTION_READ_ONLY => ZoneReportFilter::ReadOnly,
        scsi::REPORT_ZONES_OPTION_OFFLINE => ZoneReportFilter::Offline,
        scsi::REPORT_ZONES_OPTION_RWP_RECOMMENDED => ZoneReportFilter::ResetWritePointerRecommended,
        scsi::REPORT_ZONES_OPTION_NON_SEQUENTIAL => ZoneReportFilter::NonSequential,
        scsi::REPORT_ZONES_OPTION_NOT_WRITE_POINTER => ZoneReportFilter::NotWritePointer,
        _ => {
            return Err(ScsiError::IllegalRequest(
                scsi::AdditionalSenseCode::INVALID_CDB,
            ));
        }
    })
}

fn zone_type(zone_type: ZoneType) -> u8 {
    match zone_type {
        ZoneType::Conventional => scsi::ZONE_TYPE_CONVENTIONAL,
        ZoneType::SequentialWriteRequired => scsi::ZONE_TYPE_SEQUENTIAL_WRITE_REQUIRED,
        ZoneType::SequentialWritePreferred => scsi::ZONE_TYPE_SEQUENTIAL_WRITE_PREFERRED,
    }
}

fn zone_condition(condition: ZoneCondition) -> u8 {
    match condition {
        ZoneCondition::NotWritePointer => scsi::ZONE_CONDITION_NOT_WRITE_POINTER,
        ZoneCondition::Empty => scsi::ZONE_CONDITION_EMPTY,
        ZoneCondition::ImplicitlyOpened => scsi::ZONE_CONDITION_IMPLICITLY_OPENED,
        ZoneCondition::ExplicitlyOpened => scsi::ZONE_CONDITION_EXPLICITLY_OPENED,
        ZoneCondition::Closed => scsi::ZONE_CONDITION_CLOSED,
        ZoneCondition::ReadOnly => scsi::ZONE_CONDITION_READ_ONLY,
        ZoneCondition::Full => scsi::ZONE_CONDITION_FULL,
        ZoneCondition::Offline => scsi::ZONE_CONDITION_OFFLINE,
    }
}

pub async fn report_zones(
    disk: &Disk,
    external_data: &RequestBuffers<'_>,
    request: &scsi_core::Request,
) -> Result<usize, ScsiError> {
    let zoned = disk.zoned().ok_or(ScsiError::IllegalRequest(
        scsi::AdditionalSenseCode::ILLEGAL_COMMAND,
    ))?;
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

    let filter = report_filter(cdb.flags & scsi::REPORT_ZONES_OPTION_MASK)?;
    let partial = cdb.flags & scsi::REPORT_ZONES_PARTIAL != 0;
    let max_descriptors = (allocation_length - size_of::<scsi::ReportZonesData>())
        / size_of::<scsi::ZoneDescriptor>();
    let report = zoned
        .report_zones(cdb.zone_start_lba.get(), filter, max_descriptors)
        .await
        .map_err(ScsiError::Disk)?;
    let listed_count = if partial {
        report.zones.len()
    } else {
        report.total_zone_count
    };
    let header = scsi::ReportZonesData {
        zone_list_length: u32::try_from(listed_count * size_of::<scsi::ZoneDescriptor>())
            .map_err(|_| ScsiError::SrbError)?
            .into(),
        same: scsi::ZONES_TYPE_AND_LENGTH_MAY_DIFFER,
        max_lba: (disk.sector_count() - 1).into(),
        ..FromZeros::new_zeroed()
    };

    let mut writer = external_data.writer();
    writer
        .write(header.as_bytes())
        .map_err(ScsiError::MemoryAccess)?;
    for zone in &report.zones {
        let mut flags = 0;
        if zone.reset_write_pointer_recommended {
            flags |= scsi::ZONE_DESCRIPTOR_RWP_RECOMMENDED;
        }
        if zone.non_sequential {
            flags |= scsi::ZONE_DESCRIPTOR_NON_SEQUENTIAL;
        }
        let descriptor = scsi::ZoneDescriptor {
            zone_type: zone_type(zone.zone_type),
            condition: zone_condition(zone.condition) << scsi::ZONE_CONDITION_SHIFT | flags,
            zone_length: zone.length.into(),
            zone_start_lba: zone.start_lba.into(),
            write_pointer_lba: zone.write_pointer_lba.unwrap_or(u64::MAX).into(),
            ..FromZeros::new_zeroed()
        };
        writer
            .write(descriptor.as_bytes())
            .map_err(ScsiError::MemoryAccess)?;
    }

    Ok(size_of::<scsi::ReportZonesData>() + report.zones.len() * size_of::<scsi::ZoneDescriptor>())
}

pub async fn zbc_out(disk: &Disk, request: &scsi_core::Request) -> Result<usize, ScsiError> {
    let zoned = disk.zoned().ok_or(ScsiError::IllegalRequest(
        scsi::AdditionalSenseCode::ILLEGAL_COMMAND,
    ))?;
    let cdb = scsi::ZbcOut::read_from_prefix(&request.cdb)
        .map_err(|_| ScsiError::IllegalRequest(scsi::AdditionalSenseCode::INVALID_CDB))?
        .0;
    let operation = match cdb.service_action & 0x1f {
        scsi::SERVICE_ACTION_CLOSE_ZONE => ZoneOperation::Close,
        scsi::SERVICE_ACTION_FINISH_ZONE => ZoneOperation::Finish,
        scsi::SERVICE_ACTION_OPEN_ZONE => ZoneOperation::Open,
        scsi::SERVICE_ACTION_RESET_WRITE_POINTER => ZoneOperation::ResetWritePointer,
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
    let zone_id = (cdb.flags & scsi::ZBC_OUT_ALL == 0).then_some(cdb.zone_id.get());
    zoned
        .zone_operation(operation, zone_id)
        .await
        .map_err(ScsiError::Disk)?;
    Ok(0)
}

pub fn error_result(error: ZoneError, op: scsi::ScsiOp) -> scsi_core::ScsiResult {
    use scsi::AdditionalSenseCode;
    use scsi::ScsiStatus;
    use scsi::SenseKey;
    use scsi::srb::SrbStatus;

    let (srb_status, sense_key, sense_code, qualifier) = match error {
        ZoneError::LogicalBlockAddressOutOfRange => (
            SrbStatus::ERROR,
            SenseKey::ILLEGAL_REQUEST,
            AdditionalSenseCode::ILLEGAL_BLOCK,
            scsi::SCSI_SENSEQ_LOGICAL_ADDRESS_OUT_OF_RANGE,
        ),
        ZoneError::WriteBoundaryViolation => (
            SrbStatus::ERROR,
            SenseKey::ILLEGAL_REQUEST,
            AdditionalSenseCode::ILLEGAL_BLOCK,
            scsi::SCSI_SENSEQ_WRITE_BOUNDARY_VIOLATION,
        ),
        ZoneError::UnalignedWrite => (
            SrbStatus::ERROR,
            SenseKey::ILLEGAL_REQUEST,
            AdditionalSenseCode::ILLEGAL_BLOCK,
            scsi::SCSI_SENSEQ_UNALIGNED_WRITE,
        ),
        ZoneError::ReadBoundaryViolation => (
            SrbStatus::ERROR,
            SenseKey::ILLEGAL_REQUEST,
            AdditionalSenseCode::ILLEGAL_BLOCK,
            scsi::SCSI_SENSEQ_READ_BOUNDARY_VIOLATION,
        ),
        ZoneError::ReadInvalidData => (
            SrbStatus::ERROR,
            SenseKey::ILLEGAL_REQUEST,
            AdditionalSenseCode::ILLEGAL_BLOCK,
            scsi::SCSI_SENSEQ_READ_INVALID_DATA,
        ),
        ZoneError::InsufficientResources => (
            SrbStatus::ERROR,
            SenseKey::DATA_PROTECT,
            AdditionalSenseCode::RESOURCE_FAILURE,
            scsi::SCSI_SENSEQ_INSUFFICIENT_ZONE_RESOURCES,
        ),
        ZoneError::ReadOnly => (
            SrbStatus::ERROR,
            SenseKey::DATA_PROTECT,
            AdditionalSenseCode::WRITE_PROTECT,
            scsi::SCSI_SENSEQ_ZONE_IS_READ_ONLY,
        ),
        ZoneError::Offline => (
            SrbStatus::ERROR,
            if op == scsi::ScsiOp::ZBC_OUT {
                SenseKey::DATA_PROTECT
            } else {
                SenseKey::MEDIUM_ERROR
            },
            AdditionalSenseCode::COMMAND_SEQUENCE_ERROR,
            scsi::SCSI_SENSEQ_ZONE_IS_OFFLINE,
        ),
        ZoneError::InvalidOperation => (
            SrbStatus::INVALID_REQUEST,
            SenseKey::ILLEGAL_REQUEST,
            AdditionalSenseCode::INVALID_CDB,
            0,
        ),
        ZoneError::InternalFailure => (
            SrbStatus::ERROR,
            SenseKey::HARDWARE_ERROR,
            AdditionalSenseCode::INTERNAL_TARGET_FAILURE,
            0,
        ),
    };
    scsi_core::ScsiResult {
        scsi_status: ScsiStatus::CHECK_CONDITION,
        srb_status,
        tx: 0,
        sense_data: Some(scsi::SenseData::new(sense_key, sense_code, qualifier)),
    }
}
