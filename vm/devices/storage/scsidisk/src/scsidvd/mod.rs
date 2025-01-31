// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use super::scsi;
use super::AsyncScsiDisk;
use super::SavedSenseData;
use super::ScsiResult;
use super::ScsiSaveRestore;
use crate::Request;
use crate::ScsiSavedState;
use crate::SenseDataSlot;
use disk_backend::Disk;
use disk_backend::DiskError;
use guestmem::AccessError;
use guestmem::MemoryRead;
use guestmem::MemoryWrite;
use parking_lot::Mutex;
use parking_lot::RwLock;
use scsi::srb::SrbStatus;
use scsi::AdditionalSenseCode;
use scsi::FeatureNumber::{FeaturePowerManagement, FeatureRealTimeStreaming, FeatureTimeout};
use scsi::ScsiOp;
use scsi::ScsiStatus;
use scsi::SenseData;
use scsi::SenseKey;
use scsi_buffers::RequestBuffers;
use scsi_core::save_restore::DriveState;
use scsi_core::save_restore::IsoMediumEvent;
use scsi_core::save_restore::ScsiDvdSavedState;
use stackfuture::StackFuture;
use std::sync::Arc;
use thiserror::Error;
use tracing::Instrument;
use vmcore::save_restore::RestoreError;
use vmcore::save_restore::SaveError;
use zerocopy::FromBytes;
use zerocopy::FromZeros;
use zerocopy::Immutable;
use zerocopy::IntoBytes;
use zerocopy::KnownLayout;

enum Media {
    Unloaded,
    Loaded(Disk),
}

pub struct SimpleScsiDvd {
    media: Arc<RwLock<Media>>,
    media_state: Arc<Mutex<MediaState>>,
    sense_data: SenseDataSlot,
}

#[derive(Debug, Default)]
struct MediaState {
    drive_state: DriveState,
    pending_medium_event: IsoMediumEvent,
    persistent: bool,
    prevent: bool,
}

#[derive(Error, Debug)]
enum ScsiDvdError {
    #[error("illegal request, asc: {0:?}, qualifier: {1:?}")]
    IllegalRequest(AdditionalSenseCode, u8),
    #[error("data overrun")]
    DataOverrun,
    #[error("memory access error")]
    MemoryAccess(#[source] AccessError),
    #[error("io error")]
    IoError(#[source] DiskError),
    #[error("not ready, sense key: {0:?}, qualifier: {1}")]
    SenseNotReady(AdditionalSenseCode, u8),
    #[error("invalid request - no sense data")]
    IllegalRequestNoSenseData,
}

struct RequestParametersIso {
    tx: usize,
    offset: u64,
}

impl AsyncScsiDisk for SimpleScsiDvd {
    fn execute_scsi<'a>(
        &'a self,
        external_data: &'a RequestBuffers<'a>,
        request: &'a Request,
    ) -> StackFuture<'a, ScsiResult, { super::ASYNC_SCSI_DISK_STACK_SIZE }> {
        StackFuture::from(async move {
            let op = request.scsiop();

            let sector_count = self.sector_count();
            let result = match op {
                ScsiOp::INQUIRY => self.handle_inquiry_iso(external_data, request),
                ScsiOp::TEST_UNIT_READY => self.handle_test_unit_ready_iso(),
                ScsiOp::READ | ScsiOp::READ12 | ScsiOp::READ16 => {
                    self.handle_data_cdb_async_iso(external_data, request, sector_count)
                        .instrument(tracing::trace_span!("handle_data_cdb_async", ?op,))
                        .await
                }
                ScsiOp::GET_EVENT_STATUS => self.handle_get_event_status(external_data, request),
                ScsiOp::GET_CONFIGURATION => {
                    self.handle_get_configuration_iso(external_data, request)
                }
                ScsiOp::READ_CAPACITY => self.handle_read_capacity_iso(external_data, sector_count),
                ScsiOp::READ_TOC => self.handle_read_toc(external_data, request),
                ScsiOp::START_STOP_UNIT => self.handle_start_stop_unit(request).await,
                ScsiOp::MODE_SENSE10 => self.handle_mode_sense_iso(external_data, request),
                ScsiOp::REQUEST_SENSE => self.handle_request_sense_iso(external_data, request),
                ScsiOp::MEDIUM_REMOVAL => self.handle_medium_removal_iso(request),
                ScsiOp::MODE_SELECT10 => self.handle_mode_select_iso(request, external_data),
                ScsiOp::READ_TRACK_INFORMATION => {
                    self.handle_read_track_information_iso(external_data, request)
                }
                ScsiOp::READ_DVD_STRUCTURE => {
                    self.handle_read_dvd_structure(external_data, request)
                }
                ScsiOp::GET_PERFORMANCE => self.handle_get_performance(external_data, request),
                ScsiOp::MECHANISM_STATUS => self.handle_mechanism_status(external_data, request),
                ScsiOp::READ_BUFFER_CAPACITY => {
                    self.handle_read_buffer_capacity(external_data, request)
                }
                ScsiOp::READ_DISC_INFORMATION => {
                    self.handle_read_disc_information(external_data, request)
                }
                ScsiOp::SET_STREAMING => self.handle_set_streaming(external_data, request),
                _ => {
                    tracelimit::warn_ratelimited!(op = ?op, "illegal command");
                    Err(ScsiDvdError::IllegalRequest(
                        AdditionalSenseCode::ILLEGAL_COMMAND,
                        0,
                    ))
                }
            };

            self.process_result(result, op)
        })
    }
}

impl inspect::Inspect for SimpleScsiDvd {
    fn inspect(&self, req: inspect::Request<'_>) {
        let mut resp = req.respond();

        resp.field("drive_state", self.media_state.lock().drive_state);

        if let Media::Loaded(disk) = &*self.media.read() {
            resp.field("backend", disk);
        }
    }
}

const ISO_SECTOR_SIZE: u32 = 2048;

impl SimpleScsiDvd {
    pub fn new(disk: Option<Disk>) -> Self {
        assert!(disk.as_ref().is_none() || disk.as_ref().unwrap().sector_size() <= ISO_SECTOR_SIZE);

        let (media, pending_medium_event, drive_state) = if let Some(disk) = disk {
            (
                RwLock::new(Media::Loaded(disk)).into(),
                IsoMediumEvent::MediaToMedia,
                DriveState::MediumPresentTrayClosed,
            )
        } else {
            (
                RwLock::new(Media::Unloaded).into(),
                IsoMediumEvent::MediaToNoMedia,
                DriveState::MediumNotPresentTrayClosed,
            )
        };

        SimpleScsiDvd {
            media,
            media_state: Arc::new(Mutex::new(MediaState {
                pending_medium_event,
                drive_state,
                ..Default::default()
            })),
            sense_data: SenseDataSlot::default(),
        }
    }

    pub fn change_media(&self, disk: Option<Disk>) {
        if let Some(disk) = disk {
            // Insert medium
            let mut media = self.media.write();

            let mut media_state = self.media_state.lock();
            media_state.drive_state = DriveState::MediumPresentTrayOpen;
            media_state.pending_medium_event = IsoMediumEvent::NoMediaToMedia;

            *media = Media::Loaded(disk);

            tracing::debug!("completed host initiated insert");
        } else {
            // Eject medium
            let mut media = self.media.write();
            let mut media_state = self.media_state.lock();

            // This will cause the next GESN or TUR command to report medium removal
            media_state.drive_state = DriveState::MediumNotPresentTrayOpen;
            media_state.pending_medium_event = IsoMediumEvent::MediaToNoMedia;

            *media = Media::Unloaded;

            tracing::debug!("completed host initiated eject");
        }
    }

    fn sector_shift(&self) -> u8 {
        ISO_SECTOR_SIZE.trailing_zeros() as u8
    }

    fn sector_count(&self) -> u64 {
        match &*self.media.read() {
            Media::Unloaded => 0,
            Media::Loaded(disk) => disk.sector_count() / self.balancer(),
        }
    }

    fn balancer(&self) -> u64 {
        match &*self.media.read() {
            Media::Unloaded => unreachable!("cannot read/write from unloaded disk"),
            Media::Loaded(disk) => {
                // Per protocol, the sector size for an ISO is 2048. Any read request which
                // goes to backend disk with sector size <= 2048 then we need to convert
                // read offset and sector count accordingly.
                // e.g.: If disk backend has sector_size = 512 then a read request at sector n
                // should actually go to sector 4*n of backend disk. If sector count of
                // backend device is n then it should be reported as sector count/4 to the guest.
                (ISO_SECTOR_SIZE / disk.sector_size()) as u64
            }
        }
    }
}

impl ScsiSaveRestore for SimpleScsiDvd {
    fn save(&self) -> Result<Option<ScsiSavedState>, SaveError> {
        let sense = self.sense_data.get();
        let sense_data = sense.map(|sense| SavedSenseData {
            sense_key: sense.header.sense_key.0,
            additional_sense_code: sense.additional_sense_code.0,
            additional_sense_code_qualifier: sense.additional_sense_code_qualifier,
        });

        let media_state = self.media_state.lock();
        Ok(Some(ScsiSavedState::ScsiDvd(ScsiDvdSavedState {
            sense_data,
            persistent: media_state.persistent,
            prevent: media_state.prevent,
            drive_state: media_state.drive_state,
            pending_medium_event: media_state.pending_medium_event,
        })))
    }

    fn restore(&self, state: &ScsiSavedState) -> Result<(), RestoreError> {
        if let ScsiSavedState::ScsiDvd(dvd_state) = state {
            let ScsiDvdSavedState {
                sense_data,
                persistent,
                prevent,
                drive_state,
                pending_medium_event,
            } = *dvd_state;

            // restore sense data
            self.sense_data.set(
                sense_data
                    .map(|sense| {
                        SenseData::new(
                            SenseKey(sense.sense_key),
                            AdditionalSenseCode(sense.additional_sense_code),
                            sense.additional_sense_code_qualifier,
                        )
                    })
                    .as_ref(),
            );

            let mut media_state = self.media_state.lock();
            media_state.drive_state = drive_state;
            media_state.pending_medium_event = pending_medium_event;
            media_state.persistent = persistent;
            media_state.prevent = prevent;
            Ok(())
        } else {
            Err(RestoreError::InvalidSavedState(anyhow::anyhow!(
                "saved state didn't match expected format ScsiDvdSavedState"
            )))
        }
    }
}

/// Writes a VPD page.
///
/// Assumes that allocation_length is already validated to be at least
/// `size_of::<scsi::VpdPageHeader>()`.
fn write_vpd_page<T: ?Sized + IntoBytes + Immutable + KnownLayout>(
    external_data: &RequestBuffers<'_>,
    allocation_length: usize,
    page_code: u8,
    page_data: &T,
) -> Result<usize, ScsiDvdError> {
    let header = scsi::VpdPageHeader {
        device_type: scsi::READ_ONLY_DIRECT_ACCESS_DEVICE,
        page_code,
        reserved: 0,
        page_length: size_of_val(page_data).try_into().unwrap(),
    };

    let tx = std::cmp::min(
        allocation_length,
        size_of_val(&header) + size_of_val(page_data),
    );

    let mut writer = external_data.writer();
    writer
        .write(header.as_bytes())
        .map_err(ScsiDvdError::MemoryAccess)?;

    writer
        .write(&page_data.as_bytes()[..tx - size_of_val(&header)])
        .map_err(ScsiDvdError::MemoryAccess)?;

    Ok(tx)
}

impl SimpleScsiDvd {
    fn handle_inquiry_iso(
        &self,
        external_data: &RequestBuffers<'_>,
        request: &Request,
    ) -> Result<usize, ScsiDvdError> {
        let cdb = scsi::CdbInquiry::read_from_prefix(&request.cdb[..])
            .unwrap()
            .0; // TODO: zerocopy: use-rest-of-range (https://github.com/microsoft/openvmm/issues/759)

        let allocation_length = cdb.allocation_length.get() as usize;

        if external_data.len() < allocation_length {
            return Err(ScsiDvdError::DataOverrun);
        }
        // If the PAGE CODE field is not set to zero when the EVPD bit is set to zero,
        // the command shall be terminated with CHECK CONDITION status, with the sense key
        // set to ILLEGAL REQUEST, and the additional sense code set to INVALID FIELD IN CDB.
        let enable_vpd = cdb.flags.vpd();
        if cdb.page_code != 0 && !enable_vpd {
            return Err(ScsiDvdError::IllegalRequest(
                AdditionalSenseCode::INVALID_CDB,
                0,
            ));
        }

        if enable_vpd {
            if allocation_length < size_of::<scsi::VpdPageHeader>() {
                return Err(ScsiDvdError::DataOverrun);
            }

            match cdb.page_code {
                scsi::VPD_SUPPORTED_PAGES => {
                    self.handle_vpd_supported_pages_iso(external_data, allocation_length)
                }
                scsi::VPD_DEVICE_IDENTIFIERS => {
                    self.handle_vpd_device_identifiers_iso(external_data, allocation_length)
                }
                _ => Err(ScsiDvdError::IllegalRequest(
                    AdditionalSenseCode::INVALID_CDB,
                    0,
                )),
            }
        } else {
            self.handle_no_vpd_page_iso(external_data, allocation_length)
        }
    }

