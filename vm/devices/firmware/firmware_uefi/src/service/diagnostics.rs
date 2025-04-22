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
//!  - Add more doc comments
//!  - Add unit tests
use crate::UefiDevice;
use guestmem::GuestMemory;
use guestmem::GuestMemoryError;
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
// Validation error types for the advanced logger spec types
//
#[derive(Debug, Error)]
pub enum AdvancedLoggerInfoError {
    #[error("Invalid header signature: {0:#x}, expected: {1:#x}")]
    Signature(u32, u32),
    #[error("Invalid log buffer size: {0:#x}, max: {1:#x}")]
    LogBufferSize(u32, u32),
}

#[derive(Debug, Error)]
pub enum AdvancedLoggerMessageEntryV2Error {
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
    type Error = AdvancedLoggerMessageEntryV2Error;

    fn validate(&self) -> Result<(), Self::Error> {
        let signature = self.signature.to_le();
        if signature != SIG_ENTRY {
            return Err(AdvancedLoggerMessageEntryV2Error::Signature(
                signature, SIG_ENTRY,
            ));
        }

        if self.time_stamp == 0 {
            return Err(AdvancedLoggerMessageEntryV2Error::Timestamp(
                self.time_stamp,
            ));
        }

        if self.message_len > MAX_MESSAGE_LENGTH {
            return Err(AdvancedLoggerMessageEntryV2Error::MessageLength(
                self.message_len,
                MAX_MESSAGE_LENGTH,
            ));
        }
        Ok(())
    }
}

/// Errors that occur during processing
/// TODO: Add more specific error types
#[derive(Debug, Error)]
pub enum DiagnosticsError {
    #[error("Bad GPA value: {0:#x}")]
    BadGpa(u32),

