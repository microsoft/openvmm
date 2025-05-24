// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Real Time Clock Interface used by the UEFI Time Service
//!
//! Provides an interface to persist the time set by the guest (with UEFI
//! SetTime) so that the time retrieved by the guest (with UEFI GetTime)
//! reflects the time that has elapsed. Currently only used on ARM64.

use crate::UefiDevice;
use guestmem::GuestMemoryError;
use inspect::InspectMut;
use local_clock::InspectableLocalClock;
use thiserror::Error;
use time::OffsetDateTime;
use uefi_specs::hyperv::time::VmEfiTime;
use uefi_specs::uefi::common::EfiStatus;
use uefi_specs::uefi::time::EFI_TIME;
use uefi_specs::uefi::time::EfiDaylight;
use uefi_specs::uefi::time::EfiTimezone;

#[derive(Debug, Error)]
pub enum TimeServiceError {
    #[error("Invalid Argument")]
    InvalidArg,
    #[error("Time")]
    Time(#[from] time::Error),
    #[error("Overflow")]
    Overflow,
}

/// The same information as [`EFI_TIME`] but backed by [`OffsetDateTime`]
/// instead of raw numbers to make the values easier to manipulate.
struct EfiOffsetDateTime {
    datetime: OffsetDateTime,
    timezone: EfiTimezone,
    daylight: EfiDaylight,
}

impl TryFrom<EFI_TIME> for EfiOffsetDateTime {
    type Error = time::Error;

    fn try_from(v: EFI_TIME) -> Result<Self, Self::Error> {
        Ok(Self {
            datetime: OffsetDateTime::from_unix_timestamp(0)?
                .replace_year(v.year as i32)?
                .replace_month(v.month.try_into()?)?
                .replace_day(v.day)?
                .replace_hour(v.hour)?
                .replace_minute(v.minute)?
                .replace_second(v.second)?
                .replace_nanosecond(v.nanosecond)?,
            timezone: v.timezone,
            daylight: v.daylight,
        })
    }
}

impl From<EfiOffsetDateTime> for EFI_TIME {
    fn from(v: EfiOffsetDateTime) -> Self {
        Self {
            year: v.datetime.year() as u16,
            month: v.datetime.month().into(),
            day: v.datetime.day(),
            hour: v.datetime.hour(),
            minute: v.datetime.minute(),
            second: v.datetime.second(),
            pad1: 0,
            nanosecond: v.datetime.nanosecond(),
            timezone: v.timezone,
            daylight: v.daylight,
            pad2: 0,
        }
    }
}

#[derive(InspectMut)]
pub struct TimeServices {
    clock: Box<dyn InspectableLocalClock>,
    timezone: EfiTimezone,
    daylight: EfiDaylight,
}

impl TimeServices {
    /// Create a new time service using the provided clock source.
    /// SaveRestore for the clock should be handled externally.
    pub fn new(clock: Box<dyn InspectableLocalClock>) -> Self {
        Self {
            clock,
            timezone: EfiTimezone(0),
            daylight: EfiDaylight::new(),
        }
    }

    /// Get the [`LocalClock`](local_clock::LocalClock) time as [`EFI_TIME`].
    ///
    /// The clock implementation should handle any time delta between the host
    /// and guest, including timezone and daylight.
    pub fn get_time(&mut self) -> Result<EFI_TIME, TimeServiceError> {
        if !self.daylight.valid() || !self.timezone.valid() {
            return Err(TimeServiceError::InvalidArg);
        }

        let datetime: OffsetDateTime = self
            .clock
            .get_time()
            .try_into()
            .map_err(|_| TimeServiceError::Overflow)?;

        Ok(EfiOffsetDateTime {
            datetime,
            timezone: self.timezone,
            daylight: self.daylight,
        }
        .into())
    }

    /// Set the [`LocalClock`](local_clock::LocalClock) time from [`EFI_TIME`].
    ///
    /// The timezone and daylight information are saved so they can be retrieved
    /// by the guest, but not processed.
    pub fn set_time(&mut self, new_time: EFI_TIME) -> Result<(), TimeServiceError> {
        let EfiOffsetDateTime {
            datetime,
            timezone,
            daylight,
        } = new_time.try_into()?;

        if !daylight.valid() || !timezone.valid() {
            return Err(TimeServiceError::InvalidArg);
        }

        self.timezone = timezone;
        self.daylight = daylight;
        self.clock.set_time(datetime.into());

        Ok(())
    }
}

impl UefiDevice {
    /// Writes the time and status to the address specified.
    pub(crate) fn get_time(&mut self, gpa: u64) -> Result<(), GuestMemoryError> {
        let vm_time = match self.service.time.get_time() {
            Ok(time) => VmEfiTime {
                status: EfiStatus::SUCCESS.into(),
                time,
            },
            Err(e) => {
                tracing::debug!("get_time: {}", e);
                VmEfiTime {
                    status: EfiStatus::DEVICE_ERROR.into(),
                    time: Default::default(),
                }
            }
        };

        tracing::debug!("get_time: {:?}", vm_time);
        self.gm.write_plain(gpa, &vm_time)
    }