    fn handle_test_unit_ready_iso(&self) -> Result<usize, ScsiDvdError> {
        let drive_state = self.media_state.lock().drive_state;

        match drive_state {
            DriveState::MediumPresentTrayOpen => Ok(0),
            DriveState::MediumPresentTrayClosed => Ok(0),
            DriveState::MediumNotPresentTrayOpen => Err(ScsiDvdError::SenseNotReady(
                AdditionalSenseCode::NO_MEDIA_IN_DEVICE,
                scsi::MEDIUM_NOT_PRESENT_TRAY_OPEN,
            )),
            DriveState::MediumNotPresentTrayClosed => Err(ScsiDvdError::SenseNotReady(
                AdditionalSenseCode::NO_MEDIA_IN_DEVICE,
                scsi::MEDIUM_NOT_PRESENT_TRAY_CLOSED,
            )),
        }
    }

    async fn handle_data_cdb_async_iso(
        &self,
        external_data: &RequestBuffers<'_>,
        request: &Request,
        sector_count: u64,
    ) -> Result<usize, ScsiDvdError> {
        let op = request.scsiop();
        let p = self.validate_data_cdb_iso(external_data, request, sector_count, op)?;

        if p.tx != 0 {
            let mut media = None;
            if let Media::Loaded(disk) = &*self.media.read() {
                media = Some(disk.clone());
            }

            if let Some(disk) = media {
                disk.read_vectored(&external_data.subrange(0, p.tx), p.offset)
                    .await
                    .map_err(ScsiDvdError::IoError)?;
            }
        }

        Ok(p.tx)
    }

    fn handle_get_event_status(
        &self,
        external_data: &RequestBuffers<'_>,
        request: &Request,
    ) -> Result<usize, ScsiDvdError> {
        let cdb = scsi::CdbGetEventStatusNotification::read_from_prefix(&request.cdb[..])
            .unwrap()
            .0; // TODO: zerocopy: use-rest-of-range (https://github.com/microsoft/openvmm/issues/759)
        let allocation_length = cdb.event_list_length.get() as usize;
        let mut pending_medium_event = IsoMediumEvent::None;

        //  Make sure buffer is as big as the requested size
        if allocation_length > external_data.len() {
            return Err(ScsiDvdError::DataOverrun);
        }

        // don't support asynchronous mode
        if !cdb.flags.immediate() {
            return Err(ScsiDvdError::IllegalRequest(
                AdditionalSenseCode::INVALID_CDB,
                0x00,
            ));
        }

        // Go through the possible event notifications in priority order.
        // only one event class should be reported at once.
        // if no event has occurred, the driver shall report the "no change"
        // event for the highest priority requested event class.
        let mut media_status = scsi::NotificationMediaStatus::new_zeroed();
        {
            let mut media_state = self.media_state.lock();
            if (cdb.notification_class_request & scsi::NOTIFICATION_MEDIA_STATUS_CLASS_MASK) != 0 {
                pending_medium_event = media_state.pending_medium_event;
                match media_state.pending_medium_event {
                    IsoMediumEvent::None => (),
                    IsoMediumEvent::NoMediaToMedia => {
                        media_status.media_event = scsi::NOTIFICATION_MEDIA_EVENT_NEW_MEDIA;
                        media_status.status_info.set_media_present(true);

                        media_state.drive_state = DriveState::MediumPresentTrayClosed;
                        media_state.pending_medium_event = IsoMediumEvent::None;
                    }
                    IsoMediumEvent::MediaToNoMedia => {
                        media_status.media_event = scsi::NOTIFICATION_MEDIA_EVENT_MEDIA_REMOVAL;
                        media_status.status_info.set_media_present(false);

                        media_state.pending_medium_event = IsoMediumEvent::None;
                    }
                    IsoMediumEvent::MediaToMedia => {
                        media_status.media_event = scsi::NOTIFICATION_MEDIA_EVENT_MEDIA_REMOVAL;
                        media_status.status_info.set_media_present(false);

                        media_state.drive_state = DriveState::MediumNotPresentTrayOpen;
                        media_state.pending_medium_event = IsoMediumEvent::NoMediaToMedia;
                    }
                }

                media_status
                    .status_info
                    .set_door_tray_open(media_state.drive_state.tray_open());
            }
        }

        let data_transfer_length = if pending_medium_event != IsoMediumEvent::None {
            let header_size = self.iso_init_event_header(
                external_data,
                allocation_length,
                (size_of::<scsi::NotificationEventStatusHeader>() + size_of_val(&media_status)
                    - size_of::<u16>()) as u16,
                false,
                scsi::NOTIFICATION_MEDIA_STATUS_CLASS_EVENTS,
            )?;

            let body_size =
                std::cmp::min(size_of_val(&media_status), allocation_length - header_size);

            external_data
                .subrange(header_size, external_data.len() - header_size)
                .writer()
                .write(&media_status.as_bytes()[..body_size])
                .map_err(ScsiDvdError::MemoryAccess)?;

            if media_status.media_event == scsi::NOTIFICATION_MEDIA_EVENT_NEW_MEDIA {
                tracing::info!("media arrival");
            } else if media_status.media_event == scsi::NOTIFICATION_MEDIA_EVENT_MEDIA_REMOVAL {
                tracing::info!("media removal");
            }
            header_size + body_size
        } else {
            if (cdb.notification_class_request & scsi::NOTIFICATION_OPERATIONAL_CHANGE_CLASS_MASK)
                != 0
            {
                let persistent_prevented = self.media_state.lock().persistent;
                let operational_status = scsi::NotificationOperationalStatus {
                    operation_event: scsi::NOTIFICATION_POWER_EVENT_NO_CHANGE,
                    flags: scsi::OperationalStatusFlags::new()
                        .with_operational_status(scsi::NOTIFICATION_OPERATIONAL_STATUS_AVAILABLE)
                        .with_reserved2(0x00)
                        .with_persistent_prevented(persistent_prevented),
                    operation: 0.into(),
                };

                let header_size = self.iso_init_event_header(
                    external_data,
                    allocation_length,
                    (size_of::<scsi::NotificationEventStatusHeader>()
                        + size_of_val(&operational_status)
                        - size_of::<u16>()) as u16,
                    false,
                    scsi::NOTIFICATION_OPERATIONAL_CHANGE_CLASS_EVENTS,
                )?;

                let body_size = std::cmp::min(
                    size_of_val(&operational_status),
                    allocation_length - header_size,
                );
                external_data
                    .subrange(header_size, external_data.len() - header_size)
                    .writer()
                    .write(&operational_status.as_bytes()[..body_size])
                    .map_err(ScsiDvdError::MemoryAccess)?;

                header_size + body_size
            } else if (cdb.notification_class_request
                & scsi::NOTIFICATION_POWER_MANAGEMENT_CLASS_MASK)
                != 0
            {
                let header_size = self.iso_init_event_header(
                    external_data,
                    allocation_length,
                    (size_of::<scsi::NotificationEventStatusHeader>()
                        + size_of::<scsi::NotificationPowerStatus>()
                        - size_of::<u16>()) as u16,
                    false,
                    scsi::NOTIFICATION_POWER_MANAGEMENT_CLASS_EVENTS,
                )?;
                let data = scsi::NotificationPowerStatus {
                    power_event: scsi::NOTIFICATION_POWER_EVENT_NO_CHANGE,
                    power_status: scsi::NOTIFICATION_POWER_STATUS_ACTIVE,
                    reserved: [0x00, 0x00],
                };
                let body_size = std::cmp::min(
                    size_of::<scsi::NotificationPowerStatus>(),
                    allocation_length - header_size,
                );
                external_data
                    .subrange(header_size, external_data.len() - header_size)
                    .writer()
                    .write(&data.as_bytes()[..body_size])
                    .map_err(ScsiDvdError::MemoryAccess)?;

                header_size + body_size
            } else if (cdb.notification_class_request
                & scsi::NOTIFICATION_EXTERNAL_REQUEST_CLASS_MASK)
                != 0
            {
                let persistent_prevented = self.media_state.lock().persistent;
                let external_status = scsi::NotificationExternalStatus {
                    external_event: scsi::NOTIFICATION_EXTERNAL_EVENT_NO_CHANGE,
                    flags: scsi::ExternalStatusFlags::new()
                        .with_external_status(scsi::NOTIFICATION_EXTERNAL_STATUS_READY)
                        .with_reserved2(0x00)
                        .with_persistent_prevented(persistent_prevented),
                    reserved: [0x00, 0x00],
                };

                let header_size = self.iso_init_event_header(
                    external_data,
                    allocation_length,
                    (size_of::<scsi::NotificationEventStatusHeader>()
                        + size_of_val(&external_status)
                        - size_of::<u16>()) as u16,
                    false,
                    scsi::NOTIFICATION_EXTERNAL_REQUEST_CLASS_EVENTS,
                )?;
                let body_size = std::cmp::min(
                    size_of_val(&external_status),
                    allocation_length - header_size,
                );
                external_data
                    .subrange(header_size, external_data.len() - header_size)
                    .writer()
                    .write(&external_status.as_bytes()[..body_size])
                    .map_err(ScsiDvdError::MemoryAccess)?;

                header_size + body_size
            } else if (cdb.notification_class_request & scsi::NOTIFICATION_MULTI_HOST_CLASS_MASK)
                != 0
            {
                let header_size = self.iso_init_event_header(
                    external_data,
                    allocation_length,
                    (size_of::<scsi::NotificationEventStatusHeader>()
                        + size_of::<scsi::NotificationMultiHostStatus>()
                        - size_of::<u16>()) as u16,
                    false,
                    scsi::NOTIFICATION_MULTI_HOST_CLASS_EVENTS,
                )?;
                let multi_host_status = scsi::NotificationMultiHostStatus {
                    multi_host_event: scsi::NOTIFICATION_MULTI_HOST_EVENT_NO_CHANGE,
                    flags: scsi::MultiHostStatusFlags::new()
                        .with_multi_host_status(scsi::NOTIFICATION_MULTI_HOST_STATUS_READY)
                        .with_reserved2(0x00)
                        .with_persistent_prevented(self.media_state.lock().persistent),
                    priority: 0.into(),
                };
                let body_size = std::cmp::min(
                    size_of_val(&multi_host_status),
                    allocation_length - header_size,
                );
                external_data
                    .subrange(header_size, external_data.len() - header_size)
                    .writer()
                    .write(&multi_host_status.as_bytes()[..body_size])
                    .map_err(ScsiDvdError::MemoryAccess)?;

                header_size + body_size
            } else if (cdb.notification_class_request & scsi::NOTIFICATION_MEDIA_STATUS_CLASS_MASK)
                != 0
            {
                let header_size = self.iso_init_event_header(
                    external_data,
                    allocation_length,
                    (size_of::<scsi::NotificationEventStatusHeader>() + size_of_val(&media_status)
                        - size_of::<u16>()) as u16,
                    false,
                    scsi::NOTIFICATION_MEDIA_STATUS_CLASS_EVENTS,
                )?;
                media_status
                    .status_info
                    .set_door_tray_open(self.media_state.lock().drive_state.tray_open());
                media_status
                    .status_info
                    .set_media_present(self.media_state.lock().drive_state.medium_present());
                let datab: &[u8] = media_status.as_bytes();
                let body_size =
                    std::cmp::min(size_of_val(&media_status), allocation_length - header_size);
                external_data
                    .subrange(header_size, external_data.len() - header_size)
                    .writer()
                    .write(&datab[..body_size])
                    .map_err(ScsiDvdError::MemoryAccess)?;
                header_size + body_size
            } else if (cdb.notification_class_request & scsi::NOTIFICATION_DEVICE_BUSY_CLASS_MASK)
                != 0
            {
                let header_size = self.iso_init_event_header(
                    external_data,
                    allocation_length,
                    (size_of::<scsi::NotificationEventStatusHeader>()
                        + size_of::<scsi::NotificationBusyStatus>()
                        - size_of::<u16>()) as u16,
                    false,
                    scsi::NOTIFICATION_DEVICE_BUSY_CLASS_EVENTS,
                )?;
                let data = scsi::NotificationBusyStatus {
                    device_busy_event: scsi::NOTIFICATION_BUSY_EVENT_NO_EVENT,
                    device_busy_status: scsi::NOTIFICATION_BUSY_STATUS_NO_EVENT,
                    time: 0.into(),
                };
                let body_size = std::cmp::min(size_of_val(&data), allocation_length - header_size);
                external_data
                    .subrange(header_size, external_data.len() - header_size)
                    .writer()
                    .write(&data.as_bytes()[..body_size])
                    .map_err(ScsiDvdError::MemoryAccess)?;

                header_size + body_size
            } else {
                self.iso_init_event_header(
                    external_data,
                    allocation_length,
                    (size_of::<scsi::NotificationEventStatusHeader>() - size_of::<u16>()) as u16,
                    false,
                    scsi::NOTIFICATION_NO_CLASS_EVENTS,
                )?
            }
        };

        Ok(data_transfer_length)
    }

