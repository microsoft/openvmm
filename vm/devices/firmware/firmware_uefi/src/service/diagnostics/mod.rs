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

// Re-export public types from submodules
pub use formatting::EfiDiagnosticsLog;
pub use formatting::log_diagnostic_ratelimited;
pub use formatting::log_diagnostic_unrestricted;
pub use processor::ProcessingError;

use crate::UefiDevice;
use guestmem::GuestMemory;
use inspect::Inspect;

mod formatting;
mod message_accumulator;
mod parser;
mod processor;

/// Default number of EfiDiagnosticsLogs emitted per period
pub const DEFAULT_LOGS_PER_PERIOD: u32 = 150;

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
        log_handler: F,
    ) -> Result<(), ProcessingError>
    where
        F: FnMut(EfiDiagnosticsLog<'_>, u32),
    {
        // Delegate to the processor module
        processor::process_diagnostics_internal(
            &mut self.gpa,
            &mut self.has_guest_processed_before,
            allow_reprocess,
            gm,
            log_handler,
        )
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
        if let Err(error) = self.service.diagnostics.process_diagnostics(
            allow_reprocess,
            &self.gm,
            |log, raw_debug_level| match limit {
                Some(limit) => log_diagnostic_ratelimited(log, raw_debug_level, limit),
                None => log_diagnostic_unrestricted(log, raw_debug_level),
            },
        ) {
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
