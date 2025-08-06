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

#![warn(missing_docs)]

use crate::UefiDevice;
use guestmem::GuestMemory;
use guestmem::GuestMemoryError;
use inspect::Inspect;
use std::borrow::Cow;
use std::collections::BTreeMap;
use std::fmt::Debug;
use std::mem::size_of;
use thiserror::Error;
use uefi_specs::hyperv::advanced_logger::AdvancedLoggerInfo;
use uefi_specs::hyperv::advanced_logger::AdvancedLoggerMessageEntryV2;
use uefi_specs::hyperv::advanced_logger::PHASE_NAMES;
use uefi_specs::hyperv::advanced_logger::SIG_ENTRY;
use uefi_specs::hyperv::advanced_logger::SIG_HEADER;
use uefi_specs::hyperv::debug_level::DEBUG_ERROR;
use uefi_specs::hyperv::debug_level::DEBUG_FLAG_NAMES;
use uefi_specs::hyperv::debug_level::DEBUG_WARN;
use zerocopy::FromBytes;

/// Default number of EfiDiagnosticsLogs emitted per period
pub const DEFAULT_LOGS_PER_PERIOD: u32 = 150;

/// 8-byte alignment for every entry
const ALIGNMENT: usize = 8;

/// Alignment mask for the entry
const ALIGNMENT_MASK: usize = ALIGNMENT - 1;

/// Maximum allowed size of the log buffer
pub const MAX_LOG_BUFFER_SIZE: u32 = 0x400000; // 4MB

/// Maximum allowed size of a single message
pub const MAX_MESSAGE_LENGTH: u16 = 0x1000; // 4KB

// Suppress logs that contain these known error/warning messages.
// These messages are the result of known issues with our UEFI firmware that do
// not seem to affect the guest.
// TODO: Fix UEFI to resolve this errors/warnings
const SUPPRESS_LOGS: [&str; 3] = [
    "WARNING: There is mismatch of supported HashMask (0x2 - 0x7) between modules",
    "that are linking different HashInstanceLib instances!",
    "ConvertPages: failed to find range",
];

/// Represents a processed log entry from the EFI diagnostics buffer
#[derive(Debug, Clone)]
pub struct EfiDiagnosticsLog<'a> {
    /// The debug level of the log entry
    pub debug_level: u32,
    /// Hypervisor reference ticks elapsed from UEFI
    pub ticks: u64,
    /// The boot phase that produced this log entry
    pub phase: u16,
    /// The log message itself
    pub message: &'a str,
}

/// Converts a debug level to a human-readable string
fn debug_level_to_string(debug_level: u32) -> Cow<'static, str> {
    // Borrow directly from the table if only one flag is set
    if debug_level.count_ones() == 1 {
        if let Some(&(_, name)) = DEBUG_FLAG_NAMES
            .iter()
            .find(|&&(flag, _)| flag == debug_level)
        {
            return Cow::Borrowed(name);
        }
    }

    // Handle combined flags or unknown debug levels
    let flags: Vec<&str> = DEBUG_FLAG_NAMES
        .iter()
        .filter(|&&(flag, _)| debug_level & flag != 0)
        .map(|&(_, name)| name)
        .collect();

    if flags.is_empty() {
        Cow::Borrowed("UNKNOWN")
    } else {
        Cow::Owned(flags.join("+"))
    }
}

/// Converts a phase value to a human-readable string
fn phase_to_string(phase: u16) -> &'static str {
    PHASE_NAMES
        .iter()
        .find(|&&(phase_raw, _)| phase_raw == phase)
        .map(|&(_, name)| name)
        .unwrap_or("UNKNOWN")
}

/// Rate-limited handler for EFI diagnostics logs during normal processing.
pub fn log_diagnostic_ratelimited(log: EfiDiagnosticsLog<'_>, limit: u32) {
    let debug_level_str = debug_level_to_string(log.debug_level);
    let phase_str = phase_to_string(log.phase);

    match log.debug_level {
        DEBUG_WARN => tracelimit::warn_ratelimited!(
            limit: limit,
            debug_level = %debug_level_str,
            ticks = log.ticks,
            phase = %phase_str,
            log_message = log.message,
            "EFI log entry"
        ),
        DEBUG_ERROR => tracelimit::error_ratelimited!(
            limit: limit,
            debug_level = %debug_level_str,
            ticks = log.ticks,
            phase = %phase_str,
            log_message = log.message,
            "EFI log entry"
        ),
        _ => tracelimit::info_ratelimited!(
            limit: limit,
            debug_level = %debug_level_str,
            ticks = log.ticks,
            phase = %phase_str,
            log_message = log.message,
            "EFI log entry"
        ),
    }
}

