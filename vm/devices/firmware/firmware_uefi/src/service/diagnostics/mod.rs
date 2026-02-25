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
//! This file specifically should only expose the public API of the service;
//! internal implementation details should be in submodules.

use crate::UefiDevice;
use gpa::Gpa;
use guestmem::GuestMemory;
use inspect::Inspect;
use log::Log;
use mesh::payload::Protobuf;
use processor::ProcessingError;
use uefi_specs::hyperv::debug_level::DEBUG_ERROR;
use uefi_specs::hyperv::debug_level::DEBUG_INFO;
use uefi_specs::hyperv::debug_level::DEBUG_WARN;

mod accumulator;
mod gpa;
mod header;
mod log;
mod processor;

/// Default number of EfiDiagnosticsLogs emitted per period
pub const DEFAULT_LOGS_PER_PERIOD: u32 = 150;

/// Emit a diagnostic log entry with rate limiting.
///
/// # Arguments
/// * `log` - The log entry to emit
/// * `limit` - Maximum number of log entries to emit per period
fn emit_log_ratelimited(log: &Log, limit: u32) {
    if log.debug_level & DEBUG_ERROR != 0 {
        tracelimit::error_ratelimited!(
            limit: limit,
            debug_level = %log.debug_level_str(),
            ticks = log.ticks(),
            phase = %log.phase_str(),
            log_message = log.message_trimmed(),
            "EFI log entry"
        )
    } else if log.debug_level & DEBUG_WARN != 0 {
        tracelimit::warn_ratelimited!(
            limit: limit,
            debug_level = %log.debug_level_str(),
            ticks = log.ticks(),
            phase = %log.phase_str(),
            log_message = log.message_trimmed(),
            "EFI log entry"
        )
    } else {
        tracelimit::info_ratelimited!(
            limit: limit,
            debug_level = %log.debug_level_str(),
            ticks = log.ticks(),
            phase = %log.phase_str(),
            log_message = log.message_trimmed(),
            "EFI log entry"
        )
    }
}

/// Emit a diagnostic log entry without rate limiting.
///
/// # Arguments
/// * `log` - The log entry to emit
fn emit_log_unrestricted(log: &Log) {
    if log.debug_level & DEBUG_ERROR != 0 {
        tracing::error!(
            debug_level = %log.debug_level_str(),
            ticks = log.ticks(),
            phase = %log.phase_str(),
            log_message = log.message_trimmed(),
            "EFI log entry"
        )
    } else if log.debug_level & DEBUG_WARN != 0 {
        tracing::warn!(
            debug_level = %log.debug_level_str(),
            ticks = log.ticks(),
            phase = %log.phase_str(),
            log_message = log.message_trimmed(),
            "EFI log entry"
        )
    } else {
        tracing::info!(
            debug_level = %log.debug_level_str(),
            ticks = log.ticks(),
            phase = %log.phase_str(),
            log_message = log.message_trimmed(),
            "EFI log entry"
        )
    }
}

/// Log level configuration - encapsulates a u32 mask where u32::MAX means log everything
#[derive(Debug, Clone, Copy, PartialEq, Eq, Protobuf)]
#[mesh(transparent)]
pub struct LogLevel(u32);

impl LogLevel {
    /// Create default log level configuration (ERROR and WARN only)
    pub const fn make_default() -> Self {
        Self(DEBUG_ERROR | DEBUG_WARN)
    }

    /// Create info log level configuration (ERROR, WARN, and INFO)
    pub const fn make_info() -> Self {
        Self(DEBUG_ERROR | DEBUG_WARN | DEBUG_INFO)
    }

    /// Create full log level configuration (all levels)
    pub const fn make_full() -> Self {
        Self(u32::MAX)
    }

    /// Checks if a raw debug level should be logged based on this log level configuration
    pub fn should_log(self, raw_debug_level: u32) -> bool {
        if self.0 == u32::MAX {
            true // Log everything
        } else {
            (raw_debug_level & self.0) != 0
        }
    }
}

impl Default for LogLevel {
    fn default() -> Self {
        Self::make_default()
    }
}

impl Inspect for LogLevel {
    fn inspect(&self, req: inspect::Request<'_>) {
        let human_readable = log::debug_level_to_string(self.0);
        req.respond()
            .field("raw_value", self.0)
            .field("debug_levels", human_readable.as_ref());
    }
}

/// Definition of the diagnostics services state
#[derive(Inspect)]
pub struct DiagnosticsServices {
    /// The guest physical address of the diagnostics buffer
    gpa: Option<Gpa>,
    /// Whether diagnostics have been processed (prevents reprocessing spam)
    processed: bool,
    /// Log level used for filtering
    log_level: LogLevel,
}

impl DiagnosticsServices {
    /// Create a new instance of the diagnostics services
    pub fn new(log_level: LogLevel) -> DiagnosticsServices {
        DiagnosticsServices {
            gpa: None,
            processed: false,
            log_level,
        }
    }