    fn handle_get_configuration_iso(
        &self,
        external_data: &RequestBuffers<'_>,
        request: &Request,
    ) -> Result<usize, ScsiDvdError> {
        let cdb = scsi::CdbGetConfiguration::read_from_prefix(&request.cdb[..])
            .unwrap()
            .0; // TODO: zerocopy: use-rest-of-range (https://github.com/microsoft/openvmm/issues/759)
        let request_type = cdb.flags.request_type();
        let starting_feature = cdb.starting_feature.get() as usize;
        let allocation_length = cdb.allocation_length.get() as usize;
        let mut feature_size: usize = 0;

        if allocation_length > external_data.len() {
            return Err(ScsiDvdError::DataOverrun);
        }
        let mut bytes_used =
            std::cmp::min(allocation_length, size_of::<scsi::GetConfigurationHeader>());
        let header_size = bytes_used;
        let mut bytes_req = size_of::<scsi::GetConfigurationHeader>();
        match request_type {
            scsi::RequestType::ALL => {
                // Return all the Feature Descriptors Supported
                for i in 0..scsi::LIST_OF_FEATURES.len() {
                    if (scsi::LIST_OF_FEATURES[i] as usize) >= starting_feature {
                        bytes_used += self.iso_get_feature_page(
                            &external_data.subrange(bytes_used, external_data.len() - bytes_used),
                            allocation_length - bytes_used,
                            scsi::LIST_OF_FEATURES[i],
                            false,
                            &mut feature_size,
                        )?;
                        bytes_req += feature_size;
                    }
                }

                // data_length field indicates the amount of data available given a sufficient allocation length
                // following this field. Thus the size of itself should be excluded.
                self.iso_init_configuration_header(
                    &external_data.subrange(0, header_size),
                    bytes_req,
                )?;
            }
            scsi::RequestType::CURRENT => {
                // Return all the Feature Descriptors Supported
                for i in 0..scsi::LIST_OF_FEATURES.len() {
                    if (scsi::LIST_OF_FEATURES[i] as usize) >= starting_feature {
                        bytes_used += self.iso_get_feature_page(
                            &external_data.subrange(bytes_used, external_data.len() - bytes_used),
                            allocation_length - bytes_used,
                            scsi::LIST_OF_FEATURES[i],
                            true,
                            &mut feature_size,
                        )?;
                        bytes_req += feature_size;
                    }
                }

                // data_length field indicates the amount of data available given a sufficient allocation length
                // following this field. Thus the size of itself should be excluded.
                self.iso_init_configuration_header(
                    &external_data.subrange(0, header_size),
                    bytes_req,
                )?;
            }

            scsi::RequestType::ONE => {
                // Return only the Feature Descriptor that has been requested
                bytes_used += self.iso_get_feature_page(
                    &external_data.subrange(bytes_used, external_data.len() - bytes_used),
                    allocation_length - bytes_used,
                    scsi::FeatureNumber::try_from(starting_feature).unwrap(),
                    false,
                    &mut feature_size,
                )?;

                bytes_req += feature_size;

                self.iso_init_configuration_header(
                    &external_data.subrange(0, header_size),
                    bytes_req,
                )?;
            }
            _ => return Err(ScsiDvdError::IllegalRequestNoSenseData),
        }

        Ok(bytes_used)
    }

    fn handle_read_capacity_iso(
        &self,
        external_data: &RequestBuffers<'_>,
        sector_count: u64,
    ) -> Result<usize, ScsiDvdError> {
        let tx = size_of::<scsi::ReadCapacityData>();

        // media is no longer accessible
        self.validate_media_for_read()?;

        let last_lba = std::cmp::min(sector_count - 1, u32::MAX.into());
        let data = scsi::ReadCapacityData {
            logical_block_address: (last_lba as u32).into(),
            bytes_per_block: ISO_SECTOR_SIZE.into(),
        };

        external_data
            .writer()
            .write(&data.as_bytes()[..tx])
            .map_err(ScsiDvdError::MemoryAccess)?;

        Ok(tx)
    }

    fn handle_read_toc(
        &self,
        external_data: &RequestBuffers<'_>,
        request: &Request,
    ) -> Result<usize, ScsiDvdError> {
        let cdb = scsi::CdbReadToc::read_from_prefix(&request.cdb[..])
            .unwrap()
            .0; // TODO: zerocopy: use-rest-of-range (https://github.com/microsoft/openvmm/issues/759)
        let allocation_length = cdb.allocation_length.get() as usize;
        let format = cdb.format2 & 0x0f;
        let msf = cdb.flag1.msf();
        let mut formatted_toc = scsi::ReadTocFormattedToc {
            length: 0x0802.into(),
            first_complete_session: 0x01,
            last_complete_session: 0x01,
            track1: scsi::TrackData {
                reserved: 0x00,
                flag: scsi::TrackDataFlag::new().with_control(0x4).with_adr(0x1),
                track_number: 0x01,
                reserved1: 0x00,
                address: [0; 4],
            },
            trackaa: scsi::TrackData {
                reserved: 0x00,
                flag: scsi::TrackDataFlag::new().with_control(0x4).with_adr(0x1),
                track_number: 0xaa,
                reserved1: 0x00,
                address: [0; 4],
            },
        };
        let mut session_data = scsi::CdromTocSessionData {
            length: 0x000a.into(),
            first_complete_session: 0x01,
            last_complete_session: 0x01,
            track_data: scsi::TrackData {
                reserved: 0x00,
                flag: scsi::TrackDataFlag::new().with_control(0x4).with_adr(0x1),
                track_number: 0x01,
                reserved1: 0x00,
                address: [0; 4],
            },
        };
        let data_transfer_length: usize;

        self.validate_media_for_read()?;

        let last_lba = std::cmp::min(self.sector_count() - 1, u32::MAX.into());
        if allocation_length > external_data.len() {
            return Err(ScsiDvdError::DataOverrun);
        }
        match format {
            scsi::CDROM_READ_TOC_EX_FORMAT_TOC => {
                if msf {
                    formatted_toc.track1.address = [0x00, 0x00, 0x02, 0x00];
                    formatted_toc.trackaa.address = [
                        0x00,
                        (((last_lba + 150) / 75) / 60) as u8,
                        (((last_lba + 150) / 75) % 60) as u8,
                        ((last_lba + 150) % 75) as u8,
                    ]
                } else {
                    formatted_toc.track1.address = [0x00, 0x00, 0x00, 0x00];
                    formatted_toc.trackaa.address = [
                        (last_lba >> 24) as u8,
                        (last_lba >> 16) as u8,
                        (last_lba >> 8) as u8,
                        last_lba as u8,
                    ]
                }
                data_transfer_length =
                    std::cmp::min(allocation_length, size_of::<scsi::ReadTocFormattedToc>());
                external_data
                    .writer()
                    .write(&formatted_toc.as_bytes()[..data_transfer_length])
                    .map_err(ScsiDvdError::MemoryAccess)?;
            }
            scsi::CDROM_READ_TOC_EX_FORMAT_SESSION => {
                if msf {
                    session_data.track_data.address = [0x00, 0x00, 0x02, 0x00];
                } else {
                    session_data.track_data.address = [0x00, 0x00, 0x00, 0x00];
                }
                data_transfer_length =
                    std::cmp::min(allocation_length, size_of::<scsi::CdromTocSessionData>());
                external_data
                    .writer()
                    .write(&session_data.as_bytes()[..data_transfer_length])
                    .map_err(ScsiDvdError::MemoryAccess)?;
            }
            _ => {
                return Err(ScsiDvdError::IllegalRequest(
                    AdditionalSenseCode::INVALID_CDB,
                    0x00,
                ));
            }
        }
        Ok(data_transfer_length)
    }

    async fn handle_start_stop_unit(&self, request: &Request) -> Result<usize, ScsiDvdError> {
        let cdb = scsi::StartStop::read_from_prefix(&request.cdb[..])
            .unwrap()
            .0; // TODO: zerocopy: use-rest-of-range (https://github.com/microsoft/openvmm/issues/759)
        let start = cdb.flag.start();
        let load_eject = cdb.flag.load_eject();

        match (load_eject, start) {
            (false, false) => (), // stop the disc, do nothing here
            (false, true) => (),  // start the disc and make ready for access, do nothing here
            (true, false) => {
                let mut previous_media = None;
                {
                    let mut media_state = self.media_state.lock();
                    // eject the disc if permitted
                    if !media_state.prevent {
                        if media_state.drive_state == DriveState::MediumPresentTrayClosed {
                            let mut media = self.media.write();

                            // This will cause the next GESN or TUR command to report medium removal
                            media_state.pending_medium_event = IsoMediumEvent::MediaToNoMedia;

                            previous_media = Some(std::mem::replace(&mut *media, Media::Unloaded));

                            tracing::debug!("handling guest initiated eject");
                        }

                        media_state.drive_state = DriveState::MediumNotPresentTrayOpen;
                    } else if media_state.drive_state.medium_present() {
                        return Err(ScsiDvdError::IllegalRequest(
                            AdditionalSenseCode::MEDIUM_REMOVAL_PREVENTED,
                            0x02,
                        ));
                    } else {
                        return Err(ScsiDvdError::SenseNotReady(
                            AdditionalSenseCode::MEDIUM_REMOVAL_PREVENTED,
                            0x02,
                        ));
                    }
                }
                if let Some(media) = previous_media {
                    if let Media::Loaded(disk) = media {
                        if let Err(e) = disk.eject().await {
                            tracelimit::error_ratelimited!(error = ?e, "eject error");
                        } else {
                            tracing::debug!("guest initiated eject complete");
                        }
                    } else {
                        tracelimit::warn_ratelimited!("attempted to eject unloaded media");
                    }
                }
            }
            (true, true) => {
                let mut media_state = self.media_state.lock();
                if media_state.drive_state.tray_open() {
                    // Once the drive is ejected through SCSI, the media is permanently removed.
                    media_state.drive_state = DriveState::MediumNotPresentTrayClosed;
                }
            }
        }

        Ok(0)
    }

    fn handle_request_sense_iso(
        &self,
        external_data: &RequestBuffers<'_>,
        request: &Request,
    ) -> Result<usize, ScsiDvdError> {
        let cdb = scsi::CdbRequestSense::read_from_prefix(&request.cdb[..])
            .unwrap()
            .0; // TODO: zerocopy: use-rest-of-range (https://github.com/microsoft/openvmm/issues/759)
        let allocation_length = cdb.allocation_length as usize;
        let new_sense_data =
            SenseData::new(SenseKey::NO_SENSE, AdditionalSenseCode::NO_SENSE, 0x00);

        self.sense_data.set(Some(&new_sense_data));

        if external_data.len() < allocation_length {
            Err(ScsiDvdError::DataOverrun)
        } else {
            let tx = std::cmp::min(allocation_length, size_of::<SenseData>());
            external_data
                .writer()
                .write(&new_sense_data.as_bytes()[..tx])
                .map_err(ScsiDvdError::MemoryAccess)?;
            Ok(tx)
        }
    }

