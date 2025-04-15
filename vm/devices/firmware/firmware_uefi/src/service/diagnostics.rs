// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! UEFI diagnostics subsystem
//!
//! For now, this module simply holds the GPA of the advanced logger
//! buffer and sends that to the host. Hyper-V has changes to parse
//! the buffer and send it to ETW.
//!
//! Eventually, we will want to implement that parsing logic here too.

use crate::UefiDevice;
use inspect::Inspect;
use std::fmt::Debug;
use thiserror::Error;

#[derive(Debug, Error)]
pub enum DiagnosticsError {
    #[error("invalid diagnostics address")]
    InvalidAddress,
}

#[derive(Inspect)]
pub struct DiagnosticsServices {
    // #[inspect(skip)]
    // logger: Box<dyn UefiLogger>,
    gpa: u32,
}

impl DiagnosticsServices {
    pub fn new() -> DiagnosticsServices {
        DiagnosticsServices { gpa: 0 }
    }

    pub fn reset(&mut self) {
        self.gpa = 0
    }

    pub fn set_gpa(&mut self, gpa: u32) -> Result<(), DiagnosticsError> {
        if gpa == 0 || gpa == u32::MAX {
            return Err(DiagnosticsError::InvalidAddress);
        }
        self.gpa = gpa;
        // self.logger.log_event(UefiEvent::EfiDiagnosticsGpa(gpa));
        Ok(())
    }
}

impl UefiDevice {
    pub(crate) fn _set_diagnostics_gpa(&mut self, gpa: u32) {
        if let Err(err) = self.service.diagnostics.set_gpa(gpa) {
            tracelimit::error_ratelimited!(
                error = &err as &dyn std::error::Error,
                "Failed to set diagnostics GPA",
            );
        }
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
