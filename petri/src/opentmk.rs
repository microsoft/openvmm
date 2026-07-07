// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Running OpenTMK tests through Petri.
//!
//! OpenTMK boots as a UEFI guest and emits newline-delimited JSON over a serial
//! port (COM2 on x86_64). This module parses that stream, forwards each line to
//! the test log, and decides pass/fail from the assertion results and lifecycle
//! markers (`TEST_START` / `TEST_END`), accumulated in [`TmkRun`].

use crate::PetriLogFile;
use futures::AsyncBufRead;
use futures::AsyncBufReadExt;
use futures::AsyncRead;
use futures::io::BufReader;
use mesh::CancelContext;
use serde::Deserialize;
use std::time::Duration;
use tracing::Level;

/// Marker logged by the guest immediately before a test starts.
const TEST_START: &str = "TEST_START";
/// Marker logged by the guest after a test ends (success or panic).
const TEST_END: &str = "TEST_END";
/// Prefix the guest's panic handler logs before tearing down.
const PANIC_PREFIX: &str = "Panic at runtime";

/// Maximum bytes buffered for a single serial line before it is truncated.
/// Bounds host memory against a guest that never emits a newline.
const MAX_LINE_BYTES: usize = 64 * 1024;

/// A single newline-delimited JSON record emitted by OpenTMK over serial.
#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
enum TmkLine {
    /// A free-form log record.
    Log {
        /// Severity, as an OpenTMK `log::Level` string.
        level: String,
        /// The log message. Also carries the lifecycle markers.
        message: String,
        /// Source location, `file:line`.
        #[serde(default)]
        line: String,
    },
    /// An assertion result record.
    Assert {
        /// The asserted expression / description.
        message: String,
        /// Whether the assertion held.
        assertion_result: bool,
        /// Source location, `file:line`.
        #[serde(default)]
        line: String,
    },
}

/// Accumulated state of an OpenTMK run, derived purely from the serial stream.
#[derive(Debug, Default, Clone)]
pub struct TmkRun {
    /// Whether the `TEST_START` marker was seen.
    pub started: bool,
    /// Whether the `TEST_END` marker was seen.
    pub ended: bool,
    /// Number of assertions that passed.
    pub passed: u32,
    /// Number of assertions that failed.
    pub failed: u32,
    /// The guest panic message, if the run panicked.
    pub panic: Option<String>,
}

/// What a parsed serial line should contribute to the log.
struct LogTarget {
    level: Level,
    message: String,
}

/// Maps an OpenTMK `log::Level` string to a tracing [`Level`].
fn opentmk_level_to_tracing(level: &str) -> Level {
    match level {
        "ERROR" => Level::ERROR,
        "WARN" => Level::WARN,
        "INFO" => Level::INFO,
        "DEBUG" => Level::DEBUG,
        "TRACE" => Level::TRACE,
        _ => Level::INFO,
    }
}

/// Applies a single serial line to `run`, returning what should be logged.
///
/// Unparseable lines are passed through verbatim at `INFO` so nothing is dropped.
fn apply_line(run: &mut TmkRun, line: &str) -> LogTarget {
    match serde_json::from_str::<TmkLine>(line) {
        Ok(TmkLine::Log {
            level,
            message,
            line: src,
        }) => {
            match message.as_str() {
                TEST_START => run.started = true,
                TEST_END => run.ended = true,
                m if m.starts_with(PANIC_PREFIX) => run.panic = Some(message.clone()),
                _ => {}
            }
            let level = opentmk_level_to_tracing(&level);
            let message = if src.is_empty() {
                message
            } else {
                format!("{message} ({src})")
            };
            LogTarget { level, message }
        }
        Ok(TmkLine::Assert {
            message,
            assertion_result,
            line: src,
        }) => {
            if assertion_result {
                run.passed += 1;
            } else {
                run.failed += 1;
            }
            let level = if assertion_result {
                Level::INFO
            } else {
                Level::ERROR
            };
            let result = if assertion_result { "pass" } else { "FAIL" };
            let message = if src.is_empty() {
                format!("assert [{result}] {message}")
            } else {
                format!("assert [{result}] {message} ({src})")
            };
            LogTarget { level, message }
        }
        Err(_) => LogTarget {
            level: Level::INFO,
            message: line.to_string(),
        },
    }
}

/// Reads one newline-terminated line into `buf`, buffering at most `max` bytes
/// of line content; any further bytes up to the newline are consumed and
/// dropped. Returns the bytes consumed, or `0` at EOF.
async fn read_capped_line<R: AsyncBufRead + Unpin>(
    reader: &mut R,
    buf: &mut Vec<u8>,
    max: usize,
) -> std::io::Result<usize> {
    buf.clear();
    let mut consumed = 0;
    loop {
        let available = reader.fill_buf().await?;
        if available.is_empty() {
            return Ok(consumed);
        }
        if let Some(nl) = available.iter().position(|&b| b == b'\n') {
            let take = (max.saturating_sub(buf.len())).min(nl);
            buf.extend_from_slice(&available[..take]);
            let n = nl + 1;
            consumed += n;
            reader.consume_unpin(n);
            return Ok(consumed);
        }
        let n = available.len();
        let take = (max.saturating_sub(buf.len())).min(n);
        buf.extend_from_slice(&available[..take]);
        consumed += n;
        reader.consume_unpin(n);
    }
}

