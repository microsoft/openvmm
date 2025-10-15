// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Core processing logic for EFI diagnostics buffer

use crate::service::diagnostics::formatting::EfiDiagnosticsLog;
use crate::service::diagnostics::message_accumulator::AccumulationError;
use crate::service::diagnostics::message_accumulator::MessageAccumulator;
use crate::service::diagnostics::parser::EntryParseError;
use crate::service::diagnostics::parser::parse_entry;
use guestmem::GuestMemory;
use guestmem::GuestMemoryError;
use inspect::Inspect;
use mesh::payload::Protobuf;
use mesh::payload::oneof::DescribeOneof;
use mesh::payload::protofile::FieldDescriptor;
use mesh::payload::protofile::FieldType;
use mesh::payload::protofile::MessageDescription;
use mesh::payload::protofile::MessageDescriptor;
use mesh::payload::protofile::OneofDescriptor;
use mesh::payload::protofile::TopLevelDescriptor;
use thiserror::Error;
use uefi_specs::hyperv::advanced_logger::AdvancedLoggerInfo;
use uefi_specs::hyperv::advanced_logger::SIG_HEADER;
use uefi_specs::hyperv::debug_level::DEBUG_ERROR;
use uefi_specs::hyperv::debug_level::DEBUG_INFO;
use uefi_specs::hyperv::debug_level::DEBUG_WARN;

/// Maximum allowed size of the log buffer
pub const MAX_LOG_BUFFER_SIZE: u32 = 0x400000; // 4MB

/// Log level configurations with associated filter masks
#[derive(Inspect, Debug, Clone, Copy, PartialEq, Eq, Protobuf)]
#[inspect(external_tag)]
pub enum LogLevel {
    /// ERROR and WARN
    #[mesh(1)]
    Default(u32),
    /// ERROR, WARN and INFO  
    #[mesh(2)]
    Info(u32),
    /// All levels
    #[mesh(3)]
    Full,
}

impl LogLevel {
    /// Create default log level configuration
    pub const fn default() -> Self {
        LogLevel::Default(DEBUG_ERROR | DEBUG_WARN)
    }

    /// Create info log level configuration
    pub const fn _info() -> Self {
        LogLevel::Info(DEBUG_ERROR | DEBUG_WARN | DEBUG_INFO)
    }

    /// Create full log level configuration
    pub const fn _full() -> Self {
        LogLevel::Full
    }

    /// Checks if a raw debug level should be logged based
    /// on this log level configuration
    pub fn should_log(&self, raw_debug_level: u32) -> bool {
        match self {
            LogLevel::Default(mask) | LogLevel::Info(mask) => (raw_debug_level & mask) != 0,
            LogLevel::Full => true,
        }
    }
}

impl DescribeOneof for LogLevel {
    const DESCRIPTION: MessageDescription<'static> = {
        const ONEOF_DESCRIPTOR: OneofDescriptor<'static> = OneofDescriptor::new(
            "LogLevel",
            &[
                FieldDescriptor::new("", FieldType::builtin("uint32"), "default", 1),
                FieldDescriptor::new("", FieldType::builtin("uint32"), "info", 2),
                FieldDescriptor::new("", FieldType::builtin("bool"), "full", 3),
            ],
        );
        const TLD: TopLevelDescriptor<'static> = TopLevelDescriptor::message(
            "firmware.uefi.diagnostics",
            &MessageDescriptor::new("LogLevel", "", &[], &[ONEOF_DESCRIPTOR], &[]),
        );
        MessageDescription::Internal(&TLD)
    };
}

