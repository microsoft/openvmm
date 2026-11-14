// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! The UTC clock source of the RTC: a fallible clock abstraction, calendar
//! invalidation when no representable time is available, advancing the
//! guest-visible time, and holding it while the RTC is stopped.
//!
//! While the RTC is stopped, it holds the guest-visible time rather than
//! reading its clock source, which runs with the host's clock: the held time
//! runs with VM time from the moment that the RTC stopped. VM time stops after
//! the RTC and starts before it, so a saved state records the time at the
//! snapshot's cut, and a restore sets the clock source only as the RTC starts,
//! to the restored time plus the downtime that the restore adds and the VM
//! time that has run since. Neither the time that the host takes to save or
//! restore, nor the host's time between the restore and the start, reaches
//! the guest. A pause that no restore, advance, or write follows leaves the
//! clock source alone, so that the guest's time catches up with the pause.

use super::Rtc;
use super::RtcMode;
use super::RtcState;
use super::spec::CmosReg;
use super::spec::StatusRegA;
use anyhow::Context;
use inspect::Inspect;
use local_clock::InspectableLocalClock;
use local_clock::LocalClockTime;
use std::ops::RangeInclusive;
use std::time::Duration;
use vmcore::line_interrupt::LineInterrupt;
use vmcore::vmtime::VmTime;
use vmcore::vmtime::VmTimeSource;
use vmcore::vmtime::VmTimerPeriodic;

/// The guest-visible UTC times, in milliseconds from the Unix epoch, that a
/// restore or an advance sets: the times that the guest can write to the
/// calendar registers, from 0000-01-01T00:00:00Z to 9999-12-30T22:00:00Z, the
/// last whole second that jiff represents in UTC, widened on both sides by the
/// range of the Linux host clock, `KTIME_SEC_MAX` seconds, over which the
/// guest's time runs with the host's. A clock source that keeps the
/// guest-visible time as an offset from the host's time cannot overflow that
/// offset for a time in this range, as it can for one near either end of
/// `i64`.
pub(super) const CLOCK_TIME_MILLIS: RangeInclusive<i64> = {
    const HOST_CLOCK_MILLIS: i64 = i64::MAX / 1_000_000_000 * 1000;
    -62_167_219_200_000 - HOST_CLOCK_MILLIS..=253_402_207_200_000 + HOST_CLOCK_MILLIS
};

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

/// The guest-visible UTC time that a stopped RTC holds.
#[derive(Debug, Clone, Copy)]
pub(super) struct HeldClock {
    /// The guest-visible UTC time at `vmtime`.
    utc: LocalClockTime,
    /// The VM time from which the held time runs.
    vmtime: VmTime,
    /// Whether starting the RTC sets its clock source to the held time,
    /// which a restore, an advance, or a write of the time requires.
    set_on_start: bool,
}

impl HeldClock {
    /// Returns the held time at VM time `now`, or `None` if it overflows.
    /// VM time before the held time's start counts as its start.
    fn time_at(&self, now: VmTime) -> Option<LocalClockTime> {
        let run = now.checked_sub(self.vmtime).unwrap_or(Duration::ZERO);
        let run = i64::try_from(run.as_millis()).ok()?;
        self.utc
            .as_millis_since_unix_epoch()
            .checked_add(run)
            .map(LocalClockTime::from_millis_since_unix_epoch)
    }
}

impl Rtc {
    /// Creates a CMOS RTC with an injectable, fallible UTC clock source.
    pub fn new_with_mode_and_utc_clock(
        real_time_source: Box<dyn UtcClockSource>,
        interrupt: LineInterrupt,
        vmtime_source: &VmTimeSource,
        century_reg_idx: u8,
        initial_cmos: Option<[u8; 256]>,
        enlightened_interrupts: bool,
        mode: RtcMode,
    ) -> Self {
        Rtc {
            century_reg: CmosReg(century_reg_idx),
            initial_cmos,
            enlightened_interrupts,
            mode,

            real_time_source,
            interrupt,
            vmtime_alarm: vmtime_source.access("rtc-alarm"),
            vmtimer_periodic: VmTimerPeriodic::new(vmtime_source.access("rtc-periodic")),
            vmtimer_update: VmTimerPeriodic::new(vmtime_source.access("rtc-update")),

            last_update_bit_blip: LocalClockTime::from_millis_since_unix_epoch(0),
            held: None,

            state: RtcState::with_mode(initial_cmos, mode),
        }
    }

