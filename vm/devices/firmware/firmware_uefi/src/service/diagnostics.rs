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

    pub fn set_diagnostics_gpa(&mut self, gpa: u32) -> Result<(), DiagnosticsError> {
        tracelimit::info_ratelimited!("Setting diagnostics GPA to {:#x}", gpa);
        if gpa == 0 || gpa == u32::MAX {
            tracelimit::error_ratelimited!("Invalid GPA: {:#x}", gpa);
            return Err(DiagnosticsError::InvalidGpa);
        }
        self.gpa = gpa;
        Ok(())
    }

    pub fn process_diagnostics(&self, _gm: GuestMemory) -> Result<(), DiagnosticsError> {
        tracelimit::info_ratelimited!("Recieved notification to process EFI diagnostics");
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
