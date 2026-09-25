// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Core processing logic for EFI diagnostics buffer

use crate::service::diagnostics::LogLevel;
use crate::service::diagnostics::accumulator::LogAccumulator;
use crate::service::diagnostics::gpa::Gpa;
use crate::service::diagnostics::header::HeaderParseError;
use crate::service::diagnostics::header::LogBufferHeader;
use crate::service::diagnostics::log::Log;
use crate::service::diagnostics::log::LogParseError;
use guestmem::GuestMemory;
use std::collections::BTreeMap;
use thiserror::Error;

enum SuppressionMatch {
    Contains,
    Exact,
    SingleLinePrefix,
    UnsupportedImage,
}

// Temporary noise suppression pending firmware investigation and fixes.
// Keep new matches narrow so other statuses and failures remain visible.
// TODO: Fix UEFI to resolve these errors/warnings
const SUPPRESS_LOGS: &[(&str, SuppressionMatch)] = &[
    (
        "WARNING: There is mismatch of supported HashMask (0x2 - 0x7) between modules",
        SuppressionMatch::Contains,
    ),
    (
        "that are linking different HashInstanceLib instances!",
        SuppressionMatch::Contains,
    ),
    (
        "ConvertPages: failed to find range",
        SuppressionMatch::Contains,
    ),
    (
        "ConvertPages: Incompatible memory types",
        SuppressionMatch::Contains,
    ),
    ("ConvertPages: range", SuppressionMatch::Contains),
    (
        "InstallPermanentMemoryBuffer: - New Info=",
        SuppressionMatch::SingleLinePrefix,
    ),
    (
        "PeiDelayedDispatchOnEndOfPei Count of dispatch cycles is 0",
        SuppressionMatch::Exact,
    ),
    (
        "FPDT: WARNING: SEC Performance Data Hob not found, ResetEnd will be set to 0!",
        SuppressionMatch::Exact,
    ),
    (
        "OnVariablePolicyNotification: - Unable to locate variable policy protocol - Status=Not Found",
        SuppressionMatch::Exact,
    ),
    (
        "Error: Image at <address> start failed: Unsupported",
        SuppressionMatch::UnsupportedImage,
    ),
    (
        "MnpStart: MnpStartSnp failed, Already started.",
        SuppressionMatch::Exact,
    ),
    (
        "WARN [DE]: Failed to locate on-screen keyboard protocol (Not Found).",
        SuppressionMatch::Exact,
    ),
];

fn suppression_pattern(message: &str) -> Option<&'static str> {
    for &(pattern, ref match_kind) in SUPPRESS_LOGS {
        let matches = match match_kind {
            SuppressionMatch::Contains => message.contains(pattern),
            SuppressionMatch::Exact => message == pattern,
            SuppressionMatch::SingleLinePrefix => {
                message.starts_with(pattern) && !message.contains(['\r', '\n'])
            }
            SuppressionMatch::UnsupportedImage => message
                .strip_prefix("Error: Image at ")
                .and_then(|rest| rest.strip_suffix(" start failed: Unsupported"))
                .is_some_and(|address| {
                    !address.is_empty()
                        && address.len() <= 16
                        && address.bytes().all(|c| c.is_ascii_hexdigit())
                }),
        };
        if matches {
            return Some(pattern);
        }
    }
    None
}

/// Iterator over raw log entries from a buffer.
///
/// This iterator parses individual log entries from the buffer slice,
/// advancing the buffer as it goes. It stops on the first parse error.
struct RawLogIterator<'a> {
    buffer: &'a [u8],
}

impl<'a> RawLogIterator<'a> {
    fn new(buffer: &'a [u8]) -> Self {
        Self { buffer }
    }
}

impl<'a> Iterator for RawLogIterator<'a> {
    type Item = Result<(Log, usize), LogParseError>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.buffer.is_empty() {
            return None;
        }

        match Log::from_buffer(self.buffer) {
            Ok((log, consumed)) => {
                self.buffer = if consumed >= self.buffer.len() {
                    &[]
                } else {
                    &self.buffer[consumed..]
                };
                Some(Ok((log, consumed)))
            }
            Err(e) => {
                // Stop processing on error
                self.buffer = &[];
                Some(Err(e))
            }
        }
    }
}