    fn handle_mode_sense_iso(
        &self,
        external_data: &RequestBuffers<'_>,
        request: &Request,
    ) -> Result<usize, ScsiDvdError> {
        let cdb = scsi::ModeSense10::read_from_prefix(&request.cdb[..])
            .unwrap()
            .0; // TODO: zerocopy: use-rest-of-range (https://github.com/microsoft/openvmm/issues/759)
        let page_code = cdb.flags2.page_code();
        let allocation_length = cdb.allocation_length.get() as usize;
        let pc = cdb.flags2.pc() << 6;

        let mut bytes_used = std::cmp::min(allocation_length, super::MODE_PARAMETER_HEADER10_SIZE);
        let buffer_size_header = bytes_used;
        let mut bytes_required = super::MODE_PARAMETER_HEADER10_SIZE as u16;
        let mut mode_page_size: u16 = 0;

        if external_data.len() < allocation_length {
            return Err(ScsiDvdError::DataOverrun);
        }

        if pc == scsi::MODE_SENSE_SAVED_VALUES {
            return Err(ScsiDvdError::IllegalRequest(
                AdditionalSenseCode::SAVING_PARAMETER_NOT_SUPPORTED,
                0,
            ));
        }

        // Copy as much data from the mode page table as the remaining buffer.
        // If remaining buffer is big enough to copy the whole page, copy
        // the whole page length.
        match page_code {
            scsi::MODE_PAGE_ERROR_RECOVERY
            | scsi::MODE_PAGE_POWER_CONDITION
            | scsi::MODE_PAGE_CDVD_INACTIVITY => {
                bytes_used += self
                    .iso_get_mode_page(
                        &external_data.subrange(bytes_used, external_data.len() - bytes_used),
                        allocation_length - bytes_used,
                        page_code,
                        &mut mode_page_size,
                    )
                    .unwrap();
                bytes_required += mode_page_size;
                self.iso_init_mode_sense_header(
                    &external_data.subrange(0, super::MODE_PARAMETER_HEADER10_SIZE),
                    allocation_length,
                    bytes_required,
                )?;
                Ok(bytes_used)
            }
            scsi::MODE_SENSE_RETURN_ALL => {
                for i in 0..scsi::LIST_OF_MODE_PAGES.len() {
                    bytes_used += self
                        .iso_get_mode_page(
                            &external_data.subrange(bytes_used, external_data.len() - bytes_used),
                            allocation_length - bytes_used,
                            scsi::LIST_OF_MODE_PAGES[i],
                            &mut mode_page_size,
                        )
                        .unwrap();
                    bytes_required += mode_page_size;
                }
                self.iso_init_mode_sense_header(
                    &external_data.subrange(0, buffer_size_header),
                    allocation_length,
                    bytes_required,
                )?;
                Ok(bytes_used)
            }
            _ => Err(ScsiDvdError::IllegalRequest(
                AdditionalSenseCode::INVALID_CDB,
                0,
            )),
        }
    }

    fn handle_medium_removal_iso(&self, request: &Request) -> Result<usize, ScsiDvdError> {
        let cdb = scsi::CdbMediaRemoval::read_from_prefix(&request.cdb[..])
            .unwrap()
            .0; // TODO: zerocopy: use-rest-of-range (https://github.com/microsoft/openvmm/issues/759)

        // prevent/allow media removal based on the Persistent/Prevent bits
        let mut media_state = self.media_state.lock();
        media_state.persistent = cdb.flags.persistent();
        media_state.prevent = cdb.flags.prevent();
        Ok(0)
    }

    fn handle_mode_select_iso(
        &self,
        request: &Request,
        external_data: &RequestBuffers<'_>,
    ) -> Result<usize, ScsiDvdError> {
        let cdb = scsi::ModeSelect10::read_from_prefix(&request.cdb[..])
            .unwrap()
            .0; // TODO: zerocopy: use-rest-of-range (https://github.com/microsoft/openvmm/issues/759)

        let sp_bit = cdb.flags.spbit();
        let request_length = cdb.parameter_list_length.get() as usize;
        if request_length == 0 {
            return Ok(0);
        }

        if request_length > external_data.len() {
            return Err(ScsiDvdError::DataOverrun);
        }

        // If the parameter list length results in the truncation of any mode parameter header,
        // mode parameter block descriptor(s), or mode page, then the command shall be terminated
        // with CHECK CONDITION status, with the sense key set to ILLEGAL REQUEST, and the
        // additional sense code set to PARAMETER LIST LENGTH ERROR.
        if request_length < super::MODE_PARAMETER_HEADER10_SIZE {
            return Err(ScsiDvdError::IllegalRequest(
                AdditionalSenseCode::PARAMETER_LIST_LENGTH,
                0x00,
            ));
        }

        // Don't support saving pages.
        if sp_bit {
            return Err(ScsiDvdError::IllegalRequest(
                AdditionalSenseCode::INVALID_CDB,
                0x00,
            ));
        }

        if request_length == super::MODE_PARAMETER_HEADER10_SIZE {
            return Ok(0);
        }

        // Parse pages.
        // All 3 mode pages supported have the same length
        if request_length
            < super::MODE_PARAMETER_HEADER10_SIZE + size_of::<scsi::ModeReadWriteRecoveryPage>()
        {
            return Err(ScsiDvdError::IllegalRequest(
                AdditionalSenseCode::PARAMETER_LIST_LENGTH,
                0x00,
            ));
        }

        let mut buffer: Vec<u8> = vec![0; request_length];
        external_data
            .reader()
            .read(&mut buffer)
            .map_err(ScsiDvdError::MemoryAccess)?;

        let mode_page_error_recovery = scsi::ModeReadWriteRecoveryPage::read_from_prefix(
            &buffer[super::MODE_PARAMETER_HEADER10_SIZE
                ..super::MODE_PARAMETER_HEADER10_SIZE
                    + size_of::<scsi::ModeReadWriteRecoveryPage>()],
        )
        .unwrap()
        .0; // TODO: zerocopy: from-prefix (read_from_prefix): use-rest-of-range, zerocopy: err (https://github.com/microsoft/openvmm/issues/759)
        match mode_page_error_recovery.page_code {
            scsi::MODE_PAGE_ERROR_RECOVERY => {
                // ModePageErrorRecovery = (PMODE_READ_WRITE_RECOVERY_PAGE) (Buffer + sizeof (MODE_PARAMETER_HEADER10));

                // parse Read/Write error recovery mode page
                Ok(0)
            }
            scsi::MODE_PAGE_POWER_CONDITION => {
                // ModePagePowerCondition = (PPOWER_CONDITION_PAGE) (Buffer + sizeof (MODE_PARAMETER_HEADER10));

                // parse power condition mode page
                Ok(0)
            }
            scsi::MODE_PAGE_CDVD_INACTIVITY => {
                // ModePageTimeoutProtect = (PMODE_SENSE_MODE_PAGE_TIMEOUT_PROTECT) (Buffer + sizeof (MODE_PARAMETER_HEADER10));

                // parse timeout and protect mode page
                Ok(0)
            }
            _ => Err(ScsiDvdError::IllegalRequestNoSenseData),
        }
    }

    fn handle_read_track_information_iso(
        &self,
        external_data: &RequestBuffers<'_>,
        request: &Request,
    ) -> Result<usize, ScsiDvdError> {
        let cdb = scsi::CdbReadTrackInformation::read_from_prefix(&request.cdb[..])
            .unwrap()
            .0; // TODO: zerocopy: use-rest-of-range (https://github.com/microsoft/openvmm/issues/759)
        let number_type = cdb.flag.number_type();
        let open = cdb.flag.open();
        let logical_track_number = cdb.logical_track_number.get() as usize;
        let allocation_length = cdb.allocation_length.get() as usize;

        self.validate_media_for_read()?;
        let last_lba = std::cmp::min(self.sector_count() - 1, u32::MAX.into()) as u32;

        if allocation_length > external_data.len() {
            return Err(ScsiDvdError::DataOverrun);
        }

        // If it is not possible for the currently mounted disc to have open tracks, and Open is set to one,
        // then the command shall be terminated with CHECK CONDITION status and sense bytes SK/ASC/ASCQ
        // shall be set to ILLEGAL REQUEST/ INVALID FIELD IN CDB.
        if open {
            return Err(ScsiDvdError::IllegalRequest(
                AdditionalSenseCode::INVALID_CDB,
                0x00,
            ));
        }

        match number_type {
            0x0 => {
                // MAX = Last Possible Lead-out Start Address as returned by the READ DISC INFORMATION
                // command (0x00). If LBA >= MAX, the command shall be terminated with CHECK CONDITION status
                // and sense bytes SK/ASC/ ASCQ shall be set to ILLEGAL REQUEST/LOGICAL BLOCK ADDRESS OUT OF RANGE.
                return Err(ScsiDvdError::IllegalRequest(
                    AdditionalSenseCode::ILLEGAL_BLOCK,
                    0x00,
                ));
            }
            0x1 => {
                if logical_track_number != 1 {
                    // If the currently mounted disc is not CD, the command shall be terminated with CHECK CONDITION
                    // status and sense bytes SK/ASC/ASCQ shall be set to ILLEGAL REQUEST/INVALID FIELD IN CDB
                    //
                    // If TM is the Last Track Number in the Last Session as returned in READ DISC INFORMATION command
                    // Standard Disc Information. If LTN > TM, the command shall be terminated with CHECK CONDITION status
                    // and sense bytes SK/ASC/ASCQ shall be set to ILLEGAL REQUEST/INVALID FIELD IN CDB.
                    return Err(ScsiDvdError::IllegalRequest(
                        AdditionalSenseCode::INVALID_CDB,
                        0x00,
                    ));
                }
            }
            0x2 => {
                // SM is the Number of Sessions as returned by the READ DISC INFORMATION command. If LogicalTrackNumber > SM,
                // the command shall be terminated with CHECK CONDITION status and sense bytes SK/ASC/ASCQ shall be set to
                // ILLEGAL REQUEST/INVALID FIELD IN CDB.
                if logical_track_number != 1 {
                    return Err(ScsiDvdError::IllegalRequest(
                        AdditionalSenseCode::INVALID_CDB,
                        0x00,
                    ));
                }
            }
            _ => {
                return Err(ScsiDvdError::IllegalRequestNoSenseData);
            }
        }
        // Fill in logical track size
        let track_information = scsi::TrackInformation3 {
            length: 0x002e.into(),
            track_number_lsb: 0x01,
            session_number_lsb: 0x01,
            reserved: 0x00,
            track_mode: 0x04,
            /*
               UCHAR DataMode      : 4 = 0x1;
               UCHAR FixedPacket   : 1 = 0x0;
               UCHAR Packet        : 1 = 0x1;
               UCHAR Blank         : 1 = 0x0;
               UCHAR ReservedTrack : 1 = 0x0;
            */
            data_mode: 0b00100001,
            track_size: last_lba.into(),
            ..FromZeros::new_zeroed()
        };
        let tx = std::cmp::min(allocation_length, size_of::<scsi::TrackInformation3>());
        external_data
            .writer()
            .write(&track_information.as_bytes()[..tx])
            .map_err(ScsiDvdError::MemoryAccess)?;
        Ok(0)
    }

