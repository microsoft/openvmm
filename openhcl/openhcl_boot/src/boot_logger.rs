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

/// Log a message. These messages are always emitted regardless of debug or
/// release, if a corresponding logger was configured.
///
/// If you want to log something just for local debugging, use [`debug_log!`]
/// instead.
macro_rules! log {
    () => {};
    ($($arg:tt)*) => {
        {
            use core::fmt::Write;
            let _ = writeln!(&$crate::boot_logger::BOOT_LOGGER, $($arg)*);
        }
    };
}

pub(crate) use log;

/// This emits the same as [`log!`], but is intended for local debugging and is
/// linted against to not pass CI. Use for local development when you just need
/// debug prints.
//
// Expect unused macros for the same reason as unused_imports below, as there
// should be no usage of this macro normally.
#[expect(unused_macros)]
macro_rules! debug_log {
    ($($arg:tt)*) => {
        $crate::boot_logger::log!($($arg)*)
    };
}

// Expect unused imports because there should be no normal usage in code due to
// lints against it in CI.
#[expect(unused_imports)]
pub(crate) use debug_log;

/// Write the current in-memory boot log to serial output, if any.
/// Useful to capture the in-memory log before switching to a different
/// environment where the in-memory log may be lost.
pub fn boot_logger_write_memory_log_to_runtime() {
    if let Some(buf) = BOOT_LOGGER.in_memory_logger.borrow().as_ref() {
        let mut logger = BOOT_LOGGER.logger.borrow_mut();
        let _ = logger.write_str(buf.contents());
    }
}
