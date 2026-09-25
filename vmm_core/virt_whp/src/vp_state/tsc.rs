// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! IA32_TSC and IA32_TSC_DEADLINE state access with the restored-TSC clock.

use super::WhpVpStateAccess;
use crate::Error;
use crate::WhpResultExt;
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

    /// Returns the VTL0 TSC of the restored-TSC clock, unless the guest has
    /// programmed this VP's counter.
    pub(super) fn restored_tsc(&self) -> Result<Option<vp::Tsc>, Error> {
        if self.vtl == hvdef::Vtl::Vtl0 {
            let clock = self.run.vp.partition.clock.restored_tsc.lock();
            if let Some(clock) = &*clock {
                let reference = self
                    .run
                    .vp
                    .partition
                    .vtl0
                    .whp
                    .reference_time()
                    .for_op("read restored TSC reference time")?;
                if let Some(value) = clock.read(self.run.vp.index.index(), reference) {
                    return Ok(Some(vp::Tsc { value }));
                }
            }
        }
        Ok(None)
    }

    /// Restores this VP's VTL0 counter in the restored-TSC clock.
    pub(super) fn restore_tsc(&self, value: &vp::Tsc) -> Result<(), Error> {
        if self.vtl == hvdef::Vtl::Vtl0 {
            let mut clock = self.run.vp.partition.clock.restored_tsc.lock();
            if let Some(clock) = &mut *clock {
                let reference = self
                    .run
                    .vp
                    .partition
                    .vtl0
                    .whp
                    .reference_time()
                    .for_op("restore TSC reference time")?;
                clock.restore(self.run.vp.index.index(), value.value, reference);
            }
        }
        Ok(())
    }
}
