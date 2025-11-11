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
    let (header, base_gpa) = LogBufferHeader::from_guest_memory(gpa, gm)?;

    // Early exit if buffer is empty
    if header.is_empty() {
        tracelimit::info_ratelimited!(
            "EFI diagnostics' used log buffer size is 0, ending processing"
        );
        return Ok(());
    }

    // Read the log buffer from guest memory
    let buffer_start_addr = header.buffer_start_address(base_gpa)?;
    let mut buffer_data = vec![0u8; header.used_size() as usize];
    gm.read_at(buffer_start_addr as u64, &mut buffer_data)?;

    // Process the buffer
    process_buffer(&buffer_data, log_level, log_handler)?;

    Ok(())
}

/// Internal processor for log entries with suppression tracking
struct LogProcessor {
    /// Accumulator for multi-part messages
    accumulator: LogAccumulator,
    /// Map of suppressed log patterns to their counts
    suppressed_logs: BTreeMap<&'static str, u32>,
    /// Number of entries processed
    entries_processed: usize,
    /// Number of bytes read from buffer
    bytes_read: usize,
}

impl LogProcessor {
    fn new() -> Self {
        Self {
            accumulator: LogAccumulator::new(),
            suppressed_logs: BTreeMap::new(),
            entries_processed: 0,
            bytes_read: 0,
        }
    }

    /// Check if a log should be suppressed based on known patterns
    fn should_suppress(&mut self, log: &Log) -> bool {
        let mut suppress = false;
        for &pattern in &SUPPRESS_LOGS {
            if log.message.contains(pattern) {
                self.suppressed_logs
                    .entry(pattern)
                    .and_modify(|count| *count += 1)
                    .or_insert(1);
                suppress = true;
            }
        }
        suppress
    }

    /// Log summary of suppressed messages and statistics
    fn log_summary(&self) {
        for (substring, count) in &self.suppressed_logs {
            tracelimit::warn_ratelimited!(substring, count, "suppressed logs");
        }
        tracelimit::info_ratelimited!(
            entries_processed = self.entries_processed,
            bytes_read = self.bytes_read,
            "processed EFI log entries"
        );
    }

    /// Check if a log should be emitted based on level and suppression
    fn should_emit(&mut self, log: &Log, log_level: LogLevel) -> bool {
        log_level.should_log(log.debug_level) && !self.should_suppress(log)
    }
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
    let mut buffer_slice = buffer_data;
    let mut processor = LogProcessor::new();

    // Process the buffer slice until all entries are processed
    while !buffer_slice.is_empty() {
        let log = match Log::from_buffer(buffer_slice) {
            Ok(log) => log,
            Err(e) => {
                // Log the error and break - don't try to continue with corrupted data
                tracelimit::warn_ratelimited!(error = ?e, "Failed to parse log entry, stopping processing");
                break;
            }
        };

        let consumed = log.consumed_bytes;
        processor.bytes_read += consumed;

        // Feed the log into the accumulator
        processor.accumulator.feed(log)?;

        // Check if we have a complete message to emit
        if let Some(complete_log) = processor.accumulator.take() {
            processor.entries_processed += 1;

            if processor.should_emit(&complete_log, log_level) {
                log_handler(&complete_log);
            }
        }

        // Move to the next entry
        if consumed >= buffer_slice.len() {
            break; // End of buffer
        } else {
            buffer_slice = &buffer_slice[consumed..];
        }
    }

    // Process any remaining accumulated message
    if let Some(final_log) = processor.accumulator.clear() {
        processor.entries_processed += 1;

        if processor.should_emit(&final_log, log_level) {
            log_handler(&final_log);
        }
    }

    // Log suppressed message summary and statistics
    processor.log_summary();

    Ok(())
}
