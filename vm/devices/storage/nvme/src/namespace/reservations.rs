// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Persistent reservation support

use super::Namespace;
use super::map_disk_error;
use crate::error::NvmeError;
use crate::prp::PrpRange;
use crate::spec;
use crate::spec::nvm;
use disk_backend::pr::PersistentReservation;
use nvme_common::to_nvme_reservation_type;
use zerocopy::FromZeros;
use zerocopy::IntoBytes;

impl Namespace {
    pub(super) async fn reservation_register(
        &self,
        pr: &dyn PersistentReservation,
        command: &spec::Command,
    ) -> Result<(), NvmeError> {
        let cdw10 = nvm::Cdw10ReservationRegister::from(command.cdw10);
        let mut data = nvm::ReservationRegister::new_zeroed();
        let range = PrpRange::parse(&self.mem, size_of_val(&data), command.dptr)?;
        range.read(&self.mem, data.as_mut_bytes())?;

        let current_key = (!cdw10.iekey()).then_some(data.crkey);
        let ptpl = if pr.capabilities().persist_through_power_loss {
            match nvm::ChangePersistThroughPowerLoss(cdw10.cptpl()) {
                nvm::ChangePersistThroughPowerLoss::NO_CHANGE => None,
                nvm::ChangePersistThroughPowerLoss::CLEAR => Some(false),
                nvm::ChangePersistThroughPowerLoss::SET => Some(true),
                _ => return Err(spec::Status::INVALID_FIELD_IN_COMMAND.into()),
            }
        } else {
            None
        };

        match nvm::ReservationRegisterAction(cdw10.rrega()) {
            nvm::ReservationRegisterAction::REGISTER => {
                pr.register(None, data.nrkey, ptpl)
                    .await
                    .map_err(map_disk_error)?;
            }
            nvm::ReservationRegisterAction::UNREGISTER => {
                pr.register(current_key, 0, ptpl)
                    .await
                    .map_err(map_disk_error)?;
            }
            nvm::ReservationRegisterAction::REPLACE => {
                pr.register(current_key, data.nrkey, ptpl)
                    .await
                    .map_err(map_disk_error)?;
            }
            action => {
                tracelimit::warn_ratelimited!(?action, "unsupported reservation register action");
                return Err(spec::Status::INVALID_FIELD_IN_COMMAND.into());
            }
        }
        Ok(())
    }

    pub(super) async fn reservation_report(
        &self,
        pr: &dyn PersistentReservation,
        command: &spec::Command,
    ) -> Result<(), NvmeError> {
        let cdw10 = nvm::Cdw10ReservationReport::from(command.cdw10);
        let cdw11 = nvm::Cdw11ReservationReport::from(command.cdw11);
        let numd = cdw10.numd_z().saturating_add(1) as usize;
        let len = numd * 4;
        let range = PrpRange::parse(&self.mem, len, command.dptr)?;

        let report = pr.report().await.map_err(map_disk_error)?;

        let report_header = nvm::ReservationReportExtended {
            report: nvm::ReservationReport {
                generation: report.generation,
                rtype: report
                    .reservation_type
                    .map_or(nvm::ReservationType(0), to_nvme_reservation_type),
                regctl: (report.controllers.len() as u16).into(),
                ptpls: 0,
                ..FromZeros::new_zeroed()
            },
            ..FromZeros::new_zeroed()
        };

        let controllers = report.controllers.iter().map(|controller| {
            let mut hostid = [0; 16];
            let hostid_len = controller.host_id.len().min(hostid.len());
            hostid[..hostid_len].copy_from_slice(&controller.host_id[..hostid_len]);
            nvm::RegisteredControllerExtended {
                cntlid: controller.controller_id,
                rcsts: nvm::ReservationStatus::new()
                    .with_holds_reservation(controller.holds_reservation),
                hostid,
                rkey: controller.key,
                ..FromZeros::new_zeroed()
            }
        });

        let mut data;
        if cdw11.eds() {
            data = report_header.as_bytes().to_vec();
            for controller in controllers {
                data.extend(controller.as_bytes());
            }
        } else {
            data = report_header.report.as_bytes().to_vec();
            for controller in controllers {
                data.extend(
                    nvm::RegisteredController {
                        cntlid: controller.cntlid,
                        rcsts: controller.rcsts,
                        hostid: controller.hostid[..8].try_into().unwrap(),
                        rkey: controller.rkey,
                        ..FromZeros::new_zeroed()
                    }
                    .as_bytes(),
                );
            }
        };

        range.write(&self.mem, &data)?;
        Ok(())
    }