    /// Reset the diagnostics services state
    pub fn reset(&mut self) {
        self.gpa = None;
        self.processed = false;
    }

    /// Set the GPA of the diagnostics buffer
    pub fn set_gpa(&mut self, gpa: u32) {
        self.gpa = Gpa::new(gpa).ok();
    }

    /// Processes diagnostics from guest memory
    ///
    /// # Arguments
    /// * `allow_reprocess` - If true, allows processing even if already processed for guest
    /// * `gm` - Guest memory to read diagnostics from
    /// * `log_level_override` - If provided, overrides the configured log level for this processing run
    /// * `log_handler` - Function to handle each parsed log entry
    pub fn process_diagnostics<F>(
        &mut self,
        allow_reprocess: bool,
        gm: &GuestMemory,
        log_level_override: Option<LogLevel>,
        log_handler: F,
    ) -> Result<(), ProcessingError>
    where
        F: FnMut(&Log),
    {
        // Check if processing is allowed
        if self.processed && !allow_reprocess {
            tracelimit::warn_ratelimited!("Already processed diagnostics, skipping");
            return Ok(());
        }

        // Mark as processed first to prevent guest spam (even on failure)
        self.processed = true;

        // Use the override log level if provided, otherwise fall back to configured level
        let effective_log_level = log_level_override.unwrap_or(self.log_level);

        // Delegate to the processor module
        processor::process_diagnostics_internal(self.gpa, gm, effective_log_level, log_handler)
    }
}

/// The output destination for diagnostics.
pub(crate) enum DiagnosticsEmitter {
    /// Emit to tracing
    Tracing { limit: Option<u32> },
    /// Emit to a string
    String,
}

impl UefiDevice {
    /// Processes UEFI diagnostics from guest memory.
    ///
    /// # Arguments
    /// * `allow_reprocess` - If true, allows processing even if already processed for guest
    /// * `emitter` - The destination for the diagnostics output
    /// * `log_level_override` - If provided, overrides the configured log level filter for this run
    pub(crate) fn process_diagnostics(
        &mut self,
        allow_reprocess: bool,
        emitter: DiagnosticsEmitter,
        log_level_override: Option<LogLevel>,
    ) -> Result<Option<String>, ProcessingError> {
        use std::fmt::Write;
        let mut output = match emitter {
            DiagnosticsEmitter::String => Some(String::new()),
            DiagnosticsEmitter::Tracing { .. } => None,
        };

        if let Err(error) = self.service.diagnostics.process_diagnostics(
            allow_reprocess,
            &self.gm,
            log_level_override,
            |log| {
                if let Some(out) = &mut output {
                    let _ = writeln!(
                        out,
                        "({} ticks) [{}] [{}]: {}",
                        log.ticks(),
                        log.debug_level_str(),
                        log.phase_str(),
                        log.message_trimmed(),
                    );
                } else if let DiagnosticsEmitter::Tracing { limit } = emitter {
                    match limit {
                        Some(limit) => emit_log_ratelimited(log, limit),
                        None => emit_log_unrestricted(log),
                    }
                }
            },
        ) {
            match emitter {
                DiagnosticsEmitter::Tracing { .. } => {
                    tracelimit::error_ratelimited!(
                        error = &error as &dyn std::error::Error,
                        "failed to process diagnostics buffer"
                    );
                    // For tracing, we swallow the error after logging it, consistent with previous behavior
                    return Ok(None);
                }
                DiagnosticsEmitter::String => return Err(error),
            }
        }

        Ok(output)
    }
}

mod save_restore {
    use super::*;
    use vmcore::save_restore::RestoreError;
    use vmcore::save_restore::SaveError;
    use vmcore::save_restore::SaveRestore;

    mod state {
        use super::LogLevel;
        use mesh::payload::Protobuf;
        use vmcore::save_restore::SavedStateRoot;

        #[derive(Protobuf, SavedStateRoot)]
        #[mesh(package = "firmware.uefi.diagnostics")]
        pub struct SavedState {
            #[mesh(1)]
            pub gpa: Option<u32>,
            #[mesh(2)]
            pub did_flush: bool,
            #[mesh(3)]
            pub log_level: LogLevel,
        }
    }

    impl SaveRestore for DiagnosticsServices {
        type SavedState = state::SavedState;

        fn save(&mut self) -> Result<Self::SavedState, SaveError> {
            Ok(state::SavedState {
                gpa: self.gpa.map(|g| g.get()),
                did_flush: self.processed,
                log_level: self.log_level,
            })
        }

        fn restore(&mut self, state: Self::SavedState) -> Result<(), RestoreError> {
            let state::SavedState {
                gpa,
                did_flush,
                log_level,
            } = state;
            self.gpa = gpa.and_then(|g| Gpa::new(g).ok());
            self.processed = did_flush;
            self.log_level = log_level;
            Ok(())
        }
    }
}
