// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Logging support for the bootshim.
//!
//! The bootshim performs no filtering of its logging messages when running in
//! a confidential VM. This is because it runs before any keys can be accessed
//! or any guest code is executed, and therefore it can not leak anything
//! sensitive.

#[cfg(target_arch = "x86_64")]
use crate::arch::tdx::TdxIoAccess;
use crate::host_params::shim_params::IsolationType;
use crate::single_threaded::SingleThreaded;
use core::cell::RefCell;
use core::fmt;
use core::fmt::Write;
use memory_range::MemoryRange;
#[cfg(target_arch = "x86_64")]
use minimal_rt::arch::InstrIoAccess;
use minimal_rt::arch::Serial;
use string_page_buf::StringBuffer;

enum Logger {
    #[cfg(target_arch = "x86_64")]
    Serial(Serial<InstrIoAccess>),
    #[cfg(target_arch = "aarch64")]
    Serial(Serial),
    #[cfg(target_arch = "x86_64")]
    TdxSerial(Serial<TdxIoAccess>),
    None,
}

impl Logger {
    fn write_str(&mut self, s: &str) -> fmt::Result {
        match self {
            Logger::Serial(serial) => serial.write_str(s),
            #[cfg(target_arch = "x86_64")]
            Logger::TdxSerial(serial) => serial.write_str(s),
            Logger::None => Ok(()),
        }
    }
}

pub struct BootLogger {
    logger: SingleThreaded<RefCell<Logger>>,
    in_memory_logger: SingleThreaded<RefCell<Option<StringBuffer<'static>>>>,
}

pub static BOOT_LOGGER: BootLogger = BootLogger {
    logger: SingleThreaded(RefCell::new(Logger::None)),
    in_memory_logger: SingleThreaded(RefCell::new(None)),
};

/// Initialize the in-memory log buffer. This range must be identity mapped, and
/// unused by anything else.
pub fn boot_logger_memory_init(buffer: MemoryRange) {
    if buffer.is_empty() {
        return;
    }

    let log_buffer_ptr = buffer.start() as *mut u8;
    // SAFETY: At file build time, this range is enforced to be unused by
    // anything else. The rest of the bootshim will mark this range as reserved
    // and not free to be used by anything else.
    //
    // The VA is valid as we are identity mapped.
    let log_buffer_slice =
        unsafe { core::slice::from_raw_parts_mut(log_buffer_ptr, buffer.len() as usize) };

    *BOOT_LOGGER.in_memory_logger.borrow_mut() = Some(
        StringBuffer::new(log_buffer_slice)
            .expect("log buffer should be valid from fixed at build config"),
    );
}

/// Initialize the runtime boot logger, for logging to serial or other outputs.
///
/// If a runtime logger was initialized, emit any in-memory log to the
/// configured runtime output.
pub fn boot_logger_runtime_init(isolation_type: IsolationType, com3_serial_available: bool) {
    let mut logger = BOOT_LOGGER.logger.borrow_mut();

    *logger = match (isolation_type, com3_serial_available) {
        #[cfg(target_arch = "x86_64")]
        (IsolationType::None, true) => Logger::Serial(Serial::init(InstrIoAccess)),
        #[cfg(target_arch = "aarch64")]
        (IsolationType::None, true) => Logger::Serial(Serial::init()),
        #[cfg(target_arch = "x86_64")]
        (IsolationType::Tdx, true) => Logger::TdxSerial(Serial::init(TdxIoAccess)),
        _ => Logger::None,
    };

    // Emit any in-memory log to the runtime logger.
    if let Some(buf) = BOOT_LOGGER.in_memory_logger.borrow_mut().as_mut() {
        let _ = logger.write_str(buf.contents());
    }
}

impl Write for &BootLogger {
    fn write_str(&mut self, s: &str) -> fmt::Result {
        if let Some(buf) = self.in_memory_logger.borrow_mut().as_mut() {
            // Ignore the errors from the in memory logger.
            let _ = buf.append(s);
        }
        self.logger.borrow_mut().write_str(s)
    }
}

impl log::Log for BootLogger {
    fn enabled(&self, _metadata: &log::Metadata<'_>) -> bool {
        // TODO: filter level
        true
    }

    fn log(&self, record: &log::Record<'_>) {
        let _ = writeln!(&*self, "{} - {}", record.level(), record.args());
    }

    fn flush(&self) {}
}
