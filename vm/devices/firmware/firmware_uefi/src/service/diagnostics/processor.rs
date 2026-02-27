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

// Suppress logs that contain these known error/warning messages.
// These messages are the result of known issues with our UEFI firmware that do
// not seem to affect the guest.
// TODO: Fix UEFI to resolve these errors/warnings
const SUPPRESS_LOGS: [&str; 5] = [
    "WARNING: There is mismatch of supported HashMask (0x2 - 0x7) between modules",
    "that are linking different HashInstanceLib instances!",
    "ConvertPages: failed to find range",
    "ConvertPages: Incompatible memory types",
    "ConvertPages: range",
];

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
        for &pattern in &SUPPRESS_LOGS {
            if log.message.contains(pattern) {
                *self.suppressed_logs.entry(pattern).or_insert(0) += 1;
                return true;
            }
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