    /// Reads the time from address specified, updates internal state,
    /// and writes back the status.
    pub(crate) fn set_time(&mut self, gpa: u64) -> Result<(), GuestMemoryError> {
        let vm_time = self.gm.read_plain::<VmEfiTime>(gpa)?;
        let status = match self.service.time.set_time(vm_time.time) {
            Ok(_) => EfiStatus::SUCCESS,
            Err(e) => {
                tracing::debug!("set_time: {}", e);
                match e {
                    TimeServiceError::InvalidArg => EfiStatus::INVALID_PARAMETER,
                    _ => EfiStatus::DEVICE_ERROR,
                }
            }
        };

        let vm_time = VmEfiTime {
            time: vm_time.time,
            status: status.into(),
        };
        tracing::debug!("set_time: {:?}", vm_time);
        self.gm.write_plain(gpa, &vm_time)
    }
}

mod save_restore {
    use super::*;
    use vmcore::save_restore::RestoreError;
    use vmcore::save_restore::SaveError;
    use vmcore::save_restore::SaveRestore;

    mod state {
        use mesh::payload::Protobuf;
        use vmcore::save_restore::SavedStateRoot;

        #[derive(Protobuf, SavedStateRoot)]
        #[mesh(package = "firmware.uefi.time")]
        pub struct SavedState {
            #[mesh(1)]
            pub timezone: i16,
            #[mesh(2)]
            pub daylight: u8,
        }
    }

    impl SaveRestore for TimeServices {
        type SavedState = state::SavedState;

        fn save(&mut self) -> Result<Self::SavedState, SaveError> {
            Ok(state::SavedState {
                timezone: self.timezone.0,
                daylight: self.daylight.into(),
            })
        }

        fn restore(&mut self, state: Self::SavedState) -> Result<(), RestoreError> {
            let state::SavedState { timezone, daylight } = state;
            self.timezone = EfiTimezone(timezone);
            self.daylight = EfiDaylight::from(daylight);
            Ok(())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use local_clock::MockLocalClock;
    use local_clock::MockLocalClockAccessor;
    use std::time::Duration;
    use test_with_tracing::test;

    fn new_test_time_service() -> (MockLocalClockAccessor, TimeServices) {
        let time = MockLocalClock::new();
        let time_access = time.accessor();
        let service = TimeServices::new(Box::new(time));
        (time_access, service)
    }

    #[test]
    fn test_basic_get_time_set_time() {
        let (time_access, mut service) = new_test_time_service();
        let mut init_time = EFI_TIME {
            year: 2023,
            month: 11,
            day: 9,
            hour: 10,
            minute: 42,
            second: 27,
            pad1: 0,
            nanosecond: 0,
            timezone: EfiTimezone(-480),
            daylight: EfiDaylight::new().with_adjust_daylight(true),
            pad2: 0,
        };
        service.set_time(init_time).unwrap();
        time_access.tick(Duration::from_secs(2));
        init_time.second += 2;
        let new_time = service.get_time().unwrap();
        assert_eq!(init_time, new_time);
    }

    #[test]
    #[should_panic]
    fn test_validate_timezone() {
        let (_, mut service) = new_test_time_service();
        let init_time = EFI_TIME {
            year: 2023,
            month: 11,
            day: 9,
            hour: 10,
            minute: 42,
            second: 27,
            pad1: 0,
            nanosecond: 0,
            timezone: EfiTimezone(1500),
            daylight: EfiDaylight::new().with_adjust_daylight(true),
            pad2: 0,
        };
        service.set_time(init_time).unwrap();
    }

    #[test]
    #[should_panic]
    fn test_validate_daylight() {
        let (_, mut service) = new_test_time_service();
        let init_time = EFI_TIME {
            year: 2023,
            month: 11,
            day: 9,
            hour: 10,
            minute: 42,
            second: 27,
            pad1: 0,
            nanosecond: 0,
            timezone: EfiTimezone(-480),
            daylight: EfiDaylight::from(4),
            pad2: 0,
        };
        service.set_time(init_time).unwrap();
    }
}
