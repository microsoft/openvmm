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
use thiserror::Error;
use uefi_specs::hyperv::efi_diagnostics::*;
use zerocopy::FromBytes;

#[derive(Debug, Error)]
pub enum DiagnosticsError {
    #[error("invalid diagnostics gpa")]
    InvalidGpa,
    #[error("invalid header signature")]
    InvalidHeaderSignature,
    #[error("invalid header log buffer size")]
    InvalidHeaderLogBufferSize,
    #[error("invalid entry signature")]
    InvalidEntrySignature,
    #[error("invalid entry timestamp")]
    InvalidEntryTimestamp,
    #[error("invalid entry message length")]
    InvalidEntryMessageLength,
    #[error("unable to read header from guest memory")]
    HeaderParseError,
    #[error("unable to read diagnostics buffer from guest memory")]
    BufferParseError,
}

#[derive(Inspect)]
pub struct DiagnosticsServices {
    gpa: u32,
}

impl DiagnosticsServices {
    pub fn new() -> DiagnosticsServices {
        DiagnosticsServices { gpa: 0 }
    }

    pub fn reset(&mut self) {
        self.gpa = 0
    }

    fn validate_gpa(&self, gpa: u32) -> Result<(), DiagnosticsError> {
        if gpa == 0 || gpa == u32::MAX {
            tracelimit::error_ratelimited!("Invalid GPA: {:#x}", gpa);
            return Err(DiagnosticsError::InvalidGpa);
        }
        Ok(())
    }

    fn validate_header(&self, header: &AdvancedLoggerInfo) -> Result<(), DiagnosticsError> {
        if header.signature != SIG_HEADER {
            return Err(DiagnosticsError::InvalidHeaderSignature);
        }

        if header.log_buffer_size == 0 || header.log_buffer_size > MAX_LOG_BUFFER_SIZE {
            return Err(DiagnosticsError::InvalidHeaderLogBufferSize);
        }

        Ok(())
    }

    fn validate_entry(&self, entry: &AdvancedLoggerMessageEntryV2) -> Result<(), DiagnosticsError> {
        if entry.signature != SIG_ENTRY {
            return Err(DiagnosticsError::InvalidEntrySignature);
        }

        if entry.time_stamp == 0 {
            return Err(DiagnosticsError::InvalidEntryTimestamp);
        }

        if entry.message_len == 0 || entry.message_len > MAX_MESSAGE_LENGTH as u16 {
            return Err(DiagnosticsError::InvalidEntryMessageLength);
        }

        Ok(())
    }

    pub fn set_diagnostics_gpa(&mut self, gpa: u32) -> Result<(), DiagnosticsError> {
        tracelimit::info_ratelimited!("Setting diagnostics GPA to {:#x}", gpa);
        self.validate_gpa(gpa)?;
        self.gpa = gpa;
        Ok(())
    }