/// Unrestricted handler for EFI diagnostics logs during inspection.
pub fn log_diagnostic_unrestricted(log: EfiDiagnosticsLog<'_>) {
    let debug_level_str = debug_level_to_string(log.debug_level);
    let phase_str = phase_to_string(log.phase);

    match log.debug_level {
        DEBUG_WARN => tracing::warn!(
            debug_level = %debug_level_str,
            ticks = log.ticks,
            phase = %phase_str,
            log_message = log.message,
            "EFI log entry"
        ),
        DEBUG_ERROR => tracing::error!(
            debug_level = %debug_level_str,
            ticks = log.ticks,
            phase = %phase_str,
            log_message = log.message,
            "EFI log entry"
        ),
        _ => tracing::info!(
            debug_level = %debug_level_str,
            ticks = log.ticks,
            phase = %phase_str,
            log_message = log.message,
            "EFI log entry"
        ),
    }
}

/// Errors that occur when parsing entries
#[derive(Debug, Error)]
#[allow(missing_docs)]
pub enum EntryParseError {
    #[error("Expected: {0:#x}, got: {1:#x}")]
    SignatureMismatch(u32, u32),
    #[error("Expected message length < {0:#x}, got: {1:#x}")]
    MessageLength(u16, u16),
    #[error("Failed to read from buffer slice")]
    SliceRead,
    #[error("Arithmetic overflow in {0}")]
    Overflow(&'static str),
    #[error("Failed to read UTF-8 string: {0}")]
    Utf8Error(#[from] std::str::Utf8Error),
    #[error("message_end ({0:#x}) exceeds buffer slice length ({1:#x})")]
    BadMessageEnd(usize, usize),
}

/// Represents a single parsed entry from the EFI diagnostics buffer
struct EntryData<'a> {
    /// The debug level of the log entry
    debug_level: u32,
    /// Timestamp of when the log entry was created
    time_stamp: u64,
    /// The boot phase that produced this log entry
    phase: u16,
    /// The log message itself
    message: &'a str,
    /// The size of the entry in bytes (including alignment)
    entry_size: usize,
}

/// Parse a single entry from a buffer slice
fn parse_entry(buffer_slice: &[u8]) -> Result<EntryData<'_>, EntryParseError> {
    // Try to parse an entry from the buffer slice and validate it
    let (entry, _) = AdvancedLoggerMessageEntryV2::read_from_prefix(buffer_slice)
        .map_err(|_| EntryParseError::SliceRead)?;

    let signature = entry.signature;
    if signature != u32::from_le_bytes(SIG_ENTRY) {
        return Err(EntryParseError::SignatureMismatch(
            u32::from_le_bytes(SIG_ENTRY),
            signature,
        ));
    }

    if entry.message_len > MAX_MESSAGE_LENGTH {
        return Err(EntryParseError::MessageLength(
            MAX_MESSAGE_LENGTH,
            entry.message_len,
        ));
    }

    let message_offset = entry.message_offset;
    let message_len = entry.message_len;

    // Calculate message start and end offsets for boundary validation
    let message_start = message_offset as usize;
    let message_end = message_start
        .checked_add(message_len as usize)
        .ok_or(EntryParseError::Overflow("message_end"))?;

    if message_end > buffer_slice.len() {
        return Err(EntryParseError::BadMessageEnd(
            message_end,
            buffer_slice.len(),
        ));
    }

    let message = std::str::from_utf8(&buffer_slice[message_start..message_end])?;

    // Calculate size of the entry to find the offset of the next entry
    let base_offset = size_of::<AdvancedLoggerMessageEntryV2>()
        .checked_add(message_len as usize)
        .ok_or(EntryParseError::Overflow("base_offset"))?;

    // Add padding for 8-byte alignment
    let aligned_offset = base_offset
        .checked_add(ALIGNMENT_MASK)
        .ok_or(EntryParseError::Overflow("aligned_offset"))?;
    let entry_size = aligned_offset & !ALIGNMENT_MASK;

    Ok(EntryData {
        debug_level: entry.debug_level,
        time_stamp: entry.time_stamp,
        phase: entry.phase,
        message,
        entry_size,
    })
}

