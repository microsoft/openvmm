use alloc::{
    fmt::format,
    string::{String, ToString},
};
use core::fmt::Write;

use log::SetLoggerError;
use serde::Serialize;
use sync_nostd::{Mutex, MutexGuard};
use anyhow::Result;
use crate::arch::serial::{InstrIoAccess, Serial, SerialPort};

#[derive(Serialize)]
struct LogEntry {
    #[serde(rename = "type")]
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

pub(crate) fn format_log_string_to_json(
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
    out
}

pub struct TmkLogger<T> {
    pub writer: T,
}

impl<T> TmkLogger<Mutex<Option<T>>>
where
    T: Write + Send,
{
    pub fn new_provider(provider: T) -> Self {
        TmkLogger {
            writer: Mutex::new(Some(provider)),
        }
    }

    pub const fn new() -> Self {
        TmkLogger {
            writer: Mutex::new(None),
        }
    }

    pub fn set_writer(&self, writter: T) {
        self.writer
            .lock()
            .replace(writter);
    }

    pub fn get_writer(&self) -> MutexGuard<'_, Option<T>>
    where
        T: Write + Send,
    {
        self.writer.lock()
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
        self.get_writer()
            .as_mut()
            .map(|writer| {
                writer.write_str(&str)
            });
    }

    fn flush(&self) {}
}

type SerialPortWriter = Serial<InstrIoAccess>;
pub static LOGGER: TmkLogger<Mutex<Option<SerialPortWriter>>> = TmkLogger::new();

pub fn init() -> Result<(), SetLoggerError> {
    let serial = SerialPortWriter::new(SerialPort::COM2);
    LOGGER.set_writer(serial);
    
    log::set_logger(&LOGGER)
        .map(|()| log::set_max_level(log::LevelFilter::Debug))
}
