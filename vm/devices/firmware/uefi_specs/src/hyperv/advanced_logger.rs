// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Types and constants defined for EFI diagnostics.
//! Many of these types come from the UEFI Advanced Logger
//! Package from Project Mu.

use crate::uefi::time::EFI_TIME;
use thiserror::Error;
use zerocopy::FromBytes;
use zerocopy::Immutable;
use zerocopy::KnownLayout;

// Validation errors for the Advanced Logger Info structure
#[derive(Debug, Error)]
pub enum AdvancedLoggerInfoError {
    #[error("Invalid header signature: {0:#x}, expected: {1:#x}")]
    Signature(u32, u32),
    #[error("Invalid log buffer size: {0:#x}, max: {1:#x}")]
    LogBufferSize(u32, u32),
}

// Validation errors for the Advanced Logger Entry structure
#[derive(Debug, Error)]
pub enum AdvancedLoggerEntryError {
    #[error("Invalid entry signature: {0:#x}, expected: {1:#x}")]
    Signature(u32, u32),
    #[error("Invalid timestamp: {0:#x}")]
    Timestamp(u64),
    #[error("Invalid message length: {0:#x}, max: {1:#x}")]
    MessageLength(u16, u16),
}

// Advanced Logger Info validation constants
pub const MAX_LOG_BUFFER_SIZE: u32 = 0x400000; // 4MB
pub const SIG_HEADER: u32 = u32::from_le_bytes(*b"ALOG");

// Advanced Logger Entry validation constants
pub const MAX_MESSAGE_LENGTH: u16 = 0x1000; // 4KB
pub const SIG_ENTRY: u32 = u32::from_le_bytes(*b"ALM2");

// UEFI Advanced Logger Info Header, which is shared
// with the Advanced Logger Package in UEFI. The entries
// live right after.
#[repr(C, packed)]
#[derive(Debug, Copy, Clone, FromBytes, Immutable, KnownLayout)]
pub struct AdvancedLoggerInfo {
    pub signature: u32,         // Signature 'ALOG'
    pub version: u16,           // Current Version
    pub reserved: [u16; 3],     // Reserved for future
    pub log_buffer_offset: u32, // Offset from LoggerInfo to start of log
    pub reserved4: u32,
    pub log_current_offset: u32, // Offset from LoggerInfo to where to store next log entry
    pub discarded_size: u32,     // Number of bytes of messages missed
    pub log_buffer_size: u32,    // Size of allocated buffer
    pub in_permanent_ram: u8,    // Log in permanent RAM
    pub at_runtime: u8,          // After ExitBootServices
    pub gone_virtual: u8,        // After VirtualAddressChange
    pub hdw_port_initialized: u8, // HdwPort initialized
    pub hdw_port_disabled: u8,   // HdwPort is Disabled
    pub reserved2: [u8; 3],      // Reserved
    pub timer_frequency: u64,    // Ticks per second for log timing
    pub ticks_at_time: u64,      // Ticks when Time Acquired
    pub time: EFI_TIME,          // Uefi Time Field
    pub hw_print_level: u32,     // Logging level to be printed at hw port
    pub reserved3: u32,          // Reserved
}

// UEFI Advanced Logger Entry, which is shared with the
// Advanced Logger Package in UEFI. The messages live
// right after.
#[repr(C, packed)]
#[derive(Debug, Copy, Clone, FromBytes, Immutable, KnownLayout)]
pub struct AdvancedLoggerMessageEntryV2 {
    pub signature: u32,    // Signature
    pub major_version: u8, // Major version of advanced logger message structure
    pub minor_version: u8, // Minor version of advanced logger message structure
    pub debug_level: u32,  // Debug Level
    pub time_stamp: u64,   // Time stamp
    pub phase: u16,        // Boot phase that produced this message entry
    pub message_len: u16,  // Number of bytes in Message
    pub message_offset: u16, // Offset of Message from start of structure
                           // Rust does not support flexible arrays, but message lives after
}

impl AdvancedLoggerInfo {
    pub fn validate(&self) -> Result<(), AdvancedLoggerInfoError> {
        // Check the signature
        let signature = self.signature.to_le();
        if signature != SIG_HEADER {
            return Err(AdvancedLoggerInfoError::Signature(signature, SIG_HEADER));
        }

        // Check the log buffer size
        if self.log_buffer_size > MAX_LOG_BUFFER_SIZE {
            return Err(AdvancedLoggerInfoError::LogBufferSize(
                self.log_buffer_size,
                MAX_LOG_BUFFER_SIZE,
            ));
        }
        Ok(())
    }
}

impl AdvancedLoggerMessageEntryV2 {
    pub fn validate(&self) -> Result<(), AdvancedLoggerEntryError> {
        // Check the signature
        let signature = self.signature.to_le();
        if signature != SIG_ENTRY {
            return Err(AdvancedLoggerEntryError::Signature(signature, SIG_ENTRY));
        }

        // Check the timestamp
        if self.time_stamp == 0 {
            return Err(AdvancedLoggerEntryError::Timestamp(self.time_stamp));
        }

        // Check the message length
        if self.message_len > MAX_MESSAGE_LENGTH {
            return Err(AdvancedLoggerEntryError::MessageLength(
                self.message_len,
                MAX_MESSAGE_LENGTH,
            ));
        }
        Ok(())
    }
}
