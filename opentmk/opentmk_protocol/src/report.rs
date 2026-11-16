// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Serial JSON report protocol emitted by OpenTMK guests over UART and parsed
//! by host test harnesses.

use alloc::borrow::ToOwned;
use alloc::string::String;
use alloc::string::ToString;
use serde::Deserialize;

/// Marker logged by the guest immediately before a test starts.
pub const TEST_START: &str = "TEST_START";

/// Marker logged by the guest after a test ends (normal completion or panic).
pub const TEST_END: &str = "TEST_END";

/// Prefix logged by the guest's panic handler before `TEST_END`.
pub const PANIC_PREFIX: &str = "Panic at runtime";

/// Raw JSON record emitted by OpenTMK over serial.
#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
enum TmkLine {
    /// A free-form log record (`opentmk_core::tmk_logger`).
    Log {
        level: String,
        message: String,
        #[serde(default)]
        line: String,
    },
    /// An assertion result record (`opentmk::tmk_assert!`).
    Assert {
        message: String,
        assertion_result: bool,
        #[serde(default)]
        line: String,
        #[serde(default)]
        testname: serde_json::Value,
    },
}

/// Parsed event from a single line of OpenTMK serial output.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OpenTmkEvent {
    /// Guest logged [`TEST_START`].
    TestStart {
        /// Source location (`file:line`), if present.
        line: String,
    },
    /// Structured guest log entry (`type == "log"`).
    Log {
        /// Severity level string (`ERROR`, `WARN`, `INFO`, `DEBUG`, `TRACE`).
        level: String,
        /// Log message text.
        message: String,
        /// Source location (`file:line`), if present.
        line: String,
    },
    /// Structured guest assertion record (`type == "assert"`).
    Assert {
        /// Stringified asserted expression.
        expression: String,
        /// Human-readable assertion message passed to `tmk_assert!`.
        description: String,
        /// Whether the assertion held.
        passed: bool,
        /// Source location (`file:line`), if present.
        line: String,
    },
    /// Guest runtime panic (`Panic at runtime: ...`).
    Panic {
        /// Full panic log message.
        message: String,
        /// Source location (`file:line`), if present.
        line: String,
    },
    /// Guest logged [`TEST_END`].
    TestEnd {
        /// Source location (`file:line`), if present.
        line: String,
    },
    /// Non-JSON serial line (e.g. UEFI firmware output prior to `ExitBootServices`).
    Raw(String),
}

/// Failure reason returned by [`OpenTmkRun::evaluate`].
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum OpenTmkFailure {
    /// The guest logged a runtime panic.
    #[error("OpenTMK guest panicked: {0}")]
    GuestPanicked(String),
    /// `TEST_START` was never observed on serial.
    #[error("OpenTMK never reported TEST_START (guest failed to boot or run)")]
    NeverStarted,
    /// `TEST_END` was never observed before scanning stopped.
    #[error("OpenTMK never reported TEST_END (guest hung or crashed)")]
    NeverEnded,
    /// One or more `tmk_assert!` checks failed.
    #[error("OpenTMK reported {failed} failed assertion(s) ({passed} passed)")]
    AssertionsFailed {
        /// Number of passing assertions observed.
        passed: u32,
        /// Number of failing assertions observed.
        failed: u32,
    },
}

/// Accumulated state of an OpenTMK guest run derived from its serial output.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct OpenTmkRun {
    /// Whether [`TEST_START`] was seen.
    pub started: bool,
    /// Whether [`TEST_END`] was seen.
    pub ended: bool,
    /// Count of passing assertions.
    pub passed: u32,
    /// Count of failing assertions.
    pub failed: u32,
    /// Panic message, if the guest panicked.
    pub panic: Option<String>,
}

