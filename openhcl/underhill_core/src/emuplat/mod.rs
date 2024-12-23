// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

pub mod firmware;
pub mod framebuffer;
pub mod i440bx_host_pci_bridge;
pub mod local_clock;
pub mod netvsp;
pub mod non_volatile_store;
pub mod tpm;
pub mod vga_proxy;
pub mod watchdog;

use crate::servicing::EmuplatSavedState;
use parking_lot::Mutex;
use std::sync::Arc;
use vm_resource::register_static_resolvers;
use vmcore::save_restore::SaveError;
use vmcore::save_restore::SaveRestore;

// These resolvers are defined in this crate and are always linked in.
register_static_resolvers! {
    tpm::resources::GetTpmRequestAkCertHelperResolver,
}

pub struct EmuplatServicing {
    pub rtc_local_clock: Arc<Mutex<local_clock::UnderhillLocalClock>>,
    pub get_backed_adjust_gpa_range:
        Option<Arc<Mutex<i440bx_host_pci_bridge::GetBackedAdjustGpaRange>>>,
    pub netvsp_state: Vec<netvsp::RuntimeSavedState>,
}

impl EmuplatServicing {
    pub fn save(&mut self) -> Result<EmuplatSavedState, SaveError> {
        Ok(EmuplatSavedState {
            rtc_local_clock: self.rtc_local_clock.lock().save()?,
            get_backed_adjust_gpa_range: {
                self.get_backed_adjust_gpa_range
                    .as_ref()
                    .map(|x| x.lock().save())
                    .transpose()?
            },
            netvsp_state: self.netvsp_state.iter().map(|x| x.into()).collect(),
        })
    }
}
