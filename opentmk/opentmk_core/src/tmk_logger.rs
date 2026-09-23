// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Logger implementation for OpenTMK.
//! This module provides a logger that formats log messages as JSON and writes them to a specified output
//! such as a serial port.

use alloc::borrow::ToOwned;
use alloc::fmt::format;
use alloc::string::String;
use alloc::string::ToString;
use core::fmt::Write;

use log::SetLoggerError;
use serde::Serialize;
use spin::Mutex;

#[cfg(target_arch = "x86_64")]
use crate::arch::serial::InstrIoAccess;
#[cfg(target_arch = "x86_64")]
use crate::arch::serial::Serial;
use crate::arch::serial::SerialPort;
#[cfg(target_arch = "aarch64")]
use minimal_rt::arch::Serial;

#[derive(Serialize)]
struct LogEntry {
    #[serde(rename = "type")]
    log_type: &'static str,
    level: String,
    message: String,
    line: String,
}

impl LogEntry {
    fn new(level: log::Level, message: &str, line: &str) -> Self {
        LogEntry {
            log_type: "log",
            level: level.as_str().to_string(),
            message: message.to_owned(),
            line: line.to_owned(),
        }
    }
}

/// Formats a log message into a JSON string.
pub(crate) fn format_log_string_to_json(
    message: &str,
    line: &str,
    terminate_new_line: bool,
    level: log::Level,
) -> String {
    let log_entry = LogEntry::new(level, message, line);
    let mut out = serde_json::to_string(&log_entry).unwrap();
    if terminate_new_line {
        out.push('\n');
    }
    out
}

/// A logger that writes log messages to a provided writer, such as a serial port.
pub struct TmkLogger<T> {
    writer: T,
}

impl<T> TmkLogger<Mutex<Option<T>>>
where
    T: Write + Send,
{
    /// Creates a new `TmkLogger` instance without a writer.
    pub const fn new() -> Self {
        TmkLogger {
            writer: Mutex::new(None),
        }
    }

    fn set_writer(&self, writer: T) {
        *self.writer.lock() = Some(writer);
    }

    /// Writes a preformatted string if the logger has been initialized.
    pub fn write_str(&self, value: &str) {
        if let Some(writer) = self.writer.lock().as_mut() {
            _ = writer.write_str(value);
        }
    }
}

impl<T> log::Log for TmkLogger<Mutex<Option<T>>>
where
    T: Write + Send,
{
    fn enabled(&self, _metadata: &log::Metadata<'_>) -> bool {
        true
    }

    fn log(&self, record: &log::Record<'_>) {
        let str = format(*record.args());
        let line = format!(
            "{}:{}",
            record.file().unwrap_or_default(),
            record.line().unwrap_or_default()
        );
        let str = format_log_string_to_json(&str, &line, true, record.level());
        self.write_str(&str);
    }

    fn flush(&self) {}
}

#[cfg(target_arch = "x86_64")]
type SerialPortWriter = Serial<InstrIoAccess>;
#[cfg(target_arch = "x86_64")]
/// The global logger instance for x86_64 architecture.
pub static LOGGER: TmkLogger<Mutex<Option<SerialPortWriter>>> = TmkLogger::new();

#[cfg(target_arch = "aarch64")]
/// The global logger instance for aarch64 architecture.
pub static LOGGER: TmkLogger<Mutex<Option<Serial>>> = TmkLogger::new();

/// Initializes the global logger on the specified serial port.
pub fn init(port: SerialPort) -> Result<(), SetLoggerError> {
    #[cfg(target_arch = "x86_64")]
    {
        LOGGER.set_writer(SerialPortWriter::new(port, InstrIoAccess));
    }
    #[cfg(target_arch = "aarch64")]
    {
        let _ = port;
        LOGGER.set_writer(Serial {});
    }
    log::set_logger(&LOGGER).map(|()| log::set_max_level(log::LevelFilter::Debug))
}
