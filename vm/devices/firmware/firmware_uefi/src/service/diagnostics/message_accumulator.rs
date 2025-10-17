// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Message accumulation logic for EFI diagnostics buffer processing

use crate::service::diagnostics::formatting::EfiDiagnosticsLog;
use crate::service::diagnostics::formatting::debug_level_to_string;
use crate::service::diagnostics::formatting::phase_to_string;
use crate::service::diagnostics::parser::EntryData;
use crate::service::diagnostics::parser::MAX_MESSAGE_LENGTH;
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

/// Errors that can occur during message accumulation
#[derive(Debug, Error)]
pub enum AccumulationError {
    /// Accumulated message exceeds maximum allowed length
    #[error("Expected accumulated message length < {0:#x}, got: {1:#x}")]
    MessageTooLong(u16, u16),
}

/// Statistics about processed entries
#[derive(Debug, Default)]
pub struct ProcessingStats {
    /// Number of entries processed
    pub entries_processed: usize,
    /// Number of bytes read
    pub bytes_read: usize,
}

/// Manages message accumulation state and processing
pub struct MessageAccumulator {
    /// The accumulated message content
    accumulated_message: String,
    /// Current entry's debug level
    debug_level: u32,
    /// Current entry's timestamp
    time_stamp: u64,
    /// Current entry's phase
    phase: u16,
    /// Whether we're currently accumulating a message
    is_accumulating: bool,
    /// Map of suppressed log patterns to their counts
    suppressed_logs: BTreeMap<&'static str, u32>,
    /// Processing statistics
    pub stats: ProcessingStats,
}

impl MessageAccumulator {
    /// Create a new message accumulator
    pub fn new() -> Self {
        Self {
            accumulated_message: String::with_capacity(MAX_MESSAGE_LENGTH as usize),
            debug_level: 0,
            time_stamp: 0,
            phase: 0,
            is_accumulating: false,
            suppressed_logs: BTreeMap::new(),
            stats: ProcessingStats::default(),
        }
    }

    /// Process an entry and potentially emit a completed log
    ///
    /// Returns `Ok(Some(log))` if a complete log is ready to emit,
    /// `Ok(None)` if still accumulating or suppressed,
    /// `Err` if there was an error during processing.
    pub fn process_entry(
        &mut self,
        entry: &EntryData<'_>,
    ) -> Result<Option<(EfiDiagnosticsLog<'_>, u32)>, AccumulationError> {
        let EntryData {
            debug_level,
            time_stamp,
            phase,
            message,
            entry_size,
        } = entry;

        // Handle message accumulation
        if !self.is_accumulating {
            self.debug_level = *debug_level;
            self.time_stamp = *time_stamp;
            self.phase = *phase;
            self.accumulated_message.clear();
            self.is_accumulating = true;
        }

        self.accumulated_message.push_str(message);
        if self.accumulated_message.len() > MAX_MESSAGE_LENGTH as usize {
            return Err(AccumulationError::MessageTooLong(
                MAX_MESSAGE_LENGTH,
                self.accumulated_message.len() as u16,
            ));
        }

        // Update processing statistics
        self.stats.bytes_read += entry_size;

        // Handle completed messages (ending with '\n')
        if !message.is_empty() && message.ends_with('\n') {
            self.stats.entries_processed += 1;
            Ok(self.finalize_message())
        } else {
            Ok(None)
        }
    }

    /// Finalize any remaining accumulated message
    ///
    /// Should be called at the end of processing to handle any incomplete messages.
    pub fn finalize_remaining(&mut self) -> Option<(EfiDiagnosticsLog<'_>, u32)> {
        if self.is_accumulating && !self.accumulated_message.is_empty() {
            self.stats.entries_processed += 1;
            self.finalize_message()
        } else {
            None
        }
    }

    /// Log summary of suppressed messages
    pub fn log_suppressed_summary(&self) {
        for (substring, count) in &self.suppressed_logs {
            tracelimit::warn_ratelimited!(substring, count, "suppressed logs");
        }
    }

    /// Internal method to finalize the current accumulated message
    fn finalize_message(&mut self) -> Option<(EfiDiagnosticsLog<'_>, u32)> {
        self.is_accumulating = false;

        if self.check_suppression() {
            None
        } else {
            Some((
                EfiDiagnosticsLog {
                    debug_level: debug_level_to_string(self.debug_level),
                    ticks: self.time_stamp,
                    phase: phase_to_string(self.phase),
                    message: self.accumulated_message.trim_end_matches(&['\r', '\n'][..]),
                },
                self.debug_level,
            ))
        }
    }

    /// Check if the current accumulated message should be suppressed
    fn check_suppression(&mut self) -> bool {
        let mut suppress = false;
        for &pattern in &SUPPRESS_LOGS {
            if self.accumulated_message.contains(pattern) {
                self.suppressed_logs
                    .entry(pattern)
                    .and_modify(|count| *count += 1)
                    .or_insert(1);
                suppress = true;
            }
        }
        suppress
    }
}

impl Default for MessageAccumulator {
    fn default() -> Self {
        Self::new()
    }
}
