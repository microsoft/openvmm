// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! IA32_TSC_DEADLINE state access ordering.

use super::WhpVpStateAccess;
use crate::Error;
use virt::x86::vp;
use virt::x86::vp::AccessVpState;

/// IA32_TSC_DEADLINE read ordering for one state access.
#[derive(Default)]
pub(super) struct TscDeadlineRead {
    before_apic: Option<vp::TscDeadline>,
    read: bool,
}

impl WhpVpStateAccess<'_, '_> {
    /// State serialization reads APIC before IA32_TSC_DEADLINE. Read the
    /// deadline first so an expiry cannot fall between those snapshots.
    pub(super) fn read_tsc_deadline_before_apic(&mut self) -> Result<(), Error> {
        if self.caps().tsc_deadline && !self.tsc_deadline.read {
            self.tsc_deadline.before_apic = Some(self.run.vp.get_register_state(self.vtl)?);
        }
        Ok(())
    }

    /// Returns the IA32_TSC_DEADLINE value read before the APIC state, or
    /// reads it now.
    pub(super) fn saved_tsc_deadline(&mut self) -> Result<vp::TscDeadline, Error> {
        self.tsc_deadline.read = true;
        if let Some(value) = self.tsc_deadline.before_apic.take() {
            Ok(value)
        } else {
            self.run.vp.get_register_state(self.vtl)
        }
    }
}
