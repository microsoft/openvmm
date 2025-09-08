use alloc::borrow::ToOwned;
use alloc::fmt::format;
use alloc::string::String;
use alloc::string::ToString;
use core::fmt::Write;

use log::SetLoggerError;
use serde::Serialize;
use spin::Mutex;
use spin::MutexGuard;

#[cfg(target_arch = "x86_64")]
use crate::arch::serial::InstrIoAccess;
#[cfg(target_arch = "x86_64")]
use crate::arch::serial::Serial;
#[cfg(target_arch = "x86_64")]
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

pub struct TmkLogger<T> {
    pub writer: T,
}

impl<T> TmkLogger<Mutex<T>>
where
    T: Write + Send,
{
    pub const fn new(provider: T) -> Self {
        TmkLogger {
            writer: Mutex::new(provider),
        }
    }

    pub fn get_writer(&self) -> MutexGuard<'_, T>
    where
        T: Write + Send,
    {
        self.writer.lock()
    }
}

impl<T> log::Log for TmkLogger<Mutex<T>>
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
        _ = self.writer.lock().write_str(str.as_str());
    }

    fn flush(&self) {}
}

#[cfg(target_arch = "x86_64")]
type SerialPortWriter = Serial<InstrIoAccess>;
#[cfg(target_arch = "x86_64")]
pub static LOGGER: TmkLogger<Mutex<SerialPortWriter>> =
    TmkLogger::new(SerialPortWriter::new(SerialPort::COM2, InstrIoAccess));

#[cfg(target_arch = "aarch64")]
pub static LOGGER: TmkLogger<Mutex<Serial>> = TmkLogger::new(Serial {});

pub fn init() -> Result<(), SetLoggerError> {
    log::set_logger(&LOGGER).map(|()| log::set_max_level(log::LevelFilter::Debug))
}
