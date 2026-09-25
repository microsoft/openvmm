// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! The snapshot fields of the RTC saved state: the guest-visible UTC time,
//! the calendar validity, and the retired coherent-transaction state.

use super::state::SavedState;
use crate::cmos_rtc::Rtc;
use crate::cmos_rtc::spec::CmosReg;
use crate::cmos_rtc::spec::StatusRegC;
use local_clock::LocalClockTime;
use vmcore::save_restore::RestoreError;
use vmcore::save_restore::SaveError;

/// The validated snapshot fields of a saved state.
pub(super) struct Fields {
    pub(super) time_valid: bool,
    pub(super) transaction_read_mask: Option<u16>,
    pub(super) clock_time_millis: i64,
}

impl Fields {
    /// Validates the snapshot fields of `state` before any RTC state changes.
    pub(super) fn validate(state: &SavedState) -> Result<Self, RestoreError> {
        let time_valid = state.time_valid.unwrap_or(true);

        if state.transaction_read_mask.is_some() {
            return Err(RestoreError::InvalidSavedState(anyhow::anyhow!(
                "invalid RTC coherent transaction state"
            )));
        }

        let status_c = StatusRegC::from(state.cmos[CmosReg::STATUS_C.0 as usize]);
        if status_c.irq_combined()
            != (status_c.irq_update() || status_c.irq_periodic() || status_c.irq_alarm())
        {
            return Err(RestoreError::InvalidSavedState(anyhow::anyhow!(
                "inconsistent RTC status C interrupt flags"
            )));
        }

        Ok(Self {
            time_valid,
            transaction_read_mask: state.transaction_read_mask,
            clock_time_millis: state.clock_time_millis,
        })
    }
}

impl Rtc {
    /// Samples the guest-visible UTC time for the saved state.
    pub(super) fn saved_clock_time(&mut self) -> Result<i64, SaveError> {
        Ok(self
            .real_time_source
            .get_time()
            .map_err(|error| SaveError::Other(error.into()))?
            .as_millis_since_unix_epoch())
    }

    /// Restores the guest-visible UTC time of the saved state.
    pub(super) fn restore_clock(&mut self, clock_time_millis: i64) {
        self.real_time_source
            .set_time(LocalClockTime::from_millis_since_unix_epoch(
                clock_time_millis,
            ));
    }
}
