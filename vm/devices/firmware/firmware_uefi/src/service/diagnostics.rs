// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! UEFI diagnostics subsystem
//!
//! This module stores the GPA of the efi dignostics buffer.
//! When signaled, the diagnostics buffer is parsed and written to
//! trace logs.
use crate::UefiDevice;
use guestmem::GuestMemory;
use inspect::Inspect;
use std::fmt::Debug;
use std::mem::size_of;
use thiserror::Error;
use uefi_specs::hyperv::advanced_logger::AdvancedLoggerEntryError;
use uefi_specs::hyperv::advanced_logger::AdvancedLoggerInfo;
use uefi_specs::hyperv::advanced_logger::AdvancedLoggerInfoError;
use uefi_specs::hyperv::advanced_logger::AdvancedLoggerMessageEntryV2;
use uefi_specs::hyperv::advanced_logger::MAX_LOG_BUFFER_SIZE;
use uefi_specs::hyperv::advanced_logger::MAX_MESSAGE_LENGTH;
use zerocopy::FromBytes;

// Constants for parsing
const ALIGNMENT: usize = 8;
const ALIGNMENT_MASK: usize = ALIGNMENT - 1;

// Every parsed advanced logger entry turns into this
pub struct EfiDiagnosticsLog {
    pub debug_level: u32,
    pub time_stamp: u64,
    pub phase: u16,
    pub message: String,
}

// For any errors that occur during processing
#[derive(Debug, Error)]
#[error("{0}")]
pub struct DiagnosticsError(pub String);

impl From<AdvancedLoggerInfoError> for DiagnosticsError {
    fn from(err: AdvancedLoggerInfoError) -> Self {
        DiagnosticsError(err.to_string())
    }
}

impl From<AdvancedLoggerEntryError> for DiagnosticsError {
    fn from(err: AdvancedLoggerEntryError) -> Self {
        DiagnosticsError(err.to_string())
    }
}

// Holds necessary state for diagnostics services
#[derive(Inspect)]
pub struct DiagnosticsServices {}

impl DiagnosticsServices {
    pub fn new() -> DiagnosticsServices {
        DiagnosticsServices {}
    }

    pub fn reset(&mut self) {
        // Does nothing
    }

    fn validate_gpa(&self, gpa: u32) -> Result<(), DiagnosticsError> {
        if gpa == 0 || gpa == u32::MAX {
            return Err(DiagnosticsError(format!("Invalid GPA: {:#x}", gpa)));
        }
        Ok(())
    }

