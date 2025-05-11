use core::{any::type_name, fmt::Write};
use alloc::string::{String, ToString};
use serde::Serialize;
use serde_json::json;

pub fn format_asset_json_string<T>(
    s: &str,
    terminate_new_line: bool,
    line: String,
    assert_result: bool,
    testname: &T,
) -> String
where
    T: Serialize,
{
    let out = json!({
        "type:": "assert",
        "level": "WARN",
        "message": s,
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


pub fn write_str(s: &str) {
    let _ = crate::tmk_logger::LOGGER.get_writter().write_str(s);
}

#[macro_export]
macro_rules! tmk_assert {
    ($condition:expr, $message:expr) => {
        {
            let file = core::file!();
            let line = line!();
            let file_line = format!("{}:{}", file, line);
            let expn = stringify!($condition);
            let result: bool = $condition;
            let js = crate::tmk_assert::format_asset_json_string(
                &expn, true, file_line, result, &$message,
            );
            crate::tmk_assert::write_str(&js);
            if !result {
                panic!("Assertion failed: {}", $message);
            }
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
                let js = format_asset_json_string(expn, true, file_line, false, &message);
                write_str(&js);
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
                let js =
                    format_asset_json_string(expn, true, file_line, false, &"ResultTest");
                write_str(&js);
                panic!("Assertion failed: {:?}", err);
            }
        }
    }
    fn expect_assert(self, message: &str) -> T {
        match self {
            Ok(value) => {
                log::info!("result is ok, condition not met for: {}", message);
                value
            }
            Err(err) => {
                let call: &core::panic::Location<'_> = core::panic::Location::caller();
                let file_line = format!("{}:{}", call.file(), call.line());
                let expn = type_name::<Result<T, E>>();
                let js = format_asset_json_string(expn, true, file_line, false, &message);
                write_str(&js);
                panic!("Assertion failed: {:?}", err);
            }
        }
    }
}