    fn handle_read_dvd_structure(
        &self,
        external_data: &RequestBuffers<'_>,
        request: &Request,
    ) -> Result<usize, ScsiDvdError> {
        let cdb = scsi::CdbReadDVDStructure::read_from_prefix(&request.cdb[..])
            .unwrap()
            .0; // TODO: zerocopy: use-rest-of-range (https://github.com/microsoft/openvmm/issues/759)
        let media_type = cdb.media_type;
        let layer = cdb.layer;
        let allocation_length = cdb.allocation_length.get() as usize;

        self.validate_media_for_read()?;

        if allocation_length > external_data.len() {
            return Err(ScsiDvdError::DataOverrun);
        }

        // only single layer DVD media type is supported
        if media_type != 0 {
            return Err(ScsiDvdError::IllegalRequest(
                AdditionalSenseCode::INVALID_MEDIA,
                scsi::SCSI_SENSEQ_INCOMPATIBLE_FORMAT,
            ));
        }

        if layer != 0 {
            return Err(ScsiDvdError::IllegalRequestNoSenseData);
        }

        match cdb.format {
            scsi::DVD_FORMAT_LEAD_IN => {
                let tx = std::cmp::min(
                    allocation_length,
                    size_of::<scsi::ReadDVDStructurePhysicalFormatInformation>(),
                );
                let data = scsi::ReadDVDStructurePhysicalFormatInformation {
                    length: 0x802.into(),
                    reserved: [0x00, 0x00],
                    reserved2: 0x00,    // Part Version = 0 & Disk Category = DVD-ROM
                    maximum_rate: 0x04, // Maximum Rate = 30.24 Mbps & Disc Size = 120 mm,
                    /*
                        Layer Type = layer contains embossed data
                        Track = PTP
                        Number of Layers = 1
                        Reserved
                    */
                    layer: 0x00,
                    reserved3: 0x00, // Track Density = 0.74 um/track & Linear Density = 0.267 um/bit,
                    reserved4: 0x00,
                    starting_physical_sector: [0x03, 0x00, 0x00],
                    reserved5: 0x00,
                    end_physical_sector: [0x00, 0x00, 0x00],
                    reserved6: 0x00,
                    end_physical_sector_in_layer0: [0x00, 0x00, 0x00],
                    bca: 0x00,
                };
                external_data
                    .writer()
                    .write(&data.as_bytes()[..tx])
                    .map_err(ScsiDvdError::MemoryAccess)?;
                Ok(tx)
            }
            scsi::DVD_FORMAT_COPYRIGHT => {
                let tx = std::cmp::min(
                    allocation_length,
                    size_of::<scsi::ReadDVDStructureCopyrightInformation>(),
                );
                let data = scsi::ReadDVDStructureCopyrightInformation {
                    data_length: 0x0006.into(),
                    reserved: 0x00,
                    copyright_protection_system: 0x00,
                    region_management_information: 0x00,
                    reserved2: [0x00, 0x00],
                };
                external_data
                    .writer()
                    .write(&data.as_bytes()[..tx])
                    .map_err(ScsiDvdError::MemoryAccess)?;
                Ok(tx)
            }
            scsi::DVD_FORMAT_BCA => {
                // It is not mandatory for DVD players to support reading the BCA,
                // but DVD-ROM drives should according to the Mount Fuji specification.
                // However, if a DVD media does not have BCA, the command should be terminated.
                Err(ScsiDvdError::IllegalRequestNoSenseData)
            }
            scsi::DVD_FORMAT_MANUFACTURING => {
                let tx = std::cmp::min(
                    allocation_length,
                    size_of::<scsi::ReadDVDStructureManufacturingStructure>(),
                );
                let data = scsi::ReadDVDStructureManufacturingStructure {
                    data_length: 0x0802.into(),
                    reserved: [0x00, 0x00],
                };
                external_data
                    .writer()
                    .write(&data.as_bytes()[..tx])
                    .map_err(ScsiDvdError::MemoryAccess)?;
                Ok(tx)
            }
            _ => Err(ScsiDvdError::IllegalRequest(
                AdditionalSenseCode::INVALID_CDB,
                0x00,
            )),
        }
    }

    fn handle_get_performance(
        &self,
        external_data: &RequestBuffers<'_>,
        request: &Request,
    ) -> Result<usize, ScsiDvdError> {
        let cdb = scsi::CdbGetPerformance::read_from_prefix(&request.cdb[..])
            .unwrap()
            .0; // TODO: zerocopy: use-rest-of-range (https://github.com/microsoft/openvmm/issues/759)
        let data_type = cdb.data_type;
        let write = cdb.flags.write();
        let tolerance = cdb.flags.tolerance();
        let except = cdb.flags.except();
        let mut nominal_performance = scsi::GetPerformanceNominalPerformanceDescriptor {
            start_lba: 0.into(),
            start_performance: scsi::PERFORMANCE_1000_BYTES_PER_SECOND.into(),
            end_lba: 0.into(),
            end_performance: scsi::PERFORMANCE_1000_BYTES_PER_SECOND.into(),
        };

        let maximum_number_of_descriptors = cdb.maximum_number_of_descriptors.get();
        let mut data_transfer_length: u64;

        self.validate_media_for_read()?;

        let last_lba = std::cmp::min(self.sector_count() - 1, u32::MAX.into())
            .try_into()
            .expect("last_lba must be u32");
        match data_type {
            scsi::PERFORMANCE_TYPE_PERFORMANCE_DATA => {
                // only read performance data will be returned
                // 0x2 is the only defined tolerance value
                if write || (tolerance != 0x2) {
                    return Err(ScsiDvdError::IllegalRequestNoSenseData);
                }
                match except {
                    scsi::PERFORMANCE_EXCEPT_NOMINAL_PERFORMANCE => {
                        // Patch the end lba
                        nominal_performance.end_lba.set(last_lba);
                        // The Performance Data Length field shall specify the amount of result data excluding
                        // the Performance Data Length itself
                        let header_size = self.iso_init_performance_header(
                            external_data,
                            external_data.len().try_into().unwrap(),
                            (size_of::<scsi::GetPerformanceHeader>()
                                + size_of::<scsi::GetPerformanceNominalPerformanceDescriptor>()
                                - size_of::<u64>())
                            .try_into()
                            .unwrap(),
                            0,
                        )?;
                        data_transfer_length = header_size.try_into().unwrap();
                        if maximum_number_of_descriptors >= 1 {
                            let body_size = std::cmp::min(
                                external_data.len() - header_size,
                                size_of::<scsi::GetPerformanceNominalPerformanceDescriptor>(),
                            );
                            external_data
                                .subrange(header_size, external_data.len() - header_size)
                                .writer()
                                .write(&nominal_performance.as_bytes()[..body_size])
                                .map_err(ScsiDvdError::MemoryAccess)?;
                            data_transfer_length += body_size as u64;
                        }
                    }
                    scsi::PERFORMANCE_EXCEPT_ENTIRE_PERFORMANCE_LIST
                    | scsi::PERFORMANCE_EXCEPT_PERFORMANCE_EXCEPTIONS_ONLY => {
                        // The Performance Data Length field shall specify the amount of result data excluding
                        // the Performance Data Length itself
                        let header_size = self.iso_init_performance_header(
                            external_data,
                            external_data.len().try_into().unwrap(),
                            (size_of::<scsi::GetPerformanceHeader>() - size_of::<u64>())
                                .try_into()
                                .unwrap(),
                            1,
                        )?;
                        data_transfer_length = header_size.try_into().unwrap();
                    }
                    _ => {
                        return Err(ScsiDvdError::IllegalRequestNoSenseData);
                    }
                }
            }
            _ => {
                // don't support all other types
                return Err(ScsiDvdError::IllegalRequest(
                    AdditionalSenseCode::INVALID_CDB,
                    0x00,
                ));
            }
        }
        Ok(data_transfer_length as usize)
    }

    fn handle_mechanism_status(
        &self,
        external_data: &RequestBuffers<'_>,
        request: &Request,
    ) -> Result<usize, ScsiDvdError> {
        let cdb = scsi::CdbMechStatus::read_from_prefix(&request.cdb[..])
            .unwrap()
            .0; // TODO: zerocopy: use-rest-of-range (https://github.com/microsoft/openvmm/issues/759)
        let allocation_length = cdb.allocation_length.get() as usize;

        if allocation_length > external_data.len() {
            return Err(ScsiDvdError::DataOverrun);
        }

        // copy the standard header
        // since the device is a DVD-ROM, not a medium changer, no slot table will be returned
        let mechanism_status_header: scsi::MechanismStatusHeader = scsi::MechanismStatusHeader {
            flags: scsi::MechanismStatusHeaderFlags::new()
                .with_door_open(self.media_state.lock().drive_state.tray_open()),
            ..FromZeros::new_zeroed()
        };
        let tx = std::cmp::min(allocation_length, size_of::<scsi::MechanismStatusHeader>());
        external_data
            .writer()
            .write(&mechanism_status_header.as_bytes()[..tx])
            .map_err(ScsiDvdError::MemoryAccess)?;

        Ok(tx)
    }

    fn handle_read_buffer_capacity(
        &self,
        external_data: &RequestBuffers<'_>,
        request: &Request,
    ) -> Result<usize, ScsiDvdError> {
        let cdb = scsi::CdbReadBufferCapacity::read_from_prefix(&request.cdb[..])
            .unwrap()
            .0; // TODO: zerocopy: use-rest-of-range (https://github.com/microsoft/openvmm/issues/759)
        let block_info = cdb.flags.block_info();
        let allocation_length = cdb.allocation_length.get() as usize;

        if allocation_length > external_data.len() {
            return Err(ScsiDvdError::DataOverrun);
        }

        let tx = std::cmp::min(allocation_length, size_of::<scsi::ReadBufferCapacityData>());

        let data = scsi::ReadBufferCapacityData {
            data_length: (size_of::<scsi::ReadBufferCapacityData>() as u16 - 2).into(),
            block_data_returned: block_info as u8,
            ..FromZeros::new_zeroed()
        };

        external_data
            .writer()
            .write(&data.as_bytes()[..tx])
            .map_err(ScsiDvdError::MemoryAccess)?;

        Ok(tx)
    }

    fn handle_read_disc_information(
        &self,
        external_data: &RequestBuffers<'_>,
        request: &Request,
    ) -> Result<usize, ScsiDvdError> {
        let cdb = scsi::CdbReadDiscInformation::read_from_prefix(&request.cdb[..])
            .unwrap()
            .0; // TODO: zerocopy: use-rest-of-range (https://github.com/microsoft/openvmm/issues/759)
        let data_type = cdb.flags.data_type();
        let allocation_length = cdb.allocation_length.get() as usize;

        self.validate_media_for_read()?;
        if allocation_length > external_data.len() {
            return Err(ScsiDvdError::DataOverrun);
        }

        // only standard disc information is supported
        if data_type != 0x0 {
            return Err(ScsiDvdError::IllegalRequest(
                AdditionalSenseCode::INVALID_CDB,
                0x00,
            ));
        }

        let tx = std::cmp::min(allocation_length, size_of::<scsi::DiscInformation>());
        let data = scsi::DiscInformation {
            length: 0x0020.into(),
            flags1: scsi::DiscInfoFlags1::new()
                .with_disc_status(0x2)
                .with_last_session_status(0x3)
                .with_erasable(false)
                .with_reserved1(0x0),
            first_track_number: 0x01,
            number_of_sessions_lsb: 0x01,
            last_session_first_track_lsb: 0x01,
            last_session_last_track_lsb: 0x01,
            flags2: scsi::DiscInfoFlags2::new()
                .with_mrw_status(0)
                .with_mrw_dirty_bit(false)
                .with_reserved2(0)
                .with_uru(true)
                .with_dbc_v(false)
                .with_did_v(false),
            ..FromZeros::new_zeroed()
        };
        external_data
            .writer()
            .write(&data.as_bytes()[..tx])
            .map_err(ScsiDvdError::MemoryAccess)?;

        Ok(tx)
    }

    fn handle_set_streaming(
        &self,
        external_data: &RequestBuffers<'_>,
        request: &Request,
    ) -> Result<usize, ScsiDvdError> {
        let cdb = scsi::CdbSetStreaming::read_from_prefix(&request.cdb[..])
            .unwrap()
            .0; // TODO: zerocopy: use-rest-of-range (https://github.com/microsoft/openvmm/issues/759)
        let parameter_list_length = usize::from(cdb.parameter_list_length);
        let mut buffer = vec![0; parameter_list_length];

        if parameter_list_length == 0 {
            return Ok(0);
        }

        self.validate_media_for_read()?;

        if parameter_list_length > external_data.len() {
            return Err(ScsiDvdError::DataOverrun);
        }

        // If the Parameter List Length results in the truncation of Performance Descriptor,
        // the command shall be terminated with CHECK CONDITION status and sense bytes SK/ASC/ASCQ
        // shall be set to ILLEGAL REQUEST/PARAMETER LIST LENGTH ERROR.
        if parameter_list_length < size_of::<scsi::SetStreamingPerformanceDescriptor>() {
            return Err(ScsiDvdError::IllegalRequest(
                AdditionalSenseCode::PARAMETER_LIST_LENGTH,
                0x00,
            ));
        }

        // process the command
        // just do sanity check, and success the command if nothing specified is wrong
        external_data
            .reader()
            .read(&mut buffer)
            .map_err(ScsiDvdError::MemoryAccess)?;
        let performance_descriptor =
            scsi::SetStreamingPerformanceDescriptor::read_from_prefix(&buffer[..])
                .unwrap()
                .0; // TODO: zerocopy: use-rest-of-range (https://github.com/microsoft/openvmm/issues/759)

        // If RDD bit is set to one, it shall indicate that the drive is to return to its
        // default performance settings and the remaining fields in this descriptor shall be ignored.
        if performance_descriptor.flags.rdd() {
            return Ok(0);
        }
        let last_lba = std::cmp::min(self.sector_count() - 1, u32::MAX.into()) as u32;

        if u32::from(performance_descriptor.start_lba) > last_lba
            || u32::from(performance_descriptor.end_lba) > last_lba
        {
            return Err(ScsiDvdError::IllegalRequestNoSenseData);
        }

        Ok(0)
    }
}