    /// Returns the guest-visible UTC time: the held time while the RTC is
    /// stopped, and its clock source's time otherwise.
    pub(super) fn utc_time(&mut self) -> Result<LocalClockTime, UtcClockSourceError> {
        match self.held {
            Some(held) => held
                .time_at(self.vmtime_alarm.now())
                .ok_or(UtcClockSourceError),
            None => self.real_time_source.get_time(),
        }
    }

    /// Sets the guest-visible UTC time, which a stopped RTC holds until it
    /// starts.
    pub(super) fn set_utc_time(&mut self, time: LocalClockTime) {
        if self.held.is_some() {
            self.hold_utc_time(time);
        } else {
            self.real_time_source.set_time(time);
        }
    }

    /// Holds `time` as the guest-visible UTC time at the current VM time,
    /// to be set in the clock source as the RTC starts.
    pub(super) fn hold_utc_time(&mut self, time: LocalClockTime) {
        self.held = Some(HeldClock {
            utc: time,
            vmtime: self.vmtime_alarm.now(),
            set_on_start: true,
        });
    }

    /// Holds the guest-visible UTC time as the RTC stops. Without a sample of
    /// the clock source, the RTC holds nothing and keeps reading the source.
    pub(super) fn hold_clock(&mut self) {
        if self.held.is_some() {
            return;
        }
        match self.real_time_source.get_time() {
            Ok(utc) => {
                self.held = Some(HeldClock {
                    utc,
                    vmtime: self.vmtime_alarm.now(),
                    set_on_start: false,
                })
            }
            Err(error) => {
                tracelimit::warn_ratelimited!(?error, "failed to sample RTC UTC clock at stop")
            }
        }
    }

    /// Releases the held time as the RTC starts, setting the clock source to
    /// it if a restore, an advance, or a write changed it.
    pub(super) fn release_clock(&mut self) {
        let Some(held) = self.held.take() else {
            return;
        };
        if !held.set_on_start {
            return;
        }
        let time = held.time_at(self.vmtime_alarm.now()).unwrap_or_else(|| {
            tracelimit::warn_ratelimited!(
                ?held,
                "RTC time overflow while running the held time with VM time"
            );
            held.utc
        });
        self.real_time_source.set_time(time);
    }

    /// Runs the held time from the current VM time as the RTC resets, since
    /// VM time resets before the RTC does.
    pub(super) fn rebase_held_clock(&mut self) {
        let now = self.vmtime_alarm.now();
        if let Some(held) = &mut self.held {
            held.vmtime = now;
        }
    }

    /// Advances the guest-visible UTC time by `duration`, failing without
    /// changing it if the advanced time is outside [`CLOCK_TIME_MILLIS`].
    pub(super) fn advance_clock(&mut self, duration: Duration) -> anyhow::Result<()> {
        let delta = i64::try_from(duration.as_millis())
            .context("RTC downtime does not fit in milliseconds")?;
        let current = match self.held {
            Some(held) => held.utc,
            None => self
                .real_time_source
                .get_time()
                .context("failed to sample RTC UTC clock")?,
        };
        let advanced = LocalClockTime::from_millis_since_unix_epoch(
            current
                .as_millis_since_unix_epoch()
                .checked_add(delta)
                .filter(|advanced| CLOCK_TIME_MILLIS.contains(advanced))
                .context("RTC time out of range after applying snapshot downtime")?,
        );
        match &mut self.held {
            // VM time advances by the same downtime before the RTC does, so
            // the held time keeps running from the same point of VM time.
            Some(held) => {
                *held = HeldClock {
                    utc: advanced,
                    vmtime: held.vmtime.wrapping_add(duration),
                    set_on_start: true,
                }
            }
            None => self.real_time_source.set_time(advanced),
        }
        self.update_timers();
        self.sync_interrupt_line_level();
        Ok(())
    }

    /// Samples the UTC clock, invalidating the calendar if no sample is
    /// available.
    pub(super) fn sample_utc_clock(&mut self) -> Option<LocalClockTime> {
        match self.utc_time() {
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
