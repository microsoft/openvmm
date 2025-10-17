// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Log formatting and output utilities for EFI diagnostics

use std::borrow::Cow;
use uefi_specs::hyperv::advanced_logger::PHASE_NAMES;
use uefi_specs::hyperv::debug_level::DEBUG_ERROR;
use uefi_specs::hyperv::debug_level::DEBUG_FLAG_NAMES;
use uefi_specs::hyperv::debug_level::DEBUG_WARN;

/// Represents a processed log entry from the EFI diagnostics buffer
#[derive(Debug, Clone)]
pub struct EfiDiagnosticsLog<'a> {
    /// The debug level of the log entry as a human-readable string
    pub debug_level: Cow<'static, str>,
    /// Hypervisor reference ticks elapsed from UEFI
    pub ticks: u64,
    /// The boot phase that produced this log entry as a human-readable string
    pub phase: &'static str,
    /// The log message itself
    pub message: &'a str,
}

/// Converts a debug level to a human-readable string
pub fn debug_level_to_string(debug_level: u32) -> Cow<'static, str> {
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
pub fn phase_to_string(phase: u16) -> &'static str {
    PHASE_NAMES
        .iter()
        .find(|&&(phase_raw, _)| phase_raw == phase)
        .map(|&(_, name)| name)
        .unwrap_or("UNKNOWN")
}

/// Rate-limited handler for EFI diagnostics logs during normal processing.
pub fn log_diagnostic_ratelimited(log: EfiDiagnosticsLog<'_>, raw_debug_level: u32, limit: u32) {
    if raw_debug_level & DEBUG_ERROR != 0 {
        tracelimit::error_ratelimited!(
            limit: limit,
            debug_level = %log.debug_level,
            ticks = log.ticks,
            phase = %log.phase,
            log_message = log.message,
            "EFI log entry"
        )
    } else if raw_debug_level & DEBUG_WARN != 0 {
        tracelimit::warn_ratelimited!(
            limit: limit,
            debug_level = %log.debug_level,
            ticks = log.ticks,
            phase = %log.phase,
            log_message = log.message,
            "EFI log entry"
        )
    } else {
        tracelimit::info_ratelimited!(
            limit: limit,
            debug_level = %log.debug_level,
            ticks = log.ticks,
            phase = %log.phase,
            log_message = log.message,
            "EFI log entry"
        )
    }
}

/// Unrestricted handler for EFI diagnostics logs during inspection.
pub fn log_diagnostic_unrestricted(log: EfiDiagnosticsLog<'_>, raw_debug_level: u32) {
    if raw_debug_level & DEBUG_ERROR != 0 {
        tracing::error!(
            debug_level = %log.debug_level,
            ticks = log.ticks,
            phase = %log.phase,
            log_message = log.message,
            "EFI log entry"
        )
    } else if raw_debug_level & DEBUG_WARN != 0 {
        tracing::warn!(
            debug_level = %log.debug_level,
            ticks = log.ticks,
            phase = %log.phase,
            log_message = log.message,
            "EFI log entry"
        )
    } else {
        tracing::info!(
            debug_level = %log.debug_level,
            ticks = log.ticks,
            phase = %log.phase,
            log_message = log.message,
            "EFI log entry"
        )
    }
}