    pub fn process_diagnostics(
        &self,
        gpa: u32,
        gm: GuestMemory,
        logs: &mut Vec<EfiDiagnosticsLog>,
    ) -> Result<(), DiagnosticsError> {
        //
        // Step 1: Validate GPA
        //
        self.validate_gpa(gpa)?;

        //
        // Step 2: Read and validate the advanced logger header
        //
        let header: AdvancedLoggerInfo = gm.read_plain(gpa as u64).map_err(|_| {
            DiagnosticsError(format!("Failed to read AdvancedLoggerInfo at {:#x}", gpa))
        })?;
        header.validate()?;

        //
        // Step 3: Prepare processing variables
        //

        // Force clear the logs
        logs.clear();

        // Used for summary statistics
        let mut bytes_read: usize = 0;
        let mut entries_processed: usize = 0;

        // Calculate used log buffer size
        let used_log_buffer_size = header
            .log_current_offset
            .checked_sub(header.log_buffer_offset)
            .ok_or_else(|| {
                DiagnosticsError(format!(
                    "Overflow: log_current_offset ({:#x}) - log_buffer_offset ({:#x})",
                    header.log_current_offset as u32, header.log_buffer_offset as u32
                ))
            })?;

        // Validate used log buffer size
        if used_log_buffer_size == 0
            || used_log_buffer_size > header.log_buffer_size
            || used_log_buffer_size > MAX_LOG_BUFFER_SIZE
        {
            return Err(DiagnosticsError(format!(
                "Invalid used_log_buffer_size: {:#x}",
                used_log_buffer_size
            )));
        }

        // Used for accumulating multiple messages
        let mut accumulated_message = String::new();
        let mut debug_level = 0;
        let mut time_stamp = 0;
        let mut phase = 0;
        let mut is_accumulating = false;

        //
        // Step 4: Read the used portions of the log buffer
        //

        // Calculate start address of the log buffer
        let buffer_start_addr = gpa.checked_add(header.log_buffer_offset).ok_or_else(|| {
            DiagnosticsError(format!(
                "Overflow: gpa ({:#x}) + log_buffer_offset ({:#x})",
                gpa, header.log_buffer_offset as u32
            ))
        })?;

        let mut buffer_data = vec![0u8; used_log_buffer_size as usize];
        gm.read_at(buffer_start_addr as u64, &mut buffer_data)
            .map_err(|_| {
                DiagnosticsError(format!(
                    "Failed to read buffer_data at {:#x} with size {:#x}",
                    buffer_start_addr, used_log_buffer_size
                ))
            })?;

        // Empty buffer data should early exit
        if buffer_data.is_empty() {
            tracelimit::info_ratelimited!("buffer_data is empty, ending processing");
            return Ok(());
        }

        //
        // Step 5: Parse the log buffer
        //
        let mut buffer_slice = &buffer_data[..];
        while !buffer_slice.is_empty() {
            // Parse and validate the entry header
            let (entry, _) =
                AdvancedLoggerMessageEntryV2::read_from_prefix(buffer_slice).map_err(|_| {
                    DiagnosticsError(format!(
                        "Failed to read AdvancedLoggerMessageEntryV2 from buffer_slice: {:?}",
                        buffer_slice
                    ))
                })?;
            entry.validate()?;

            //
            // Step 5a: Validate message boundaries
            //

            // Calculate message start and end offsets
            let message_start = entry.message_offset as usize;
            let message_end = message_start
                .checked_add(entry.message_len as usize)
                .ok_or_else(|| {
                    DiagnosticsError(format!(
                        "Overflow: message_start ({}) + message_length ({})",
                        message_start, entry.message_len as u16
                    ))
                })?;

            // Validate message end fits within the buffer slice
            if message_end > buffer_slice.len() {
                return Err(DiagnosticsError(format!(
                    "message_end exceeds buffer_slice: {} > {}",
                    message_end,
                    buffer_slice.len()
                )));
            }

            // Get the message
            let message = String::from_utf8_lossy(&buffer_slice[message_start..message_end]);

            //
            // Step 5b: Handle message accumulation
            //
            if !is_accumulating {
                debug_level = entry.debug_level;
                time_stamp = entry.time_stamp;
                phase = entry.phase;
                accumulated_message.clear();
                is_accumulating = true;
            }
            accumulated_message.push_str(&message);

            // Validate that the accumulated message is not too long
            if accumulated_message.len() > MAX_MESSAGE_LENGTH as usize {
                return Err(DiagnosticsError(format!(
                    "accumulated_message exceeds maximum length: {}. Max: {}",
                    accumulated_message.len(),
                    MAX_MESSAGE_LENGTH
                )));
            }

            // Print completed messages (ending with a newline) to the trace log
            if !message.is_empty() && message.ends_with('\n') {
                logs.push(EfiDiagnosticsLog {
                    debug_level,
                    time_stamp,
                    phase,
                    message: accumulated_message.clone(),
                });
                entries_processed += 1;
                is_accumulating = false;
            }

            //
            // Step 5c: Move to the next entry
            //

            // Calculate base offset (entry header size + message length)
            let base_offset = size_of::<AdvancedLoggerMessageEntryV2>()
                .checked_add(entry.message_len as usize)
                .ok_or_else(|| {
                    DiagnosticsError(format!(
                        "Overflow: AdvancedLoggerMessageEntryV2 size ({}) + message_len ({})",
                        size_of::<AdvancedLoggerMessageEntryV2>(),
                        entry.message_len as u16
                    ))
                })?;

            // Add padding for 8-byte alignment
            let aligned_offset = base_offset.checked_add(ALIGNMENT_MASK).ok_or_else(|| {
                DiagnosticsError(format!(
                    "Overflow: base_offset ({}) + {}",
                    base_offset, ALIGNMENT_MASK
                ))
            })?;
            let next_offset = aligned_offset & !ALIGNMENT_MASK;

            // Update overall bytes read counter
            bytes_read = bytes_read.checked_add(next_offset).ok_or_else(|| {
                DiagnosticsError(format!(
                    "Overflow: bytes_read ({}) + next_offset ({})",
                    bytes_read, next_offset
                ))
            })?;

            // Advanced to the next entry with boundary checks
            if next_offset >= buffer_slice.len() {
                // We have reached the end of the buffer
                break;
            }
            buffer_slice = &buffer_slice[next_offset..];
        }

        // Process remaining messages
        if is_accumulating {
            logs.push(EfiDiagnosticsLog {
                debug_level,
                time_stamp,
                phase,
                message: accumulated_message.clone(),
            });
            entries_processed += 1;
        }

        // Print summary statistics
        tracelimit::info_ratelimited!(
            "EFI Diagnostics: Processed {} entries, Read {} bytes",
            entries_processed,
            bytes_read
        );

        Ok(())
    }
}

impl UefiDevice {
    pub(crate) fn process_diagnostics(&mut self, gpa: u32, gm: GuestMemory) {
        // Do not process if already done
        if self.processed_diagnostics {
            tracelimit::info_ratelimited!("EFI Diagnostics already processed, skipping");
            return;
        }
        self.processed_diagnostics = true;

        // Collect diagnostics logs
        let mut logs = Vec::new();
        match self
            .service
            .diagnostics
            .process_diagnostics(gpa, gm, &mut logs)
        {
            Ok(_) => {
                // Print the logs to the trace log
                for log in logs.iter() {
                    tracing::info!(
                        "EFI Diagnostics: Debug Level: {}, Timestamp: {}, Phase: {}, Message: {}",
                        log.debug_level,
                        log.time_stamp,
                        log.phase,
                        log.message.trim_end_matches(&['\r', '\n'][..]) // Remove trailing newlines
                    );
                }
            }
            Err(error) => {
                tracelimit::error_ratelimited!(
                    "EFI Diagnostics: Encountered an error during processing {}",
                    error
                );
            }
        }

        // Reset stored gpa
        self.diagnostics_gpa = 0;
    }
}

mod save_restore {
    use super::*;
    use vmcore::save_restore::NoSavedState;
    use vmcore::save_restore::RestoreError;
    use vmcore::save_restore::SaveError;
    use vmcore::save_restore::SaveRestore;

    impl SaveRestore for DiagnosticsServices {
        type SavedState = NoSavedState;

        fn save(&mut self) -> Result<Self::SavedState, SaveError> {
            Ok(NoSavedState)
        }

        fn restore(&mut self, NoSavedState: Self::SavedState) -> Result<(), RestoreError> {
            Ok(())
        }
    }
}
