// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! This module provides `LogAccumulator` which handles the assembly of
//! multi-part log messages that span multiple buffer entries.

use crate::service::diagnostics::log::Log;
use crate::service::diagnostics::log::LogParseError;
use crate::service::diagnostics::log::MAX_MESSAGE_LENGTH;

/// Handles multi-part messages where a single logical log entry spans
/// multiple buffer entries. Messages are accumulated until a newline
/// is encountered, then emitted as a complete log.
pub struct LogAccumulator {
    /// The current log being accumulated (if any)
    current: Option<Log>,
}

impl LogAccumulator {
    /// Create a new log accumulator
    pub fn new() -> Self {
        Self { current: None }
    }

    /// Feed a log entry into the accumulator
    ///
    /// If the log completes a message (ends with newline), it can be
    /// retrieved with `take()`. Otherwise, it's accumulated with the
    /// current message.
    ///
    /// # Arguments
    /// * `log` - The log entry to feed
    ///
    /// # Errors
    /// Returns `LogParseError::MessageTooLong` if the accumulated message
    /// exceeds the maximum length.
    pub fn feed(&mut self, log: Log) -> Result<(), LogParseError> {
        match self.current.take() {
            None => {
                // No current message, start accumulating
                self.current = Some(log);
            }
            Some(mut current) => {
                // Append to existing message
                current.message.push_str(&log.message);

                // Validate total length
                if current.message.len() > MAX_MESSAGE_LENGTH as usize {
                    return Err(LogParseError::MessageTooLong(
                        MAX_MESSAGE_LENGTH,
                        current.message.len() as u16,
                    ));
                }

                self.current = Some(current);
            }
        }

        Ok(())
    }

    /// Take the current accumulated log if it's complete
    ///
    /// Returns `Some(log)` if there's a complete message (ending with newline),
    /// otherwise returns `None`. Once taken, the accumulator is reset.
    pub fn take(&mut self) -> Option<Log> {
        if let Some(ref log) = self.current {
            if log.is_complete() {
                return self.current.take();
            }
        }
        None
    }

    /// Clear the current accumulated log (if any)
    ///
    /// Useful for finalizing incomplete messages at the end of processing.
    pub fn clear(&mut self) -> Option<Log> {
        self.current.take()
    }
}

impl Default for LogAccumulator {
    fn default() -> Self {
        Self::new()
    }
}
