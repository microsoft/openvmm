// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! This module handles Persistent Reservation IN/OUT commands.

use super::ScsiError;
use super::SimpleScsiDisk;
use crate::scsi;
use crate::scsi::PriFullStatusDescriptorHeader;
use crate::scsi::PriFullStatusListHeader;
use crate::scsi::PriRegistrationListHeader;
use crate::scsi::PriReportCapabilities;
use crate::scsi::PriReservationDescriptor;
use crate::scsi::PriReservationListHeader;
use crate::RequestBuffers;
use guestmem::MemoryRead;
use guestmem::MemoryWrite;
use scsi::AdditionalSenseCode;
use scsi::ScsiOp;
use scsi_core::Request;
use zerocopy::FromBytes;
use zerocopy::FromZeros;
use zerocopy::Immutable;
use zerocopy::IntoBytes;
use zerocopy::KnownLayout;

fn from_scsi_reservation_type(
    scsi_type: scsi::ReservationType,
) -> Option<disk_backend::pr::ReservationType> {
    let reservation_type = match scsi_type {
        scsi::ReservationType::WRITE_EXCLUSIVE => disk_backend::pr::ReservationType::WriteExclusive,
        scsi::ReservationType::EXCLUSIVE => disk_backend::pr::ReservationType::ExclusiveAccess,
        scsi::ReservationType::WRITE_EXCLUSIVE_REGISTRANTS => {
            disk_backend::pr::ReservationType::WriteExclusiveRegistrantsOnly
        }
        scsi::ReservationType::EXCLUSIVE_REGISTRANTS => {
            disk_backend::pr::ReservationType::ExclusiveAccessRegistrantsOnly
        }
        scsi::ReservationType::WRITE_EXCLUSIVE_ALL_REGISTRANTS => {
            disk_backend::pr::ReservationType::WriteExclusiveAllRegistrants
        }
        scsi::ReservationType::EXCLUSIVE_ALL_REGISTRANTS => {
            disk_backend::pr::ReservationType::ExclusiveAccessAllRegistrants
        }
        _ => return None,
    };
    Some(reservation_type)
}

fn to_scsi_reservation_type(
    reservation_type: disk_backend::pr::ReservationType,
) -> scsi::ReservationType {
    match reservation_type {
        disk_backend::pr::ReservationType::WriteExclusive => scsi::ReservationType::WRITE_EXCLUSIVE,
        disk_backend::pr::ReservationType::ExclusiveAccess => scsi::ReservationType::EXCLUSIVE,
        disk_backend::pr::ReservationType::WriteExclusiveRegistrantsOnly => {
            scsi::ReservationType::WRITE_EXCLUSIVE_REGISTRANTS
        }
        disk_backend::pr::ReservationType::ExclusiveAccessRegistrantsOnly => {
            scsi::ReservationType::EXCLUSIVE_REGISTRANTS
        }
        disk_backend::pr::ReservationType::WriteExclusiveAllRegistrants => {
            scsi::ReservationType::WRITE_EXCLUSIVE_ALL_REGISTRANTS
        }
        disk_backend::pr::ReservationType::ExclusiveAccessAllRegistrants => {
            scsi::ReservationType::EXCLUSIVE_ALL_REGISTRANTS
        }
    }
}

impl SimpleScsiDisk {
    async fn run_persistent_reservation_report_capabilities(
        &self,
        external_data: &RequestBuffers<'_>,
        allocation_length: usize,
    ) -> Result<usize, ScsiError> {
        let pr = self.disk.pr().ok_or(ScsiError::IllegalRequest(
            AdditionalSenseCode::ILLEGAL_COMMAND,
        ))?;

        let length = size_of::<PriReportCapabilities>();
        if allocation_length < length {
            return Err(ScsiError::IllegalRequest(AdditionalSenseCode::INVALID_CDB));
        }

        let caps = pr.capabilities();
        let persist_through_power_loss_active = if caps.persist_through_power_loss {
            let report = pr.report().await.map_err(ScsiError::Disk)?;
            report.persist_through_power_loss
        } else {
            false
        };

        let flags = scsi::PriReportCapabilitiesFlags::new()
            .with_all_target_ports_capable(true)
            .with_persist_through_power_loss_capable(caps.persist_through_power_loss)
            .with_persist_through_power_loss_activated(persist_through_power_loss_active)
            .with_type_mask_valid(true);

        let type_mask = scsi::PriReportCapabilitiesTypeMask::new()
            .with_write_exclusive(caps.write_exclusive)
            .with_exclusive_access(caps.exclusive_access)
            .with_write_exclusive_registrants_only(caps.write_exclusive_registrants_only)
            .with_exclusive_access_registrants_only(caps.exclusive_access_registrants_only)
            .with_write_exclusive_all_registrants(caps.write_exclusive_all_registrants)
            .with_exclusive_access_all_registrants(caps.exclusive_access_all_registrants);

        let data = PriReportCapabilities {
            length: (length as u16).into(),
            flags,
            type_mask,
            reserved7: [0; 2],
        };

        // Copy as much as we can.
        let tx = std::cmp::min(allocation_length, length);
        external_data
            .writer()
            .write(&data.as_bytes()[..tx])
            .map_err(ScsiError::MemoryAccess)?;
        Ok(tx)
    }

