// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! x86_64-specific implementation for reading from the RTC (Real-Time Clock) and CMOS.

use super::io::inb;
use super::io::outb;
// CMOS/RTC I/O ports
const CMOS_ADDRESS: u16 = 0x70;
const CMOS_DATA: u16 = 0x71;

// RTC register addresses
const RTC_SECONDS: u8 = 0x00;
const RTC_MINUTES: u8 = 0x02;
const RTC_HOURS: u8 = 0x04;
const RTC_DAY: u8 = 0x07;
const RTC_MONTH: u8 = 0x08;
const RTC_YEAR: u8 = 0x09;
const RTC_STATUS_A: u8 = 0x0A;
const RTC_STATUS_B: u8 = 0x0B;

#[repr(C)]
#[derive(Debug, Clone, Copy)]
/// Represents date and time read from the RTC.
pub struct DateTime {
    seconds: u8,
    minutes: u8,
    hours: u8,
    day: u8,
    month: u8,
    year: u8,
}

// implement display as ISO 8601 format
impl core::fmt::Display for DateTime {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(
            f,
            "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}Z",
            2000 + self.year as u64,
            self.month,
            self.day,
            self.hours,
            self.minutes,
            self.seconds
        )
    }
}

/// convert datetime to Unix epoch
impl DateTime {
    /// Converts the DateTime to seconds since the Unix epoch (1970-01-01T00:00:00Z).
    pub fn unix_epoch_sec(&self) -> u64 {
        // Check if a year is a leap year
        let is_leap_year = |year: u64| -> bool {
            (year.is_multiple_of(4) && !year.is_multiple_of(100)) || year.is_multiple_of(400)
        };

        // Define days in each month (0-indexed array)
        let days_in_month = [31, 28, 31, 30, 31, 30, 31, 31, 30, 31, 30, 31];

        // Calculate days since Unix epoch (1970-01-01)
        let year = 2000 + self.year as u64;
        let month = self.month as u64;
        let day = self.day as u64;

        // Days from years
        let mut days = 0u64;
        for y in 1970..year {
            days += 365 + if is_leap_year(y) { 1 } else { 0 };
        }

        // Add days from months in current year
        for m in 1..month {
            days += days_in_month[m as usize - 1] as u64;
            // Add leap day if February and leap year
            if m == 2 && is_leap_year(year) {
                days += 1;
            }
        }

        // Add days of current month
        days += day - 1; // -1 because we want elapsed days
        let hours = self.hours as u64;
        let minutes = self.minutes as u64;
        let seconds = self.seconds as u64;

        (days * 24 + hours) * 3600 + (minutes * 60) + seconds
    }
}

// Read from CMOS/RTC register
fn read_cmos(reg: u8) -> u8 {
    outb(CMOS_ADDRESS, reg);
    inb(CMOS_DATA)
}

// Check if RTC update is in progress
fn rtc_update_in_progress() -> bool {
    read_cmos(RTC_STATUS_A) & 0x80 != 0
}

// Convert BCD to binary if needed
fn bcd_to_binary(bcd: u8) -> u8 {
    (bcd & 0x0F) + ((bcd >> 4) * 10)
}

/// Read current date and time from RTC
pub fn read_rtc() -> DateTime {
    // Wait for any update to complete
    while rtc_update_in_progress() {}

    let mut datetime = DateTime {
        seconds: read_cmos(RTC_SECONDS),
        minutes: read_cmos(RTC_MINUTES),
        hours: read_cmos(RTC_HOURS),
        day: read_cmos(RTC_DAY),
        month: read_cmos(RTC_MONTH),
        year: read_cmos(RTC_YEAR),
    };

    // Check if we need to wait for another update cycle
    while rtc_update_in_progress() {}

    // Read again to ensure consistency
    let seconds_check = read_cmos(RTC_SECONDS);
    if seconds_check != datetime.seconds {
        datetime.seconds = seconds_check;
        datetime.minutes = read_cmos(RTC_MINUTES);
        datetime.hours = read_cmos(RTC_HOURS);
        datetime.day = read_cmos(RTC_DAY);
        datetime.month = read_cmos(RTC_MONTH);
        datetime.year = read_cmos(RTC_YEAR);
    }

    // Check RTC format (BCD vs binary)
    let status_b = read_cmos(RTC_STATUS_B);
    let is_bcd = (status_b & 0x04) == 0;

    if is_bcd {
        datetime.seconds = bcd_to_binary(datetime.seconds);
        datetime.minutes = bcd_to_binary(datetime.minutes);
        datetime.hours = bcd_to_binary(datetime.hours);
        datetime.day = bcd_to_binary(datetime.day);
        datetime.month = bcd_to_binary(datetime.month);
        datetime.year = bcd_to_binary(datetime.year);
    }

    // Handle 12-hour format if needed
    if (status_b & 0x02) == 0 && (datetime.hours & 0x80) != 0 {
        datetime.hours = ((datetime.hours & 0x7F) + 12) % 24;
    }

    datetime
}

/// Busy-wait delay for specified seconds using RTC
pub fn delay_sec(seconds: u64) {
    let start = read_rtc().unix_epoch_sec();
    let end = start + seconds;
    loop {
        let current = read_rtc().unix_epoch_sec();
        if current >= end {
            break;
        }
    }
}
