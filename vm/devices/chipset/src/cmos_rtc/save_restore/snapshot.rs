// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! The snapshot fields of the RTC saved state: the guest-visible UTC time,
//! the calendar validity, the last update blip, and the deadlines of the
//! timers.
//!
//! A restore keeps each enabled timer's saved deadline, so that its next
//! interrupt comes at the same VM time as it would have without the
//! snapshot, and keeps the calendar registers as they were saved. A deadline
//! that the snapshot's cut passed fires as the RTC starts. A restore refuses
//! a guest-visible time outside `CLOCK_TIME_MILLIS`, which the RTC's clock
//! source cannot hold.

use super::state::SavedState;
use crate::cmos_rtc::Rtc;
use crate::cmos_rtc::UPDATE_PERIOD;
use crate::cmos_rtc::clock::CLOCK_TIME_MILLIS;
use crate::cmos_rtc::periodic_timer_period;
use crate::cmos_rtc::spec::CmosReg;
use crate::cmos_rtc::spec::StatusRegA;
use crate::cmos_rtc::spec::StatusRegB;
use crate::cmos_rtc::spec::StatusRegC;
use std::time::Duration;
use vmcore::save_restore::RestoreError;
use vmcore::save_restore::SaveError;
use vmcore::vmtime::VmTime;

/// The longest time from an alarm's arming to its deadline: a day.
const ALARM_PERIOD: Duration = Duration::from_secs(24 * 60 * 60);

/// The validated snapshot fields of a saved state.
pub(super) struct Fields {
    pub(super) time_valid: bool,
    pub(super) clock_time_millis: i64,
    pub(super) last_update_millis: i64,
    pub(super) deadlines: Deadlines,
}

/// The saved deadlines of the enabled timers.
pub(super) struct Deadlines {
    alarm: Option<VmTime>,
    periodic: Option<(Duration, VmTime)>,
    update: Option<VmTime>,
}

impl Fields {
    /// Validates the snapshot fields of `state` at VM time `now` before any
    /// RTC state changes.
    pub(super) fn validate(state: &SavedState, now: VmTime) -> Result<Self, RestoreError> {
        let status_c = StatusRegC::from(state.cmos[CmosReg::STATUS_C.0 as usize]);
        if status_c.irq_combined()
            != (status_c.irq_update() || status_c.irq_periodic() || status_c.irq_alarm())
        {
            return Err(RestoreError::InvalidSavedState(anyhow::anyhow!(
                "inconsistent RTC status C interrupt flags"
            )));
        }
        if !CLOCK_TIME_MILLIS.contains(&state.clock_time_millis) {
            return Err(RestoreError::InvalidSavedState(anyhow::anyhow!(
                "RTC clock time of {} ms from the Unix epoch is out of range",
                state.clock_time_millis
            )));
        }

        let status_a = StatusRegA::from(state.cmos[CmosReg::STATUS_A.0 as usize]);
        let status_b = StatusRegB::from(state.cmos[CmosReg::STATUS_B.0 as usize]);
        let periodic_period = status_b
            .irq_enable_periodic()
            .then(|| periodic_timer_period(status_a))
            .flatten();
        let deadlines = Deadlines {
            alarm: enabled_deadline(
                "alarm",
                status_b.irq_enable_alarm().then_some(ALARM_PERIOD),
                state.alarm_deadline,
                now,
            )?,
            periodic: enabled_deadline("periodic", periodic_period, state.periodic_deadline, now)?
                .zip(periodic_period)
                .map(|(deadline, period)| (period, deadline)),
            update: enabled_deadline(
                "update",
                status_b.irq_enable_update().then_some(UPDATE_PERIOD),
                state.update_deadline,
                now,
            )?,
        };

        Ok(Self {
            time_valid: state.time_valid,
            clock_time_millis: state.clock_time_millis,
            last_update_millis: state.last_update_millis,
            deadlines,
        })
    }
}

/// Returns the saved `deadline` of a timer that runs at most `period` ahead
/// of VM time, or `None` if the timer is disabled, which `period` being
/// `None` indicates. A timer arms its deadline at most a period after the VM
/// time that arms it, which is no later than the snapshot's cut, so a later
/// deadline is invalid.
fn enabled_deadline(
    timer: &str,
    period: Option<Duration>,
    deadline: Option<VmTime>,
    now: VmTime,
) -> Result<Option<VmTime>, RestoreError> {
    let (Some(period), Some(deadline)) = (period, deadline) else {
        return Ok(None);
    };
    if let Some(ahead) = deadline.checked_sub(now) {
        if ahead > period {
            return Err(RestoreError::InvalidSavedState(anyhow::anyhow!(
                "RTC {timer} deadline is {ahead:?} after the restored VM time, more than its period of {period:?}"
            )));
        }
    }
    Ok(Some(deadline))
}

impl Rtc {
    /// Samples the guest-visible UTC time for the saved state.
    pub(super) fn saved_clock_time(&mut self) -> Result<i64, SaveError> {
        Ok(self
            .utc_time()
            .map_err(|error| SaveError::Other(error.into()))?
            .as_millis_since_unix_epoch())
    }

    /// Arms the enabled timers at their saved deadlines, and those without
    /// one as if the guest had just enabled them.
    pub(super) fn restore_timers(&mut self, deadlines: Deadlines) {
        self.vmtime_alarm.cancel_timeout();
        self.vmtimer_periodic.cancel();
        self.vmtimer_update.cancel();

        if let Some(deadline) = deadlines.alarm {
            self.vmtime_alarm.set_timeout(deadline);
        }
        if let Some((period, deadline)) = deadlines.periodic {
            self.vmtimer_periodic.start_at(period, deadline);
        }
        if let Some(deadline) = deadlines.update {
            self.vmtimer_update.start_at(UPDATE_PERIOD, deadline);
        }
        self.update_timers();
    }
}