    pub fn process_diagnostics(&self, gm: GuestMemory) -> Result<(), DiagnosticsError> {
        //
        // Step 1: Validate GPA
        //
        self.validate_gpa(self.gpa)?;

        //
        // Step 2: Read and validate the advanced logger header
        //
        let header: AdvancedLoggerInfo = gm
            .read_plain(self.gpa as u64)
            .map_err(|_| DiagnosticsError::HeaderParseError)?;
        self.validate_header(&header)?;
        let log_buffer_offset = header.log_buffer_offset;
        let log_current_offset = header.log_current_offset;
        tracelimit::info_ratelimited!(
            "Buffer offset: {:#x}, Log Current offset: {:#x}",
            log_buffer_offset,
            log_current_offset
        );

        //
        // Step 3: Prepare processing variables
        //

        // Used for summary statistics
        let mut bytes_read: usize = 0;
        let mut entries_processed: usize = 0;

        // Calculate used log buffer size
        let used_log_buffer_size = header
            .log_current_offset
            .checked_sub(header.log_buffer_offset)
            .ok_or_else(|| DiagnosticsError::InvalidHeaderLogBufferSize)?;

        // Validate used log buffer size
        if used_log_buffer_size == 0
            || used_log_buffer_size > header.log_buffer_size
            || used_log_buffer_size > MAX_LOG_BUFFER_SIZE
        {
            tracelimit::error_ratelimited!(
                "Invalid used log buffer size: {:#x}",
                used_log_buffer_size
            );
            return Err(DiagnosticsError::InvalidHeaderLogBufferSize);
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

        // Calcualte start address of the log buffer
        let buffer_start_addr = self
            .gpa
            .checked_add(header.log_buffer_offset)
            .ok_or_else(|| DiagnosticsError::InvalidGpa)?;

        let mut buffer_data = vec![0u8; used_log_buffer_size as usize];
        gm.read_at(buffer_start_addr as u64, &mut buffer_data)
            .map_err(|_| DiagnosticsError::BufferParseError)?;

        // Empty buffer data should early exit
        if buffer_data.is_empty() {
            tracelimit::info_ratelimited!("Diagnostics buffer is empty");
            return Ok(());
        }

        //
        // Step 5: Parse the log buffer
        //
        let mut buffer_slice = &buffer_data[..];
        while !buffer_slice.is_empty() {
            // Parse and validate the entry header
            let (entry, _) = AdvancedLoggerMessageEntryV2::read_from_prefix(buffer_slice)
                .map_err(|_| DiagnosticsError::BufferParseError)?;
            self.validate_entry(&entry)?;

            // Validate message bounds
            // TODO: Double check if the types are appropriate here...
            let message_start = entry.message_offset as usize;
            let message_end = (entry.message_offset as usize)
                .checked_add(entry.message_len as usize)
                .ok_or_else(|| DiagnosticsError::InvalidEntryMessageLength)?;
            if message_end as usize > buffer_slice.len() {
                tracelimit::error_ratelimited!(
                    "Message end exceeds buffer size: {} > {}",
                    message_end,
                    buffer_slice.len()
                );
                return Err(DiagnosticsError::InvalidEntryMessageLength);
            }

            // Get the message
            let message = String::from_utf8_lossy(&buffer_slice[message_start..message_end]);

            //
            // Step 5a: Handle message accumulation
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
                tracelimit::error_ratelimited!(
                    "Accumulated message length exceeds maximum: {}",
                    accumulated_message.len()
                );
                return Err(DiagnosticsError::InvalidEntryMessageLength);
            }

            // Print completed messages (ending with a newline) to the trace log
            if !message.is_empty() && message.ends_with('\n') {
                tracelimit::info_ratelimited!(
                    "EFI Diagnostics: Debug Level: {:#x}, Time Stamp: {:#x}, Phase: {:#x}, Message: {}",
                    debug_level,
                    time_stamp,
                    phase,
                    accumulated_message
                );
                entries_processed += 1;
                is_accumulating = false;
            }

            //
            // Step 5b: Move to the next entry
            //

            // Calculate base offset (entry header size + message length)
            let base_offset = size_of::<AdvancedLoggerMessageEntryV2>()
                .checked_add(entry.message_len as usize)
                .ok_or_else(|| DiagnosticsError::InvalidEntryMessageLength)?;

            // Add padding for 8-byte alignment
            let aligned_offset = base_offset
                .checked_add(7)
                .ok_or_else(|| DiagnosticsError::InvalidEntryMessageLength)?;
            let next_offset = aligned_offset & !7;

            // Update overall bytes read counter
            bytes_read = bytes_read
                .checked_add(next_offset)
                .ok_or_else(|| DiagnosticsError::InvalidEntryMessageLength)?;

            // Advanced to the next entry with boundary checks
            if next_offset >= buffer_slice.len() {
                // We have reached the end of the buffer
                break;
            }
            buffer_slice = &buffer_slice[next_offset..];
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
    pub(crate) fn set_diagnostics_gpa(&mut self, gpa: u32) {
        let _ = self.service.diagnostics.set_diagnostics_gpa(gpa);
    }

    pub(crate) fn process_diagnostics(&self, gm: GuestMemory) {
        let _ = self.service.diagnostics.process_diagnostics(gm);
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
