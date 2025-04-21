// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! UEFI diagnostics service
//!
//! This service handles processing of the EFI diagnostics buffer,
//! producing friendly logs for any telemetry during the UEFI boot
//! process.
//!
//! The EFI diagnostics buffer follows the specification of Project Mu's
//! Advanced Logger package, whose relevant types are defined in the Hyper-V
//! specification within the uefi_specs crate.
//!
//! TODO:
//!  - Document functions
//!  - Add unit tests
//!  - Change ProcessingError from struct to enum to be more granular
//!    on the processing errors that occur?
use crate::UefiDevice;
use guestmem::GuestMemory;
use inspect::Inspect;
use std::fmt::Debug;
use std::mem::size_of;
use thiserror::Error;
use uefi_specs::hyperv::advanced_logger::AdvancedLoggerInfo;
use uefi_specs::hyperv::advanced_logger::AdvancedLoggerMessageEntryV2;
use uefi_specs::hyperv::advanced_logger::SIG_ENTRY;
use uefi_specs::hyperv::advanced_logger::SIG_HEADER;
use zerocopy::FromBytes;

//
// Constants for parsing
//
const ALIGNMENT: usize = 8;
const ALIGNMENT_MASK: usize = ALIGNMENT - 1;
pub const MAX_LOG_BUFFER_SIZE: u32 = 0x400000; // 4MB
pub const MAX_MESSAGE_LENGTH: u16 = 0x1000; // 4KB

//
// Represents a processed log entry from the EFI diagnostics buffer
//
#[derive(Debug, Clone)]
pub struct EfiDiagnosticsLog {
    pub debug_level: u32, // The debug level of the log entry
    pub time_stamp: u64,  // Timestamp of when the log entry was created
    pub phase: u16,       // The boot phase that produced this log entry
    pub message: String,  // The log message itself
}

//
// Error types for the advanced logger spec types
//
#[derive(Debug, Error)]
pub enum AdvancedLoggerInfoError {
    #[error("Invalid header signature: {0:#x}, expected: {1:#x}")]
    Signature(u32, u32),
    #[error("Invalid log buffer size: {0:#x}, max: {1:#x}")]
    LogBufferSize(u32, u32),
}

#[derive(Debug, Error)]
pub enum AdvancedLoggerEntryError {
    #[error("Invalid entry signature: {0:#x}, expected: {1:#x}")]
    Signature(u32, u32),
    #[error("Invalid timestamp: {0:#x}")]
    Timestamp(u64),
    #[error("Invalid message length: {0:#x}, max: {1:#x}")]
    MessageLength(u16, u16),
}

//
// Validation extension trait for the advanced logger spec types
//
pub trait Validateable {
    type Error;
    fn validate(&self) -> Result<(), Self::Error>;
}

impl Validateable for AdvancedLoggerInfo {
    type Error = AdvancedLoggerInfoError;

    fn validate(&self) -> Result<(), Self::Error> {
        let signature = self.signature.to_le();
        if signature != SIG_HEADER {
            return Err(AdvancedLoggerInfoError::Signature(signature, SIG_HEADER));
        }

        if self.log_buffer_size > MAX_LOG_BUFFER_SIZE {
            return Err(AdvancedLoggerInfoError::LogBufferSize(
                self.log_buffer_size,
                MAX_LOG_BUFFER_SIZE,
            ));
        }
        Ok(())
    }
}

impl Validateable for AdvancedLoggerMessageEntryV2 {
    type Error = AdvancedLoggerEntryError;

    fn validate(&self) -> Result<(), Self::Error> {
        let signature = self.signature.to_le();
        if signature != SIG_ENTRY {
            return Err(AdvancedLoggerEntryError::Signature(signature, SIG_ENTRY));
        }

        if self.time_stamp == 0 {
            return Err(AdvancedLoggerEntryError::Timestamp(self.time_stamp));
        }

        if self.message_len > MAX_MESSAGE_LENGTH {
            return Err(AdvancedLoggerEntryError::MessageLength(
                self.message_len,
                MAX_MESSAGE_LENGTH,
            ));
        }
        Ok(())
    }
}

//
// Generic error raised when processing the
// efi diagnostics buffer
//
#[derive(Debug, Error)]
#[error("{0}")]
pub struct ProcessingError(pub String);

impl From<AdvancedLoggerInfoError> for ProcessingError {
    fn from(err: AdvancedLoggerInfoError) -> Self {
        ProcessingError(err.to_string())
    }
}

impl From<AdvancedLoggerEntryError> for ProcessingError {
    fn from(err: AdvancedLoggerEntryError) -> Self {
        ProcessingError(err.to_string())
    }
}

//
// Define the state and functinality of this service
//
#[derive(Inspect)]
pub struct DiagnosticsServices {}

impl DiagnosticsServices {
    pub fn new() -> DiagnosticsServices {
        DiagnosticsServices {}
    }

    pub fn reset(&mut self) {
        // Does nothing
    }

    fn validate_gpa(&self, gpa: u32) -> Result<(), ProcessingError> {
        if gpa == 0 || gpa == u32::MAX {
            return Err(ProcessingError(format!("Invalid GPA: {:#x}", gpa)));
        }
        Ok(())
    }

