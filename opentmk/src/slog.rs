#![feature(panic_location)]

use core::any::type_name;
use core::fmt::Write;
use core::result;

use crate::arch::serial::{InstrIoAccess, Serial};
use crate::sync::Mutex;
use alloc::string::{String, ToString};
#[no_std]
use serde_json::json;
use serde::Serialize;
pub enum Level {
    DEBUG = 0,
    INFO = 1,
    WARNING = 2,
    ERROR = 3,
    CRITICAL = 4,
}

pub fn get_json_string(s: &String, terminate_new_line: bool, level: Level) -> String {
    let out = json!({
        "type:": "log",
        "message": s,
        "level": match level {
            Level::DEBUG => "DEBUG",
            Level::INFO => "INFO",
            Level::WARNING => "WARNING",
            Level::ERROR => "ERROR",
            Level::CRITICAL => "CRITICAL",
        }
    });
    let mut out = out.to_string();
    if terminate_new_line {
        out.push('\n');
    }
    return out;
}

pub fn get_json_test_assertion_string<T>(
    s: &str,
    terminate_new_line: bool,
    line: String,
    assert_result: bool,
    testname: &T,
) -> String where T: Serialize {
    let out = json!({
        "type:": "assertion",
        "message": s,
        "level": "CRITICAL",
        "line": line,
        "assertion_result": assert_result,
        "testname": testname,
    });
    let mut out = out.to_string();
    if terminate_new_line {
        out.push('\n');
    }
    return out;
}

pub static mut SERIAL: Serial<InstrIoAccess> = Serial::new(InstrIoAccess {});

#[macro_export]
macro_rules! tmk_assert {
    ($condition:expr, $message:expr) => {{
        use core::fmt::Write;
        let file = core::file!();
        let line = line!();
        let file_line = format!("{}:{}", file, line);
        let expn = stringify!($condition);
        let result: bool = $condition;
        let js =
            crate::slog::get_json_test_assertion_string(&expn, true, file_line, result, &$message);
        unsafe { crate::slog::SERIAL.write_str(&js) };
        if !result {
            panic!("Assertion failed: {}", $message);
        }
    }};
}

#[macro_export]
macro_rules! logt {
    ($($arg:tt)*) => {
        {
        use core::fmt::Write;
        let message = format!($($arg)*);
        let js = crate::slog::get_json_string(&message, true, crate::slog::Level::INFO);
        unsafe { crate::slog::SERIAL.write_str(&js) };
        }
    };
}

#[macro_export]
macro_rules! errorlog {
    ($($arg:tt)*) => {
        {
        use core::fmt::Write;
        let message = format!($($arg)*);
        let js = crate::slog::get_json_string(&message, true, crate::slog::Level::ERROR);
        unsafe { crate::slog::SERIAL.write_str(&js) };
        }
    };
}

#[macro_export]
macro_rules! debuglog {
    ($($arg:tt)*) => {
        {
            use core::fmt::Write;

        let message = format!($($arg)*);
        let js = crate::slog::get_json_string(&message, true, crate::slog::Level::DEBUG);
        unsafe { crate::slog::SERIAL.write_str(&js) };
        }
    };
}

#[macro_export]
macro_rules! infolog {
    ($($arg:tt)*) => {
        {
            use core::fmt::Write;

        let message = format!($($arg)*);
        let js = crate::slog::get_json_string(&message, true, crate::slog::Level::INFO);
        unsafe { crate::slog::SERIAL.write_str(&js) };
        }
    };
}

#[macro_export]
macro_rules! warninglog {
    ($($arg:tt)*) => {
        {
            use core::fmt::Write;

        let message = format!($($arg)*);
        let js = crate::slog::get_json_string(&message, true, crate::slog::Level::WARNING);
        unsafe { crate::slog::SERIAL.write_str(&js) };
        }
    };
}

#[macro_export]
macro_rules! criticallog {
    ($($arg:tt)*) => {
        {
            use core::fmt::Write;

        let message = format!($($arg)*);
        let js = crate::slog::get_json_string(&message, true, crate::slog::Level::CRITICAL);
        unsafe { crate::slog::SERIAL.write_str(&js) };
        }
    };
}

#[macro_export]
macro_rules! slog {

    ($serial:expr, $($arg:tt)*) => {
        let mut serial : &mut Mutex<Serial<InstrIoAccess>> = &mut $serial;
        let message = format!($($arg)*);
        let js = slog::get_json_string(&message, true, crate::slog::Level::INFO);
        {
            let mut serial = serial.lock();
            serial.write_str(&js);
        }
    };

}

pub trait AssertResult<T, E> {
    fn unpack_assert(self) -> T;
    fn expect_assert(self, message: &str) -> T;
}

pub trait AssertOption<T> {
    fn expect_assert(self, message: &str) -> T;
}

impl<T> AssertOption<T> for Option<T> {
    fn expect_assert(self, message: &str) -> T {
        match self {
            Some(value) => value,
            None => {
                let call: &core::panic::Location<'_> = core::panic::Location::caller();
                let file_line = format!("{}:{}", call.file(), call.line());
                let expn = type_name::<Option<T>>();
                let js = crate::slog::get_json_test_assertion_string(
                    expn, true, file_line, false, &message,
                );
                unsafe { crate::slog::SERIAL.write_str(&js) };
                panic!("Assertion failed: {}", message);
            }
        }
    }
}

impl<T, E> AssertResult<T, E> for Result<T, E>
where
    E: core::fmt::Debug,
{
    fn unpack_assert(self) -> T {
        match self {
            Ok(value) => value,
            Err(err) => {
                let call: &core::panic::Location<'_> = core::panic::Location::caller();
                let file_line = format!("{}:{}", call.file(), call.line());
                let expn = type_name::<Result<T, E>>();
                let js = crate::slog::get_json_test_assertion_string(
                    expn,
                    true,
                    file_line,
                    false,
                    &"ResultTest",
                );
                unsafe { crate::slog::SERIAL.write_str(&js) };
                panic!("Assertion failed: {:?}", err);
            }
        }
    }
    fn expect_assert(self, message: &str) -> T {
        match self {
            Ok(value) => {
                infolog!("result is ok, condition not met for: {}", message);
                value
            }
            Err(err) => {
                let call: &core::panic::Location<'_> = core::panic::Location::caller();
                let file_line = format!("{}:{}", call.file(), call.line());
                let expn = type_name::<Result<T, E>>();
                let js = crate::slog::get_json_test_assertion_string(
                    expn, true, file_line, false, &message,
                );
                unsafe { crate::slog::SERIAL.write_str(&js) };

                panic!("Assertion failed: {:?}", err);
            }
        }
    }
}
