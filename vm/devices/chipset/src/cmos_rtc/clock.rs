// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! The UTC clock source of the RTC: a fallible clock abstraction, calendar
//! invalidation when no representable time is available, and advancing the
//! guest-visible time.

use super::Rtc;
use super::spec::CmosReg;
use super::spec::StatusRegA;
use anyhow::Context;
use inspect::Inspect;
use local_clock::InspectableLocalClock;
use local_clock::LocalClockTime;
use std::time::Duration;

/// A fallible UTC clock source for the RTC.
pub trait UtcClockSource: Inspect + Send {
    /// Samples UTC as milliseconds from the Unix epoch.
    fn get_time(&mut self) -> Result<LocalClockTime, UtcClockSourceError>;

    /// Updates the guest-visible UTC epoch.
    fn set_time(&mut self, new_time: LocalClockTime);
}

/// Indicates that an RTC UTC clock source could not provide a sample.
#[derive(Debug, thiserror::Error)]
#[error("UTC clock sample is unavailable")]
pub struct UtcClockSourceError;

#[derive(Inspect)]
#[inspect(transparent)]
pub(super) struct LocalClockUtcSource(pub(super) Box<dyn InspectableLocalClock>);

impl UtcClockSource for LocalClockUtcSource {
    fn get_time(&mut self) -> Result<LocalClockTime, UtcClockSourceError> {
        Ok(self.0.get_time())
    }

    fn set_time(&mut self, new_time: LocalClockTime) {
        self.0.set_time(new_time)
    }
}

impl Rtc {
    /// Advances the guest-visible UTC time by `duration`.
    pub(super) fn advance_clock(&mut self, duration: Duration) -> anyhow::Result<()> {
        let delta = i64::try_from(duration.as_millis())
            .context("RTC downtime does not fit in milliseconds")?;
        let current = self
            .real_time_source
            .get_time()
            .context("failed to sample RTC UTC clock")?;
        let advanced = current
            .as_millis_since_unix_epoch()
            .checked_add(delta)
            .context("RTC time overflow while applying snapshot downtime")?;
        self.real_time_source
            .set_time(LocalClockTime::from_millis_since_unix_epoch(advanced));
        self.update_timers();
        self.update_interrupt_line_level();
        Ok(())
    }

    /// Samples the UTC clock, invalidating the calendar if no sample is
    /// available.
    pub(super) fn sample_utc_clock(&mut self) -> Option<LocalClockTime> {
        match self.real_time_source.get_time() {
            Ok(real_time) => Some(real_time),
            Err(error) => {
                tracelimit::warn_ratelimited!(?error, "failed to sample RTC UTC clock");
                self.invalidate_calendar();
                None
            }
        }
    }

    /// Returns whether the calendar registers can represent the year of
    /// `clock_time`, invalidating the calendar if they cannot.
    pub(super) fn validate_calendar_year(&mut self, clock_time: &jiff::civil::DateTime) -> bool {
        if !(0..=9999).contains(&clock_time.year()) {
            tracelimit::warn_ratelimited!(
                year = clock_time.year(),
                "RTC UTC year is not representable"
            );
            self.invalidate_calendar();
            return false;
        }
        true
    }

    /// Returns status register A when the UTC clock cannot be sampled: an
    /// update is reported in progress and the calendar is invalidated.
    pub(super) fn status_a_without_clock(&mut self, mut data: StatusRegA) -> u8 {
        data.set_update(true);
        self.invalidate_calendar();
        data.into()
    }

    pub(super) fn invalidate_calendar(&mut self) {
        for reg in [
            CmosReg::SECOND,
            CmosReg::MINUTE,
            CmosReg::HOUR,
            CmosReg::DAY_OF_WEEK,
            CmosReg::DAY_OF_MONTH,
            CmosReg::MONTH,
            CmosReg::YEAR,
            self.century_reg,
        ] {
            self.state.cmos[reg] = 0;
        }
        self.state.time_valid = false;
    }
}
