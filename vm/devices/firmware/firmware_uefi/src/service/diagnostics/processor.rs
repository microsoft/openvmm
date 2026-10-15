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
use crate::service::diagnostics::suppressor::LogSuppressor;
use guestmem::GuestMemory;
use thiserror::Error;

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
    suppressor: LogSuppressor,
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
            suppressor: LogSuppressor::new(),
            entries_processed: 0,
            entries_emitted: 0,
            bytes_read: 0,
        }
    }

    /// Log summary of suppressed messages and statistics
    fn log_summary(&self) {
        self.suppressor.log_summary();
        tracelimit::info_ratelimited!(
            entries_processed = self.entries_processed,
            entries_emitted = self.entries_emitted,
            bytes_read = self.bytes_read,
            "processed EFI log entries"
        );
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
                if processor.suppressor.should_emit(&complete_log, log_level) {
                    processor.entries_emitted += 1;
                    log_handler(&complete_log);
                }
            }
        }

        if let Some(final_log) = processor.accumulator.clear() {
            processor.entries_processed += 1;
            if processor.suppressor.should_emit(&final_log, log_level) {
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
    fn processing_filters_assembled_messages_and_preserves_other_errors() {
        let mut buffer = Vec::new();
        append_entry(&mut buffer, "Error: Image at 0003FC79000");
        append_entry(&mut buffer, " start failed: Unsupported\r\n");
        append_entry(&mut buffer, "[Bds] Unable to boot!\n");
        append_entry(
            &mut buffer,
            "MnpStart: MnpStartSnp failed, Already started.\n",
        );
        append_entry(
            &mut buffer,
            "SecurityLock::LockType: SOFTWARE_LOCK, Module: 42857F0A-13F2-4B21-8A23-53D3F714B840, Function: LockCapsuleInterface, Output: Lock Capsule Interface\n",
        );
        append_entry(
            &mut buffer,
            "VmbusRootIsChannelAllowed: Channel not allowed during boot (",
        );
        append_entry(&mut buffer, "525074DC-8985-46E2-8057-A307DC18A502).\r\n");
        append_entry(&mut buffer, "Unexpected boot failure\n");
        append_entry(&mut buffer, "Boot order is empty");
        let mut emitted = Vec::new();
        LogProcessor::process_buffer(&buffer, LogLevel::make_default(), |log| {
            emitted.push(log.message_trimmed().to_owned());
        })
        .unwrap();
        assert_eq!(emitted, ["Unexpected boot failure"]);

        buffer.clear();
        append_entry(
            &mut buffer,
            "PeiDelayedDispatchOnEndOfPei Count of dispatch cycles is 0",
        );
        emitted.clear();
        LogProcessor::process_buffer(&buffer, LogLevel::make_default(), |log| {
            emitted.push(log.message_trimmed().to_owned());
        })
        .unwrap();
        assert!(emitted.is_empty());
    }
}