    async fn run_persistent_reservation_read_keys(
        &self,
        external_data: &RequestBuffers<'_>,
        allocation_length: usize,
    ) -> Result<usize, ScsiError> {
        if allocation_length < size_of::<PriRegistrationListHeader>() {
            return Err(ScsiError::IllegalRequest(AdditionalSenseCode::INVALID_CDB));
        }

        let report = self
            .disk
            .pr()
            .expect("validated in validate_persistent_reservation_in")
            .report()
            .await
            .map_err(ScsiError::Disk)?;

        let len = report.controllers.len() * 8;
        let header = PriRegistrationListHeader {
            generation: report.generation.into(),
            additional_length: (len as u32).into(),
        };

        let mut data = header.as_bytes().to_vec();
        for controller in &report.controllers {
            data.extend(&controller.key.to_be_bytes());
        }

        // Copy as much as we can
        let tx = std::cmp::min(allocation_length, data.len());
        external_data
            .writer()
            .write(&data[..tx])
            .map_err(ScsiError::MemoryAccess)?;

        Ok(tx)
    }

    async fn run_persistent_reservation_read_reservations(
        &self,
        external_data: &RequestBuffers<'_>,
        allocation_length: usize,
    ) -> Result<usize, ScsiError> {
        #[repr(C)]
        #[derive(IntoBytes, Immutable, KnownLayout, FromBytes)]
        struct Output {
            header: PriReservationListHeader,
            descriptor: PriReservationDescriptor,
        }

        if allocation_length < size_of::<PriReservationListHeader>() {
            return Err(ScsiError::IllegalRequest(AdditionalSenseCode::INVALID_CDB));
        }

        let report = self
            .disk
            .pr()
            .expect("validated in validate_persistent_reservation_in")
            .report()
            .await
            .map_err(ScsiError::Disk)?;

        let has_reservation;
        let no_reservation;
        let data = if let Some(reservation_type) = report.reservation_type {
            has_reservation = Output {
                header: PriReservationListHeader {
                    generation: report.generation.into(),
                    additional_length: (size_of::<PriReservationDescriptor>() as u32).into(),
                },
                descriptor: PriReservationDescriptor {
                    type_scope: scsi::PersistentReserveTypeScope::new()
                        .with_reserve_type(to_scsi_reservation_type(reservation_type)),
                    reservation_key: report
                        .controllers
                        .iter()
                        .find_map(|d| d.holds_reservation.then_some(d.key))
                        .unwrap_or(0)
                        .into(),
                    ..FromZeros::new_zeroed()
                },
            };
            has_reservation.as_bytes()
        } else {
            no_reservation = PriReservationListHeader {
                generation: report.generation.into(),
                additional_length: 0_u32.into(),
            };
            no_reservation.as_bytes()
        };

        // Copy as much as we can
        let tx = std::cmp::min(allocation_length, data.len());
        external_data
            .writer()
            .write(&data[..tx])
            .map_err(ScsiError::MemoryAccess)?;

        Ok(tx)
    }

    async fn run_persistent_reservation_read_full_status(
        &self,
        external_data: &RequestBuffers<'_>,
        allocation_length: usize,
    ) -> Result<usize, ScsiError> {
        if allocation_length < size_of::<PriFullStatusListHeader>() {
            return Err(ScsiError::IllegalRequest(AdditionalSenseCode::INVALID_CDB));
        }

        let report = self
            .disk
            .pr()
            .expect("validated in validate_persistent_reservation_in")
            .report()
            .await
            .map_err(ScsiError::Disk)?;

        let mut data = PriFullStatusListHeader::new_zeroed().as_bytes().to_vec();

        for controller in &report.controllers {
            let header = PriFullStatusDescriptorHeader {
                reservation_key: controller.key.into(),
                flags: scsi::PriFullStatusDescriptorHeaderFlags::new()
                    .with_all_target_ports(true)
                    .with_reservation_holder(controller.holds_reservation),
                type_scope: scsi::PersistentReserveTypeScope::new().with_reserve_type(
                    controller
                        .holds_reservation
                        .then_some(report.reservation_type)
                        .flatten()
                        .map_or(scsi::ReservationType(0), to_scsi_reservation_type),
                ),
                relative_target_port_identifier: controller.controller_id.into(),
                additional_descriptor_length: (controller.host_id.len() as u32).into(),
                ..FromZeros::new_zeroed()
            };

            data.extend(header.as_bytes());
            data.extend(&controller.host_id);
        }

        let header = PriFullStatusListHeader {
            generation: report.generation.into(),
            additional_length: ((data.len() - size_of::<PriFullStatusListHeader>()) as u32).into(),
        };

        header.write_to_prefix(data.as_mut_slice()).unwrap(); // PANIC: Infallable since data extended to fit header above.

        // Copy as much as we can
        let tx = std::cmp::min(allocation_length, data.len());
        external_data
            .writer()
            .write(&data.as_bytes()[..tx])
            .map_err(ScsiError::MemoryAccess)?;

        Ok(tx)
    }

