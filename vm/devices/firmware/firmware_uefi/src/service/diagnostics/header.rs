// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! This module provides `LogBufferHeader`, a trusted representation of an
//! Advanced Logger buffer header.

use crate::service::diagnostics::gpa::Gpa;
use guestmem::GuestMemory;
use guestmem::GuestMemoryError;
use thiserror::Error;
use uefi_specs::hyperv::advanced_logger::AdvancedLoggerInfo;
use uefi_specs::hyperv::advanced_logger::SIG_HEADER;

/// Maximum allowed size of the log buffer (4MB)
pub const MAX_LOG_BUFFER_SIZE: u32 = 0x400000;

/// Errors that occur when parsing header from untrusted guest memory
#[derive(Debug, Error)]
pub enum HeaderParseError {
    /// Expected header signature does not match
    #[error("Expected header signature: {0:#x}, got: {1:#x}")]
    SignatureMismatch(u32, u32),
    /// Log buffer size exceeds maximum allowed size
    #[error("Log buffer size {1:#x} exceeds maximum {0:#x}")]
    BufferSizeExceeded(u32, u32),
    /// Used buffer size is invalid (current < offset or exceeds max)
    #[error("Used buffer size {1:#x} is invalid (max: {0:#x})")]
    InvalidUsedSize(u32, u32),
    /// Arithmetic overflow occurred during calculation
    #[error("Arithmetic overflow in {0}")]
    Overflow(&'static str),
    /// No GPA has been set
    #[error("No GPA set")]
    NoGpa,
    /// Failed to read from guest memory
    #[error("Failed to read from guest memory: {0}")]
    GuestMemoryRead(#[from] GuestMemoryError),
}

/// Represents the header metadata for the Advanced Logger buffer.
#[derive(Debug, Clone)]
pub struct LogBufferHeader {
    /// Base GPA of the log buffer header
    base_gpa: Gpa,
    /// Offset from the header start to the beginning of the log buffer
    buffer_offset: u32,
    /// Size of data currently in the buffer
    used_size: u32,
}

impl LogBufferHeader {
    /// Parse and validate a log buffer header from guest memory at the given GPA.
    ///
    /// # Arguments
    /// * `gpa` - Optional guest physical address of the log buffer header
    /// * `gm` - Guest memory to read the header from
    ///
    /// # Returns
    /// The validated header on success, or a `HeaderParseError` on failure.
    pub fn from_guest_memory(gpa: Option<Gpa>, gm: &GuestMemory) -> Result<Self, HeaderParseError> {
        let gpa = gpa.ok_or(HeaderParseError::NoGpa)?;

        let raw_header: AdvancedLoggerInfo = gm.read_plain(gpa.as_u64())?;

        let expected_sig = u32::from_le_bytes(SIG_HEADER);
        if raw_header.signature != expected_sig {
            return Err(HeaderParseError::SignatureMismatch(
                expected_sig,
                raw_header.signature,
            ));
        }

        if raw_header.log_buffer_size > MAX_LOG_BUFFER_SIZE {
            return Err(HeaderParseError::BufferSizeExceeded(
                MAX_LOG_BUFFER_SIZE,
                raw_header.log_buffer_size,
            ));
        }

        let used_size = raw_header
            .log_current_offset
            .checked_sub(raw_header.log_buffer_offset)
            .ok_or_else(|| HeaderParseError::Overflow("used_size"))?;

        if used_size > raw_header.log_buffer_size || used_size > MAX_LOG_BUFFER_SIZE {
            return Err(HeaderParseError::InvalidUsedSize(
                MAX_LOG_BUFFER_SIZE,
                used_size,
            ));
        }

        Ok(Self {
            base_gpa: gpa,
            buffer_offset: raw_header.log_buffer_offset,
            used_size,
        })
    }

    /// Get the size of data currently in the buffer.
    pub fn used_size(&self) -> u32 {
        self.used_size
    }

    /// Check if the buffer is empty.
    pub fn is_empty(&self) -> bool {
        self.used_size == 0
    }

    /// Get the guest physical address where the log buffer starts.
    ///
    /// # Returns
    /// The GPA where the log buffer data begins, or a `HeaderParseError`
    /// if the calculation would overflow.
    pub fn buffer_start_gpa(&self) -> Result<Gpa, HeaderParseError> {
        let address = self
            .base_gpa
            .get()
            .checked_add(self.buffer_offset)
            .ok_or_else(|| HeaderParseError::Overflow("buffer_start_gpa"))?;

        Gpa::new(address).map_err(|_| HeaderParseError::Overflow("buffer_start_gpa"))
    }
}