/// Errors that occur during processing
#[derive(Debug, Error)]
pub enum ProcessingError {
    /// Failed to parse a log entry from the buffer
    #[error("Failed to parse entry: {0}")]
    EntryParse(#[from] EntryParseError),
    /// Failed during message accumulation process
    #[error("Failed during message accumulation: {0}")]
    MessageAccumulation(#[from] AccumulationError),
    /// Log buffer header signature does not match expected value
    #[error("Expected: {0:#x}, got: {1:#x}")]
    HeaderSignatureMismatch(u32, u32),
    /// Log buffer size exceeds maximum allowed size
    #[error("Expected log buffer size < {0:#x}, got: {1:#x}")]
    HeaderBufferSize(u32, u32),
    /// Invalid guest physical address provided
    #[error("Bad GPA value: {0:#x}")]
    BadGpa(u32),
    /// No guest physical address has been set
    #[error("No GPA set")]
    NoGpa,
    /// Failed to read data from guest memory
    #[error("Failed to read from guest memory: {0}")]
    GuestMemoryRead(#[from] GuestMemoryError),
    /// Arithmetic overflow occurred during calculation
    #[error("Arithmetic overflow in {0}")]
    Overflow(&'static str),
    /// Used log buffer size is invalid
    #[error("Expected used log buffer size < {0:#x}, got: {1:#x}")]
    BadUsedBufferSize(u32, u32),
}

/// Processes diagnostics from guest memory (internal implementation)
///
/// # Arguments
/// * `gpa` - Mutable reference to the GPA option
/// * `has_processed_before` - Mutable reference to the processing flag
/// * `allow_reprocess` - If true, allows processing even if already processed for guest
/// * `gm` - Guest memory to read diagnostics from
/// * `log_handler` - Function to handle each parsed log entry
pub fn process_diagnostics_internal<F>(
    gpa: &mut Option<u32>,
    has_processed_before: &mut bool,
    allow_reprocess: bool,
    gm: &GuestMemory,
    log_level: LogLevel,
    log_handler: F,
) -> Result<(), ProcessingError>
where
    F: FnMut(EfiDiagnosticsLog<'_>, u32),
{
    // Prevents the guest from spamming diagnostics processing
    if !allow_reprocess {
        if *has_processed_before {
            tracelimit::warn_ratelimited!("Already processed diagnostics, skipping");
            return Ok(());
        }
        *has_processed_before = true;
    }

    // Validate the GPA
    let gpa_value = match *gpa {
        Some(gpa_val) if gpa_val != 0 && gpa_val != u32::MAX => gpa_val,
        Some(invalid_gpa) => return Err(ProcessingError::BadGpa(invalid_gpa)),
        None => return Err(ProcessingError::NoGpa),
    };

    // Read and validate the header from the guest memory
    let header: AdvancedLoggerInfo = gm.read_plain(gpa_value as u64)?;

    let signature = header.signature;
    if signature != u32::from_le_bytes(SIG_HEADER) {
        return Err(ProcessingError::HeaderSignatureMismatch(
            u32::from_le_bytes(SIG_HEADER),
            signature,
        ));
    }

    if header.log_buffer_size > MAX_LOG_BUFFER_SIZE {
        return Err(ProcessingError::HeaderBufferSize(
            MAX_LOG_BUFFER_SIZE,
            header.log_buffer_size,
        ));
    }

    // Calculate the used portion of the log buffer
    let used_log_buffer_size = header
        .log_current_offset
        .checked_sub(header.log_buffer_offset)
        .ok_or_else(|| ProcessingError::Overflow("used_log_buffer_size"))?;

    // Early exit if there is no buffer to process
    if used_log_buffer_size == 0 {
        tracelimit::info_ratelimited!(
            "EFI diagnostics' used log buffer size is 0, ending processing"
        );
        return Ok(());
    }

    if used_log_buffer_size > header.log_buffer_size || used_log_buffer_size > MAX_LOG_BUFFER_SIZE {
        return Err(ProcessingError::BadUsedBufferSize(
            MAX_LOG_BUFFER_SIZE,
            used_log_buffer_size,
        ));
    }

    // Calculate start address of the log buffer and read it
    let buffer_start_addr = gpa_value
        .checked_add(header.log_buffer_offset)
        .ok_or_else(|| ProcessingError::Overflow("buffer_start_addr"))?;

    let mut buffer_data = vec![0u8; used_log_buffer_size as usize];
    gm.read_at(buffer_start_addr as u64, &mut buffer_data)?;

    // Process the buffer
    process_buffer(&buffer_data, log_level, log_handler)?;

    Ok(())
}

/// Process the log buffer and emit completed log entries
fn process_buffer<F>(
    buffer_data: &[u8],
    log_level: LogLevel,
    mut log_handler: F,
) -> Result<(), ProcessingError>
where
    F: FnMut(EfiDiagnosticsLog<'_>, u32),
{
    let mut buffer_slice = buffer_data;
    let mut accumulator = MessageAccumulator::new();

    // Process the buffer slice until all entries are processed
    while !buffer_slice.is_empty() {
        let entry = parse_entry(buffer_slice)?;

        // Process the entry through the accumulator
        if let Some((log, raw_debug_level)) = accumulator.process_entry(&entry)?
            && log_level.should_log(raw_debug_level)
        {
            log_handler(log, raw_debug_level);
        }

        // Move to the next entry
        if entry.entry_size >= buffer_slice.len() {
            break; // End of buffer
        } else {
            buffer_slice = &buffer_slice[entry.entry_size..];
        }
    }

    // Process any remaining accumulated message
    if let Some((log, raw_debug_level)) = accumulator.finalize_remaining()
        && log_level.should_log(raw_debug_level)
    {
        log_handler(log, raw_debug_level);
    }

    // Log suppressed message summary and statistics
    accumulator.log_suppressed_summary();
    tracelimit::info_ratelimited!(
        entries_processed = accumulator.stats.entries_processed,
        bytes_read = accumulator.stats.bytes_read,
        "processed EFI log entries"
    );

    Ok(())
}