    async fn handle_persistent_reserve_in(
        &self,
        external_data: &RequestBuffers<'_>,
        request: &Request,
    ) -> Result<usize, ScsiError> {
        let cdb = scsi::PersistentReserveIn::read_from_prefix(&request.cdb[..])
            .unwrap()
            .0; // TODO: zerocopy: use-rest-of-range (https://github.com/microsoft/openvmm/issues/759)
        let allocation_length = cdb.allocation_length.get() as usize;
        if allocation_length > external_data.len() {
            tracelimit::error_ratelimited!(
                allocation_length,
                external_data = external_data.len(),
                "invalid cdb"
            );
            return Err(ScsiError::IllegalRequest(AdditionalSenseCode::INVALID_CDB));
        }

        let service_action = cdb.service_action.service_action();

        // Handle Report Capabilities here because it does not
        // depend on Namespace Reservation Capabilities.
        if service_action == scsi::ServiceActionIn::REPORT_CAPABILITIES {
            return self
                .run_persistent_reservation_report_capabilities(external_data, allocation_length)
                .await;
        }

        // Check if the device supports persistent reserve.
        if !self.support_pr {
            tracing::debug!("the device doesn't support persistent reserve");
            return Err(ScsiError::IllegalRequest(
                AdditionalSenseCode::ILLEGAL_COMMAND,
            ));
        }

        match service_action {
            scsi::ServiceActionIn::READ_KEYS => {
                self.run_persistent_reservation_read_keys(external_data, allocation_length)
                    .await
            }
            scsi::ServiceActionIn::READ_RESERVATIONS => {
                self.run_persistent_reservation_read_reservations(external_data, allocation_length)
                    .await
            }
            scsi::ServiceActionIn::READ_FULL_STATUS => {
                self.run_persistent_reservation_read_full_status(external_data, allocation_length)
                    .await
            }
            service_action => {
                tracing::debug!(?service_action, "invalid cdb");
                Err(ScsiError::IllegalRequest(AdditionalSenseCode::INVALID_CDB))
            }
        }
    }

