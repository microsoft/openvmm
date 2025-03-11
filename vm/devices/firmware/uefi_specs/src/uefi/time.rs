// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! UEFI Time Services.

use bitfield_struct::bitfield;
use core::fmt::Display;
use static_assertions::const_assert_eq;
use zerocopy::FromBytes;
use zerocopy::FromZeros;
use zerocopy::Immutable;
use zerocopy::IntoBytes;
use zerocopy::KnownLayout;

/// UEFI Time Structure
///
/// UEFI spec 8.3 - Time Services
///
/// ```text
///  Year:       1900 - 9999
///  Month:      1 - 12
///  Day:        1 - 31
///  Hour:       0 - 23
///  Minute:     0 - 59
///  Second:     0 - 59
///  Nanosecond: 0 - 999,999,999
///  TimeZone:   -1440 to 1440 or 2047
/// ```
#[repr(C)]
#[derive(Copy, Clone, Debug, IntoBytes, FromBytes, Immutable, KnownLayout, PartialEq, Eq)]
#[cfg_attr(feature = "inspect", derive(inspect::Inspect), inspect(display))]
pub struct EFI_TIME {
    pub year: u16,
    pub month: u8,
    pub day: u8,
    pub hour: u8,
    pub minute: u8,
    pub second: u8,
    pub pad1: u8,
    pub nanosecond: u32,
    pub timezone: EfiTimezone,
    pub daylight: EfiDaylight,
    pub pad2: u8,
}

// Default + FromBytes: The UEFI spec explicitly uses all-zero EFI_TIME as a
// default value
impl Default for EFI_TIME {
    fn default() -> Self {
        Self::new_zeroed()
    }
}

const_assert_eq!(size_of::<EFI_TIME>(), 16);

impl EFI_TIME {
    /// EFI_TIME with all fields set to zero
    pub const ZEROED: EFI_TIME = EFI_TIME {
        year: 0,
        month: 0,
        day: 0,
        hour: 0,
        minute: 0,
        second: 0,
        pad1: 0,
        nanosecond: 0,
        timezone: EfiTimezone(0),
        daylight: EfiDaylight::new(),
        pad2: 0,
    };
}

impl Display for EFI_TIME {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        // ISO-8601: 2022-02-17T04:54:13Z
        write!(
            f,
            "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}",
            self.year, self.month, self.day, self.hour, self.minute, self.second
        )?;
        if self.nanosecond != 0 {
            write!(f, ".{:09}", self.nanosecond)?;
        }
        if self.timezone.0 == 0 {
            write!(f, "Z")?;
        } else if self.timezone != EFI_UNSPECIFIED_TIMEZONE {
            let sign = if self.timezone.0 > 0 { '+' } else { '-' };
            let timezone = (self.timezone.0 as i32).abs();
            write!(f, "{sign}{:02}:{:02}", timezone / 60, timezone % 60)?;
        }
        Ok(())
    }
}

/// Value Definition for EFI_TIME.TimeZone
/// from UEFI spec 8.3 - Time Services
pub const EFI_UNSPECIFIED_TIMEZONE: EfiTimezone = EfiTimezone(0x07FF);

/// Timezone in minutes from UTC
///
/// Valid values include -1440 to 1440 or 2047 (EFI_UNSPECIFIED_TIMEZONE)
#[derive(Copy, Clone, Debug, IntoBytes, FromBytes, Immutable, KnownLayout, PartialEq, Eq)]
#[repr(transparent)]
#[cfg_attr(feature = "inspect", derive(inspect::Inspect), inspect(transparent))]
pub struct EfiTimezone(pub i16);

impl EfiTimezone {
    pub fn valid(&self) -> bool {
        self.0 > -1440 && (self.0 < 1440 || *self == EFI_UNSPECIFIED_TIMEZONE)
    }
}

/// Bit Definitions for EFI_TIME.EfiDaylight
/// from UEFI spec 8.3 - Time Services
#[bitfield(u8)]
#[derive(IntoBytes, FromBytes, Immutable, KnownLayout, PartialEq, Eq)]
#[cfg_attr(feature = "inspect", derive(inspect::Inspect), inspect(transparent))]
pub struct EfiDaylight {
    /// EFI_TIME_ADJUST_DAYLIGHT
    ///
    /// the time is affected by daylight savings time
    pub adjust_daylight: bool,

    /// EFI_TIME_IN_DAYLIGHT
    ///
    /// the time has been adjusted for daylight savings time
    pub in_daylight: bool,

    #[bits(6)]
    rsvd: u8,
}

impl EfiDaylight {
    pub fn valid(&self) -> bool {
        self.rsvd() == 0
    }
}
