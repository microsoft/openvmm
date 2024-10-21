// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Common routines for interoperating between [`nvme_spec`] and
//! [`disk_backend`] types.

#![forbid(unsafe_code)]
#![warn(missing_docs)]

use disk_backend::pr;
use nvme_spec::nvm;
use thiserror::Error;

/// Converts a `disk_backend` reservation type to an NVMe reservation type.
pub fn to_nvme_reservation_type(reservation_type: pr::ReservationType) -> nvm::ReservationType {
    match reservation_type {
        pr::ReservationType::WriteExclusive => nvm::ReservationType::WRITE_EXCLUSIVE,
        pr::ReservationType::ExclusiveAccess => nvm::ReservationType::EXCLUSIVE_ACCESS,
        pr::ReservationType::WriteExclusiveRegistrantsOnly => {
            nvm::ReservationType::WRITE_EXCLUSIVE_REGISTRANTS_ONLY
        }
        pr::ReservationType::ExclusiveAccessRegistrantsOnly => {
            nvm::ReservationType::EXCLUSIVE_ACCESS_REGISTRANTS_ONLY
        }
        pr::ReservationType::WriteExclusiveAllRegistrants => {
            nvm::ReservationType::WRITE_EXCLUSIVE_ALL_REGISTRANTS
        }
        pr::ReservationType::ExclusiveAccessAllRegistrants => {
            nvm::ReservationType::EXCLUSIVE_ACCESS_ALL_REGISTRANTS
        }
    }
}

/// Error returned by [`from_nvme_reservation_type`].
#[derive(Debug, Error)]
#[error("invalid nvme reservation type: {0:#x?}")]
pub struct InvalidReservationType(nvm::ReservationType);

/// Converts a `disk_backend` reservation type from an NVMe reservation type.
pub fn from_nvme_reservation_type(
    nvme_type: nvm::ReservationType,
) -> Result<pr::ReservationType, InvalidReservationType> {
    let reservation_type = match nvme_type {
        nvm::ReservationType::WRITE_EXCLUSIVE => pr::ReservationType::WriteExclusive,
        nvm::ReservationType::EXCLUSIVE_ACCESS => pr::ReservationType::ExclusiveAccess,
        nvm::ReservationType::WRITE_EXCLUSIVE_REGISTRANTS_ONLY => {
            pr::ReservationType::WriteExclusiveRegistrantsOnly
        }
        nvm::ReservationType::EXCLUSIVE_ACCESS_REGISTRANTS_ONLY => {
            pr::ReservationType::ExclusiveAccessRegistrantsOnly
        }
        nvm::ReservationType::WRITE_EXCLUSIVE_ALL_REGISTRANTS => {
            pr::ReservationType::WriteExclusiveAllRegistrants
        }
        nvm::ReservationType::EXCLUSIVE_ACCESS_ALL_REGISTRANTS => {
            pr::ReservationType::ExclusiveAccessAllRegistrants
        }
        _ => return Err(InvalidReservationType(nvme_type)),
    };
    Ok(reservation_type)
}

/// Builds `disk_backend` reservation capabilities from NVMe reservation capabilities.
pub fn from_nvme_reservation_capabilities(
    rescap: nvm::ReservationCapabilities,
) -> pr::ReservationCapabilities {
    pr::ReservationCapabilities {
        write_exclusive: rescap.write_exclusive(),
        exclusive_access: rescap.exclusive_access(),
        write_exclusive_registrants_only: rescap.write_exclusive_registrants_only(),
        exclusive_access_registrants_only: rescap.exclusive_access_registrants_only(),
        write_exclusive_all_registrants: rescap.write_exclusive_all_registrants(),
        exclusive_access_all_registrants: rescap.exclusive_access_all_registrants(),
        persist_through_power_loss: rescap.persist_through_power_loss(),
    }
}

/// Parses an NVMe reservation report into a `disk_backend` reservation report.
pub fn from_nvme_reservation_report(
    report_header: &nvm::ReservationReport,
    controllers: &[nvm::RegisteredControllerExtended],
) -> Result<pr::ReservationReport, InvalidReservationType> {
    let reservation_type = if report_header.rtype.0 != 0 {
        Some(from_nvme_reservation_type(report_header.rtype)?)
    } else {
        None
    };

    let controllers = controllers
        .iter()
        .map(|controller| pr::RegisteredController {
            key: controller.rkey,
            holds_reservation: controller.rcsts.holds_reservation(),
            controller_id: controller.cntlid,
            host_id: controller.hostid.to_vec(),
        })
        .collect();

    let report = pr::ReservationReport {
        generation: report_header.gen,
        reservation_type,
        controllers,
        persist_through_power_loss: report_header.ptpls != 0,
    };

    Ok(report)
}