    async fn handle_persistent_reserve_out(
        &self,
        external_data: &RequestBuffers<'_>,
        request: &Request,
    ) -> Result<usize, ScsiError> {
        let cdb = scsi::PersistentReserveOut::read_from_prefix(&request.cdb[..])
            .unwrap()
            .0; // TODO: zerocopy: use-rest-of-range (https://github.com/microsoft/openvmm/issues/759)
        let service_action = cdb.service_action.service_action();
        const EXPECT_PARAMETER_LIST_LENGTH: usize = size_of::<scsi::ProParameterList>();

        // Fail request if the CDB PERSISTENT_RESERVE_OUT SCOPE parameter is supported.
        // NOTE: Only RESERVATION_SCOPE_LU (00h) is supported per SCSI to NVMe translation spec,
        //       but SCSI spec isn't that strict. Here we ignore the SCOPE check for REGISTER,
        //       REGISTER_AND_IGNORE_EXISTING_KEY, CLEAR, and REGISTER_AND_MOVE service actions per SCSI spec.
        let reservation_scope = cdb.type_scope.scope();
        if reservation_scope != scsi::RESERVATION_SCOPE_LU
            && (!matches!(
                service_action,
                scsi::ServiceActionOut::REGISTER
                    | scsi::ServiceActionOut::REGISTER_IGNORE_EXISTING
                    | scsi::ServiceActionOut::CLEAR
                    | scsi::ServiceActionOut::REGISTER_AND_MOVE
            ))
        {
            tracing::debug!(
                ?service_action,
                reservation_scope,
                "invalid reservation_scope"
            );
            return Err(ScsiError::IllegalRequest(AdditionalSenseCode::INVALID_CDB));
        }

        // Fail the request if device doesn't support persist reservation commands.
        if !self.support_pr {
            tracing::debug!("the device doesn't support persistent reserve");
            return Err(ScsiError::IllegalRequest(
                AdditionalSenseCode::ILLEGAL_COMMAND,
            ));
        }

        if external_data.len() < EXPECT_PARAMETER_LIST_LENGTH {
            tracelimit::error_ratelimited!(external_data = external_data.len(), "invalid cdb");
            return Err(ScsiError::IllegalRequest(AdditionalSenseCode::INVALID_CDB));
        }

        let parameter_list: scsi::ProParameterList = external_data
            .reader()
            .read_plain()
            .map_err(ScsiError::MemoryAccess)?;

        // Check if SpecifyInitiatorPorts in ParameterList is supported (only 0 is supported).
        if parameter_list.flags.specify_initiator_ports() {
            tracing::debug!(
                flags = ?parameter_list.flags,
                "SpecifyInitiatorPorts is not supported"
            );
            return Err(ScsiError::IllegalRequest(
                AdditionalSenseCode::INVALID_FIELD_PARAMETER_LIST,
            ));
        }

        // Per SCSI spec, the PARAMETER LIST LIENGTH field shall contain 24(0x18h) if the following conditions are true:
        //      a) The SPEC_I_PT bit is set to zero; and
        //      b) The service action is not REGISTER AND MOVE.
        // If the SPEC_I_PT bit is set to zero, the service action is not REGISTER AND MOVE, and the parameter list length is not 24,
        // then the command shall be terminated with CHECK CONDITION status, with the sense key set to ILLEGAL REQUEST, and the additional sense code
        // set to PARAMETER LIST LENGTH ERROR.
        let parameter_list_length = cdb.parameter_list_length.get() as usize;
        if parameter_list_length != EXPECT_PARAMETER_LIST_LENGTH
            && service_action != scsi::ServiceActionOut::REGISTER_AND_MOVE
        {
            tracelimit::error_ratelimited!(parameter_list_length, "invalid parameter list length");
            return Err(ScsiError::IllegalRequest(
                AdditionalSenseCode::PARAMETER_LIST_LENGTH,
            ));
        }

        let pr = self.disk.pr().expect("validated above");

        let reservation_type = || {
            from_scsi_reservation_type(cdb.type_scope.reserve_type()).ok_or_else(|| {
                tracing::debug!(type_scope = ?cdb.type_scope, "invalid reservation_type");
                ScsiError::IllegalRequest(AdditionalSenseCode::INVALID_CDB)
            })
        };

        let reservation_key = parameter_list.reservation_key.get();
        let service_action_reservation_key = parameter_list.service_action_reservation_key.get();
        let ptpl = pr
            .capabilities()
            .persist_through_power_loss
            .then_some(parameter_list.flags.aptpl());

        match service_action {
            scsi::ServiceActionOut::REGISTER => pr
                .register(Some(reservation_key), service_action_reservation_key, ptpl)
                .await
                .map_err(ScsiError::Disk)?,

            scsi::ServiceActionOut::RESERVE => pr
                .reserve(reservation_key, reservation_type()?)
                .await
                .map_err(ScsiError::Disk)?,

            scsi::ServiceActionOut::RELEASE => pr
                .release(reservation_key, reservation_type()?)
                .await
                .map_err(ScsiError::Disk)?,

            scsi::ServiceActionOut::CLEAR => {
                pr.clear(reservation_key).await.map_err(ScsiError::Disk)?
            }

            scsi::ServiceActionOut::PREEMPT => pr
                .preempt(
                    reservation_key,
                    service_action_reservation_key,
                    reservation_type()?,
                    false,
                )
                .await
                .map_err(ScsiError::Disk)?,

            scsi::ServiceActionOut::PREEMPT_ABORT => pr
                .preempt(
                    reservation_key,
                    service_action_reservation_key,
                    reservation_type()?,
                    true,
                )
                .await
                .map_err(ScsiError::Disk)?,

            scsi::ServiceActionOut::REGISTER_IGNORE_EXISTING => pr
                .register(None, service_action_reservation_key, ptpl)
                .await
                .map_err(ScsiError::Disk)?,

            service_action => {
                tracing::debug!(?service_action, "unsupported service action");
                return Err(ScsiError::IllegalRequest(AdditionalSenseCode::INVALID_CDB));
            }
        }

        Ok(EXPECT_PARAMETER_LIST_LENGTH)
    }

    pub(crate) async fn handle_persistent_reserve(
        &self,
        external_data: &RequestBuffers<'_>,
        request: &Request,
    ) -> Result<usize, ScsiError> {
        if request.scsiop() == ScsiOp::PERSISTENT_RESERVE_IN {
            self.handle_persistent_reserve_in(external_data, request)
                .await
        } else {
            self.handle_persistent_reserve_out(external_data, request)
                .await
        }
    }
}