    pub fn process_diagnostics(
        &self,
        gpa: u32,
        gm: GuestMemory,
        logs: &mut Vec<EfiDiagnosticsLog>,
    ) -> Result<(), ProcessingError> {
        //
        // Step 1: Validate GPA
        //
        self.validate_gpa(gpa)?;

        //
        // Step 2: Read and validate the advanced logger header
        //
        let header: AdvancedLoggerInfo = gm.read_plain(gpa as u64).map_err(|_| {
            ProcessingError(format!("Failed to read AdvancedLoggerInfo at {:#x}", gpa))
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

        // Copy packed fields to local variables to avoid unaligned access
        let log_current_offset = header.log_current_offset;
        let log_buffer_offset = header.log_buffer_offset;

        // Calculate used log buffer size using the local variables
        let used_log_buffer_size = log_current_offset
            .checked_sub(log_buffer_offset)
            .ok_or_else(|| {
                ProcessingError(format!(
                    "Overflow: log_current_offset ({:#x}) - log_buffer_offset ({:#x})",
                    log_current_offset, log_buffer_offset
                ))
            })?;

        // Validate used log buffer size
        if used_log_buffer_size == 0
            || used_log_buffer_size > header.log_buffer_size
            || used_log_buffer_size > MAX_LOG_BUFFER_SIZE
        {
            return Err(ProcessingError(format!(
                "Invalid used_log_buffer_size: {:#x}",
                used_log_buffer_size
            )));
        }

        // Used for accumulating multiple messages
        let mut accumulated_message = String::with_capacity(MAX_MESSAGE_LENGTH as usize);
        let mut debug_level = 0;
        let mut time_stamp = 0;
        let mut phase = 0;
        let mut is_accumulating = false;

        //
        // Step 4: Read the used portions of the log buffer
        //

        // Calculate start address of the log buffer
        let buffer_start_addr = gpa.checked_add(log_buffer_offset).ok_or_else(|| {
            ProcessingError(format!(
                "Overflow: gpa ({:#x}) + log_buffer_offset ({:#x})",
                gpa, log_buffer_offset
            ))
        })?;

        let mut buffer_data = vec![0u8; used_log_buffer_size as usize];
        gm.read_at(buffer_start_addr as u64, &mut buffer_data)
            .map_err(|_| {
                ProcessingError(format!(
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
                    ProcessingError(format!(
                        "Failed to read AdvancedLoggerMessageEntryV2 from buffer_slice: {:?}",
                        buffer_slice
                    ))
                })?;
            entry.validate()?;

            //
            // Step 5a: Validate message boundaries
            //

            // Copy packed fields to local variables to avoid unaligned access
            let message_offset = entry.message_offset;
            let message_len = entry.message_len;

            // Calculate message start and end offsets
            let message_start = message_offset as usize;
            let message_end = message_start
                .checked_add(message_len as usize)
                .ok_or_else(|| {
                    ProcessingError(format!(
                        "Overflow: message_start ({}) + message_length ({})",
                        message_start, message_len
                    ))
                })?;

            // Validate message end fits within the buffer slice
            if message_end > buffer_slice.len() {
                return Err(ProcessingError(format!(
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
                return Err(ProcessingError(format!(
                    "accumulated_message exceeds maximum length: {}. Max: {}",
                    accumulated_message.len(),
                    MAX_MESSAGE_LENGTH
                )));
            }

            // Completed messages (ending with '\n') become log entries
            if !message.is_empty() && message.ends_with('\n') {
                logs.push(EfiDiagnosticsLog {
                    debug_level,
                    time_stamp,
                    phase,
                    message: std::mem::take(&mut accumulated_message)
                        .trim_end_matches(&['\r', '\n'][..])
                        .to_string(),
                });
                entries_processed += 1;
                is_accumulating = false;
            }

            //
            // Step 5c: Move to the next entry
            //

            // Calculate base offset (entry header size + message length)
            let base_offset = size_of::<AdvancedLoggerMessageEntryV2>()
                .checked_add(message_len as usize)
                .ok_or_else(|| {
                    ProcessingError(format!(
                        "Overflow: AdvancedLoggerMessageEntryV2 size ({}) + message_len ({})",
                        size_of::<AdvancedLoggerMessageEntryV2>(),
                        message_len
                    ))
                })?;

            // Add padding for 8-byte alignment
            let aligned_offset = base_offset.checked_add(ALIGNMENT_MASK).ok_or_else(|| {
                ProcessingError(format!(
                    "Overflow: base_offset ({}) + {}",
                    base_offset, ALIGNMENT_MASK
                ))
            })?;
            let next_offset = aligned_offset & !ALIGNMENT_MASK;

            // Update overall bytes read counter
            bytes_read = bytes_read.checked_add(next_offset).ok_or_else(|| {
                ProcessingError(format!(
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
                message: std::mem::take(&mut accumulated_message)
                    .trim_end_matches(&['\r', '\n'][..])
                    .to_string(),
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
                        log.message
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