impl OpenTmkRun {
    /// Parses `line`, updates `self`, and returns the corresponding [`OpenTmkEvent`].
    ///
    /// Lines that are not valid OpenTMK JSON records return [`OpenTmkEvent::Raw`]
    /// without mutating run state.
    pub fn apply_line(&mut self, line: &str) -> OpenTmkEvent {
        match serde_json::from_str::<TmkLine>(line) {
            Ok(TmkLine::Log {
                level,
                message,
                line,
            }) => match message.as_str() {
                TEST_START => {
                    self.started = true;
                    OpenTmkEvent::TestStart { line }
                }
                TEST_END => {
                    self.ended = true;
                    OpenTmkEvent::TestEnd { line }
                }
                m if m.starts_with(PANIC_PREFIX) => {
                    self.panic = Some(message.clone());
                    OpenTmkEvent::Panic { message, line }
                }
                _ => OpenTmkEvent::Log {
                    level,
                    message,
                    line,
                },
            },
            Ok(TmkLine::Assert {
                message: expression,
                assertion_result: passed,
                line,
                testname,
            }) => {
                if passed {
                    self.passed += 1;
                } else {
                    self.failed += 1;
                }
                let description = match testname {
                    serde_json::Value::Null => String::new(),
                    serde_json::Value::String(s) => s,
                    other => other.to_string(),
                };
                OpenTmkEvent::Assert {
                    expression,
                    description,
                    passed,
                    line,
                }
            }
            Err(_) => OpenTmkEvent::Raw(line.to_owned()),
        }
    }

    /// Evaluates whether the completed run passed default OpenTMK criteria.
    pub fn evaluate(&self) -> Result<(), OpenTmkFailure> {
        if let Some(panic) = &self.panic {
            return Err(OpenTmkFailure::GuestPanicked(panic.clone()));
        }
        if !self.started {
            return Err(OpenTmkFailure::NeverStarted);
        }
        if !self.ended {
            return Err(OpenTmkFailure::NeverEnded);
        }
        if self.failed > 0 {
            return Err(OpenTmkFailure::AssertionsFailed {
                passed: self.passed,
                failed: self.failed,
            });
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run_lines(lines: &[&str]) -> OpenTmkRun {
        let mut run = OpenTmkRun::default();
        for line in lines {
            run.apply_line(line);
        }
        run
    }

    #[test]
    fn passing_run_evaluates_ok() {
        let run = run_lines(&[
            r#"{"type":"log","level":"WARN","message":"TEST_START","line":"a:1"}"#,
            r#"{"type":"assert","level":"WARN","message":"vp_count == 4","line":"b:2","assertion_result":true,"testname":"vp count should be 4"}"#,
            r#"{"type":"assert","level":"WARN","message":"vtl == Vtl::Vtl0","line":"b:3","assertion_result":true,"testname":"vtl is vtl0"}"#,
            r#"{"type":"log","level":"WARN","message":"TEST_END","line":"a:4"}"#,
        ]);
        assert!(run.started && run.ended);
        assert_eq!(run.passed, 2);
        assert_eq!(run.failed, 0);
        assert!(run.panic.is_none());
        assert_eq!(run.evaluate(), Ok(()));
    }

    #[test]
    fn failing_assert_then_panic_fails() {
        let run = run_lines(&[
            r#"{"type":"log","level":"WARN","message":"TEST_START","line":"a:1"}"#,
            r#"{"type":"assert","level":"WARN","message":"vp_count == 4","line":"b:2","assertion_result":false,"testname":"vp count"}"#,
            r#"{"type":"log","level":"ERROR","message":"Panic at runtime: Assertion failed: vp count","line":"r:5"}"#,
            r#"{"type":"log","level":"WARN","message":"TEST_END","line":"a:6"}"#,
        ]);
        assert_eq!(run.failed, 1);
        assert!(matches!(
            run.evaluate(),
            Err(OpenTmkFailure::GuestPanicked(_))
        ));
    }

    #[test]
    fn missing_test_end_is_a_hang() {
        let run = run_lines(&[
            r#"{"type":"log","level":"WARN","message":"TEST_START","line":"a:1"}"#,
            r#"{"type":"assert","level":"WARN","message":"x","line":"b:2","assertion_result":true}"#,
        ]);
        assert_eq!(run.evaluate(), Err(OpenTmkFailure::NeverEnded));
    }

    #[test]
    fn never_started_fails() {
        let run = run_lines(&[]);
        assert_eq!(run.evaluate(), Err(OpenTmkFailure::NeverStarted));
    }

    #[test]
    fn events_preserve_assert_details_and_raw_lines() {
        let mut run = OpenTmkRun::default();
        assert_eq!(
            run.apply_line("UEFI firmware banner"),
            OpenTmkEvent::Raw("UEFI firmware banner".into())
        );
        assert_eq!(
            run.apply_line(
                r#"{"type":"assert","level":"WARN","message":"x == 1","line":"t.rs:10","assertion_result":true,"testname":"x must be 1"}"#
            ),
            OpenTmkEvent::Assert {
                expression: "x == 1".into(),
                description: "x must be 1".into(),
                passed: true,
                line: "t.rs:10".into(),
            }
        );
    }
}