/// Errors that occur during processing
#[derive(Debug, Error)]
#[allow(missing_docs)]
pub enum DiagnosticsError {
    #[error("Failed to parse entry: {0}")]
    EntryParse(#[from] EntryParseError),
    #[error("Expected: {0:#x}, got: {1:#x}")]
    HeaderSignatureMismatch(u32, u32),
    #[error("Expected log buffer size < {0:#x}, got: {1:#x}")]
    HeaderBufferSize(u32, u32),
    #[error("Bad GPA value: {0:#x}")]
    BadGpa(u32),
    #[error("No GPA set")]
    NoGpa,
    #[error("Failed to read from guest memory: {0}")]
    GuestMemoryRead(#[from] GuestMemoryError),
    #[error("Arithmetic overflow in {0}")]
    Overflow(&'static str),
    #[error("Expected used log buffer size < {0:#x}, got: {1:#x}")]
    BadUsedBufferSize(u32, u32),
    #[error("Expected accumulated message length < {0:#x}, got: {1:#x}")]
    BadAccumulatedMessageLength(u16, u16),
}

/// Definition of the diagnostics services state
#[derive(Inspect)]
pub struct DiagnosticsServices {
    /// The guest physical address of the diagnostics buffer
    gpa: Option<u32>,
    /// Flag indicating if guest-initiated processing has occurred before
    has_guest_processed_before: bool,
}

impl DiagnosticsServices {
    /// Create a new instance of the diagnostics services
    pub fn new() -> DiagnosticsServices {
        DiagnosticsServices {
            gpa: None,
            has_guest_processed_before: false,
        }
    }

    /// Reset the diagnostics services state
    pub fn reset(&mut self) {
        self.gpa = None;
        self.has_guest_processed_before = false;
    }

    /// Set the GPA of the diagnostics buffer
    pub fn set_gpa(&mut self, gpa: u32) {
        self.gpa = match gpa {
            0 => None,
            _ => Some(gpa),
        }
    }

    /// Processes diagnostics from guest memory
    ///
    /// # Arguments
    /// * `allow_reprocess` - If true, allows processing even if already processed for guest
    /// * `gm` - Guest memory to read diagnostics from
    /// * `log_handler` - Function to handle each parsed log entry
    fn process_diagnostics<F>(
        &mut self,
        allow_reprocess: bool,
        gm: &GuestMemory,
        mut log_handler: F,
    ) -> Result<(), DiagnosticsError>
    where
        F: FnMut(EfiDiagnosticsLog<'_>),
    {
        // Prevent the guest from spamming diagnostics processing
        // unless explicitly allowed
        if !allow_reprocess {
            if self.has_guest_processed_before {
                tracelimit::warn_ratelimited!("Already processed diagnostics, skipping");
                return Ok(());
            }
            self.has_guest_processed_before = true;
        }

        // Validate the GPA
        let gpa = match self.gpa {
            Some(gpa) if gpa != 0 && gpa != u32::MAX => gpa,
            Some(invalid_gpa) => return Err(DiagnosticsError::BadGpa(invalid_gpa)),
            None => return Err(DiagnosticsError::NoGpa),
        };

        // Read and validate the header from the guest memory
        let header: AdvancedLoggerInfo = gm.read_plain(gpa as u64)?;

        let signature = header.signature;
        if signature != u32::from_le_bytes(SIG_HEADER) {
            return Err(DiagnosticsError::HeaderSignatureMismatch(
                u32::from_le_bytes(SIG_HEADER),
                signature,
            ));
        }

        if header.log_buffer_size > MAX_LOG_BUFFER_SIZE {
            return Err(DiagnosticsError::HeaderBufferSize(
                MAX_LOG_BUFFER_SIZE,
                header.log_buffer_size,
            ));
        }

        // Calculate the used portion of the log buffer
        let used_log_buffer_size = header
            .log_current_offset
            .checked_sub(header.log_buffer_offset)
            .ok_or_else(|| DiagnosticsError::Overflow("used_log_buffer_size"))?;

        // Early exit if there is no buffer to process
        if used_log_buffer_size == 0 {
            tracelimit::info_ratelimited!(
                "EFI diagnostics' used log buffer size is 0, ending processing"
            );
            return Ok(());
        }

        if used_log_buffer_size > header.log_buffer_size
            || used_log_buffer_size > MAX_LOG_BUFFER_SIZE
        {
            return Err(DiagnosticsError::BadUsedBufferSize(
                MAX_LOG_BUFFER_SIZE,
                used_log_buffer_size,
            ));
        }

        // Calculate start address of the log buffer
        let buffer_start_addr = gpa
            .checked_add(header.log_buffer_offset)
            .ok_or_else(|| DiagnosticsError::Overflow("buffer_start_addr"))?;

        // Now read the used log buffer into a vector
        let mut buffer_data = vec![0u8; used_log_buffer_size as usize];
        gm.read_at(buffer_start_addr as u64, &mut buffer_data)?;

        // Maintain a slice of the buffer that needs to be processed
        let mut buffer_slice = &buffer_data[..];

        // Message accumulation state
        let mut accumulated_message = String::with_capacity(MAX_MESSAGE_LENGTH as usize);
        let mut debug_level = 0;
        let mut time_stamp = 0;
        let mut phase = 0;
        let mut is_accumulating = false;

        // Used for tracking what has been processed
        let mut bytes_read: usize = 0;
        let mut entries_processed: usize = 0;

        let mut suppressed_logs = BTreeMap::new();

        // Process the buffer slice until all entries are processed
        while !buffer_slice.is_empty() {
            let entry = parse_entry(buffer_slice)?;

            // Handle message accumulation
            if !is_accumulating {
                debug_level = entry.debug_level;
                time_stamp = entry.time_stamp;
                phase = entry.phase;
                accumulated_message.clear();
                is_accumulating = true;
            }

            accumulated_message.push_str(entry.message);
            if accumulated_message.len() > MAX_MESSAGE_LENGTH as usize {
                return Err(DiagnosticsError::BadAccumulatedMessageLength(
                    MAX_MESSAGE_LENGTH,
                    accumulated_message.len() as u16,
                ));
            }

            // Handle completed messages (ending with '\n')
            if !entry.message.is_empty() && entry.message.ends_with('\n') {
                let mut suppress = false;
                for log in SUPPRESS_LOGS {
                    if accumulated_message.contains(log) {
                        suppressed_logs
                            .entry(log)
                            .and_modify(|c| *c += 1)
                            .or_insert(1);
                        suppress = true;
                    }
                }
                if !suppress {
                    log_handler(EfiDiagnosticsLog {
                        debug_level,
                        ticks: time_stamp,
                        phase,
                        message: accumulated_message.trim_end_matches(&['\r', '\n'][..]),
                    });
                }
                is_accumulating = false;
                entries_processed += 1;
            }

            // Update bytes read and move to the next entry
            bytes_read = bytes_read
                .checked_add(entry.entry_size)
                .ok_or_else(|| DiagnosticsError::Overflow("bytes_read"))?;

            if entry.entry_size >= buffer_slice.len() {
                break; // End of buffer
            } else {
                buffer_slice = &buffer_slice[entry.entry_size..];
            }
        }

        // Process any remaining accumulated message
        if is_accumulating && !accumulated_message.is_empty() {
            log_handler(EfiDiagnosticsLog {
                debug_level,
                ticks: time_stamp,
                phase,
                message: accumulated_message.trim_end_matches(&['\r', '\n'][..]),
            });
            entries_processed += 1;
        }

        for (substring, count) in suppressed_logs {
            tracelimit::warn_ratelimited!(substring, count, "suppressed logs")
        }

        // Print summary statistics
        tracelimit::info_ratelimited!(entries_processed, bytes_read, "processed EFI log entries");

        Ok(())
    }
}

impl UefiDevice {
    /// Processes UEFI diagnostics from guest memory.
    ///
    /// When a limit is provided, traces are rate-limited to avoid spam.
    /// When no limit is provided, traces are unrestricted.
    ///
    /// # Arguments
    /// * `allow_reprocess` - If true, allows processing even if already processed for guest
    /// * `limit` - Maximum number of logs to process per period, or `None` for no limit
    pub(crate) fn process_diagnostics(&mut self, allow_reprocess: bool, limit: Option<u32>) {
        if let Err(error) =
            self.service
                .diagnostics
                .process_diagnostics(allow_reprocess, &self.gm, |log| match limit {
                    Some(limit) => log_diagnostic_ratelimited(log, limit),
                    None => log_diagnostic_unrestricted(log),
                })
        {
            tracelimit::error_ratelimited!(
                error = &error as &dyn std::error::Error,
                "failed to process diagnostics buffer"
            );
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
            pub did_flush: bool,
        }
    }

    impl SaveRestore for DiagnosticsServices {
        type SavedState = state::SavedState;

        fn save(&mut self) -> Result<Self::SavedState, SaveError> {
            Ok(state::SavedState {
                gpa: self.gpa,
                did_flush: self.has_guest_processed_before,
            })
        }

        fn restore(&mut self, state: Self::SavedState) -> Result<(), RestoreError> {
            let state::SavedState { gpa, did_flush } = state;
            self.gpa = gpa;
            self.has_guest_processed_before = did_flush;
            Ok(())
        }
    }
}
