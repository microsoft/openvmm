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
#[cfg(target_arch = "x86_64")]
use minimal_rt::arch::InstrIoAccess;
use minimal_rt::arch::Serial;

/// The logging type to use.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LoggerType {
    Serial,
}

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
}

pub static BOOT_LOGGER: BootLogger = BootLogger {
    logger: SingleThreaded(RefCell::new(Logger::None)),
};

/// Initialize the boot logger. This replaces any previous init calls.
///
/// If a given `logger_type` is unavailable on a given isolation type, the
/// logger will ignore it, and no logging will be initialized.
pub fn boot_logger_init(isolation_type: IsolationType, logger_type: LoggerType) {
    let mut logger = BOOT_LOGGER.logger.borrow_mut();

    *logger = match (isolation_type, logger_type) {
        #[cfg(target_arch = "x86_64")]
        (IsolationType::None, LoggerType::Serial) => Logger::Serial(Serial::init(InstrIoAccess)),
        #[cfg(target_arch = "aarch64")]
        (IsolationType::None, LoggerType::Serial) => Logger::Serial(Serial::init()),
        #[cfg(target_arch = "x86_64")]
        (IsolationType::Tdx, LoggerType::Serial) => Logger::TdxSerial(Serial::init(TdxIoAccess)),
        _ => Logger::None,
    };
}

impl Write for &BootLogger {
    fn write_str(&mut self, s: &str) -> fmt::Result {
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
// Allow unused macros for the same reason as unused_imports below, as there
// should be no usage of this macro normally.
#[allow(unused_macros)]
macro_rules! debug_log {
    ($($arg:tt)*) => {
        $crate::boot_logger::log!($($arg)*)
    };
}

// Allow unused imports because there should be no normal usage in code due to
// lints against it in CI.
#[allow(unused_imports)]
pub(crate) use debug_log;