impl SimpleScsiDvd {
    fn iso_init_event_header(
        &self,
        external_data: &RequestBuffers<'_>,
        buffer_size: usize,
        event_data_length: u16,
        nea: bool,
        notification_class: u8,
    ) -> Result<usize, ScsiDvdError> {
        let mut header: scsi::NotificationEventStatusHeader =
            scsi::NotificationEventStatusHeader::new_zeroed();
        header.event_data_length.set(event_data_length);
        header.flags.set_nea(nea);
        header.flags.set_notification_class(notification_class);
        header.supported_event_classes = scsi::NOTIFICATION_OPERATIONAL_CHANGE_CLASS_MASK
            | scsi::NOTIFICATION_POWER_MANAGEMENT_CLASS_MASK
            | scsi::NOTIFICATION_EXTERNAL_REQUEST_CLASS_MASK
            | scsi::NOTIFICATION_MEDIA_STATUS_CLASS_MASK
            | scsi::NOTIFICATION_MULTI_HOST_CLASS_MASK
            | scsi::NOTIFICATION_DEVICE_BUSY_CLASS_MASK;
        let tx: usize = std::cmp::min(
            size_of::<scsi::NotificationEventStatusHeader>(),
            buffer_size,
        );
        external_data
            .writer()
            .write(&header.as_bytes()[..tx])
            .map_err(ScsiDvdError::MemoryAccess)?;
        Ok(tx)
    }

    fn iso_get_feature_page(
        &self,
        external_data: &RequestBuffers<'_>,
        buffer_size: usize,
        feature_code: scsi::FeatureNumber,
        current_only: bool,
        feature_size: &mut usize,
    ) -> Result<usize, ScsiDvdError> {
        let mut bytes_used: usize = 0;
        let mut profile_list = scsi::GetConfigurationFeatureDataProfileList {
            header: scsi::FeatureHeader {
                feature_code: (scsi::FeatureNumber::FeatureProfileList as u16).into(),
                flags: scsi::FeatureHeaderFlags::new()
                    .with_current(true)
                    .with_persistent(true)
                    .with_version(0)
                    .with_reserved(0),
                additional_length: (2 * (size_of::<scsi::FeatureDataProfileList>() as u8)),
            },
            profile: [
                scsi::FeatureDataProfileList {
                    profile_number: scsi::PROFILE_DVD_ROM.into(),
                    current: 0x01,
                    reserved: 0x00,
                },
                scsi::FeatureDataProfileList {
                    profile_number: scsi::PROFILE_CD_ROM.into(),
                    current: 0x00,
                    reserved: 0x00,
                },
            ],
        };
        let mut random_readable = scsi::FeatureDataRandomReadable {
            header: scsi::FeatureHeader {
                feature_code: (scsi::FeatureNumber::FeatureRandomReadable as u16).into(),
                flags: scsi::FeatureHeaderFlags::new()
                    .with_current(true)
                    .with_persistent(false)
                    .with_version(0)
                    .with_reserved(0),
                additional_length: 0x08,
            },
            logical_block_size: scsi::ISO_SECTOR_SIZE.into(),
            blocking: 16.into(),
            error_recovery_page_present: 0x01,
            reserved: 0x00,
        };

        let mut dvd_read = scsi::FeatureDataDvdRead {
            header: scsi::FeatureHeader {
                feature_code: (scsi::FeatureNumber::FeatureDvdRead as u16).into(),
                flags: scsi::FeatureHeaderFlags::new()
                    .with_current(true)
                    .with_persistent(false)
                    .with_version(0x01)
                    .with_reserved(0),
                additional_length: 0x04,
            },
            multi_110: 0x00,
            reserved: 0x00,
            dual_dash_r: 0x00,
            reserved2: 0x00,
        };

        let mut real_time_streaming = scsi::FeatureDataRealTimeStreaming {
            header: scsi::FeatureHeader {
                feature_code: (FeatureRealTimeStreaming as u16).into(),
                flags: scsi::FeatureHeaderFlags::new()
                    .with_current(true)
                    .with_persistent(true)
                    .with_version(0x05)
                    .with_reserved(0),
                additional_length: 0x04,
            },
            flags: scsi::RealTimeStreamingFlags::new()
                .with_stream_recording(false)
                .with_write_speed_in_get_perf(false)
                .with_write_speed_in_mp2_a(false)
                .with_set_cdspeed(false)
                .with_read_buffer_capacity_block(true)
                .with_reserved1(0x00),
            reserved: [0x00, 0x00, 0x00],
        };

        *feature_size = 0;
        match feature_code {
            scsi::FeatureNumber::FeatureProfileList => {
                profile_list.profile[0].current =
                    self.media_state.lock().drive_state.medium_present() as u8;
                *feature_size = size_of::<scsi::GetConfigurationFeatureDataProfileList>();
                bytes_used = std::cmp::min(*feature_size, buffer_size);
                if bytes_used > 0 {
                    external_data
                        .writer()
                        .write(&profile_list.as_bytes()[..bytes_used])
                        .map_err(ScsiDvdError::MemoryAccess)?;
                }
            }
            scsi::FeatureNumber::FeatureCore => {
                *feature_size = size_of::<scsi::FeatureDataCore>();
                bytes_used = std::cmp::min(*feature_size, buffer_size);
                let data = scsi::FeatureDataCore {
                    header: scsi::FeatureHeader {
                        feature_code: (scsi::FeatureNumber::FeatureCore as u16).into(),
                        flags: scsi::FeatureHeaderFlags::new()
                            .with_current(true)
                            .with_persistent(true)
                            .with_version(0x02)
                            .with_reserved(0),
                        additional_length: 0x08,
                    },
                    ..FromZeros::new_zeroed()
                };
                if bytes_used > 0 {
                    external_data
                        .writer()
                        .write(&data.as_bytes()[..bytes_used])
                        .map_err(ScsiDvdError::MemoryAccess)?;
                }
            }
            scsi::FeatureNumber::FeatureMorphing => {
                *feature_size = size_of::<scsi::FeatureDataMorphing>();
                bytes_used = std::cmp::min(*feature_size, buffer_size);
                let data = scsi::FeatureDataMorphing {
                    header: scsi::FeatureHeader {
                        feature_code: (scsi::FeatureNumber::FeatureMorphing as u16).into(),
                        flags: scsi::FeatureHeaderFlags::new()
                            .with_current(true)
                            .with_persistent(true)
                            .with_version(0x02)
                            .with_reserved(0),
                        additional_length: 0x08,
                    },
                    flags: scsi::FeatureMorphingFlags::new()
                        .with_asynchronous(false)
                        .with_ocevent(true)
                        .with_reserved1(0x00),
                    reserved2: [0x00, 0x00, 0x00],
                };
                if bytes_used > 0 {
                    external_data
                        .writer()
                        .write(&data.as_bytes()[..bytes_used])
                        .map_err(ScsiDvdError::MemoryAccess)?;
                }
            }
            scsi::FeatureNumber::FeatureRemovableMedium => {
                *feature_size = size_of::<scsi::FeatureDataRemovableMedium>();
                bytes_used = std::cmp::min(*feature_size, buffer_size);
                let data = scsi::FeatureDataRemovableMedium {
                    header: scsi::FeatureHeader {
                        feature_code: (scsi::FeatureNumber::FeatureRemovableMedium as u16).into(),
                        flags: scsi::FeatureHeaderFlags::new()
                            .with_current(true)
                            .with_persistent(true)
                            .with_version(0x01)
                            .with_reserved(0),
                        additional_length: 0x04,
                    },
                    flags: scsi::RemovableMediumFlags::new()
                        .with_lockable(true)
                        .with_dbml(false)
                        .with_default_to_prevent(false)
                        .with_eject(true)
                        .with_load(true)
                        .with_loading_mechanism(0x01),
                    reserved2: [0x00, 0x00, 0x00],
                };
                if bytes_used > 0 {
                    external_data
                        .writer()
                        .write(&data.as_bytes()[..bytes_used])
                        .map_err(ScsiDvdError::MemoryAccess)?;
                }
            }
            scsi::FeatureNumber::FeatureRandomReadable => {
                let medium_present = self.media_state.lock().drive_state.medium_present();
                if !current_only || medium_present {
                    random_readable.header.flags.set_current(medium_present);
                    *feature_size = size_of::<scsi::FeatureDataRandomReadable>();
                    bytes_used = std::cmp::min(*feature_size, buffer_size);
                    if bytes_used > 0 {
                        external_data
                            .writer()
                            .write(&random_readable.as_bytes()[..bytes_used])
                            .map_err(ScsiDvdError::MemoryAccess)?;
                    }
                }
            }
            scsi::FeatureNumber::FeatureCdRead => {
                if !current_only {
                    *feature_size = size_of::<scsi::FeatureDataCdRead>();
                    bytes_used = std::cmp::min(*feature_size, buffer_size);
                    let data = scsi::FeatureDataCdRead {
                        header: scsi::FeatureHeader {
                            feature_code: (scsi::FeatureNumber::FeatureCdRead as u16).into(),
                            flags: scsi::FeatureHeaderFlags::new()
                                .with_current(false)
                                .with_persistent(false)
                                .with_version(0x02)
                                .with_reserved(0),
                            additional_length: 0x04,
                        },
                        flags: scsi::CDReadFlags::new()
                            .with_cd_text(false)
                            .with_c2_error_data(false)
                            .with_reserved(0x00)
                            .with_digital_audio_play(false),
                        reserved2: [0x00, 0x00, 0x00],
                    };
                    if bytes_used > 0 {
                        external_data
                            .writer()
                            .write(&data.as_bytes()[..bytes_used])
                            .map_err(ScsiDvdError::MemoryAccess)?;
                    }
                }
            }
            scsi::FeatureNumber::FeatureDvdRead => {
                let medium_present = self.media_state.lock().drive_state.medium_present();
                if !current_only || medium_present {
                    *feature_size = size_of::<scsi::FeatureDataDvdRead>();
                    bytes_used = std::cmp::min(*feature_size, buffer_size);
                    dvd_read.header.flags.set_current(medium_present);
                    if bytes_used > 0 {
                        external_data
                            .writer()
                            .write(&dvd_read.as_bytes()[..bytes_used])
                            .map_err(ScsiDvdError::MemoryAccess)?;
                    }
                }
            }
            FeaturePowerManagement => {
                *feature_size = size_of::<scsi::FeatureDataPowerManagement>();
                bytes_used = std::cmp::min(*feature_size, buffer_size);
                let data = scsi::FeatureDataPowerManagement {
                    header: scsi::FeatureHeader {
                        feature_code: (FeaturePowerManagement as u16).into(),
                        flags: scsi::FeatureHeaderFlags::new()
                            .with_current(true)
                            .with_persistent(true)
                            .with_version(0x00)
                            .with_reserved(0),
                        additional_length: 0x00,
                    },
                };
                if bytes_used > 0 {
                    external_data
                        .writer()
                        .write(&data.as_bytes()[..bytes_used])
                        .map_err(ScsiDvdError::MemoryAccess)?;
                }
            }
            FeatureTimeout => {
                *feature_size = size_of::<scsi::FeatureDataTimeout>();
                bytes_used = std::cmp::min(*feature_size, buffer_size);
                let data = scsi::FeatureDataTimeout {
                    header: scsi::FeatureHeader {
                        feature_code: (FeatureTimeout as u16).into(),
                        flags: scsi::FeatureHeaderFlags::new()
                            .with_current(true)
                            .with_persistent(true)
                            .with_version(0x01)
                            .with_reserved(0),
                        additional_length: 0x04,
                    },
                    group: 0x01,
                    reserved: 0x00,
                    unit_length: 512.into(),
                };
                if bytes_used > 0 {
                    external_data
                        .writer()
                        .write(&data.as_bytes()[..bytes_used])
                        .map_err(ScsiDvdError::MemoryAccess)?;
                }
            }
            FeatureRealTimeStreaming => {
                let medium_present = self.media_state.lock().drive_state.medium_present();
                if !current_only || medium_present {
                    *feature_size = size_of::<scsi::FeatureDataRealTimeStreaming>();
                    bytes_used = std::cmp::min(*feature_size, buffer_size);
                    real_time_streaming.header.flags.set_current(medium_present);
                    if bytes_used > 0 {
                        external_data
                            .writer()
                            .write(&real_time_streaming.as_bytes()[..bytes_used])
                            .map_err(ScsiDvdError::MemoryAccess)?;
                    }
                }
            }
            _ => {}
        }
        Ok(bytes_used)
    }

