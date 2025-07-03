use core::panic::PanicInfo;
use core::ptr::{read_volatile, write_volatile};
use super::io::{inb, outb};
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
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) ->
    core::fmt::Result {
        write!(f, "{:02}:{:02}:{:02} {:02}-{:02}-{:04} UTC",
               self.hours, self.minutes, self.seconds,
               self.day, self.month, 2000 + self.year as u64)
    }
}

// convert datetime to Unix epoch
impl DateTime {
    pub fn to_unix_epoch_sec(&self) -> u64 {
        let mut days = self.day as u64;
        days += (self.month as u64 - 1) * 30; // Approximation, not accurate for all months
        days += (self.year as u64 + 2000 - 1970) * 365; // Approximation, not accounting for leap years
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

// Read current date and time from RTC
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

pub fn delay_sec(seconds: u64) {
    let start = read_rtc().to_unix_epoch_sec();
    let end = start + seconds;
    loop {
        let current = read_rtc().to_unix_epoch_sec();
        if current >= end {
            break;
        }
    }
}