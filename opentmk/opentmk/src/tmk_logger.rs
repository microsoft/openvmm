use core::fmt::Write;

use alloc::{
    fmt::format,
    string::{String, ToString},
};
use log::SetLoggerError;
use sync_nostd::{Mutex, MutexGuard};

use crate::arch::serial::{InstrIoAccess, Serial};
use serde::Serialize;

#[derive(Serialize)]
struct LogEntry {
    log_type: &'static str,
    level: String,
    message: String,
    line: String,
}

impl LogEntry {
    fn new(level: log::Level, message: &String, line: &String) -> Self {
        LogEntry {
            log_type: "log",
            level: level.as_str().to_string(),
            message: message.clone(),
            line: line.clone(),
        }
    }
}

pub fn format_log_string_to_json(
    message: &String,
    line: &String,
    terminate_new_line: bool,
    level: log::Level,
) -> String {
    let log_entry = LogEntry::new(level, message, line);
    let out = serde_json::to_string(&log_entry).unwrap();
    let mut out = out.to_string();
    if terminate_new_line {
        out.push('\n');
    }
    return out;
}

pub struct TmkLogger<T> {
    pub writter: T,
}

impl<T> TmkLogger<Mutex<T>>
where
    T: Write + Send,
{
    pub const fn new(provider: T) -> Self {
        TmkLogger {
            writter: Mutex::new(provider),
        }
    }

    pub fn get_writter(&self) -> MutexGuard<'_, T>
    where
        T: Write + Send,
    {
        self.writter.lock()
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
        _ = self.writter.lock().write_str(str.as_str());
    }

    fn flush(&self) {}
}

pub static LOGGER: TmkLogger<Mutex<Serial<InstrIoAccess>>> =
    TmkLogger::new(Serial::new(InstrIoAccess {}));

pub fn init() -> Result<(), SetLoggerError> {
    log::set_logger(&LOGGER).map(|()| log::set_max_level(log::LevelFilter::Debug))
}