    fn iso_init_configuration_header(
        &self,
        external_data: &RequestBuffers<'_>,
        data_length: usize,
    ) -> Result<usize, ScsiDvdError> {
        let current_profile = if self.media_state.lock().drive_state.medium_present() {
            scsi::PROFILE_DVD_ROM
        } else {
            0
        };

        // data_length field indicates the amount of data available given a sufficient allocation length
        // following this field. Thus the size of itself should be excluded.
        let header = scsi::GetConfigurationHeader {
            data_length: (data_length as u32).into(),
            current_profile: current_profile.into(),
            ..FromZeros::new_zeroed()
        };

        let tx = external_data.len();

        external_data
            .writer()
            .write(&header.as_bytes()[..tx])
            .map_err(ScsiDvdError::MemoryAccess)?;

        Ok(tx)
    }

    fn handle_no_vpd_page_iso(
        &self,
        external_data: &RequestBuffers<'_>,
        allocation_length: usize,
    ) -> Result<usize, ScsiDvdError> {
        // return standard inquiry data
        let data = scsi::InquiryData {
            header: scsi::InquiryDataHeader {
                device_type: 0x05,
                flags2: scsi::InquiryDataFlag2::new().with_removable_media(true),
                versions: 0x00,
                flags3: scsi::InquiryDataFlag3::new()
                    .with_response_data_format(scsi::T10_RESPONSE_DATA_SPC3),
                additional_length: (scsi::INQUIRY_DATA_BUFFER_SIZE
                    - (size_of::<scsi::InquiryDataHeader>() as u8)),
            },
            vendor_id: *b"Msft    ",
            product_id: *b"Virtual DVD-ROM ",
            product_revision_level: *b"1.0 ",
            ..FromZeros::new_zeroed()
        };

        let tx = std::cmp::min(allocation_length, size_of::<scsi::InquiryData>());
        external_data
            .writer()
            .write(&data.as_bytes()[..tx])
            .map_err(ScsiDvdError::MemoryAccess)?;

        Ok(tx)
    }

    fn handle_vpd_supported_pages_iso(
        &self,
        external_data: &RequestBuffers<'_>,
        allocation_length: usize,
    ) -> Result<usize, ScsiDvdError> {
        let page = [scsi::VPD_SUPPORTED_PAGES, scsi::VPD_DEVICE_IDENTIFIERS];

        write_vpd_page(
            external_data,
            allocation_length,
            scsi::VPD_SUPPORTED_PAGES,
            &page,
        )
    }

    fn handle_vpd_device_identifiers_iso(
        &self,
        external_data: &RequestBuffers<'_>,
        allocation_length: usize,
    ) -> Result<usize, ScsiDvdError> {
        // This is the ID used by VHDMP on Windows.
        let context_guid = [
            0x73, 0x05, 0xe3, 0x43, 0x77, 0x03, 0x54, 0x46, 0x94, 0x95, 0x7d, 0x7c, 0xed, 0x62,
            0x4a, 0x7d,
        ];
        let page = scsi::IsoVpdIdentifiers {
            id_page: scsi::VpdIdentificationDescriptor {
                code_set: scsi::VPD_CODE_SET_BINARY,
                identifiertype: scsi::VPD_IDENTIFIER_TYPE_VENDOR_ID,
                reserved3: 0x00,
                identifier_length: (size_of::<scsi::IsoVpdIdentifiers>()
                    - size_of::<scsi::VpdIdentificationDescriptor>())
                    as u8,
            },
            vendor_id: *b"Msft    ",
            context_guid,
        };

        write_vpd_page(
            external_data,
            allocation_length,
            scsi::VPD_DEVICE_IDENTIFIERS,
            &page,
        )
    }

    fn validate_data_cdb_iso(
        &self,
        external_data: &RequestBuffers<'_>,
        request: &Request,
        sector_count: u64,
        op: ScsiOp,
    ) -> Result<RequestParametersIso, ScsiDvdError> {
        let (len, offset) = match op {
            ScsiOp::READ => {
                let cdb = scsi::Cdb10::read_from_prefix(&request.cdb[..]).unwrap().0; // TODO: zerocopy: use-rest-of-range (https://github.com/microsoft/openvmm/issues/759)
                (
                    cdb.transfer_blocks.get() as u64,
                    cdb.logical_block.get() as u64,
                )
            }
            ScsiOp::READ12 => {
                let cdb = scsi::Cdb12::read_from_prefix(&request.cdb[..]).unwrap().0; // TODO: zerocopy: use-rest-of-range (https://github.com/microsoft/openvmm/issues/759)
                (
                    cdb.transfer_blocks.get() as u64,
                    cdb.logical_block.get() as u64,
                )
            }
            ScsiOp::READ16 => {
                let cdb = scsi::Cdb16::read_from_prefix(&request.cdb[..]).unwrap().0; // TODO: zerocopy: use-rest-of-range (https://github.com/microsoft/openvmm/issues/759)
                (cdb.transfer_blocks.get() as u64, cdb.logical_block.get())
            }
            _ => unreachable!(),
        };

        if len == 0 {
            return Ok(RequestParametersIso {
                tx: len as usize,
                offset: 0,
            });
        }

        self.validate_media_for_read()?;

        let sector_shift = self.sector_shift();
        let max = external_data.len() >> sector_shift;
        if len as usize > max {
            tracelimit::error_ratelimited!(len, max, "illegal block");
            return Err(ScsiDvdError::IllegalRequest(
                AdditionalSenseCode::ILLEGAL_BLOCK,
                0,
            ));
        }

        if sector_count <= offset || sector_count - offset < len {
            tracelimit::error_ratelimited!(sector_count, offset, len, "illegal block");
            return Err(ScsiDvdError::IllegalRequest(
                AdditionalSenseCode::ILLEGAL_BLOCK,
                0,
            ));
        }

        let tx = (len as usize) << sector_shift;
        Ok(RequestParametersIso {
            tx,
            offset: (offset * self.balancer()),
        })
    }

    fn iso_get_mode_page(
        &self,
        external_data: &RequestBuffers<'_>,
        buffer_size: usize,
        page_code: u8,
        mode_page_size: &mut u16,
    ) -> Result<usize, ScsiDvdError> {
        let tx: usize;
        let page_size: usize;
        match page_code {
            scsi::MODE_PAGE_ERROR_RECOVERY => {
                page_size = size_of::<scsi::ModeReadWriteRecoveryPage>();
                tx = std::cmp::min(page_size, buffer_size);
                let data = scsi::ModeReadWriteRecoveryPage {
                    page_code: 0x01,
                    page_length: 0x0a,
                    bit_info: 0u8,
                    read_retry_count: 0u8,
                    reserved: [0u8, 0u8, 0u8, 0u8],
                    write_retry_count: 0u8,
                    reserved2: [0u8, 0u8, 0u8],
                };
                external_data
                    .writer()
                    .write(&data.as_bytes()[..tx])
                    .map_err(ScsiDvdError::MemoryAccess)?;
                *mode_page_size = page_size as u16;
            }
            scsi::MODE_PAGE_POWER_CONDITION => {
                page_size = size_of::<scsi::PowerConditionPage>();
                tx = std::cmp::min(page_size, buffer_size);
                let data = scsi::PowerConditionPage {
                    page_code: 0x1a,
                    page_length: 0x0a,
                    ..FromZeros::new_zeroed()
                };
                external_data
                    .writer()
                    .write(&data.as_bytes()[..tx])
                    .map_err(ScsiDvdError::MemoryAccess)?;
                *mode_page_size = page_size as u16;
            }
            scsi::MODE_PAGE_CDVD_INACTIVITY => {
                page_size = size_of::<scsi::ModeSenseModePageTimeoutProtect>();
                tx = std::cmp::min(page_size, buffer_size);
                let data = scsi::ModeSenseModePageTimeoutProtect {
                    page_code: 0x1d,
                    page_length: 0x0a,
                    reserved: [0u8, 0u8],
                    bit_info: 0x00,
                    reserved2: 0x00,
                    group_one_minimum_timeout: [0x00, 0x05],
                    group_two_minimum_timeout: [0x00, 0x14],
                    group_three_timeout: [0x00, 0x0a],
                };
                external_data
                    .writer()
                    .write(&data.as_bytes()[..tx])
                    .map_err(ScsiDvdError::MemoryAccess)?;
                *mode_page_size = page_size as u16;
            }
            _ => {
                *mode_page_size = 0;
                tx = 0;
            }
        };
        Ok(tx)
    }

    fn iso_init_mode_sense_header(
        &self,
        external_data: &RequestBuffers<'_>,
        buffer_size: usize,
        data_length: u16,
    ) -> Result<usize, ScsiDvdError> {
        let data = scsi::ModeParameterHeader10 {
            mode_data_length: (data_length - (size_of::<u16>() as u16)).into(),
            block_descriptor_length: 0.into(),
            ..FromZeros::new_zeroed()
        };
        let tx = std::cmp::min(super::MODE_PARAMETER_HEADER10_SIZE, buffer_size);
        external_data
            .writer()
            .write(&data.as_bytes()[..tx])
            .map_err(ScsiDvdError::MemoryAccess)?;
        Ok(tx)
    }

    fn validate_media_for_read(&self) -> Result<(), ScsiDvdError> {
        let drive_state = self.media_state.lock().drive_state;

        match drive_state {
            DriveState::MediumPresentTrayOpen => Ok(()),
            DriveState::MediumPresentTrayClosed => Ok(()),
            DriveState::MediumNotPresentTrayOpen => Err(ScsiDvdError::SenseNotReady(
                AdditionalSenseCode::NO_MEDIA_IN_DEVICE,
                scsi::MEDIUM_NOT_PRESENT_TRAY_OPEN,
            )),
            DriveState::MediumNotPresentTrayClosed => Err(ScsiDvdError::SenseNotReady(
                AdditionalSenseCode::NO_MEDIA_IN_DEVICE,
                scsi::MEDIUM_NOT_PRESENT_TRAY_CLOSED,
            )),
        }
    }

    fn iso_init_performance_header(
        &self,
        external_data: &RequestBuffers<'_>,
        buffer_size: u64,
        data_length: u64,
        except: u8,
    ) -> Result<usize, ScsiDvdError> {
        let header = scsi::GetPerformanceHeader {
            total_data_length: (data_length as u32).into(),
            except: except & 0x01,
            ..FromZeros::new_zeroed()
        };

        let tx =
            std::cmp::min(size_of::<scsi::GetPerformanceHeader>() as u64, buffer_size) as usize;
        external_data
            .writer()
            .write(&header.as_bytes()[..tx])
            .map_err(ScsiDvdError::MemoryAccess)?;
        Ok(tx)
    }

    fn process_result(&self, result: Result<usize, ScsiDvdError>, op: ScsiOp) -> ScsiResult {
        let result = match result {
            Ok(tx) => ScsiResult {
                scsi_status: ScsiStatus::GOOD,
                srb_status: SrbStatus::SUCCESS,
                tx,
                sense_data: None,
            },
            Err(err) => match err {
                ScsiDvdError::IllegalRequest(sense_code, sense_qualifier) => ScsiResult {
                    scsi_status: ScsiStatus::CHECK_CONDITION,
                    srb_status: SrbStatus::ERROR,
                    tx: 0,
                    sense_data: Some(illegal_request_sense_iso(sense_code, sense_qualifier)),
                },
                ScsiDvdError::DataOverrun => ScsiResult {
                    scsi_status: ScsiStatus::CHECK_CONDITION,
                    srb_status: SrbStatus::DATA_OVERRUN,
                    tx: 0,
                    sense_data: None,
                },
                ScsiDvdError::MemoryAccess(_) => ScsiResult {
                    scsi_status: ScsiStatus::CHECK_CONDITION,
                    srb_status: SrbStatus::INVALID_REQUEST,
                    tx: 0,
                    sense_data: Some(illegal_request_sense_iso(
                        AdditionalSenseCode::INVALID_CDB,
                        0,
                    )),
                },
                ScsiDvdError::SenseNotReady(sense_code, sense_qualifier) => ScsiResult {
                    scsi_status: ScsiStatus::CHECK_CONDITION,
                    srb_status: SrbStatus::ERROR,
                    tx: 0,
                    sense_data: Some(SenseData::new(
                        SenseKey::NOT_READY,
                        sense_code,
                        sense_qualifier,
                    )),
                },
                ScsiDvdError::IoError(err) => match err {
                    DiskError::UnsupportedEject => ScsiResult {
                        scsi_status: ScsiStatus::CHECK_CONDITION,
                        srb_status: SrbStatus::INVALID_REQUEST,
                        tx: 0,
                        sense_data: Some(illegal_request_sense_iso(
                            AdditionalSenseCode::ILLEGAL_COMMAND,
                            0,
                        )),
                    },
                    DiskError::ReservationConflict => ScsiResult {
                        scsi_status: ScsiStatus::RESERVATION_CONFLICT,
                        srb_status: SrbStatus::ERROR,
                        tx: 0,
                        sense_data: Some(SenseData::new(
                            SenseKey::ILLEGAL_REQUEST,
                            AdditionalSenseCode::COMMAND_SEQUENCE_ERROR,
                            scsi::SCSI_SENSEQ_CAPACITY_DATA_CHANGED,
                        )),
                    },
                    _ => ScsiResult {
                        scsi_status: ScsiStatus::CHECK_CONDITION,
                        srb_status: SrbStatus::ERROR,
                        tx: 0,
                        sense_data: None,
                    },
                },
                ScsiDvdError::IllegalRequestNoSenseData => ScsiResult {
                    scsi_status: ScsiStatus::CHECK_CONDITION,
                    srb_status: SrbStatus::INVALID_REQUEST,
                    tx: 0,
                    sense_data: None,
                },
            },
        };

        self.sense_data.set(result.sense_data.as_ref());
        tracing::trace!(scsi_result = ?result, ?op, "process_result completed.");

        result
    }
}