/// Reads the OpenTMK serial stream, forwarding every line to `log_file` and
/// accumulating run state.
///
/// Returns once `TEST_END` is seen, the stream hits EOF, or `timeout` elapses.
/// On timeout the partial [`TmkRun`] has `ended == false`, which [`evaluate`]
/// reports as a hang.
pub async fn scan_opentmk_serial(
    reader: impl AsyncRead + Unpin,
    log_file: &PetriLogFile,
    timeout: Duration,
) -> TmkRun {
    let mut run = TmkRun::default();
    let scan = async {
        let mut reader = BufReader::new(reader);
        let mut buf = Vec::new();
        loop {
            // Bounded read: a wedged or malicious guest that never emits a
            // newline must not grow `buf` without limit. Over-long lines are
            // truncated to `MAX_LINE_BYTES`.
            match read_capped_line(&mut reader, &mut buf, MAX_LINE_BYTES).await {
                Ok(0) | Err(_) => break,
                Ok(_) => {}
            }
            let line = String::from_utf8_lossy(&buf);
            let line = line.trim_end();
            if line.is_empty() {
                continue;
            }
            let LogTarget { level, message } = apply_line(&mut run, line);
            log_file.write_entry_fmt(None, level, format_args!("{message}"));
            if run.ended {
                break;
            }
        }
    };
    if CancelContext::new()
        .with_timeout(timeout)
        .until_cancelled(scan)
        .await
        .is_err()
    {
        log_file.write_entry_fmt(
            None,
            Level::ERROR,
            format_args!("OpenTMK serial scan timed out after {timeout:?}"),
        );
    }
    run
}

/// Evaluates a completed [`TmkRun`]; errors if the run did not pass.
///
/// A run passes only if the guest started, ended cleanly, did not panic, and
/// reported no failed assertions.
pub fn evaluate(run: &TmkRun) -> anyhow::Result<()> {
    if let Some(panic) = &run.panic {
        anyhow::bail!("OpenTMK guest panicked: {panic}");
    }
    if !run.started {
        anyhow::bail!("OpenTMK never reported TEST_START (guest failed to boot or run)");
    }
    if !run.ended {
        anyhow::bail!("OpenTMK never reported TEST_END (guest hung or crashed)");
    }
    if run.failed > 0 {
        anyhow::bail!(
            "OpenTMK reported {} failed assertion(s) ({} passed)",
            run.failed,
            run.passed
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Drives `apply_line` over a sequence of lines and returns the final state.
    fn run_lines(lines: &[&str]) -> TmkRun {
        let mut run = TmkRun::default();
        for line in lines {
            apply_line(&mut run, line);
        }
        run
    }

    #[test]
    fn passing_run_evaluates_ok() {
        let run = run_lines(&[
            r#"{"type":"log","level":"WARN","message":"TEST_START","line":"a:1"}"#,
            r#"{"type":"assert","level":"WARN","message":"vp_count == 4","line":"b:2","assertion_result":true}"#,
            r#"{"type":"assert","level":"WARN","message":"vtl is vtl0","line":"b:3","assertion_result":true}"#,
            r#"{"type":"log","level":"WARN","message":"TEST_END","line":"a:4"}"#,
        ]);
        assert!(run.started && run.ended);
        assert_eq!(run.passed, 2);
        assert_eq!(run.failed, 0);
        assert!(run.panic.is_none());
        evaluate(&run).unwrap();
    }

    #[test]
    fn failing_assert_then_panic_fails() {
        let run = run_lines(&[
            r#"{"type":"log","level":"WARN","message":"TEST_START","line":"a:1"}"#,
            r#"{"type":"assert","level":"WARN","message":"vp_count == 4","line":"b:2","assertion_result":false}"#,
            r#"{"type":"log","level":"ERROR","message":"Panic at runtime: Assertion failed: vp count","line":"r:5"}"#,
            r#"{"type":"log","level":"WARN","message":"TEST_END","line":"a:6"}"#,
        ]);
        assert_eq!(run.failed, 1);
        assert!(run.panic.is_some());
        assert!(evaluate(&run).is_err());
    }

    #[test]
    fn missing_test_end_is_a_hang() {
        let run = run_lines(&[
            r#"{"type":"log","level":"WARN","message":"TEST_START","line":"a:1"}"#,
            r#"{"type":"assert","level":"WARN","message":"x","line":"b:2","assertion_result":true}"#,
        ]);
        assert!(run.started && !run.ended);
        assert!(evaluate(&run).is_err());
    }

    #[test]
    fn never_started_fails() {
        let run = run_lines(&[]);
        assert!(evaluate(&run).is_err());
    }

    #[test]
    fn log_levels_map_and_non_json_passes_through() {
        assert_eq!(opentmk_level_to_tracing("ERROR"), Level::ERROR);
        assert_eq!(opentmk_level_to_tracing("DEBUG"), Level::DEBUG);
        assert_eq!(opentmk_level_to_tracing("bogus"), Level::INFO);

        let mut run = TmkRun::default();
        let target = apply_line(&mut run, "not json at all");
        assert_eq!(target.level, Level::INFO);
        assert_eq!(target.message, "not json at all");
    }
}