    pub(super) async fn reservation_acquire(
        &self,
        pr: &dyn PersistentReservation,
        command: &spec::Command,
    ) -> Result<(), NvmeError> {
        let cdw10 = nvm::Cdw10ReservationAcquire::from(command.cdw10);
        let mut data = nvm::ReservationAcquire::new_zeroed();
        let range = PrpRange::parse(&self.mem, size_of_val(&data), command.dptr)?;
        range.read(&self.mem, data.as_mut_bytes())?;

        // According to the spec, this is never to be set.
        if cdw10.iekey() {
            return Err(spec::Status::INVALID_FIELD_IN_COMMAND.into());
        }

        let reservation_type = from_nvme_reservation_type(cdw10.rtype())?;
        match nvm::ReservationAcquireAction(cdw10.racqa()) {
            nvm::ReservationAcquireAction::ACQUIRE => {
                pr.reserve(data.crkey, reservation_type)
                    .await
                    .map_err(map_disk_error)?;
            }
            nvm::ReservationAcquireAction::PREEMPT => {
                pr.preempt(data.crkey, data.prkey, reservation_type, false)
                    .await
                    .map_err(map_disk_error)?;
            }
            nvm::ReservationAcquireAction::PREEMPT_AND_ABORT => {
                pr.preempt(data.crkey, data.prkey, reservation_type, true)
                    .await
                    .map_err(map_disk_error)?;
            }
            action => {
                tracelimit::warn_ratelimited!(?action, "unsupported reservation acquire action");
                return Err(spec::Status::INVALID_FIELD_IN_COMMAND.into());
            }
        }

        Ok(())
    }

    pub(super) async fn reservation_release(
        &self,
        pr: &dyn PersistentReservation,
        command: &spec::Command,
    ) -> Result<(), NvmeError> {
        let cdw10 = nvm::Cdw10ReservationRelease::from(command.cdw10);
        let mut data = nvm::ReservationRelease::new_zeroed();
        let range = PrpRange::parse(&self.mem, size_of_val(&data), command.dptr)?;
        range.read(&self.mem, data.as_mut_bytes())?;

        // According to the spec, this is never to be set.
        if cdw10.iekey() {
            return Err(spec::Status::INVALID_FIELD_IN_COMMAND.into());
        }

        match nvm::ReservationReleaseAction(cdw10.rrela()) {
            nvm::ReservationReleaseAction::RELEASE => pr
                .release(data.crkey, from_nvme_reservation_type(cdw10.rtype())?)
                .await
                .map_err(map_disk_error)?,
            nvm::ReservationReleaseAction::CLEAR => {
                pr.clear(data.crkey).await.map_err(map_disk_error)?
            }
            action => {
                tracelimit::warn_ratelimited!(?action, "unsupported reservation release action");
                return Err(spec::Status::INVALID_FIELD_IN_COMMAND.into());
            }
        }

        Ok(())
    }

    pub(super) async fn get_reservation_persistence(
        &self,
        pr: &dyn PersistentReservation,
    ) -> Result<nvme_spec::Cdw11FeatureReservationPersistence, NvmeError> {
        let report = pr.report().await.map_err(map_disk_error)?;
        Ok(nvme_spec::Cdw11FeatureReservationPersistence::new()
            .with_ptpl(report.persist_through_power_loss))
    }
}

fn from_nvme_reservation_type(
    nvme_type: u8,
) -> Result<disk_backend::pr::ReservationType, NvmeError> {
    let reservation_type = nvm::ReservationType(nvme_type);
    nvme_common::from_nvme_reservation_type(reservation_type)
        .map_err(|err| NvmeError::new(spec::Status::INVALID_FIELD_IN_COMMAND, err))
}