    #[error("Failed to read from guest memory: {0}")]
    GuestMemoryRead(#[from] GuestMemoryError),

    #[error("Invalid UTF-8 in message: {0}")]
    Utf8Error(#[from] std::str::Utf8Error),

    #[error("Encountered arithmetic overflow: {0}")]
    Overflow(String),

    #[error("Failed to validate AdvancedLoggerInfo: {0}")]
    HeaderValidation(#[from] AdvancedLoggerInfoError),

    #[error("Failed to validate AdvancedLoggerMessageEntryV2: {0}")]
    EntryValidation(#[from] AdvancedLoggerMessageEntryV2Error),

    #[error("Failed to read buffer data: {0}")]
    BoundsError(#[from] std::io::Error),

    #[error("General error: {0}")]
    GeneralError(String),
}

/// Definition of the diagnostics services state
#[derive(Inspect)]
pub struct DiagnosticsServices {
    gpa: Option<u32>,   // The guest physical address of the diagnostics buffer
    ebs_complete: bool, // Flag indicating if ExitBootServices has been reached
}

impl DiagnosticsServices {
    /// Create a new instance of the diagnostics services
    pub fn new() -> DiagnosticsServices {
        DiagnosticsServices {
            gpa: None,
            ebs_complete: false,
        }
    }

    /// Reset the diagnostics services state
    pub fn reset(&mut self) {
        self.gpa = None;
        self.ebs_complete = false;
    }

    /// Set the GPA of the diagnostics buffer
    pub fn set_gpa(&mut self, gpa: u32) {
        self.gpa = match gpa {
            0 => None,
            _ => Some(gpa),
        }
    }

    /// Mark the Exit Boot Services (EBS) event complete
    pub fn set_ebs_complete(&mut self) {
        self.ebs_complete = true;
    }

    /// Process the diagnostics buffer into friendly logs
    pub fn process_diagnostics(
        &self,
        gm: GuestMemory,
        logs: &mut Vec<EfiDiagnosticsLog>,
    ) -> Result<(), DiagnosticsError> {
        // Do not proceed if we have encountered ExitBootServices
        if self.ebs_complete {
            tracelimit::warn_ratelimited!("Diagnostics: EBS complete, skipping processing");
            return Ok(());
        }

        //
        // Step 1: Validate GPA
        //
        let gpa = match self.gpa {
            Some(gpa) if gpa != 0 && gpa != u32::MAX => gpa,
            Some(invalid_gpa) => return Err(DiagnosticsError::BadGpa(invalid_gpa)),
            None => {
                return Err(DiagnosticsError::GeneralError(
                    "No diagnostics GPA set".to_string(),
                ));
            }
        };

        //
        // Step 2: Read and validate the advanced logger header
        //
        let header: AdvancedLoggerInfo = gm.read_plain(gpa as u64)?;
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
                DiagnosticsError::Overflow(format!(
                    "log_current_offset ({:#x}) - log_buffer_offset ({:#x})",
                    log_current_offset, log_buffer_offset
                ))
            })?;

        // Validate used log buffer size
        if used_log_buffer_size == 0
            || used_log_buffer_size > header.log_buffer_size
            || used_log_buffer_size > MAX_LOG_BUFFER_SIZE
        {
            return Err(DiagnosticsError::GeneralError(format!(
                "Invalid used log buffer size: {:#x}, max: {:#x}",
                used_log_buffer_size, MAX_LOG_BUFFER_SIZE
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
            DiagnosticsError::Overflow(format!(
                "gpa ({:#x}) + log_buffer_offset ({:#x})",
                gpa, log_buffer_offset
            ))
        })?;

        let mut buffer_data = vec![0u8; used_log_buffer_size as usize];
        gm.read_at(buffer_start_addr as u64, &mut buffer_data)?;

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
                AdvancedLoggerMessageEntryV2::read_from_prefix(buffer_slice).map_err(|error| {
                    DiagnosticsError::GeneralError(format!(
                        "Failed to parse entry from buffer_slice: {}",
                        error
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
                    DiagnosticsError::Overflow(format!(
                        "message_start ({}) + message_len ({})",
                        message_start, message_len
                    ))
                })?;

            // Validate message end fits within the buffer slice
            if message_end > buffer_slice.len() {
                return Err(DiagnosticsError::GeneralError(format!(
                    "message_end ({}) exceeds buffer slice length ({})",
                    message_end,
                    buffer_slice.len()
                )));
            }

            // Get the message
            let message = std::str::from_utf8(&buffer_slice[message_start..message_end])?;

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
            accumulated_message.push_str(message);

            // Validate that the accumulated message is not too long
            if accumulated_message.len() > MAX_MESSAGE_LENGTH as usize {
                return Err(DiagnosticsError::GeneralError(format!(
                    "Accumulated message length ({}) exceeds max length ({})",
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
                    DiagnosticsError::Overflow(format!(
                        "size_of::<AdvancedLoggerMessageEntryV2> ({}) + message_len ({})",
                        size_of::<AdvancedLoggerMessageEntryV2>(),
                        message_len
                    ))
                })?;

            // Add padding for 8-byte alignment
            let aligned_offset = base_offset.checked_add(ALIGNMENT_MASK).ok_or_else(|| {
                DiagnosticsError::Overflow(format!(
                    "base_offset ({}) + ALIGNMENT_MASK ({})",
                    base_offset, ALIGNMENT_MASK
                ))
            })?;
            let next_offset = aligned_offset & !ALIGNMENT_MASK;

            // Update overall bytes read counter
            bytes_read = bytes_read.checked_add(next_offset).ok_or_else(|| {
                DiagnosticsError::Overflow(format!(
                    "bytes_read ({}) + next_offset ({})",
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
    /// Process the diagnostics buffer and log the entries to tracing
    pub(crate) fn process_diagnostics(&mut self, gm: GuestMemory) {
        // Collect diagnostics logs and send to tracing
        let mut logs = Vec::new();
        match self.service.diagnostics.process_diagnostics(gm, &mut logs) {
            Ok(_) => {
                for log in logs.iter() {
                    tracing::info!(
                        debug_level = log.debug_level,
                        timestamp = log.time_stamp,
                        phase = log.phase,
                        description = %log.message,
                        "EFI Diagnostics:"
                    );
                }
            }
            Err(error) => {
                tracelimit::error_ratelimited!(
                    error = &error as &dyn std::error::Error,
                    "EFI Diagnostics: Failed to process diagnostics buffer"
                );
            }
        }
    }
}

mod save_restore {
    use super::*;
    use vmcore::save_restore::RestoreError;
    use vmcore::save_restore::SaveError;
    use vmcore::save_restore::SaveRestore;

    mod state {
        use mesh::payload::Protobuf;
        use vmcore::save_restore::SavedStateRoot;

        #[derive(Protobuf, SavedStateRoot)]
        #[mesh(package = "firmware.uefi.diagnostics")]
        pub struct SavedState {
            #[mesh(1)]
            pub gpa: Option<u32>,
            #[mesh(2)]
            pub ebs_complete: bool,
        }
    }

    impl SaveRestore for DiagnosticsServices {
        type SavedState = state::SavedState;

        fn save(&mut self) -> Result<Self::SavedState, SaveError> {
            Ok(state::SavedState {
                gpa: self.gpa,
                ebs_complete: self.ebs_complete,
            })
        }

        fn restore(&mut self, state: Self::SavedState) -> Result<(), RestoreError> {
            let state::SavedState { gpa, ebs_complete } = state;
            self.gpa = gpa;
            self.ebs_complete = ebs_complete;
            Ok(())
        }
    }
}