/// Errors that occur during processing
#[derive(Debug, Error)]
pub enum ProcessingError {
    /// Failed to parse header from guest memory
    #[error("Failed to parse header: {0}")]
    HeaderParse(#[from] HeaderParseError),
    /// Failed to parse a log entry from the buffer
    #[error("Failed to parse log: {0}")]
    LogParse(#[from] LogParseError),
    /// Failed to read from guest memory
    #[error("Failed to read from guest memory: {0}")]
    GuestMemoryRead(#[from] guestmem::GuestMemoryError),
}

/// Processes diagnostics from guest memory (internal implementation)
///
/// # Arguments
/// * `gpa` - The GPA of the diagnostics buffer
/// * `gm` - Guest memory to read diagnostics from
/// * `log_level` - Log level for filtering
/// * `log_handler` - Function to handle each parsed log entry
pub fn process_diagnostics_internal<F>(
    gpa: Option<Gpa>,
    gm: &GuestMemory,
    log_level: LogLevel,
    log_handler: F,
) -> Result<(), ProcessingError>
where
    F: FnMut(&Log),
{
    // Parse and validate the header
    let header = LogBufferHeader::from_guest_memory(gpa, gm)?;

    // Early exit if buffer is empty
    if header.is_empty() {
        tracelimit::info_ratelimited!(
            "EFI diagnostics' used log buffer size is 0, ending processing"
        );
        return Ok(());
    }

    // Read the log buffer from guest memory
    let buffer_start_gpa = header.buffer_start_gpa()?;
    let mut buffer_data = vec![0u8; header.used_size() as usize];
    gm.read_at(buffer_start_gpa.as_u64(), &mut buffer_data)?;

    // Process the buffer
    LogProcessor::process_buffer(&buffer_data, log_level, log_handler)
}

/// Internal processor for log entries with suppression tracking
struct LogProcessor {
    /// Accumulator for multi-part messages
    accumulator: LogAccumulator,
    /// Map of suppressed log patterns to their counts
    suppressed_logs: BTreeMap<&'static str, u32>,
    /// Number of entries processed
    entries_processed: usize,
    /// Number of entries emitted (passed level/suppression filters)
    entries_emitted: usize,
    /// Number of bytes read from buffer
    bytes_read: usize,
}

impl LogProcessor {
    fn new() -> Self {
        Self {
            accumulator: LogAccumulator::new(),
            suppressed_logs: BTreeMap::new(),
            entries_processed: 0,
            entries_emitted: 0,
            bytes_read: 0,
        }
    }

    /// Check if a log should be suppressed based on known patterns
    fn should_suppress(&mut self, log: &Log) -> bool {
        if let Some(pattern) = suppression_pattern(log.message_trimmed()) {
            *self.suppressed_logs.entry(pattern).or_insert(0) += 1;
            return true;
        }
        false
    }

    /// Log summary of suppressed messages and statistics
    fn log_summary(&self) {
        for (substring, count) in &self.suppressed_logs {
            tracelimit::warn_ratelimited!(substring, count, "suppressed logs");
        }
        tracelimit::info_ratelimited!(
            entries_processed = self.entries_processed,
            entries_emitted = self.entries_emitted,
            bytes_read = self.bytes_read,
            "processed EFI log entries"
        );
    }

    /// Check if a log should be emitted based on level and suppression
    fn should_emit(&mut self, log: &Log, log_level: LogLevel) -> bool {
        log_level.should_log(log.debug_level) && !self.should_suppress(log)
    }

    /// Process the log buffer and emit completed log entries
    fn process_buffer<F>(
        buffer_data: &[u8],
        log_level: LogLevel,
        mut log_handler: F,
    ) -> Result<(), ProcessingError>
    where
        F: FnMut(&Log),
    {
        let mut processor = Self::new();

        for result in RawLogIterator::new(buffer_data) {
            let (log, bytes_consumed) = match result {
                Ok((log, bytes)) => (log, bytes),
                Err(e) => {
                    tracelimit::warn_ratelimited!(error = ?e, "Failed to parse log entry, stopping processing");
                    break;
                }
            };

            processor.bytes_read += bytes_consumed;
            processor.accumulator.feed(log)?;

            if let Some(complete_log) = processor.accumulator.take() {
                processor.entries_processed += 1;
                if processor.should_emit(&complete_log, log_level) {
                    processor.entries_emitted += 1;
                    log_handler(&complete_log);
                }
            }
        }

        if let Some(final_log) = processor.accumulator.clear() {
            processor.entries_processed += 1;
            if processor.should_emit(&final_log, log_level) {
                processor.entries_emitted += 1;
                log_handler(&final_log);
            }
        }

        processor.log_summary();
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::service::diagnostics::log::ALIGNMENT;
    use std::mem::size_of;
    use test_with_tracing::test;
    use uefi_specs::hyperv::advanced_logger::AdvancedLoggerMessageEntryV2;
    use uefi_specs::hyperv::advanced_logger::DXE_PHASE;
    use uefi_specs::hyperv::advanced_logger::SIG_ENTRY;
    use uefi_specs::hyperv::debug_level::DEBUG_ERROR;
    use uefi_specs::hyperv::debug_level::DEBUG_INFO;

    const EXACT_MESSAGES: [&str; 5] = [
        "PeiDelayedDispatchOnEndOfPei Count of dispatch cycles is 0",
        "FPDT: WARNING: SEC Performance Data Hob not found, ResetEnd will be set to 0!",
        "OnVariablePolicyNotification: - Unable to locate variable policy protocol - Status=Not Found",
        "MnpStart: MnpStartSnp failed, Already started.",
        "WARN [DE]: Failed to locate on-screen keyboard protocol (Not Found).",
    ];

    fn log(message: &str) -> Log {
        Log {
            debug_level: DEBUG_ERROR,
            time_stamp: 0,
            phase: DXE_PHASE,
            message: message.to_owned(),
        }
    }

    #[test]
    fn approved_messages_are_suppressed_with_or_without_line_endings() {
        let mut processor = LogProcessor::new();
        for message in EXACT_MESSAGES.into_iter().chain([
                "InstallPermanentMemoryBuffer: - New Info=44FE000, Buffer Offset=50, Current Offset=1000, Size=4194224, Discarded=5511",
                "InstallPermanentMemoryBuffer: - New Info=123ABCD, Buffer Offset=50, Current Offset=1000, Size=4194224, Discarded=0",
                "Error: Image at 0003FC79000 start failed: Unsupported",
                "Error: Image at 0003FBD0000 start failed: Unsupported",
                "Error: Image at abcdef0123456789 start failed: Unsupported",
            ]) {
                for ending in ["", "\n", "\r\n"] {
                    assert!(
                        processor.should_suppress(&log(&format!("{message}{ending}"))),
                        "{message:?} with ending {ending:?}"
                    );
                }
            }
        assert_eq!(processor.suppressed_logs.len(), 7);
    }

    #[test]
    fn channel_and_boot_failures_remain_visible() {
        let mut processor = LogProcessor::new();
        for guid in [
            "0E0B6031-5213-4934-818B-38D90CED39DB",
            "525074DC-8985-46E2-8057-A307DC18A502",
            "57164F39-9115-4E78-AB55-382F3BD5422D",
            "9527E630-D0AE-497B-ADCE-E80AB0175CAF",
            "A9A0F4E7-5A45-4D96-B827-8A841E8C03E6",
            "CFA8B69E-5B4A-4CC0-B98B-8BA1A1F3F95A",
        ] {
            let log = log(&format!(
                "VmbusRootIsChannelAllowed: Channel not allowed during boot ({guid}).\n"
            ));
            assert!(processor.should_emit(&log, LogLevel::make_default()));
        }
        for message in ["[Bds] Unable to boot!\n", "Boot order is empty\n"] {
            assert!(processor.should_emit(&log(message), LogLevel::make_default()));
        }
        assert!(processor.suppressed_logs.is_empty());
    }

    #[test]
    fn similar_messages_and_other_statuses_remain_visible() {
        let mut processor = LogProcessor::new();
        for message in [
            "PeiDelayedDispatchOnEndOfPei Count of dispatch cycles is 1",
            "PeiDelayedDispatchOnEndOfPei Count of dispatch cycles is 01",
            "OnVariablePolicyNotification: - Unable to locate variable policy protocol - Status=Device Error",
            "MnpStart: MnpStartSnp failed, Device Error.",
            "WARN [DE]: Failed to locate on-screen keyboard protocol (Device Error).",
            "InstallPermanentMemoryBuffer: allocation failed",
            "InstallPermanentMemoryBuffer: - New Info=123\nUnexpected failure",
            "Error: Image at 0003FC79000 start failed: Security Violation",
            "Error: Image at 0003FC79000 start failed: Unsupported operation",
            "Error: Image at  start failed: Unsupported",
            "Error: Image at not-an-address start failed: Unsupported",
            "Error: Image at 12345678901234567 start failed: Unsupported",
            "Error: Image at 123\n456 start failed: Unsupported",
            "Error: Image at 0003FC79000 start failed: Unsupported\nUnexpected failure",
        ] {
            assert!(!processor.should_suppress(&log(message)), "{message}");
        }
        for message in EXACT_MESSAGES {
            assert!(!processor.should_suppress(&log(&format!("Unexpected: {message}"))));
            assert!(!processor.should_suppress(&log(&format!("{message}\nUnexpected failure"))));
        }
        assert!(processor.suppressed_logs.is_empty());
    }

    #[test]
    fn existing_filters_and_suppression_counts_are_preserved() {
        let mut processor = LogProcessor::new();
        for pattern in [
            "WARNING: There is mismatch of supported HashMask (0x2 - 0x7) between modules",
            "that are linking different HashInstanceLib instances!",
            "ConvertPages: failed to find range",
            "ConvertPages: Incompatible memory types",
            "ConvertPages: range",
        ] {
            for _ in 0..2 {
                assert!(processor.should_suppress(&log(&format!("prefix {pattern} suffix\n"))));
            }
            assert_eq!(processor.suppressed_logs[pattern], 2);
        }
        for address in ["0003FC79000", "0003FBD0000"] {
            assert!(processor.should_suppress(&log(&format!(
                "Error: Image at {address} start failed: Unsupported"
            ))));
        }
        assert_eq!(
            processor.suppressed_logs["Error: Image at <address> start failed: Unsupported"],
            2
        );
    }

    #[test]
    fn level_filtering_still_precedes_suppression() {
        let mut processor = LogProcessor::new();
        let mut log = log(EXACT_MESSAGES[0]);
        log.debug_level = DEBUG_INFO;
        assert!(!processor.should_emit(&log, LogLevel::make_default()));
        assert!(processor.suppressed_logs.is_empty());
        assert!(!processor.should_emit(&log, LogLevel::make_full()));
        assert_eq!(processor.suppressed_logs[EXACT_MESSAGES[0]], 1);
    }

    fn append_entry(buffer: &mut Vec<u8>, message: &str) {
        let header_size = size_of::<AdvancedLoggerMessageEntryV2>() as u16;
        buffer.extend_from_slice(&SIG_ENTRY);
        buffer.extend_from_slice(&[2, 0]);
        buffer.extend_from_slice(&DEBUG_ERROR.to_le_bytes());
        buffer.extend_from_slice(&0u64.to_le_bytes());
        buffer.extend_from_slice(&DXE_PHASE.to_le_bytes());
        buffer.extend_from_slice(&(message.len() as u16).to_le_bytes());
        buffer.extend_from_slice(&header_size.to_le_bytes());
        buffer.extend_from_slice(message.as_bytes());
        buffer.resize(buffer.len().next_multiple_of(ALIGNMENT), 0);
    }

    #[test]
    fn processing_filters_assembled_messages_and_preserves_boot_errors() {
        let mut buffer = Vec::new();
        append_entry(&mut buffer, "Error: Image at 0003FC79000");
        append_entry(&mut buffer, " start failed: Unsupported\r\n");
        append_entry(&mut buffer, "[Bds] Unable to boot!\n");
        append_entry(
            &mut buffer,
            "MnpStart: MnpStartSnp failed, Already started.\n",
        );
        append_entry(&mut buffer, "Boot order is empty");
        let mut emitted = Vec::new();
        LogProcessor::process_buffer(&buffer, LogLevel::make_default(), |log| {
            emitted.push(log.message_trimmed().to_owned());
        })
        .unwrap();
        assert_eq!(emitted, ["[Bds] Unable to boot!", "Boot order is empty"]);

        buffer.clear();
        append_entry(&mut buffer, EXACT_MESSAGES[0]);
        emitted.clear();
        LogProcessor::process_buffer(&buffer, LogLevel::make_default(), |log| {
            emitted.push(log.message_trimmed().to_owned());
        })
        .unwrap();
        assert!(emitted.is_empty());
    }
}