fn illegal_request_sense_iso(sense_code: AdditionalSenseCode, sense_qualifier: u8) -> SenseData {
    match sense_code {
        AdditionalSenseCode::ILLEGAL_COMMAND
        | AdditionalSenseCode::INVALID_CDB
        | AdditionalSenseCode::NO_SENSE
        | AdditionalSenseCode::INVALID_FIELD_PARAMETER_LIST
        | AdditionalSenseCode::PARAMETER_LIST_LENGTH
        | AdditionalSenseCode::ILLEGAL_BLOCK
        | AdditionalSenseCode::INVALID_MEDIA
        | AdditionalSenseCode::SAVING_PARAMETER_NOT_SUPPORTED
        | AdditionalSenseCode::MEDIUM_REMOVAL_PREVENTED => {
            SenseData::new(SenseKey::ILLEGAL_REQUEST, sense_code, sense_qualifier)
        }
        _ => unreachable!(),
    }
}

#[cfg(test)]
mod tests {
    use super::Media;
    use crate::scsi;
    use crate::scsidvd::SimpleScsiDvd;
    use crate::scsidvd::ISO_SECTOR_SIZE;
    use crate::SavedSenseData;
    use crate::ScsiSaveRestore;
    use crate::ScsiSavedState;
    use disk_backend::Disk;
    use disk_backend::DiskError;
    use disk_backend::DiskIo;
    use guestmem::GuestMemory;
    use guestmem::MemoryWrite;
    use inspect::Inspect;
    use pal_async::async_test;
    use scsi::AdditionalSenseCode;
    use scsi::ScsiOp;
    use scsi::SenseKey;
    use scsi_buffers::OwnedRequestBuffers;
    use scsi_buffers::RequestBuffers;
    use scsi_core::save_restore::ScsiDvdSavedState;
    use scsi_core::AsyncScsiDisk;
    use scsi_core::Request;

    use zerocopy::IntoBytes;

    #[derive(Debug)]
    struct TestDisk {
        sector_count: u64,
        sector_size: u32,
        storage: Vec<u8>,
        read_only: bool,
    }

    impl Inspect for TestDisk {
        fn inspect(&self, req: inspect::Request<'_>) {
            req.respond();
        }
    }

    impl TestDisk {
        pub fn new(sector_size: u32, sector_count: u64, read_only: bool) -> TestDisk {
            let buffer = make_repeat_data_buffer(sector_count as usize, sector_size as usize);

            TestDisk {
                sector_count,
                sector_size,
                storage: buffer,
                read_only,
            }
        }
    }

    impl DiskIo for TestDisk {
        fn disk_type(&self) -> &str {
            "test"
        }

        fn sector_count(&self) -> u64 {
            self.sector_count
        }

        fn sector_size(&self) -> u32 {
            self.sector_size
        }

        fn is_read_only(&self) -> bool {
            self.read_only
        }

        fn disk_id(&self) -> Option<[u8; 16]> {
            None
        }

        fn physical_sector_size(&self) -> u32 {
            self.sector_size
        }

        fn is_fua_respected(&self) -> bool {
            false
        }

        async fn eject(&self) -> Result<(), DiskError> {
            Err(DiskError::UnsupportedEject)
        }

        async fn read_vectored(
            &self,
            buffers: &RequestBuffers<'_>,
            sector: u64,
        ) -> Result<(), DiskError> {
            let offset = sector as usize * self.sector_size() as usize;
            let end_point = offset + buffers.len();

            if self.storage.len() < end_point {
                return Err(DiskError::IllegalBlock);
            }

            buffers.writer().write(&self.storage[offset..end_point])?;
            Ok(())
        }

        async fn write_vectored(
            &self,
            _buffers: &RequestBuffers<'_>,
            _sector: u64,
            _fua: bool,
        ) -> Result<(), DiskError> {
            todo!()
        }

        async fn sync_cache(&self) -> Result<(), DiskError> {
            todo!()
        }

        async fn unmap(
            &self,
            _sector: u64,
            _count: u64,
            _block_level_only: bool,
        ) -> Result<(), DiskError> {
            Ok(())
        }

        fn unmap_behavior(&self) -> disk_backend::UnmapBehavior {
            disk_backend::UnmapBehavior::Ignored
        }
    }

    fn new_scsi_dvd(sector_size: u32, sector_count: u64, read_only: bool) -> SimpleScsiDvd {
        let disk = TestDisk::new(sector_size, sector_count, read_only);
        let scsi_dvd = SimpleScsiDvd::new(Some(Disk::new(disk).unwrap()));
        let sector_shift = ISO_SECTOR_SIZE.trailing_zeros() as u8;
        assert_eq!(scsi_dvd.sector_count(), sector_count / scsi_dvd.balancer());
        assert_eq!(scsi_dvd.sector_shift(), sector_shift);
        if let Media::Loaded(disk) = &*scsi_dvd.media.read() {
            assert_eq!(disk.is_read_only(), read_only);
            assert_eq!(disk.sector_size(), sector_size);
        } else {
            panic!("unexpected Media::Unloaded");
        }
        scsi_dvd
    }

    fn make_repeat_data_buffer(sector_count: usize, sector_size: usize) -> Vec<u8> {
        let mut buf = vec![0u8; sector_count * sector_size];
        let mut temp = vec![0u8; sector_size];
        assert!(sector_size > 2);
        temp[sector_size / 2 - 1] = 2;
        temp[sector_size / 2] = 3;

        for i in (0..buf.len()).step_by(temp.len()) {
            let end_point = i + temp.len();
            buf[i..end_point].copy_from_slice(&temp);
        }

        buf
    }

    async fn check_execute_scsi(
        scsi_dvd: &mut SimpleScsiDvd,
        external_data: &RequestBuffers<'_>,
        request: &Request,
        pass: bool,
    ) {
        let result = scsi_dvd.execute_scsi(external_data, request).await;
        match pass {
            true if result.scsi_status != scsi::ScsiStatus::GOOD => {
                panic!(
                    "execute_scsi failed! request: {:?} result: {:?}",
                    request, result
                );
            }
            false if result.scsi_status == scsi::ScsiStatus::GOOD => {
                panic!(
                    "execute_scsi passed! request: {:?} result: {:?}",
                    request, result
                );
            }
            _ => (),
        }
    }

    fn make_cdb16_request(operation_code: ScsiOp, start_lba: u64, lba_count: u32) -> Request {
        let cdb = scsi::Cdb16 {
            operation_code,
            flags: scsi::Cdb16Flags::new(),
            logical_block: start_lba.into(),
            transfer_blocks: lba_count.into(),
            reserved2: 0,
            control: 0,
        };
        let mut data = [0u8; 16];
        data[..].copy_from_slice(cdb.as_bytes());
        Request {
            cdb: data,
            srb_flags: 0,
        }
    }

    fn check_guest_memory(
        guest_mem: &GuestMemory,
        start_lba: u64,
        buff: &[u8],
        sector_size: usize,
    ) -> bool {
        let mut b = vec![0u8; buff.len()];
        if guest_mem.read_at(start_lba, &mut b).is_err() {
            panic!("guest_mem read error");
        };
        buff[..].eq(&b[..]) && (b[sector_size / 2 - 1] == 2) && (b[sector_size / 2] == 3)
    }

    fn save_scsi_dvd(scsi_dvd: &SimpleScsiDvd) -> ScsiDvdSavedState {
        let saved_state =
            if let Some(ScsiSavedState::ScsiDvd(saved_state)) = scsi_dvd.save().unwrap() {
                saved_state
            } else {
                panic!("saved_state cannot be none")
            };
        let media_state = scsi_dvd.media_state.lock();
        assert_eq!(saved_state.persistent, media_state.persistent);
        assert_eq!(saved_state.prevent, media_state.prevent);
        assert_eq!(saved_state.drive_state, media_state.drive_state);
        assert_eq!(
            saved_state.pending_medium_event,
            media_state.pending_medium_event
        );
        let sense = scsi_dvd.sense_data.get();
        let sense_data = sense.map(|sense| SavedSenseData {
            sense_key: sense.header.sense_key.0,
            additional_sense_code: sense.additional_sense_code.0,
            additional_sense_code_qualifier: sense.additional_sense_code_qualifier,
        });
        assert_eq!(saved_state.sense_data, sense_data);
        saved_state
    }

    fn restore_scsi_dvd(saved_state: ScsiDvdSavedState, scsi_dvd: &SimpleScsiDvd) {
        if scsi_dvd
            .restore(&ScsiSavedState::ScsiDvd(saved_state))
            .is_err()
        {
            panic!("restore scsi dvd failed. saved_state {:?}", saved_state);
        }

        let media_state = scsi_dvd.media_state.lock();
        assert_eq!(saved_state.persistent, media_state.persistent);
        assert_eq!(saved_state.prevent, media_state.prevent);
        assert_eq!(saved_state.drive_state, media_state.drive_state);
        assert_eq!(
            saved_state.pending_medium_event,
            media_state.pending_medium_event
        );
        let sense = scsi_dvd.sense_data.get();
        let sense_data = sense.map(|sense| SavedSenseData {
            sense_key: sense.header.sense_key.0,
            additional_sense_code: sense.additional_sense_code.0,
            additional_sense_code_qualifier: sense.additional_sense_code_qualifier,
        });
        assert_eq!(saved_state.sense_data, sense_data);
    }

    #[test]
    fn validate_new_scsi_dvd() {
        new_scsi_dvd(512, 2048, true);
    }

    #[async_test]
    async fn validate_read16() {
        let sector_size = 512;
        let sector_count = 2048;
        let mut scsi_dvd = new_scsi_dvd(sector_size, sector_count, true);

        let dvd_sector_size = ISO_SECTOR_SIZE as u64;
        let dvd_sector_count = scsi_dvd.sector_count();
        let external_data =
            OwnedRequestBuffers::linear(0, (dvd_sector_size * dvd_sector_count) as usize, true);
        let guest_mem = GuestMemory::allocate(4096);
        let start_lba = 0;
        let lba_count = 2;
        let request = make_cdb16_request(ScsiOp::READ16, start_lba, lba_count);

        println!("read disk to guest_mem2 ...");
        check_execute_scsi(
            &mut scsi_dvd,
            &external_data.buffer(&guest_mem),
            &request,
            true,
        )
        .await;

        println!("validate guest_mem2 ...");
        let data = make_repeat_data_buffer(sector_count as usize, sector_size as usize);
        assert_eq!(
            check_guest_memory(
                &guest_mem,
                0,
                &data[..(ISO_SECTOR_SIZE * lba_count) as usize],
                sector_size as usize
            ),
            true
        );
    }

    #[test]
    fn validate_save_restore_scsi_dvd_no_change() {
        let scsi_dvd = new_scsi_dvd(512, 2048, true);
        let saved_state = save_scsi_dvd(&scsi_dvd);
        restore_scsi_dvd(saved_state, &scsi_dvd);
    }

    #[test]
    fn validate_save_restore_scsi_dvd_with_sense_data() {
        let scsi_dvd = new_scsi_dvd(512, 2048, true);
        let mut saved_state = save_scsi_dvd(&scsi_dvd);
        saved_state.sense_data = Some(SavedSenseData {
            sense_key: SenseKey::UNIT_ATTENTION.0,
            additional_sense_code: AdditionalSenseCode::OPERATING_CONDITIONS_CHANGED.0,
            additional_sense_code_qualifier: scsi::SCSI_SENSEQ_OPERATING_DEFINITION_CHANGED,
        });
        restore_scsi_dvd(saved_state, &scsi_dvd);
    }
}
