use alloc::string::{String, ToString};
use core::fmt::Write;
use serde::Serialize;

#[derive(Serialize)]
struct AssertJson<'a, T>
where
    T: Serialize,
{
    #[serde(rename = "type")]
    type_: &'a str,
    level: &'a str,
    message: &'a str,
    line: String,
    assertion_result: bool,
    testname: &'a T,
}

impl<'a, T> AssertJson<'a, T>
where
    T: Serialize,
{
    fn new(
        type_: &'a str,
        level: &'a str,
        message: &'a str,
        line: String,
        assertion_result: bool,
        testname: &'a T,
    ) -> Self {
        Self {
            type_,
            level,
            message,
            line,
            assertion_result,
            testname,
        }
    }
}

pub fn format_assert_json_string<T>(
    s: &str,
    terminate_new_line: bool,
    line: String,
    assert_result: bool,
    testname: &T,
) -> String
where
    T: Serialize,
{
    let assert_json = AssertJson::new("assert", "WARN", s, line, assert_result, testname);

    let out = serde_json::to_string(&assert_json).expect("Failed to serialize assert JSON");
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
    ($condition:expr, $message:expr) => {{
        let file = core::file!();
        let line = line!();
        let file_line = format!("{}:{}", file, line);
        let expn = stringify!($condition);
        let result: bool = $condition;
        let js =
            crate::tmk_assert::format_assert_json_string(&expn, true, file_line, result, &$message);
        crate::tmk_assert::write_str(&js);
        if !result {
            panic!("Assertion failed: {}", $message);
        }
    }};
}