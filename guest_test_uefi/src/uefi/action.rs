// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Generic boot-action selector for `guest_test_uefi`.
//!
//! Petri selects the guest's behavior by seeding a UEFI variable (via a
//! `CUSTOM_UEFI` NVRAM delta in the VMGS) that this application reads at
//! startup. When the variable is absent or unrecognized, the default action is
//! to run the normal test suite; specific values select alternate behaviors
//! such as requesting hibernation.

use uefi::cstr16;
use uefi::runtime;
use uefi::runtime::VariableVendor;

/// The UEFI variable, under the EFI global namespace, that petri writes to
/// select a guest action. Keep in sync with the petri-side seeding helper.
const ACTION_VAR_NAME: &uefi::CStr16 = cstr16!("PetriBootAction");

/// The action a caller requested this application to perform at startup.
pub enum GuestAction {
    /// Run the normal `guest_test_uefi` test suite (the default).
    RunTests,
    /// Request hibernation from the platform, then never return.
    Hibernate,
}

impl GuestAction {
    /// Map a raw variable value to an action, tolerating a trailing NUL.
    fn from_value(value: &[u8]) -> Self {
        let value = value.split(|&b| b == 0).next().unwrap_or(value);
        match value {
            b"hibernate" => GuestAction::Hibernate,
            _ => GuestAction::RunTests,
        }
    }
}

/// Read the petri-selected action, defaulting to running the test suite when
/// the selector variable is absent or unreadable.
pub fn selected_action() -> GuestAction {
    match runtime::get_variable_boxed(ACTION_VAR_NAME, &VariableVendor::GLOBAL_VARIABLE) {
        Ok((value, _)) => GuestAction::from_value(&value),
        Err(_) => GuestAction::RunTests,
    }
}
