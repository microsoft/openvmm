// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Assertion handling and logging in JSON format.
//! This module provides a custom assertion macro `tmk_assert!` that logs assertion results in
//! JSON format. It also includes utility functions for formatting and writing log messages.

use alloc::string::String;
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

pub(crate) fn format_assert_json_string<T>(
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

    let mut out = serde_json::to_string(&assert_json).expect("Failed to serialize assert JSON");
    if terminate_new_line {
        out.push('\n');
    }
    out
}

pub(crate) fn write_str(s: &str) {
    _ = crate::tmk_logger::LOGGER.get_writer().write_str(s);
}

#[macro_export]
/// Asserts that a condition is true, logging the result in JSON format.
/// If the condition is false, it panics with the provided message.
macro_rules! tmk_assert {
    ($condition:expr, $message:expr) => {{
        let file = core::file!();
        let line = line!();
        let file_line = format!("{}:{}", file, line);
        let expn = stringify!($condition);
        let result: bool = $condition;
        let js = $crate::tmk_assert::format_assert_json_string(
            &expn, true, file_line, result, &$message,
        );
        $crate::tmk_assert::write_str(&js);
        if !result {
            panic!("Assertion failed: {}", $message);
        }
    }};
}
